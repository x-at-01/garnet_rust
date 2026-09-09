use core::str::from_utf8;
use std::sync::atomic::Ordering;

use wdev::Device;
use wedb_hash::{ExpireOpt as HashExpireOpt, ExpireResult as HashExpireResult, FieldValueCodec};
use whasher::{HashSet, hash_set_with_capacity};
use wkv::{
  HASH_MAX_COMPACT_ENTRIES, HASH_MAX_COMPACT_VALUE, MAX_COMPACT_TOTAL_BYTES, StoreSession,
};
use wval::{CollectionType, CompactHashCodec, MetaValue, StorageEncoding};

use super::{zset::SCAN_RESERVE_CAP, *};
use crate::error::Result;

/// 将哈希从紧凑内联存储原子跃迁为打平分块存储（复用单缓冲区，消除提前落盘）
pub(crate) async fn promote_hash_to_flattened<D: Device>(
  session: &StoreSession<D>,
  _key: &[u8],
  meta: &mut MetaValue,
  compact_payload: &[u8],
) -> Result<()> {
  meta.set_encoding(StorageEncoding::Flattened);
  StoreSession::<D>::set_meta_chunk_info(&mut meta.reserved, 0, 0);

  let mut fields_to_append = Vec::new();
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  let mut val_buf = Vec::new();

  for entry in CompactHashCodec::iter(compact_payload) {
    if let Some(exp) = entry.expire_at_ms
      && exp <= now
    {
      continue;
    }
    let sub_k = session.hash_sub_key(meta.key_id, meta.version, entry.field);
    val_buf.clear();
    FieldValueCodec::encode_to_buf(entry.value, entry.expire_at_ms, &mut val_buf);
    session.upsert_raw(&sub_k, &val_buf).await?;
    fields_to_append.push(entry.field);
  }

  meta.size = fields_to_append.len() as u64;
  if !fields_to_append.is_empty() {
    session
      .append_hash_fields_batch(meta, &fields_to_append)
      .await?;
  }

  Ok(())
}

/// 紧凑哈希单遍融合清理：委托 wval::CompactHashCodec::purge_and_delete
///
/// 一次扫描同时完成过期条目淘汰与多字段删除（原地压缩，零额外堆分配），
/// 格式知识收敛回值层编解码器，本侧仅保留错误类型桥接
pub(crate) fn compact_hash_purge_and_delete(
  buf: &mut Vec<u8>,
  fields: &[&[u8]],
  now: u64,
) -> Result<(usize, usize)> {
  CompactHashCodec::purge_and_delete(buf, fields, now).map_err(Into::into)
}

/// 尝试将打平 Hash 降级收缩为 Compact 编码（元素数 <= 16 时）
pub(crate) async fn try_demote_hash<D: Device>(
  session: &StoreSession<D>,
  user_key: &[u8],
  meta: &mut MetaValue,
) -> Result<bool> {
  if meta.encoding() != StorageEncoding::Flattened
    || meta.size == 0
    || meta.size > AUTO_DEMOTE_MAX_SIZE
  {
    return Ok(false);
  }
  let (max_chunk_id, _) = StoreSession::<D>::get_meta_chunk_info(&meta.reserved);
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  let mut entries = Vec::with_capacity(meta.size as usize);
  let mut seen: HashSet<u128> = hash_set_with_capacity(meta.size as usize);
  let mut sub_keys_to_delete = Vec::new();

  'outer: for chunk_id in 0..=max_chunk_id {
    let chunk_k = session.hash_chunk_key(meta.key_id, meta.version, chunk_id);
    if let Some(buf) = session.read_raw(&chunk_k).await?
      && let Ok(iter) = wedb_hash::FieldChunkCodec::iter(&buf)
    {
      for field in iter {
        if seen.insert(whasher::fast_hash128(field)) {
          let sub_k = session.hash_sub_key(meta.key_id, meta.version, field);
          if let Some(raw) = session.read_raw(&sub_k).await? {
            sub_keys_to_delete.push(sub_k);
            if let Ok((expire_at, val)) = FieldValueCodec::decode(&raw)
              && expire_at.is_none_or(|exp| exp > now)
            {
              if field.len() > HASH_MAX_COMPACT_VALUE || val.len() > HASH_MAX_COMPACT_VALUE {
                return Ok(false);
              }
              entries.push((field.to_vec(), val.to_vec(), expire_at));
              if entries.len() >= meta.size as usize {
                break 'outer;
              }
            }
          }
        }
      }
    }
  }

  if entries.is_empty() {
    // 崩溃一致性（先 meta 后数据）：先递增版本写 meta 判死（size==0 删元记录），
    // 后删旧版本子键与分块；中途崩溃仅遗留旧版本孤儿子键，可被 is_stale_subkey 判死回收
    let old_version = meta.version;
    meta.bump_version();
    meta.size = 0;
    session.save_meta(user_key, meta).await?;
    for sub_k in sub_keys_to_delete {
      let _ = session.delete_raw(&sub_k).await?;
    }
    for chunk_id in 0..=max_chunk_id {
      let chunk_k = session.hash_chunk_key(meta.key_id, old_version, chunk_id);
      let _ = session.delete_raw(&chunk_k).await?;
    }
    return Ok(true);
  }

  let Ok(payload) = CompactHashCodec::encode(
    entries
      .iter()
      .map(|(f, v, exp)| (f.as_slice(), v.as_slice(), *exp)),
  ) else {
    return Ok(false);
  };

  if payload.len() > MAX_COMPACT_TOTAL_BYTES {
    return Ok(false);
  }

  // 崩溃一致性（先 meta 后数据）：先翻转编码写 compact meta（版本递增隔离新载荷），
  // 后删旧版本子键与分块。中途崩溃时读路径立即可从内联载荷自洽读取，遗留的旧版本
  // 孤儿子键带陈旧版本号，可被 is_stale_subkey 判死并由紧缩回收（泄漏有界可自愈）
  let old_version = meta.version;
  meta.bump_version();
  meta.set_encoding(StorageEncoding::Compact);
  meta.size = entries.len() as u64;
  StoreSession::<D>::set_meta_chunk_info(&mut meta.reserved, 0, 0);
  // 载荷含 TTL 字段则置位 reserved 标志（写路径据此守卫过期淘汰扫描）
  if entries.iter().any(|(_, _, exp)| exp.is_some()) {
    StoreSession::<D>::set_meta_has_expire(&mut meta.reserved);
  }
  session.save_compact_meta(user_key, meta, &payload).await?;

  // 清理旧版本的打平子键与分块（键名均携带 old_version）
  for sub_k in sub_keys_to_delete {
    let _ = session.delete_raw(&sub_k).await?;
  }
  for chunk_id in 0..=max_chunk_id {
    let chunk_k = session.hash_chunk_key(meta.key_id, old_version, chunk_id);
    let _ = session.delete_raw(&chunk_k).await?;
  }
  Ok(true)
}

