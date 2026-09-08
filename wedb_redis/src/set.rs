use std::sync::atomic::Ordering;

use wdev::Device;
use whasher::{HashSet, hash_set_with_capacity, new_hash_set};
use wkv::{
  MAX_COMPACT_TOTAL_BYTES, RawCollectionRead, SET_MAX_COMPACT_ENTRIES, SET_MAX_COMPACT_VALUE,
  StoreSession,
};
use wrecord::{CollectionType, CompactSetCodec, MetaValue, StorageEncoding};

use super::{
  zset::{SCAN_RESERVE_CAP, SRANDMEMBER_MAX_SAMPLE},
  *,
};
use crate::error::Result;

/// 集合成员归属探针（SINTERCARD 预加载：循环前一次性装载，循环内零元数据重读）
enum SetProbe<'a> {
  /// 紧凑载荷一次性物化为成员哈希集（零拷贝借用成员切片）
  Compact(HashSet<&'a [u8]>),
  /// 打平编码：复用 key_id/version 仅构造子键点查
  Flattened { key_id: u64, version: u64 },
}

impl SetProbe<'_> {
  /// 成员归属判定：Compact 纯内存哈希集 O(1)，Flattened 单子键点查
  async fn contains<D: Device>(&self, session: &StoreSession<D>, member: &[u8]) -> Result<bool> {
    match self {
      Self::Compact(set) => Ok(set.contains(member)),
      Self::Flattened { key_id, version } => {
        let sub_k = session.set_sub_key(*key_id, *version, member);
        Ok(session.contains_key_raw(&sub_k).await?)
      }
    }
  }
}

/// 将集合从紧凑内联存储原子跃迁为打平分块存储（消除提前落盘，等待调用方合并提交）
pub(crate) async fn promote_set_to_flattened<D: Device>(
  session: &StoreSession<D>,
  _key: &[u8],
  meta: &mut MetaValue,
  compact_payload: &[u8],
) -> Result<()> {
  meta.set_encoding(StorageEncoding::Flattened);
  StoreSession::<D>::set_meta_chunk_info(&mut meta.reserved, 0, 0);

  let mut members_to_append = Vec::new();
  for member in CompactSetCodec::iter_members(compact_payload) {
    let sub_k = session.set_sub_key(meta.key_id, meta.version, member);
    session.upsert_raw(&sub_k, &[]).await?;
    members_to_append.push(member);
  }

  meta.size = members_to_append.len() as u64;
  if !members_to_append.is_empty() {
    session
      .append_set_members_batch(meta, &members_to_append)
      .await?;
  }

  Ok(())
}

/// SADD 无锁内核（调用方必须已持有 `key` 的独占桶锁）
pub(crate) async fn sadd_unlocked<M: AsRef<[u8]>, D: Device>(
  session: &StoreSession<D>,
  key: &[u8],
  members: impl IntoIterator<Item = M>,
) -> Result<usize> {
  let member_vec: Vec<M> = members.into_iter().collect();
  if member_vec.is_empty() {
    return Ok(0);
  }

  let (mut meta, raw_opt) = match session
    .load_collection_raw_write(key, CollectionType::Set)
    .await?
  {
    Some(res) => res,
    None => {
      let key_id = session.store.next_key_id.fetch_add(1, Ordering::Relaxed);
      let has_large = member_vec
        .iter()
        .any(|m| m.as_ref().len() > SET_MAX_COMPACT_VALUE);
      let total_bytes: usize = member_vec.iter().map(|m| m.as_ref().len() + 4).sum();
      if !has_large
        && member_vec.len() <= SET_MAX_COMPACT_ENTRIES
        && total_bytes <= MAX_COMPACT_TOTAL_BYTES
      {
        let mut meta = MetaValue::new(key_id, CollectionType::Set, 0, 0);
        meta.set_encoding(StorageEncoding::Compact);
        // 预估总字节数已知（成员长 + 4 字节前缀），一次性分配消除批量插入反复扩容
        let mut raw = Vec::with_capacity(total_bytes);
        for m in &member_vec {
          CompactSetCodec::insert(&mut raw, m.as_ref())?;
        }
        let added = CompactSetCodec::count(&raw).unwrap_or(0);
        meta.size = added as u64;
        session.save_compact_meta(key, &meta, &raw).await?;
        return Ok(added);
      } else {
        let mut meta = MetaValue::new(key_id, CollectionType::Set, 0, 0);
        meta.set_encoding(StorageEncoding::Flattened);
        StoreSession::<D>::set_meta_chunk_info(&mut meta.reserved, 0, 0);
        (meta, None)
      }
    }
  };

  let initial_encoding = meta.encoding();
  let initial_size = meta.size;

  if meta.encoding() == StorageEncoding::Compact {
    let mut raw = raw_opt.unwrap_or_default();
    let has_large = member_vec
      .iter()
      .any(|m| m.as_ref().len() > SET_MAX_COMPACT_VALUE);
    let added_bytes: usize = member_vec.iter().map(|m| m.as_ref().len() + 4).sum();
    if has_large
      || (meta.size as usize + member_vec.len() > SET_MAX_COMPACT_ENTRIES)
      || (raw.len() + added_bytes > MAX_COMPACT_TOTAL_BYTES)
    {
      promote_set_to_flattened(session, key, &mut meta, &raw).await?;
    } else {
      let mut added = 0;
      for m in &member_vec {
        if CompactSetCodec::insert(&mut raw, m.as_ref())? {
          added += 1;
        }
      }
      if added > 0 {
        meta.size = CompactSetCodec::count(&raw).unwrap_or(0) as u64;
        session.save_compact_meta(key, &meta, &raw).await?;
      }
      return Ok(added);
    }
  }

  // Flattened 分支
  let mut added_cnt = 0;
  let mut new_members = Vec::with_capacity(member_vec.len());
  let mut seen_in_batch = hash_set_with_capacity(member_vec.len());
  for member in &member_vec {
    let m = member.as_ref();
    if !seen_in_batch.insert(m) {
      continue;
    }
    let sub_k = session.set_sub_key(meta.key_id, meta.version, m);
    if !session.contains_key_raw(&sub_k).await? {
      session.upsert_raw(&sub_k, &[]).await?;
      meta.inc_size(1);
      added_cnt += 1;
      new_members.push(m);
    }
  }

  if !new_members.is_empty() {
    session
      .append_set_members_batch(&mut meta, &new_members)
      .await?;
  }

  if added_cnt > 0 || meta.size > initial_size || meta.encoding() != initial_encoding {
    session.save_meta(key, &meta).await?;
  }
  // 显式累计新增计数返回（与 compact 分支口径一致），杜绝 size 漂移时的 u64 减法下溢
  Ok(added_cnt)
}