/// HDEL 无锁内核（调用方必须已持有 `key` 的独占桶锁）
pub(crate) async fn hdel_unlocked<D: Device>(
  session: &StoreSession<D>,
  key: &[u8],
  fields: &[&[u8]],
) -> Result<usize> {
  let (mut meta, raw_opt) = match session
    .load_collection_raw_write(key, CollectionType::Hash)
    .await?
  {
    Some(res) => res,
    None => return Ok(0),
  };
  if meta.size == 0 {
    return Ok(0);
  }

  let now = coarsetime::Clock::now_since_epoch().as_millis();

  if meta.encoding() == StorageEncoding::Compact {
    if let Some(mut raw) = raw_opt {
      // 单遍融合：过期淘汰与多字段删除一次扫描完成（旧实现 purge + N 次 delete_field 共 N+1 遍）
      let (purged, del_cnt) = compact_hash_purge_and_delete(&mut raw, fields, now)?;
      if del_cnt > 0 || purged > 0 {
        meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
        session.save_compact_meta(key, &meta, &raw).await?;
      }
      return Ok(del_cnt);
    }
    return Ok(0);
  }

  let mut del_cnt = 0;
  let mut modified = false;
  for &f in fields {
    let sub_k = session.hash_sub_key(meta.key_id, meta.version, f);
    if let Some(raw) = session.read_raw(&sub_k).await? {
      let is_expired = match FieldValueCodec::decode(&raw) {
        Ok((Some(exp), _)) => exp <= now,
        _ => false,
      };
      if session.delete_raw(&sub_k).await? {
        meta.dec_size(1);
        modified = true;
        if !is_expired {
          del_cnt += 1;
        }
      }
    }
  }

  if modified {
    if meta.size == 0 {
      session.save_meta(key, &meta).await?;
    } else if meta.size <= AUTO_DEMOTE_MAX_SIZE {
      if !try_demote_hash(session, key, &mut meta).await? {
        session.save_meta(key, &meta).await?;
      }
    } else {
      session.save_meta(key, &meta).await?;
    }
  }
  Ok(del_cnt)
}

pub trait HashCommands<D: Device> {
  /// 设置哈希字段 (HSET)
  ///
  /// 入口获取目标键独占桶锁（两阶段锁）：跨 await 的 load -> 内存改 -> save 读改写窗口内
  /// 串行化同键写操作，杜绝并发连接静默丢更新（严格对标 Garnet LockAKey = Exclusive）
  async fn hset(
    &self,
    key: &[u8],
    field: impl AsRef<[u8]>,
    value: impl AsRef<[u8]>,
  ) -> Result<bool>;

  /// 仅当字段不存在时设置 (HSETNX，独占桶锁串行化同键读改写窗口)
  async fn hsetnx(
    &self,
    key: &[u8],
    field: impl AsRef<[u8]>,
    value: impl AsRef<[u8]>,
  ) -> Result<bool>;

  /// 批量设置多个字段 (HMSET，独占桶锁串行化同键读改写窗口)
  async fn hmset<F: AsRef<[u8]>, V: AsRef<[u8]>>(
    &self,
    key: &[u8],
    pairs: impl IntoIterator<Item = (F, V)>,
  ) -> Result<usize>;

  /// 获取指定字段的值 (HGET，支持惰性淘汰已过期字段，零内存搬移)
  async fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>>;

  /// 批量获取多个字段的值 (HMGET，单次元数据加载)
  ///
  /// Compact 编码对同一载荷切片做 N 次内存查找（O(payload + N)），Flattened 编码复用
  /// key_id/version 仅构造子键点查，均消除逐字段重读元数据与整载荷的 O(N×payload) 读放大
  async fn hmget(&self, key: &[u8], fields: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>>;

  /// 删除一个或多个字段 (HDEL，独占桶锁串行化同键读改写窗口)
  async fn hdel(&self, key: &[u8], fields: &[&[u8]]) -> Result<usize>;

  /// 获取字段数量 (HLEN，纯元数据探测，零紧凑载荷读取与解析)
  async fn hlen(&self, key: &[u8]) -> Result<usize>;

  /// 检查字段是否存在且未过期 (HEXISTS，零拷贝探测)
  async fn hexists(&self, key: &[u8], field: &[u8]) -> Result<bool>;

  /// 获取字段值长度 (HSTRLEN，零拷贝探测切片长度)
  async fn hstrlen(&self, key: &[u8], field: &[u8]) -> Result<usize>;

  /// 获取所有字段名 (HKEYS，只读取分块而不读取字段值)
  async fn hkeys(&self, key: &[u8]) -> Result<Vec<Vec<u8>>>;

  /// 获取所有值 (HVALS)
  async fn hvals(&self, key: &[u8]) -> Result<Vec<Vec<u8>>>;

  /// 整数递增 (HINCRBY，保留字段既有过期时间 TTL，独占桶锁串行化同键读改写窗口)
  async fn hincrby(&self, key: &[u8], field: &[u8], incr: i64) -> Result<i64>;

  /// 浮点数递增 (HINCRBYFLOAT，保留字段既有过期时间 TTL，独占桶锁串行化同键读改写窗口)
  async fn hincrbyfloat(&self, key: &[u8], field: &[u8], incr: f64) -> Result<f64>;

  /// 设置字段过期时间 (HEXPIRE，独占桶锁串行化同键读改写窗口)
  async fn hexpire(
    &self,
    key: &[u8],
    field: &[u8],
    expire_at_ms: u64,
    option: HashExpireOpt,
  ) -> Result<HashExpireResult>;

  /// 获取字段 TTL (HTTL)
  async fn httl(&self, key: &[u8], field: &[u8]) -> Result<i64>;

  /// 移除字段过期时间 (HPERSIST，独占桶锁串行化同键读改写窗口)
  async fn hpersist(&self, key: &[u8], field: &[u8]) -> Result<bool>;

  /// 获取哈希所有字段与值 (HGETALL)
  async fn hgetall(&self, key: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>>;

  /// 游标扫描哈希字段 (HSCAN，流式分块跳跃寻址与按需读取，早停机制严格限制内存)
  async fn hscan(
    &self,
    key: &[u8],
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> Result<(usize, Vec<(Vec<u8>, Vec<u8>)>)>;
}

impl<D: Device> HashCommands<D> for StoreSession<D> {
  /// 设置哈希字段 (HSET)
  ///
  /// 入口获取目标键独占桶锁（两阶段锁）：跨 await 的 load -> 内存改 -> save 读改写窗口内
  /// 串行化同键写操作，杜绝并发连接静默丢更新（严格对标 Garnet LockAKey = Exclusive）
  async fn hset(
    &self,
    key: &[u8],
    field: impl AsRef<[u8]>,
    value: impl AsRef<[u8]>,
  ) -> Result<bool> {
    let field = field.as_ref();
    let value = value.as_ref();
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;

    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => {
        let key_id = self.store.next_key_id.fetch_add(1, Ordering::Relaxed);
        if value.len() <= HASH_MAX_COMPACT_VALUE
          && field.len() + value.len() + 16 <= MAX_COMPACT_TOTAL_BYTES
        {
          let mut meta = MetaValue::new(key_id, CollectionType::Hash, 1, 1);
          meta.set_encoding(StorageEncoding::Compact);
          // 预估容量：2 字节头 + 字段/值长度前缀 + 过期标记，避免插入时反复扩容
          let mut raw = Vec::with_capacity(field.len() + value.len() + 16);
          CompactHashCodec::set_field(&mut raw, field, value, None)?;
          meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
          self.save_compact_meta(key, &meta, &raw).await?;
          return Ok(true);
        } else {
          let mut meta = MetaValue::new(key_id, CollectionType::Hash, 1, 1);
          meta.set_encoding(StorageEncoding::Flattened);
          Self::set_meta_chunk_info(&mut meta.reserved, 0, 0);
          let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
          let encoded = FieldValueCodec::encode_buf(value, None);
          self.upsert_raw(&sub_k, &encoded).await?;
          self.append_hash_field(&mut meta, field).await?;
          self.save_meta(key, &meta).await?;
          return Ok(true);
        }
      }
    };

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      let now = coarsetime::Clock::now_since_epoch().as_millis();
      // 仅当载荷可能含 TTL 字段时才做过期淘汰扫描（reserved 标志位守卫，无 TTL 键零扫描）
      if Self::get_meta_has_expire(&meta.reserved) {
        CompactHashCodec::purge_expired(&mut raw, now)?;
      }

      // 超大字段/值无法紧凑内联：先晋升既有载荷，再单独落盘该字段
      if value.len() > HASH_MAX_COMPACT_VALUE || field.len() > u16::MAX as usize {
        let is_new = CompactHashCodec::find(&raw, field).is_none();
        promote_hash_to_flattened(self, key, &mut meta, &raw).await?;
        let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
        let encoded_val = FieldValueCodec::encode_buf(value, None);
        self.upsert_raw(&sub_k, &encoded_val).await?;
        if is_new {
          meta.inc_size(1);
          self.append_hash_field(&mut meta, field).await?;
        }
        self.save_meta(key, &meta).await?;
        return Ok(is_new);
      }

      // 单遍投机写入：set_field 内部查找与改写融合并返回是否新插入，省去前置 find 全量扫描；
      // 越界（总字节或条目数超限）则带着已写入的新值整体晋升打平（晋升路径全量落盘并重计 size）
      let is_new = CompactHashCodec::set_field(&mut raw, field, value, None)?;
      if raw.len() > MAX_COMPACT_TOTAL_BYTES
        || CompactHashCodec::count(&raw).unwrap_or(0) > HASH_MAX_COMPACT_ENTRIES
      {
        promote_hash_to_flattened(self, key, &mut meta, &raw).await?;
        self.save_meta(key, &meta).await?;
      } else {
        meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
        self.save_compact_meta(key, &meta, &raw).await?;
      }
      return Ok(is_new);
    }

    // Flattened 分支
    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let old_raw = self.read_raw(&sub_k).await?;
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    // 区分"物理缺席真新增"与"过期/损坏记录原位替换"：惰性过期字段仍被 meta.size 计数
    // 且已登记分块索引，原位覆写时严禁重复计数或重复登记，否则 HLEN 永久虚高、分块索引膨胀
    // （对标 C# HashObjectImpl.HashSet 先 DeleteExpiredItems 再计数）
    let (is_new, physically_absent) = match old_raw {
      None => (true, true),
      Some(bytes) => match FieldValueCodec::decode(&bytes) {
        Ok((Some(exp), _)) if exp <= now => (true, false),
        Ok(_) => (false, false),
        Err(_) => (true, false),
      },
    };

    let encoded_val = FieldValueCodec::encode_buf(value, None);
    self.upsert_raw(&sub_k, &encoded_val).await?;

    if physically_absent {
      meta.inc_size(1);
      self.append_hash_field(&mut meta, field).await?;
    }
    self.save_meta(key, &meta).await?;

    Ok(is_new)
  }

  /// 仅当字段不存在时设置 (HSETNX，独占桶锁串行化同键读改写窗口)
  async fn hsetnx(
    &self,
    key: &[u8],
    field: impl AsRef<[u8]>,
    value: impl AsRef<[u8]>,
  ) -> Result<bool> {
    let field = field.as_ref();
    let value = value.as_ref();
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;

    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => {
        let key_id = self.store.next_key_id.fetch_add(1, Ordering::Relaxed);
        if value.len() <= HASH_MAX_COMPACT_VALUE
          && field.len() + value.len() + 16 <= MAX_COMPACT_TOTAL_BYTES
        {
          let mut meta = MetaValue::new(key_id, CollectionType::Hash, 1, 1);
          meta.set_encoding(StorageEncoding::Compact);
          // 预估容量：2 字节头 + 字段/值长度前缀 + 过期标记，避免插入时反复扩容
          let mut raw = Vec::with_capacity(field.len() + value.len() + 16);
          CompactHashCodec::set_field(&mut raw, field, value, None)?;
          meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
          self.save_compact_meta(key, &meta, &raw).await?;
          return Ok(true);
        } else {
          let mut meta = MetaValue::new(key_id, CollectionType::Hash, 1, 1);
          meta.set_encoding(StorageEncoding::Flattened);
          Self::set_meta_chunk_info(&mut meta.reserved, 0, 0);
          let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
          let encoded = FieldValueCodec::encode_buf(value, None);
          self.upsert_raw(&sub_k, &encoded).await?;
          self.append_hash_field(&mut meta, field).await?;
          self.save_meta(key, &meta).await?;
          return Ok(true);
        }
      }
    };

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      let now = coarsetime::Clock::now_since_epoch().as_millis();
      // 仅当载荷可能含 TTL 字段时才做过期淘汰扫描（reserved 标志位守卫，无 TTL 键零扫描）
      if Self::get_meta_has_expire(&meta.reserved) {
        CompactHashCodec::purge_expired(&mut raw, now)?;
      }
      if CompactHashCodec::find(&raw, field).is_some() {
        return Ok(false);
      }

      let cur_count = CompactHashCodec::count(&raw).unwrap_or(0);
      if value.len() > HASH_MAX_COMPACT_VALUE
        || cur_count >= HASH_MAX_COMPACT_ENTRIES
        || (raw.len() + field.len() + value.len() + 16 > MAX_COMPACT_TOTAL_BYTES)
      {
        promote_hash_to_flattened(self, key, &mut meta, &raw).await?;
        let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
        let encoded_val = FieldValueCodec::encode_buf(value, None);
        self.upsert_raw(&sub_k, &encoded_val).await?;
        meta.inc_size(1);
        self.append_hash_field(&mut meta, field).await?;
        self.save_meta(key, &meta).await?;
        return Ok(true);
      } else {
        CompactHashCodec::set_field(&mut raw, field, value, None)?;
        meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(true);
      }
    }

    // Flattened 分支
    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let old_raw = self.read_raw(&sub_k).await?;
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    if let Some(bytes) = old_raw.as_ref()
      && let Ok((expire_at, _)) = FieldValueCodec::decode(bytes)
      && expire_at.is_none_or(|exp| exp > now)
    {
      return Ok(false);
    }