/// 尝试将打平 Set 降级收缩为 Compact 编码（元素数 <= 16 时）
pub(crate) async fn try_demote_set<D: Device>(
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
  let mut members = Vec::with_capacity(meta.size as usize);
  let mut seen: HashSet<u128> = hash_set_with_capacity(meta.size as usize);
  let mut sub_keys_to_delete = Vec::new();

  'outer: for chunk_id in 0..=max_chunk_id {
    let chunk_k = session.set_chunk_key(meta.key_id, meta.version, chunk_id);
    if let Some(buf) = session.read_raw(&chunk_k).await?
      && let Ok(iter) = wedb_set::MemberChunkCodec::iter(&buf)
    {
      for member in iter {
        if seen.insert(whasher::fast_hash128(member)) {
          let sub_k = session.set_sub_key(meta.key_id, meta.version, member);
          if session.contains_key_raw(&sub_k).await? {
            if member.len() > SET_MAX_COMPACT_VALUE {
              return Ok(false);
            }
            members.push(member.to_vec());
            sub_keys_to_delete.push(sub_k);
            if members.len() >= meta.size as usize {
              break 'outer;
            }
          }
        }
      }
    }
  }

  if members.is_empty() {
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
      let chunk_k = session.set_chunk_key(meta.key_id, old_version, chunk_id);
      let _ = session.delete_raw(&chunk_k).await?;
    }
    return Ok(true);
  }

  let Ok(payload) = CompactSetCodec::encode(members.iter().map(|m| m.as_slice())) else {
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
  meta.size = members.len() as u64;
  StoreSession::<D>::set_meta_chunk_info(&mut meta.reserved, 0, 0);
  session.save_compact_meta(user_key, meta, &payload).await?;

  // 清理旧版本的打平子键与分块（键名均携带 old_version）
  for sub_k in sub_keys_to_delete {
    let _ = session.delete_raw(&sub_k).await?;
  }
  for chunk_id in 0..=max_chunk_id {
    let chunk_k = session.set_chunk_key(meta.key_id, old_version, chunk_id);
    let _ = session.delete_raw(&chunk_k).await?;
  }
  Ok(true)
}

/// SREM 无锁内核（调用方必须已持有 `key` 的独占桶锁）
pub(crate) async fn srem_iter_unlocked<M: AsRef<[u8]>, D: Device>(
  session: &StoreSession<D>,
  key: &[u8],
  members: impl IntoIterator<Item = M>,
) -> Result<usize> {
  let (mut meta, raw_opt) = match session
    .load_collection_raw_write(key, CollectionType::Set)
    .await?
  {
    Some(res) => res,
    None => return Ok(0),
  };
  if meta.size == 0 {
    return Ok(0);
  }

  if meta.encoding() == StorageEncoding::Compact {
    if let Some(mut raw) = raw_opt {
      let mut rem_cnt = 0;
      for m in members {
        if CompactSetCodec::remove(&mut raw, m.as_ref()).unwrap_or(false) {
          rem_cnt += 1;
        }
      }
      if rem_cnt > 0 {
        meta.size = CompactSetCodec::count(&raw).unwrap_or(0) as u64;
        session.save_compact_meta(key, &meta, &raw).await?;
      }
      return Ok(rem_cnt);
    }
    return Ok(0);
  }

  let mut rem_cnt = 0;
  for m in members {
    let sub_k = session.set_sub_key(meta.key_id, meta.version, m.as_ref());
    if session.delete_raw(&sub_k).await? {
      rem_cnt += 1;
      meta.dec_size(1);
    }
  }

  if rem_cnt > 0 {
    if meta.size == 0 {
      session.save_meta(key, &meta).await?;
    } else if meta.size <= AUTO_DEMOTE_MAX_SIZE {
      if !try_demote_set(session, key, &mut meta).await? {
        session.save_meta(key, &meta).await?;
      }
    } else {
      session.save_meta(key, &meta).await?;
    }
  }
  Ok(rem_cnt)
}