    let encoded_val = FieldValueCodec::encode_buf(value, None);
    self.upsert_raw(&sub_k, &encoded_val).await?;
    // 物理缺席才计数登记；过期/损坏记录为原位替换（字段已在分块索引且被 size 计数）
    if old_raw.is_none() {
      meta.inc_size(1);
      self.append_hash_field(&mut meta, field).await?;
    }
    self.save_meta(key, &meta).await?;
    Ok(true)
  }

  /// 批量设置多个字段 (HMSET，独占桶锁串行化同键读改写窗口)
  async fn hmset<F: AsRef<[u8]>, V: AsRef<[u8]>>(
    &self,
    key: &[u8],
    pairs: impl IntoIterator<Item = (F, V)>,
  ) -> Result<usize> {
    let pair_vec: Vec<(F, V)> = pairs.into_iter().collect();
    if pair_vec.is_empty() {
      return Ok(0);
    }
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;

    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => {
        let key_id = self.store.next_key_id.fetch_add(1, Ordering::Relaxed);
        let has_large = pair_vec
          .iter()
          .any(|(_, v)| v.as_ref().len() > HASH_MAX_COMPACT_VALUE);
        let total_bytes: usize = pair_vec
          .iter()
          .map(|(f, v)| f.as_ref().len() + v.as_ref().len() + 16)
          .sum();
        if !has_large
          && pair_vec.len() <= HASH_MAX_COMPACT_ENTRIES
          && total_bytes <= MAX_COMPACT_TOTAL_BYTES
        {
          let mut meta = MetaValue::new(key_id, CollectionType::Hash, 1, 0);
          meta.set_encoding(StorageEncoding::Compact);
          // 预估总字节数已知，一次性分配消除批量插入过程中的反复扩容
          let mut raw = Vec::with_capacity(total_bytes);
          for (f, v) in &pair_vec {
            CompactHashCodec::set_field(&mut raw, f.as_ref(), v.as_ref(), None)?;
          }
          meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
          self.save_compact_meta(key, &meta, &raw).await?;
          return Ok(pair_vec.len());
        } else {
          let mut meta = MetaValue::new(key_id, CollectionType::Hash, 1, 0);
          meta.set_encoding(StorageEncoding::Flattened);
          Self::set_meta_chunk_info(&mut meta.reserved, 0, 0);
          (meta, None)
        }
      }
    };

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      let now = coarsetime::Clock::now_since_epoch().as_millis();
      // 仅当载荷可能含 TTL 字段时才做过期淘汰扫描（reserved 标志位守卫，无 TTL 键零扫描）
      if Self::get_meta_has_expire(&meta.reserved) {
        CompactHashCodec::purge_expired(&mut raw, now)?;
      }
      let cur_count = CompactHashCodec::count(&raw).unwrap_or(0);
      let has_large = pair_vec
        .iter()
        .any(|(_, v)| v.as_ref().len() > HASH_MAX_COMPACT_VALUE);
      let added_bytes: usize = pair_vec
        .iter()
        .map(|(f, v)| f.as_ref().len() + v.as_ref().len() + 16)
        .sum();
      if has_large
        || (cur_count + pair_vec.len() > HASH_MAX_COMPACT_ENTRIES)
        || (raw.len() + added_bytes > MAX_COMPACT_TOTAL_BYTES)
      {
        promote_hash_to_flattened(self, key, &mut meta, &raw).await?;
      } else {
        for (f, v) in &pair_vec {
          let field = f.as_ref();
          let value = v.as_ref();
          CompactHashCodec::set_field(&mut raw, field, value, None)?;
        }
        meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(pair_vec.len());
      }
    }

    // Flattened 分支
    let mut count = 0;
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let mut val_buf = Vec::new();
    let mut new_fields = Vec::with_capacity(pair_vec.len());
    for (f, v) in &pair_vec {
      let field = f.as_ref();
      let value = v.as_ref();
      let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
      let old_raw = self.read_raw(&sub_k).await?;
      // 过期/损坏记录为原位替换，不得重复计数或重复登记分块索引（对标 hset Flattened 分支）
      let (_, physically_absent) = match old_raw {
        None => (true, true),
        Some(bytes) => match FieldValueCodec::decode(&bytes) {
          Ok((Some(exp), _)) if exp <= now => (true, false),
          Ok(_) => (false, false),
          Err(_) => (true, false),
        },
      };

      val_buf.clear();
      FieldValueCodec::encode_to_buf(value, None, &mut val_buf);
      self.upsert_raw(&sub_k, &val_buf).await?;

      if physically_absent {
        meta.inc_size(1);
        new_fields.push(field);
      }
      count += 1;
    }

    if !new_fields.is_empty() {
      self
        .append_hash_fields_batch(&mut meta, &new_fields)
        .await?;
    }

    if count > 0 {
      self.save_meta(key, &meta).await?;
    }
    Ok(count)
  }

  /// 获取指定字段的值 (HGET，支持惰性淘汰已过期字段，零内存搬移)
  async fn hget(&self, key: &[u8], field: &[u8]) -> Result<Option<Vec<u8>>> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(None),
    };
    let meta = raw_read.meta;

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload()
        && let Some(entry) = CompactHashCodec::find(raw, field)
      {
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        if let Some(exp) = entry.expire_at_ms
          && exp <= now
        {
          self.hdel(key, &[field]).await?;
          return Ok(None);
        }
        return Ok(Some(entry.value.to_vec()));
      }
      return Ok(None);
    }

    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let raw = match self.read_raw(&sub_k).await? {
      Some(b) => b,
      None => return Ok(None),
    };

    let (expire_at, val) = FieldValueCodec::decode(&raw)?;
    if let Some(exp) = expire_at {
      let now = coarsetime::Clock::now_since_epoch().as_millis();
      if exp <= now {
        self.hdel(key, &[field]).await?;
        return Ok(None);
      }
    }

    Ok(Some(val.to_vec()))
  }

  /// 批量获取多个字段的值 (HMGET，单次元数据加载)
  ///
  /// Compact 编码对同一载荷切片做 N 次内存查找（O(payload + N)），Flattened 编码复用
  /// key_id/version 仅构造子键点查，均消除逐字段重读元数据与整载荷的 O(N×payload) 读放大
  async fn hmget(&self, key: &[u8], fields: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
    if fields.is_empty() {
      return Ok(Vec::new());
    }
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(vec![None; fields.len()]),
    };

    if raw_read.meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload() {
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        // 过期字段与 HGETALL/HKEYS 读路径同口径：跳过返回 None（惰性物理淘汰交给写路径）
        return Ok(
          fields
            .iter()
            .map(|&f| {
              CompactHashCodec::find(raw, f)
                .filter(|entry| entry.expire_at_ms.is_none_or(|exp| exp > now))
                .map(|entry| entry.value.to_vec())
            })
            .collect(),
        );
      }
      return Ok(vec![None; fields.len()]);
    }

    // Flattened 编码：闭包内解析过期前缀并仅拷贝值载荷（整记录零拷贝）
    let key_id = raw_read.meta.key_id;
    let version = raw_read.meta.version;
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let mut res = Vec::with_capacity(fields.len());
    for &f in fields {
      let sub_k = self.hash_sub_key(key_id, version, f);
      let decoded = self
        .read_raw_with(&sub_k, |raw| {
          FieldValueCodec::decode(raw).map(|(expire_at, val)| (expire_at, val.to_vec()))
        })
        .await?;
      res.push(match decoded {
        Some(pair) => {
          let (expire_at, val) = pair?;
          if expire_at.is_some_and(|exp| exp <= now) {
            None
          } else {
            Some(val)
          }
        }
        None => None,
      });
    }
    Ok(res)
  }

  /// 删除一个或多个字段 (HDEL，独占桶锁串行化同键读改写窗口)
  async fn hdel(&self, key: &[u8], fields: &[&[u8]]) -> Result<usize> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    hdel_unlocked(self, key, fields).await
  }

  /// 获取字段数量 (HLEN，纯元数据探测，零紧凑载荷读取与解析)
  async fn hlen(&self, key: &[u8]) -> Result<usize> {
    let meta = match self
      .load_collection_meta_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(0),
    };
    Ok(meta.size as usize)
  }

  /// 检查字段是否存在且未过期 (HEXISTS，零拷贝探测)
  async fn hexists(&self, key: &[u8], field: &[u8]) -> Result<bool> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(false),
    };
    let meta = raw_read.meta;

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload()
        && let Some(entry) = CompactHashCodec::find(raw, field)
      {
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        if let Some(exp) = entry.expire_at_ms
          && exp <= now
        {
          self.hdel(key, &[field]).await?;
          return Ok(false);
        }
        return Ok(true);
      }
      return Ok(false);
    }

    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let raw = match self.read_raw(&sub_k).await? {
      Some(b) => b,
      None => return Ok(false),
    };

    let (expire_at, _) = FieldValueCodec::decode(&raw)?;
    if let Some(exp) = expire_at {
      let now = coarsetime::Clock::now_since_epoch().as_millis();
      if exp <= now {
        self.hdel(key, &[field]).await?;
        return Ok(false);
      }
    }

    Ok(true)
  }

  /// 获取字段值长度 (HSTRLEN，零拷贝探测切片长度)
  async fn hstrlen(&self, key: &[u8], field: &[u8]) -> Result<usize> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(0),
    };
    let meta = raw_read.meta;

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload()
        && let Some(entry) = CompactHashCodec::find(raw, field)
      {
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        if let Some(exp) = entry.expire_at_ms
          && exp <= now
        {
          self.hdel(key, &[field]).await?;
          return Ok(0);
        }
        return Ok(entry.value.len());
      }
      return Ok(0);
    }

    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let raw = match self.read_raw(&sub_k).await? {
      Some(b) => b,
      None => return Ok(0),
    };

    let (expire_at, val) = FieldValueCodec::decode(&raw)?;
    if let Some(exp) = expire_at {
      let now = coarsetime::Clock::now_since_epoch().as_millis();
      if exp <= now {
        self.hdel(key, &[field]).await?;
        return Ok(0);
      }
    }

    Ok(val.len())
  }

  /// 获取所有字段名 (HKEYS，只读取分块而不读取字段值)
  async fn hkeys(&self, key: &[u8]) -> Result<Vec<Vec<u8>>> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(Vec::new()),
    };
    let meta = raw_read.meta;
    if meta.size == 0 {
      return Ok(Vec::new());
    }

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload() {
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        let cap = CompactHashCodec::count(raw).unwrap_or(0);
        let mut fields = Vec::with_capacity(cap);
        for e in CompactHashCodec::iter(raw) {
          if e.expire_at_ms.is_none_or(|exp| exp > now) {
            fields.push(e.field.to_vec());
          }
        }
        return Ok(fields);
      }
      return Ok(Vec::new());
    }

    let (max_chunk_id, _) = Self::get_meta_chunk_info(&meta.reserved);
    let cap = (meta.size as usize).min(65536);
    let mut seen: HashSet<u128> = hash_set_with_capacity(cap);
    let mut results = Vec::with_capacity(cap);
    let now = coarsetime::Clock::now_since_epoch().as_millis();

    for chunk_id in 0..=max_chunk_id {
      let chunk_k = self.hash_chunk_key(meta.key_id, meta.version, chunk_id);
      let buf = match self.read_raw(&chunk_k).await? {
        Some(b) => b,
        None => continue,
      };
      if let Ok(iter) = wedb_hash::FieldChunkCodec::iter(&buf) {
        for field in iter {
          if seen.insert(whasher::fast_hash128(field)) {
            let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
            // 闭包内只解析 expire 前缀判定存活，不触碰 value（避免仅为头部信息拷贝整条记录）
            let alive = self
              .read_raw_with(&sub_k, |raw| {
                matches!(FieldValueCodec::decode(raw), Ok((exp, _)) if exp.is_none_or(|e| e > now))
              })
              .await?
              .unwrap_or(false);
            if alive {
              results.push(field.to_vec());
              if results.len() >= meta.size as usize {
                return Ok(results);
              }
            }
          }
        }
      }
    }

    Ok(results)
  }

  /// 获取所有值 (HVALS)
  async fn hvals(&self, key: &[u8]) -> Result<Vec<Vec<u8>>> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(Vec::new()),
    };
    let meta = raw_read.meta;
    if meta.size == 0 {
      return Ok(Vec::new());
    }

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload() {
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        let cap = CompactHashCodec::count(raw).unwrap_or(0);
        let mut vals = Vec::with_capacity(cap);
        for e in CompactHashCodec::iter(raw) {
          if e.expire_at_ms.is_none_or(|exp| exp > now) {
            vals.push(e.value.to_vec());
          }
        }
        return Ok(vals);
      }
      return Ok(Vec::new());
    }

    let (max_chunk_id, _) = Self::get_meta_chunk_info(&meta.reserved);
    let cap = (meta.size as usize).min(65536);
    let mut seen: HashSet<u128> = hash_set_with_capacity(cap);
    let mut results = Vec::with_capacity(cap);
    let now = coarsetime::Clock::now_since_epoch().as_millis();

    for chunk_id in 0..=max_chunk_id {
      let chunk_k = self.hash_chunk_key(meta.key_id, meta.version, chunk_id);
      let buf = match self.read_raw(&chunk_k).await? {
        Some(b) => b,
        None => continue,
      };
      if let Ok(iter) = wedb_hash::FieldChunkCodec::iter(&buf) {
        for field in iter {
          if seen.insert(whasher::fast_hash128(field)) {
            let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
            // 闭包内解析并仅拷贝值载荷（消除整条记录堆拷贝后再二次拷贝值的读放大）
            // 外层 None 为子键缺席，内层 None 为过期或损坏记录
            let decoded = self
              .read_raw_with(&sub_k, |raw| match FieldValueCodec::decode(raw) {
                Ok((exp, val)) if exp.is_none_or(|e| e > now) => Some(val.to_vec()),
                _ => None,
              })
              .await?;
            if let Some(Some(val)) = decoded {
              results.push(val);
              if results.len() >= meta.size as usize {
                return Ok(results);
              }
            }
          }
        }
      }
    }

    Ok(results)
  }

  /// 整数递增 (HINCRBY，保留字段既有过期时间 TTL，独占桶锁串行化同键读改写窗口)
  async fn hincrby(&self, key: &[u8], field: &[u8], incr: i64) -> Result<i64> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => {
        let key_id = self.store.next_key_id.fetch_add(1, Ordering::Relaxed);
        let mut itoa_buf = itoa::Buffer::new();
        let val_bytes = itoa_buf.format(incr).as_bytes();
        let mut meta = MetaValue::new(key_id, CollectionType::Hash, 1, 1);
        meta.set_encoding(StorageEncoding::Compact);
        // 预估容量：2 字节头 + 字段/值长度前缀 + 过期标记，避免插入时反复扩容
        let mut raw = Vec::with_capacity(field.len() + val_bytes.len() + 16);
        CompactHashCodec::set_field(&mut raw, field, val_bytes, None)?;
        meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(incr);
      }
    };

    let now = coarsetime::Clock::now_since_epoch().as_millis();

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      // 仅当载荷可能含 TTL 字段时才做过期淘汰扫描（reserved 标志位守卫，无 TTL 键零扫描）
      if Self::get_meta_has_expire(&meta.reserved) {
        CompactHashCodec::purge_expired(&mut raw, now)?;
      }
      let found = CompactHashCodec::find(&raw, field);
      let (current_val, expire_at_ms) = match found {
        Some(entry) => {
          let s = from_utf8(entry.value).map_err(|_| wedb_hash::Error::InvalidNumber)?;
          let n = s
            .parse::<i64>()
            .map_err(|_| wedb_hash::Error::InvalidNumber)?;
          (n, entry.expire_at_ms)
        }
        None => (0, None),
      };

      let new_val = current_val
        .checked_add(incr)
        .ok_or(wedb_hash::Error::InvalidNumber)?;
      let mut itoa_buf = itoa::Buffer::new();
      let val_bytes = itoa_buf.format(new_val).as_bytes();

      // 是否新字段直接取自上文单次 find 结果，消除重复全量扫描
      let is_new = found.is_none();
      let cur_count = CompactHashCodec::count(&raw).unwrap_or(0);
      if val_bytes.len() > HASH_MAX_COMPACT_VALUE
        || (is_new && cur_count >= HASH_MAX_COMPACT_ENTRIES)
        || (raw.len() + field.len() + val_bytes.len() + 16 > MAX_COMPACT_TOTAL_BYTES)
      {
        promote_hash_to_flattened(self, key, &mut meta, &raw).await?;
        let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
        let encoded = FieldValueCodec::encode_buf(val_bytes, expire_at_ms);
        self.upsert_raw(&sub_k, &encoded).await?;
        if is_new {
          meta.inc_size(1);
          self.append_hash_field(&mut meta, field).await?;
        }
        self.save_meta(key, &meta).await?;
        return Ok(new_val);
      } else {
        CompactHashCodec::set_field(&mut raw, field, val_bytes, expire_at_ms)?;
        if expire_at_ms.is_some() {
          // 载荷仍含 TTL 字段，置位 reserved 标志保持后续淘汰扫描守卫生效
          Self::set_meta_has_expire(&mut meta.reserved);
        }
        meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(new_val);
      }
    }

    // Flattened 分支
    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let old_raw = self.read_raw(&sub_k).await?;
    // 过期/损坏记录为原位替换（保留既有 TTL 语义清零），不得重复计数或重复登记分块索引
    let (current_val, expire_at_ms, physically_absent) = match old_raw {
      Some(bytes) => match FieldValueCodec::decode(&bytes) {
        Ok((Some(exp), _)) if exp <= now => (0, None, false),
        Ok((exp, val_slice)) => {
          let s = from_utf8(val_slice).map_err(|_| wedb_hash::Error::InvalidNumber)?;
          let n = s
            .parse::<i64>()
            .map_err(|_| wedb_hash::Error::InvalidNumber)?;
          (n, exp, false)
        }
        Err(_) => (0, None, false),
      },
      None => (0, None, true),
    };

    let new_val = current_val
      .checked_add(incr)
      .ok_or(wedb_hash::Error::InvalidNumber)?;
    let mut itoa_buf = itoa::Buffer::new();
    let val_bytes = itoa_buf.format(new_val).as_bytes();

    let encoded = FieldValueCodec::encode_buf(val_bytes, expire_at_ms);
    self.upsert_raw(&sub_k, &encoded).await?;

    if physically_absent {
      meta.inc_size(1);
      self.append_hash_field(&mut meta, field).await?;
    }
    self.save_meta(key, &meta).await?;

    Ok(new_val)
  }

  /// 浮点数递增 (HINCRBYFLOAT，保留字段既有过期时间 TTL，独占桶锁串行化同键读改写窗口)
  async fn hincrbyfloat(&self, key: &[u8], field: &[u8], incr: f64) -> Result<f64> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => {
        if incr.is_nan() || incr.is_infinite() {
          return Err(wedb_hash::Error::InvalidNumber.into());
        }
        let key_id = self.store.next_key_id.fetch_add(1, Ordering::Relaxed);
        let mut zmij_buf = zmij::Buffer::new();
        let val_bytes = zmij_buf.format(incr).as_bytes();
        let mut meta = MetaValue::new(key_id, CollectionType::Hash, 1, 1);
        meta.set_encoding(StorageEncoding::Compact);
        // 预估容量：2 字节头 + 字段/值长度前缀 + 过期标记，避免插入时反复扩容
        let mut raw = Vec::with_capacity(field.len() + val_bytes.len() + 16);
        CompactHashCodec::set_field(&mut raw, field, val_bytes, None)?;
        meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(incr);
      }
    };

    let now = coarsetime::Clock::now_since_epoch().as_millis();

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      // 仅当载荷可能含 TTL 字段时才做过期淘汰扫描（reserved 标志位守卫，无 TTL 键零扫描，
      // 对齐 hset/hincrby 写法）
      if Self::get_meta_has_expire(&meta.reserved) {
        CompactHashCodec::purge_expired(&mut raw, now)?;
      }
      let (current_val, expire_at_ms) = match CompactHashCodec::find(&raw, field) {
        Some(entry) => {
          let s = from_utf8(entry.value).map_err(|_| wedb_hash::Error::InvalidNumber)?;
          let n = s
            .parse::<f64>()
            .map_err(|_| wedb_hash::Error::InvalidNumber)?;
          (n, entry.expire_at_ms)
        }
        None => (0.0, None),
      };

      let new_val = current_val + incr;
      if new_val.is_nan() || new_val.is_infinite() {
        return Err(wedb_hash::Error::InvalidNumber.into());
      }
      let mut zmij_buf = zmij::Buffer::new();
      let val_bytes = zmij_buf.format(new_val).as_bytes();

      let is_new = CompactHashCodec::find(&raw, field).is_none();
      let cur_count = CompactHashCodec::count(&raw).unwrap_or(0);
      if val_bytes.len() > HASH_MAX_COMPACT_VALUE
        || (is_new && cur_count >= HASH_MAX_COMPACT_ENTRIES)
        || (raw.len() + field.len() + val_bytes.len() + 16 > MAX_COMPACT_TOTAL_BYTES)
      {
        promote_hash_to_flattened(self, key, &mut meta, &raw).await?;
        let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
        let encoded = FieldValueCodec::encode_buf(val_bytes, expire_at_ms);
        self.upsert_raw(&sub_k, &encoded).await?;
        if is_new {
          meta.inc_size(1);
          self.append_hash_field(&mut meta, field).await?;
        }
        self.save_meta(key, &meta).await?;
        return Ok(new_val);
      } else {
        CompactHashCodec::set_field(&mut raw, field, val_bytes, expire_at_ms)?;
        if expire_at_ms.is_some() {
          // 载荷仍含 TTL 字段，置位 reserved 标志保持后续淘汰扫描守卫生效
          Self::set_meta_has_expire(&mut meta.reserved);
        }
        meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(new_val);
      }
    }

    // Flattened 分支
    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let old_raw = self.read_raw(&sub_k).await?;
    // 过期/损坏记录为原位替换（保留既有 TTL 语义清零），不得重复计数或重复登记分块索引
    let (current_val, expire_at_ms, physically_absent) = match old_raw {
      Some(bytes) => match FieldValueCodec::decode(&bytes) {
        Ok((Some(exp), _)) if exp <= now => (0.0, None, false),
        Ok((exp, val_slice)) => {
          let s = from_utf8(val_slice).map_err(|_| wedb_hash::Error::InvalidNumber)?;
          let n = s
            .parse::<f64>()
            .map_err(|_| wedb_hash::Error::InvalidNumber)?;
          (n, exp, false)
        }
        Err(_) => (0.0, None, false),
      },
      None => (0.0, None, true),
    };

    let new_val = current_val + incr;
    if new_val.is_nan() || new_val.is_infinite() {
      return Err(wedb_hash::Error::InvalidNumber.into());
    }
    let mut zmij_buf = zmij::Buffer::new();
    let val_bytes = zmij_buf.format(new_val).as_bytes();

    let encoded = FieldValueCodec::encode_buf(val_bytes, expire_at_ms);
    self.upsert_raw(&sub_k, &encoded).await?;

    if physically_absent {
      meta.inc_size(1);
      self.append_hash_field(&mut meta, field).await?;
    }
    self.save_meta(key, &meta).await?;

    Ok(new_val)
  }

  /// 设置字段过期时间 (HEXPIRE，独占桶锁串行化同键读改写窗口)
  async fn hexpire(
    &self,
    key: &[u8],
    field: &[u8],
    expire_at_ms: u64,
    option: HashExpireOpt,
  ) -> Result<HashExpireResult> {
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(HashExpireResult::KeyNotFound),
    };

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(mut raw) = raw_opt {
        // 仅当载荷可能含 TTL 字段时才做过期淘汰扫描（reserved 标志位守卫，无 TTL 键零扫描）
        if Self::get_meta_has_expire(&meta.reserved) {
          CompactHashCodec::purge_expired(&mut raw, now)?;
        }
        let Some(entry) = CompactHashCodec::find(&raw, field) else {
          return Ok(HashExpireResult::KeyNotFound);
        };
        let curr_exp = entry.expire_at_ms;

        // 先校验选项冲突，后处理过去时间戳 (对齐 hash.rs::hexpire 与 Redis 7.4 判序)
        if let Some(curr) = curr_exp {
          if option.nx {
            return Ok(HashExpireResult::ExpireConditionNotMet);
          }
          if option.gt && expire_at_ms <= curr {
            return Ok(HashExpireResult::ExpireConditionNotMet);
          }
          if option.lt && expire_at_ms >= curr {
            return Ok(HashExpireResult::ExpireConditionNotMet);
          }
        } else if option.xx || option.gt {
          return Ok(HashExpireResult::ExpireConditionNotMet);
        }

        if expire_at_ms <= now {
          let _ = CompactHashCodec::delete_field(&mut raw, field);
          meta.size = CompactHashCodec::count(&raw).unwrap_or(0) as u64;
          self.save_compact_meta(key, &meta, &raw).await?;
          return Ok(HashExpireResult::KeyAlreadyExpired);
        }

        // 栈缓冲零堆分配提取紧凑值切片，超长安全回退
        const STACK_LIMIT: usize = 256;
        let val_len = entry.value.len();
        if val_len <= STACK_LIMIT {
          let mut val_stack = [0u8; STACK_LIMIT];
          val_stack[..val_len].copy_from_slice(entry.value);
          CompactHashCodec::set_field(&mut raw, field, &val_stack[..val_len], Some(expire_at_ms))?;
        } else {
          let val_vec = entry.value.to_vec();
          CompactHashCodec::set_field(&mut raw, field, &val_vec, Some(expire_at_ms))?;
        }
        // 载荷自此含 TTL 字段，置位 reserved 标志保持后续淘汰扫描守卫生效
        Self::set_meta_has_expire(&mut meta.reserved);
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(HashExpireResult::Ok);
      } else {
        return Ok(HashExpireResult::KeyNotFound);
      }
    }

    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let raw = match self.read_raw(&sub_k).await? {
      Some(b) => b,
      None => return Ok(HashExpireResult::KeyNotFound),
    };

    let (curr_exp, val) = FieldValueCodec::decode(&raw)?;

    // 已过期未淘字段：物理清理后按不存在处理 (对齐 hash.rs check_and_purge_expired 口径)
    // 已持本键独占桶锁，必须复用无锁内核，重入 hdel 会自旋等待自身锁直至 LockTimeout
    if let Some(c) = curr_exp
      && c <= now
    {
      hdel_unlocked(self, key, &[field]).await?;
      return Ok(HashExpireResult::KeyNotFound);
    }

    // 先校验选项冲突，后处理过去时间戳 (对齐 hash.rs::hexpire 与 Redis 7.4 判序)
    if let Some(curr) = curr_exp {
      if option.nx {
        return Ok(HashExpireResult::ExpireConditionNotMet);
      }
      if option.gt && expire_at_ms <= curr {
        return Ok(HashExpireResult::ExpireConditionNotMet);
      }
      if option.lt && expire_at_ms >= curr {
        return Ok(HashExpireResult::ExpireConditionNotMet);
      }
    } else if option.xx || option.gt {
      return Ok(HashExpireResult::ExpireConditionNotMet);
    }

    // 过去时间戳即过期即删：已持本键独占桶锁，复用无锁内核避免桶锁重入自锁
    if expire_at_ms <= now {
      hdel_unlocked(self, key, &[field]).await?;
      return Ok(HashExpireResult::KeyAlreadyExpired);
    }

    let encoded = FieldValueCodec::encode_buf(val, Some(expire_at_ms));
    self.upsert_raw(&sub_k, &encoded).await?;
    Ok(HashExpireResult::Ok)
  }

  /// 获取字段 TTL (HTTL)
  async fn httl(&self, key: &[u8], field: &[u8]) -> Result<i64> {
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(-2),
    };
    let meta = raw_read.meta;

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload()
        && let Some(entry) = CompactHashCodec::find(raw, field)
      {
        match entry.expire_at_ms {
          None => return Ok(-1),
          Some(exp) => {
            if exp <= now {
              self.hdel(key, &[field]).await?;
              return Ok(-2);
            } else {
              return Ok((exp - now) as i64);
            }
          }
        }
      }
      return Ok(-2);
    }

    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let raw = match self.read_raw(&sub_k).await? {
      Some(b) => b,
      None => return Ok(-2),
    };

    let (expire_at, _) = FieldValueCodec::decode(&raw)?;
    match expire_at {
      None => Ok(-1),
      Some(exp) => {
        if exp <= now {
          self.hdel(key, &[field]).await?;
          Ok(-2)
        } else {
          Ok((exp - now) as i64)
        }
      }
    }
  }

  /// 移除字段过期时间 (HPERSIST，独占桶锁串行化同键读改写窗口)
  async fn hpersist(&self, key: &[u8], field: &[u8]) -> Result<bool> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let (meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(false),
    };

    let now = coarsetime::Clock::now_since_epoch().as_millis();

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(mut raw) = raw_opt {
        // 仅当载荷可能含 TTL 字段时才做过期淘汰扫描（reserved 标志位守卫，对齐 hincrbyfloat）
        if Self::get_meta_has_expire(&meta.reserved) {
          CompactHashCodec::purge_expired(&mut raw, now)?;
        }
        let entry = match CompactHashCodec::find(&raw, field) {
          Some(e) if e.expire_at_ms.is_some() => e,
          _ => return Ok(false),
        };

        const STACK_LIMIT: usize = 256;
        let val_len = entry.value.len();
        if val_len <= STACK_LIMIT {
          let mut val_stack = [0u8; STACK_LIMIT];
          val_stack[..val_len].copy_from_slice(entry.value);
          CompactHashCodec::set_field(&mut raw, field, &val_stack[..val_len], None)?;
        } else {
          let val_vec = entry.value.to_vec();
          CompactHashCodec::set_field(&mut raw, field, &val_vec, None)?;
        }
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(true);
      }
      return Ok(false);
    }

    let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
    let raw = match self.read_raw(&sub_k).await? {
      Some(b) => b,
      None => return Ok(false),
    };

    let (expire_at, val) = FieldValueCodec::decode(&raw)?;
    if expire_at.is_none() {
      return Ok(false);
    }

    let now = coarsetime::Clock::now_since_epoch().as_millis();
    // 已过期字段：物理清理后按无 TTL 语义返回 false（已持本键独占桶锁，复用无锁内核避免桶锁重入自锁）
    if let Some(exp) = expire_at
      && exp <= now
    {
      hdel_unlocked(self, key, &[field]).await?;
      return Ok(false);
    }

    let encoded = FieldValueCodec::encode_buf(val, None);
    self.upsert_raw(&sub_k, &encoded).await?;
    Ok(true)
  }

  /// 获取哈希所有字段与值 (HGETALL)
  async fn hgetall(&self, key: &[u8]) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok(Vec::new()),
    };
    let meta = raw_read.meta;
    if meta.size == 0 {
      return Ok(Vec::new());
    }

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload() {
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        let cap = CompactHashCodec::count(raw).unwrap_or(0);
        let mut entries = Vec::with_capacity(cap);
        for e in CompactHashCodec::iter(raw) {
          if e.expire_at_ms.is_none_or(|exp| exp > now) {
            entries.push((e.field.to_vec(), e.value.to_vec()));
          }
        }
        return Ok(entries);
      }
      return Ok(Vec::new());
    }

    let (max_chunk_id, _) = Self::get_meta_chunk_info(&meta.reserved);
    let cap = (meta.size as usize).min(65536);
    let mut seen: HashSet<u128> = hash_set_with_capacity(cap);
    let mut results = Vec::with_capacity(cap);
    let now = coarsetime::Clock::now_since_epoch().as_millis();

    for chunk_id in 0..=max_chunk_id {
      let chunk_k = self.hash_chunk_key(meta.key_id, meta.version, chunk_id);
      let buf = match self.read_raw(&chunk_k).await? {
        Some(b) => b,
        None => continue,
      };
      if let Ok(iter) = wedb_hash::FieldChunkCodec::iter(&buf) {
        for field in iter {
          if seen.insert(whasher::fast_hash128(field)) {
            let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
            // 闭包内解析并仅拷贝值载荷（消除整条记录堆拷贝后再二次拷贝值的读放大）
            // 外层 None 为子键缺席，内层 None 为过期或损坏记录
            let decoded = self
              .read_raw_with(&sub_k, |raw| match FieldValueCodec::decode(raw) {
                Ok((exp, val)) if exp.is_none_or(|e| e > now) => Some(val.to_vec()),
                _ => None,
              })
              .await?;
            if let Some(Some(val)) = decoded {
              results.push((field.to_vec(), val));
              if results.len() >= meta.size as usize {
                return Ok(results);
              }
            }
          }
        }
      }
    }

    Ok(results)
  }

  /// 游标扫描哈希字段 (HSCAN，流式分块跳跃寻址与按需读取，早停机制严格限制内存)
  async fn hscan(
    &self,
    key: &[u8],
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> Result<(usize, Vec<(Vec<u8>, Vec<u8>)>)> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Hash)
      .await?
    {
      Some(res) => res,
      None => return Ok((0, Vec::new())),
    };
    let meta = raw_read.meta;
    if meta.size == 0 {
      return Ok((0, Vec::new()));
    }

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload() {
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        let pat = pattern.unwrap_or(b"*");
        let limit = if count == 0 { 10 } else { count };

        // 存活成员总数：游标终止基准 (对标 C# HashObject.Scan 的 expiredKeysCount 修正)
        // C# 口径：游标 = 非过期成员遍历序位置，先按位置 skip 再应用 pattern 过滤；
        // 每检视一个存活成员推进一次游标，收满 limit 个匹配即停；
        // 单遍边走边判定替代存活总数预扫描 + 正式扫描的两遍遍历：迭代自然耗尽即本轮终点（游标归零），
        // 收满 limit 时探针窥视一位即知是否尚有存活成员，无需预知总数
        let mut items: Vec<(Vec<u8>, Vec<u8>)> = Vec::with_capacity(limit.min(SCAN_RESERVE_CAP));
        let mut alive_iter = CompactHashCodec::iter(raw)
          .filter(|e| e.expire_at_ms.is_none_or(|exp| exp > now))
          .skip(cursor)
          .enumerate();
        while let Some((pos, e)) = alive_iter.next() {
          if glob_match(pat, e.field) {
            items.push((e.field.to_vec(), e.value.to_vec()));
            if items.len() >= limit {
              let next_cur = if alive_iter.next().is_none() {
                0
              } else {
                pos + 1
              };
              return Ok((next_cur, items));
            }
          }
        }
        return Ok((0, items));
      }
      return Ok((0, Vec::new()));
    }

    let (max_chunk_id, _) = Self::get_meta_chunk_info(&meta.reserved);
    let cap = Self::CHUNK_CAPACITY as usize;
    let chunk_idx = cursor / cap;
    if chunk_idx > max_chunk_id as usize {
      return Ok((0, Vec::new()));
    }
    let start_chunk_id = chunk_idx as u32;
    let start_elem_idx = cursor % cap;

    let limit = if count == 0 { 10 } else { count };
    let mut items = Vec::with_capacity(limit.min(SCAN_RESERVE_CAP));
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let mut seen: HashSet<u128> = hash_set_with_capacity(limit.min(SCAN_RESERVE_CAP));

    for chunk_id in start_chunk_id..=max_chunk_id {
      let chunk_k = self.hash_chunk_key(meta.key_id, meta.version, chunk_id);
      let buf = match self.read_raw(&chunk_k).await? {
        Some(b) => b,
        None => continue,
      };

      if let Ok(iter) = wedb_hash::FieldChunkCodec::iter(&buf) {
        let total_in_chunk = iter.len();
        for (elem_idx, field) in iter.enumerate() {
          if chunk_id == start_chunk_id && elem_idx < start_elem_idx {
            continue;
          }

          if seen.insert(whasher::fast_hash128(field)) {
            let sub_k = self.hash_sub_key(meta.key_id, meta.version, field);
            if let Some(raw) = self.read_raw(&sub_k).await?
              && let Ok((expire_at, val)) = FieldValueCodec::decode(&raw)
            {
              if let Some(exp) = expire_at
                && exp <= now
              {
                continue;
              }

              let matches = match pattern {
                Some(p) => glob_match(p, field),
                None => true,
              };
              if matches {
                items.push((field.to_vec(), val.to_vec()));

                if items.len() >= limit {
                  let next_cursor = if elem_idx + 1 < total_in_chunk && elem_idx + 1 < cap {
                    (chunk_id as usize)
                      .saturating_mul(cap)
                      .saturating_add(elem_idx + 1)
                  } else if chunk_id < max_chunk_id {
                    (chunk_id as usize).saturating_add(1).saturating_mul(cap)
                  } else {
                    0
                  };
                  return Ok((next_cursor, items));
                }
              }
            }
          }
        }
      }
    }

    Ok((0, items))
  }
}