/// 从单个集合分块中随机抽样有效成员并追加至结果集，若满足目标数量则返回 true 提前终止采样
pub(crate) async fn sample_from_set_chunk<D: Device>(
  session: &StoreSession<D>,
  meta: &MetaValue,
  chunk_id: u32,
  target: usize,
  seen: &mut HashSet<u128>,
  results: &mut Vec<Vec<u8>>,
) -> Result<bool> {
  let chunk_k = session.set_chunk_key(meta.key_id, meta.version, chunk_id);
  let Some(buf) = session.read_raw(&chunk_k).await? else {
    return Ok(false);
  };
  if let Ok(iter) = wedb_set::MemberChunkCodec::iter(&buf) {
    let mut chunk_members: Vec<&[u8]> = Vec::with_capacity(iter.len());
    chunk_members.extend(iter);
    if !chunk_members.is_empty() {
      let len = chunk_members.len();
      let start_idx = fastrand::usize(0..len);
      for offset in 0..len {
        let m = chunk_members[(start_idx + offset) % len];
        let m_hash = whasher::fast_hash128(m);
        if seen.insert(m_hash) {
          let sub_k = session.set_sub_key(meta.key_id, meta.version, m);
          if session.contains_key_raw(&sub_k).await? {
            results.push(m.to_vec());
            if results.len() >= target {
              return Ok(true);
            }
          }
        }
      }
    }
  }
  Ok(false)
}

/// 从打平分块集合中随机采样至多 count 个互异有效成员，按需按块读取，防止大集合全量物化 OOM
pub(crate) async fn sample_flattened_set_members<D: Device>(
  session: &StoreSession<D>,
  meta: &MetaValue,
  count: usize,
) -> Result<Vec<Vec<u8>>> {
  let target = count.min(meta.size as usize);
  if target == 0 {
    return Ok(Vec::new());
  }

  let (max_chunk_id, _) = StoreSession::<D>::get_meta_chunk_info(&meta.reserved);
  let num_chunks = (max_chunk_id as usize).saturating_add(1);
  let cap = target.min(65536);
  let mut seen: HashSet<u128> = hash_set_with_capacity(cap);
  let mut results = Vec::with_capacity(cap);
  const MAX_RANDOM_CHUNKS: usize = 16;
  let mut visited_chunks = [0u32; MAX_RANDOM_CHUNKS];
  let mut visited_count = 0usize;

  // 若 target 小于总数，优先随机挑选分块进行单分块局部采样
  let random_attempts = if target < meta.size as usize {
    num_chunks.min(MAX_RANDOM_CHUNKS)
  } else {
    0
  };

  for _ in 0..random_attempts {
    let chunk_id = fastrand::u32(0..=max_chunk_id);
    if visited_chunks[..visited_count].contains(&chunk_id) {
      continue;
    }
    visited_chunks[visited_count] = chunk_id;
    visited_count += 1;
    if sample_from_set_chunk(session, meta, chunk_id, target, &mut seen, &mut results).await? {
      return Ok(results);
    }
  }

  // 若随机分块未能凑齐目标数量（稀疏集合或大采样需求），从随机起始分块环形扫描补充
  let start_chunk = fastrand::usize(0..num_chunks);
  for i in 0..num_chunks {
    let chunk_id = ((start_chunk + i) % num_chunks) as u32;
    if visited_chunks[..visited_count].contains(&chunk_id) {
      continue;
    }
    if sample_from_set_chunk(session, meta, chunk_id, target, &mut seen, &mut results).await? {
      return Ok(results);
    }
  }

  Ok(results)
}

/// SRANDMEMBER 采样核心：count > 0 部分洗牌取互异前缀；count < 0 可重复采样
pub(crate) fn pick_random_members(mut all: Vec<Vec<u8>>, count: isize) -> Vec<Vec<u8>> {
  let total = all.len();
  if count > 0 {
    let pick_count = (count as usize).min(total);
    for i in 0..pick_count {
      let j = fastrand::usize(i..total);
      all.swap(i, j);
    }
    all.truncate(pick_count);
    all
  } else {
    // 负数：恰好 |count| 个可重复成员；防御性截断，与 wedb_set SetObject 口径一致
    let pick_count = count.unsigned_abs();
    let pick_count = pick_count.min(SRANDMEMBER_MAX_SAMPLE);
    let mut res = Vec::with_capacity(pick_count.min(8192));
    for _ in 0..pick_count {
      let idx = fastrand::usize(0..total);
      res.push(all[idx].clone());
    }
    res
  }
}

/// dest 与全部源键合并、排序去重后整体获取独占桶锁（SINTERSTORE/SUNIONSTORE/SDIFFSTORE 共用样板）
/// MultiBucketGuard 仅借用锁索引自身（桶下标在加锁瞬间已解析完成），不借用临时键列表，
/// 故局部 Vec 构造的键列表在函数返回后销毁不会令守卫悬垂
pub(crate) fn lock_keys_sorted<'a, D: Device>(
  session: &'a StoreSession<D>,
  dest: &[u8],
  keys: &[&[u8]],
) -> Result<windex::MultiBucketGuard<'a>> {
  let mut lock_keys: Vec<&[u8]> = Vec::with_capacity(keys.len() + 1);
  lock_keys.push(dest);
  lock_keys.extend(keys.iter().copied());
  lock_keys.sort_unstable();
  lock_keys.dedup();
  Ok(
    session
      .store
      .index
      .acquire_keys_lock_exclusive(&lock_keys)?,
  )
}

pub trait SetCommands<D: Device> {
  /// 添加集合成员 (SADD)
  ///
  /// 入口获取目标键独占桶锁（两阶段锁）：跨 await 的 load -> 内存改 -> save 读改写窗口内
  /// 串行化同键写操作，杜绝并发连接静默丢更新（严格对标 Garnet LockAKey = Exclusive）
  async fn sadd<M: AsRef<[u8]>>(
    &self,
    key: &[u8],
    members: impl IntoIterator<Item = M>,
  ) -> Result<usize>;

  /// 移除集合成员 (SREM)
  async fn srem(&self, key: &[u8], members: &[&[u8]]) -> Result<usize>;

  /// 泛型流式移除集合成员（独占桶锁串行化同键读改写窗口）
  async fn srem_iter<M: AsRef<[u8]>>(
    &self,
    key: &[u8],
    members: impl IntoIterator<Item = M>,
  ) -> Result<usize>;

  /// 检查成员是否存在 (SISMEMBER，零内存搬移切片检索)
  async fn sismember(&self, key: &[u8], member: &[u8]) -> Result<bool>;

  /// 批量检查成员是否存在 (SMISMEMBER，单次元数据探测与紧凑切片内存遍历)
  async fn smismember(&self, key: &[u8], members: &[&[u8]]) -> Result<Vec<bool>>;

  /// 获取集合所有成员 (SMEMBERS，定长分块键直查，基数达标即停，零无谓 I/O)
  async fn smembers(&self, key: &[u8]) -> Result<Vec<Vec<u8>>>;

  /// 获取集合基数 (SCARD，纯元数据探测，零紧凑载荷读取与解析)
  async fn scard(&self, key: &[u8]) -> Result<usize>;

  /// 随机弹出至多 count 个成员 (SPOP，双模优化，独占桶锁串行化同键读改写窗口)
  async fn spop(&self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>>;

  /// 随机获取成员 (SRANDMEMBER，不移除成员，双模直接采样)
  ///
  /// 对标 C# SetObjectImpl.SetRandomMember：count > 0 返回至多 count 个互不相同成员；
  /// count < 0 返回恰好 |count| 个**可重复**成员（可超过集合基数）；count == 0 返回空
  async fn srandmember(&self, key: &[u8], count: isize) -> Result<Vec<Vec<u8>>>;

  /// 将成员从源集合原子移动到目标集合 (SMOVE)
  async fn smove(&self, source: &[u8], dest: &[u8], member: &[u8]) -> Result<bool>;

  /// 游标扫描集合成员 (SSCAN，流式分块跳跃寻址与按需读取，早停机制严格限制内存)
  async fn sscan(
    &self,
    key: &[u8],
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> Result<(usize, Vec<Vec<u8>>)>;

  /// 多集合求交集 (SINTER)
  async fn sinter(&self, keys: &[&[u8]]) -> Result<Vec<Vec<u8>>>;

  /// 多集合求交集并存储到目标键 (SINTERSTORE)
  ///
  /// dest 与全部源键排序去重后整体获取独占桶锁（锁引擎内部再按桶下标全序加锁防死锁），
  /// 交集计算与落盘全程原子，杜绝计算窗口内源集合被并发修改导致的结果撕裂
  async fn sinterstore(&self, dest: &[u8], keys: &[&[u8]]) -> Result<usize>;

  /// 多集合求交集基数，支持 LIMIT 提前终止 (SINTERCARD)
  ///
  /// 以基数最小的集合为驱动集逐成员探针，命中数达 LIMIT 立即返回，
  /// 避免物化完整交集（严格对标 Redis/Garnet LIMIT 语义）。
  /// 循环前为每个非驱动键一次性装载元数据与紧凑载荷，循环内纯内存判定，
  /// 消除逐成员 × 逐键的元数据重复读取（旧实现 O(D×K) 次 meta I/O）
  async fn sintercard(&self, keys: &[&[u8]], limit: usize) -> Result<usize>;

  /// 多集合求并集 (SUNION)
  async fn sunion(&self, keys: &[&[u8]]) -> Result<Vec<Vec<u8>>>;

  /// 多集合求并集并存储到目标键 (SUNIONSTORE，dest + 源键整体独占桶锁保护，口径同 SINTERSTORE)
  async fn sunionstore(&self, dest: &[u8], keys: &[&[u8]]) -> Result<usize>;

  /// 多集合求差集 (SDIFF)
  async fn sdiff(&self, keys: &[&[u8]]) -> Result<Vec<Vec<u8>>>;

  /// 多集合求差集并存储到目标键 (SDIFFSTORE，dest + 源键整体独占桶锁保护，口径同 SINTERSTORE)
  async fn sdiffstore(&self, dest: &[u8], keys: &[&[u8]]) -> Result<usize>;
}

impl<D: Device> SetCommands<D> for StoreSession<D> {
  /// 添加集合成员 (SADD)
  ///
  /// 入口获取目标键独占桶锁（两阶段锁）：跨 await 的 load -> 内存改 -> save 读改写窗口内
  /// 串行化同键写操作，杜绝并发连接静默丢更新（严格对标 Garnet LockAKey = Exclusive）
  async fn sadd<M: AsRef<[u8]>>(
    &self,
    key: &[u8],
    members: impl IntoIterator<Item = M>,
  ) -> Result<usize> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    sadd_unlocked(self, key, members).await
  }

  /// 移除集合成员 (SREM)
  #[inline]
  async fn srem(&self, key: &[u8], members: &[&[u8]]) -> Result<usize> {
    self.srem_iter(key, members.iter().copied()).await
  }

  /// 泛型流式移除集合成员（独占桶锁串行化同键读改写窗口）
  async fn srem_iter<M: AsRef<[u8]>>(
    &self,
    key: &[u8],
    members: impl IntoIterator<Item = M>,
  ) -> Result<usize> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    srem_iter_unlocked(self, key, members).await
  }

  /// 检查成员是否存在 (SISMEMBER，零内存搬移切片检索)
  async fn sismember(&self, key: &[u8], member: &[u8]) -> Result<bool> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Set)
      .await?
    {
      Some(res) => res,
      None => return Ok(false),
    };

    if raw_read.meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload() {
        return Ok(CompactSetCodec::contains(raw, member));
      }
      return Ok(false);
    }

    let sub_k = self.set_sub_key(raw_read.meta.key_id, raw_read.meta.version, member);
    Ok(self.contains_key_raw(&sub_k).await?)
  }

  /// 批量检查成员是否存在 (SMISMEMBER，单次元数据探测与紧凑切片内存遍历)
  async fn smismember(&self, key: &[u8], members: &[&[u8]]) -> Result<Vec<bool>> {
    if members.is_empty() {
      return Ok(Vec::new());
    }
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Set)
      .await?
    {
      Some(res) => res,
      None => return Ok(vec![false; members.len()]),
    };
    if raw_read.meta.size == 0 {
      return Ok(vec![false; members.len()]);
    }

    if raw_read.meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload() {
        return Ok(
          members
            .iter()
            .map(|&m| CompactSetCodec::contains(raw, m))
            .collect(),
        );
      }
      return Ok(vec![false; members.len()]);
    }

    let key_id = raw_read.meta.key_id;
    let version = raw_read.meta.version;
    let mut res = Vec::with_capacity(members.len());
    for &m in members {
      let sub_k = self.set_sub_key(key_id, version, m);
      res.push(self.contains_key_raw(&sub_k).await?);
    }
    Ok(res)
  }

  /// 获取集合所有成员 (SMEMBERS，定长分块键直查，基数达标即停，零无谓 I/O)
  async fn smembers(&self, key: &[u8]) -> Result<Vec<Vec<u8>>> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Set)
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
        let cap = CompactSetCodec::count(raw).unwrap_or(0);
        let mut members = Vec::with_capacity(cap);
        for m in CompactSetCodec::iter_members(raw) {
          members.push(m.to_vec());
        }
        return Ok(members);
      }
      return Ok(Vec::new());
    }

    let (max_chunk_id, _) = Self::get_meta_chunk_info(&meta.reserved);
    let target_count = meta.size as usize;
    let cap = target_count.min(65536);
    let mut seen: HashSet<u128> = hash_set_with_capacity(cap);
    let mut results = Vec::with_capacity(cap);

    for chunk_id in 0..=max_chunk_id {
      let chunk_k = self.set_chunk_key(meta.key_id, meta.version, chunk_id);
      let buf = match self.read_raw(&chunk_k).await? {
        Some(b) => b,
        None => continue,
      };
      if let Ok(iter) = wedb_set::MemberChunkCodec::iter(&buf) {
        for member in iter {
          if seen.insert(whasher::fast_hash128(member)) {
            let sub_k = self.set_sub_key(meta.key_id, meta.version, member);
            if self.contains_key_raw(&sub_k).await? {
              results.push(member.to_vec());
              if results.len() >= target_count {
                return Ok(results);
              }
            }
          }
        }
      }
    }

    Ok(results)
  }

  /// 获取集合基数 (SCARD，纯元数据探测，零紧凑载荷读取与解析)
  async fn scard(&self, key: &[u8]) -> Result<usize> {
    let meta = match self
      .load_collection_meta_read(key, CollectionType::Set)
      .await?
    {
      Some(res) => res,
      None => return Ok(0),
    };
    Ok(meta.size as usize)
  }

  /// 随机弹出至多 count 个成员 (SPOP，双模优化，独占桶锁串行化同键读改写窗口)
  async fn spop(&self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>> {
    if count == 0 {
      return Ok(Vec::new());
    }
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;

    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::Set)
      .await?
    {
      Some(res) => res,
      None => return Ok(Vec::new()),
    };
    if meta.size == 0 {
      return Ok(Vec::new());
    }

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(mut raw) = raw_opt {
        let total = CompactSetCodec::count(&raw).unwrap_or(0);
        if total == 0 {
          return Ok(Vec::new());
        }
        let pop_count = count.min(total);
        if pop_count == total {
          let mut popped = Vec::with_capacity(total);
          for m in CompactSetCodec::iter_members(&raw) {
            popped.push(m.to_vec());
          }
          meta.size = 0;
          self.save_compact_meta(key, &meta, &[]).await?;
          return Ok(popped);
        }

        if pop_count == 1 {
          let idx = fastrand::usize(0..total);
          let Some(target) = CompactSetCodec::iter_members(&raw)
            .nth(idx)
            .map(|m| m.to_vec())
          else {
            return Ok(Vec::new());
          };
          CompactSetCodec::remove(&mut raw, &target)?;
          meta.size = (total - 1) as u64;
          self.save_compact_meta(key, &meta, &raw).await?;
          return Ok(vec![target]);
        }

        // 多元素弹出：单次物化后前缀洗牌，消除多次扫表的 O(K*N) 与 picked_indices 分配
        let mut all_members: Vec<Vec<u8>> = CompactSetCodec::iter_members(&raw)
          .map(|m| m.to_vec())
          .collect();
        for i in 0..pop_count {
          let j = fastrand::usize(i..total);
          all_members.swap(i, j);
        }
        all_members.truncate(pop_count);
        for m in &all_members {
          CompactSetCodec::remove(&mut raw, m)?;
        }
        meta.size = CompactSetCodec::count(&raw).unwrap_or(0) as u64;
        self.save_compact_meta(key, &meta, &raw).await?;
        return Ok(all_members);
      }
      return Ok(Vec::new());
    }

    // 打平模式：利用分块随机采样/弹出，避免大集合全量物化 OOM
    let target = count.min(meta.size as usize);
    if target == 0 {
      return Ok(Vec::new());
    }
    let popped = sample_flattened_set_members(self, &meta, target).await?;
    if popped.is_empty() {
      return Ok(Vec::new());
    }
    // 已持本键独占锁，直接复用无锁内核避免桶锁重入自锁
    srem_iter_unlocked(self, key, &popped).await?;
    Ok(popped)
  }

  /// 随机获取成员 (SRANDMEMBER，不移除成员，双模直接采样)
  ///
  /// 对标 C# SetObjectImpl.SetRandomMember：count > 0 返回至多 count 个互不相同成员；
  /// count < 0 返回恰好 |count| 个**可重复**成员（可超过集合基数）；count == 0 返回空
  async fn srandmember(&self, key: &[u8], count: isize) -> Result<Vec<Vec<u8>>> {
    if count == 0 {
      return Ok(Vec::new());
    }

    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Set)
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
        // 单成员快速路径：无需物化整个集合，直接利用迭代器取目标索引
        if count == 1 {
          let total = CompactSetCodec::count(raw).unwrap_or(0);
          if total > 0 {
            let idx = fastrand::usize(0..total);
            if let Some(m) = CompactSetCodec::iter_members(raw).nth(idx) {
              return Ok(vec![m.to_vec()]);
            }
          }
          return Ok(Vec::new());
        }

        // 紧凑布局条目数有限（≤128），直接物化后统一采样
        let members: Vec<Vec<u8>> = CompactSetCodec::iter_members(raw)
          .map(|m| m.to_vec())
          .collect();
        if members.is_empty() {
          return Ok(Vec::new());
        }
        return Ok(pick_random_members(members, count));
      }
      return Ok(Vec::new());
    }

    // 打平模式：利用分块随机采样防护，避免大集合全量物化 OOM
    if count > 0 {
      let target = (count as usize).min(meta.size as usize);
      sample_flattened_set_members(self, &meta, target).await
    } else {
      let pick_count = count.unsigned_abs().min(SRANDMEMBER_MAX_SAMPLE);
      let pool_size = if pick_count == 1 {
        1
      } else {
        (meta.size as usize).min(pick_count.max(64)).min(1024)
      };
      let pool = sample_flattened_set_members(self, &meta, pool_size).await?;
      if pool.is_empty() {
        return Ok(Vec::new());
      }
      let mut res = Vec::with_capacity(pick_count.min(8192));
      for _ in 0..pick_count {
        let idx = fastrand::usize(0..pool.len());
        res.push(pool[idx].clone());
      }
      Ok(res)
    }
  }

  /// 将成员从源集合原子移动到目标集合 (SMOVE)
  async fn smove(&self, source: &[u8], dest: &[u8], member: &[u8]) -> Result<bool> {
    // 双键独占两阶段锁（严格对标 C# Garnet SetMove：源与目标键均 SaveKeyEntryToLock(Exclusive)），
    // 锁引擎按桶下标全局排序并去重，杜绝死锁；防止迁移窗口内成员在两侧同时不可见
    let _guard = self
      .store
      .index
      .acquire_keys_lock_exclusive(&[source, dest])?;

    if !self.sismember(source, member).await? {
      return Ok(false);
    }
    if source == dest {
      return Ok(true);
    }
    // 预检目标键类型：若目标键已存在且非集合类型则立即报错，防止源集合成员被提前误删
    let _ = self.load_collection_meta(dest, CollectionType::Set).await?;
    // 已持双键独占锁，直接复用无锁内核避免桶锁重入自锁
    srem_iter_unlocked(self, source, [member]).await?;
    sadd_unlocked(self, dest, [member]).await?;
    Ok(true)
  }

  /// 游标扫描集合成员 (SSCAN，流式分块跳跃寻址与按需读取，早停机制严格限制内存)
  async fn sscan(
    &self,
    key: &[u8],
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> Result<(usize, Vec<Vec<u8>>)> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::Set)
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
        let total = CompactSetCodec::count(raw).unwrap_or(0);
        if cursor >= total {
          return Ok((0, Vec::new()));
        }
        let pat = pattern.unwrap_or(b"*");
        let limit = if count == 0 { 10 } else { count };
        let mut items = Vec::with_capacity(limit.min(SCAN_RESERVE_CAP));

        let mut next_cursor = 0;
        for (idx, member) in CompactSetCodec::iter_members(raw).enumerate() {
          if idx < cursor {
            continue;
          }
          if glob_match(pat, member) {
            items.push(member.to_vec());
          }
          if items.len() >= limit {
            next_cursor = if idx + 1 < total { idx + 1 } else { 0 };
            return Ok((next_cursor, items));
          }
        }
        return Ok((next_cursor, items));
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
    let mut seen: HashSet<u128> = hash_set_with_capacity(limit.min(SCAN_RESERVE_CAP));

    for chunk_id in start_chunk_id..=max_chunk_id {
      let chunk_k = self.set_chunk_key(meta.key_id, meta.version, chunk_id);
      let buf = match self.read_raw(&chunk_k).await? {
        Some(b) => b,
        None => continue,
      };

      if let Ok(iter) = wedb_set::MemberChunkCodec::iter(&buf) {
        let total_in_chunk = iter.len();
        for (elem_idx, member) in iter.enumerate() {
          if chunk_id == start_chunk_id && elem_idx < start_elem_idx {
            continue;
          }

          if seen.insert(whasher::fast_hash128(member)) {
            let sub_k = self.set_sub_key(meta.key_id, meta.version, member);
            if self.contains_key_raw(&sub_k).await? {
              let matches = match pattern {
                Some(p) => glob_match(p, member),
                None => true,
              };
              if matches {
                items.push(member.to_vec());

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

  /// 多集合求交集 (SINTER)
  async fn sinter(&self, keys: &[&[u8]]) -> Result<Vec<Vec<u8>>> {
    if keys.is_empty() {
      return Ok(Vec::new());
    }
    let first_members = self.smembers(keys[0]).await?;
    if first_members.is_empty() || keys.len() == 1 {
      return Ok(first_members);
    }
    let mut candidate_set: HashSet<Vec<u8>> = first_members.into_iter().collect();

    for &k in &keys[1..] {
      if candidate_set.is_empty() {
        return Ok(Vec::new());
      }
      let current = self.smembers(k).await?;
      if current.is_empty() {
        return Ok(Vec::new());
      }
      let cur_set: HashSet<&[u8]> = current.iter().map(|m| m.as_slice()).collect();
      candidate_set.retain(|item| cur_set.contains(item.as_slice()));
    }

    Ok(candidate_set.into_iter().collect())
  }

  /// 多集合求交集并存储到目标键 (SINTERSTORE)
  ///
  /// dest 与全部源键排序去重后整体获取独占桶锁（锁引擎内部再按桶下标全序加锁防死锁），
  /// 交集计算与落盘全程原子，杜绝计算窗口内源集合被并发修改导致的结果撕裂
  async fn sinterstore(&self, dest: &[u8], keys: &[&[u8]]) -> Result<usize> {
    let _key_lock = lock_keys_sorted(self, dest, keys)?;

    let inter = self.sinter(keys).await?;
    self.delete(dest).await?;
    if inter.is_empty() {
      return Ok(0);
    }
    // 已持 dest 独占锁，直接复用无锁内核避免桶锁重入自锁
    let count = sadd_unlocked(self, dest, inter).await?;
    Ok(count)
  }

  /// 多集合求交集基数，支持 LIMIT 提前终止 (SINTERCARD)
  ///
  /// 以基数最小的集合为驱动集逐成员探针，命中数达 LIMIT 立即返回，
  /// 避免物化完整交集（严格对标 Redis/Garnet LIMIT 语义）。
  /// 循环前为每个非驱动键一次性装载元数据与紧凑载荷，循环内纯内存判定，
  /// 消除逐成员 × 逐键的元数据重复读取（旧实现 O(D×K) 次 meta I/O）
  async fn sintercard(&self, keys: &[&[u8]], limit: usize) -> Result<usize> {
    if keys.is_empty() {
      return Ok(0);
    }

    // 选取基数最小的集合作为驱动集，最小化成员探针总次数
    let mut driver_idx = 0;
    let mut driver_card = self.scard(keys[0]).await?;
    for (i, &k) in keys.iter().enumerate().skip(1) {
      let card = self.scard(k).await?;
      if card < driver_card {
        driver_card = card;
        driver_idx = i;
      }
    }
    if driver_card == 0 {
      return Ok(0);
    }

    // 预加载非驱动键元数据与紧凑载荷，任一键为空集则交集必为空
    let mut reads: Vec<Option<RawCollectionRead>> = Vec::with_capacity(keys.len());
    for (i, &k) in keys.iter().enumerate() {
      reads.push(if i == driver_idx {
        None
      } else {
        match self
          .load_collection_raw_read(k, CollectionType::Set)
          .await?
        {
          Some(rr) => Some(rr),
          None => return Ok(0),
        }
      });
    }

    // 物化探针：Compact 一次读出建成员哈希集（零拷贝借用切片），Flattened 复用 key_id/version
    let mut probes: Vec<SetProbe<'_>> = Vec::with_capacity(reads.len() - 1);
    for (i, read) in reads.iter().enumerate() {
      if i != driver_idx
        && let Some(rr) = read
      {
        probes.push(match rr.compact_payload() {
          Some(raw) => SetProbe::Compact(CompactSetCodec::iter_members(raw).collect()),
          None => SetProbe::Flattened {
            key_id: rr.meta.key_id,
            version: rr.meta.version,
          },
        });
      }
    }

    let members = self.smembers(keys[driver_idx]).await?;
    let mut count = 0usize;
    for m in &members {
      let mut in_all = true;
      for probe in &probes {
        if !probe.contains(self, m.as_slice()).await? {
          in_all = false;
          break;
        }
      }
      if in_all {
        count += 1;
        if limit > 0 && count >= limit {
          return Ok(limit);
        }
      }
    }
    Ok(count)
  }

  /// 多集合求并集 (SUNION)
  async fn sunion(&self, keys: &[&[u8]]) -> Result<Vec<Vec<u8>>> {
    let mut union_set = new_hash_set();
    for &k in keys {
      let members = self.smembers(k).await?;
      union_set.extend(members);
    }
    Ok(union_set.into_iter().collect())
  }

  /// 多集合求并集并存储到目标键 (SUNIONSTORE，dest + 源键整体独占桶锁保护，口径同 SINTERSTORE)
  async fn sunionstore(&self, dest: &[u8], keys: &[&[u8]]) -> Result<usize> {
    let _key_lock = lock_keys_sorted(self, dest, keys)?;

    let union_res = self.sunion(keys).await?;
    self.delete(dest).await?;
    if union_res.is_empty() {
      return Ok(0);
    }
    // 已持 dest 独占锁，直接复用无锁内核避免桶锁重入自锁
    let count = sadd_unlocked(self, dest, union_res).await?;
    Ok(count)
  }

  /// 多集合求差集 (SDIFF)
  async fn sdiff(&self, keys: &[&[u8]]) -> Result<Vec<Vec<u8>>> {
    if keys.is_empty() {
      return Ok(Vec::new());
    }
    let first_members = self.smembers(keys[0]).await?;
    if first_members.is_empty() || keys.len() == 1 {
      return Ok(first_members);
    }
    let mut candidate_set: HashSet<Vec<u8>> = first_members.into_iter().collect();

    for &k in &keys[1..] {
      if candidate_set.is_empty() {
        break;
      }
      let current = self.smembers(k).await?;
      for m in current {
        candidate_set.remove(&m);
      }
    }

    Ok(candidate_set.into_iter().collect())
  }

  /// 多集合求差集并存储到目标键 (SDIFFSTORE，dest + 源键整体独占桶锁保护，口径同 SINTERSTORE)
  async fn sdiffstore(&self, dest: &[u8], keys: &[&[u8]]) -> Result<usize> {
    let _key_lock = lock_keys_sorted(self, dest, keys)?;

    let diff = self.sdiff(keys).await?;
    self.delete(dest).await?;
    if diff.is_empty() {
      return Ok(0);
    }
    // 已持 dest 独占锁，直接复用无锁内核避免桶锁重入自锁
    let count = sadd_unlocked(self, dest, diff).await?;
    Ok(count)
  }
}
