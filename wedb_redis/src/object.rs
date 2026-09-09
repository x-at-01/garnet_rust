use std::sync::atomic::Ordering;

use wdev::Device;
use wedb_object::GarnetObject;
use windex::{HashBucket, HashBucketEntry};
use wkv::{StoreSession, read_cache::is_read_cache_addr};
use wval::{CollectionType, MetaValue};

use super::*;
use crate::error::Result;

/// 遍历哈希索引中全部存活用户键并执行回调（严格对照 C# Garnet UnifiedStoreGetDBKeys：
/// 过滤墓碑、内部打平子键、分块键与已清空集合残留的幽灵元记录）
/// 回调收到去标签后的用户键切片，返回 false 时提前终止全量遍历。
/// 调用方须确保当前线程处于 LightEpoch 纪元保护下。
pub(crate) async fn for_each_live_user_key<F, D: Device>(
  session: &StoreSession<D>,
  mut on_key: F,
) -> Result<()>
where
  F: FnMut(&[u8]) -> bool,
{
  let begin_addr = session.store.begin_address();
  let prefix = session.session_prefix();
  let prefix_slice = prefix.as_slice();

  for bucket in session.store.index.buckets.iter() {
    let mut curr_bucket = bucket;
    loop {
      for item in curr_bucket.entries.iter().take(HashBucket::DATA_ENTRIES) {
        let raw = item.load(Ordering::Acquire);
        if raw == 0 {
          continue;
        }
        let entry = HashBucketEntry::from_raw(raw);
        if entry.is_tentative() {
          continue;
        }
        let addr = entry.address();
        let mut effective_addr = addr;
        let mut rc_handled = false;
        while is_read_cache_addr(effective_addr) {
          let mut next_addr = 0;
          let res =
            session
              .store
              .read_cache
              .with_record(effective_addr, |rec_key, rec_val, prev| {
                next_addr = prev;
                match live_user_key(rec_key, rec_val, prefix_slice) {
                  Some(k) if !on_key(k) => KeyScanProbe::Stop,
                  Some(_) => KeyScanProbe::Continue,
                  None => KeyScanProbe::Skip,
                }
              });
          if let Some(probe_res) = res {
            if matches!(probe_res, KeyScanProbe::Stop) {
              return Ok(());
            }
            rc_handled = true;
            break;
          }
          if next_addr == 0 {
            break;
          }
          effective_addr = next_addr;
        }

        if rc_handled {
          continue;
        }

        if is_read_cache_addr(effective_addr) {
          effective_addr = session.store.read_cache.skip_read_cache(effective_addr);
        }
        if effective_addr < begin_addr {
          continue;
        }

        let probe = if let Some(probe) =
          session
            .store
            .hlog
            .with_memory_record(effective_addr, |rec| {
              if rec.is_tombstone() {
                return Ok(KeyScanProbe::Skip);
              }
              match live_user_key(rec.key(), rec.value(), prefix_slice) {
                Some(k) if !on_key(k) => Ok(KeyScanProbe::Stop),
                Some(_) => Ok(KeyScanProbe::Continue),
                None => Ok(KeyScanProbe::Skip),
              }
            })? {
          probe
        } else {
          // 磁盘冷数据回退：异步读取记录后执行相同过滤
          let record = match session.store.hlog.read_record(effective_addr).await {
            Ok(r) => r,
            Err(e) => {
              if effective_addr < session.store.begin_address() {
                continue;
              }
              return Err(e.into());
            }
          };
          let (Ok(key), Ok(val)) = (record.key(), record.value()) else {
            continue;
          };
          if record.is_tombstone().unwrap_or(true) {
            continue;
          }
          match live_user_key(key, val, prefix_slice) {
            Some(k) if !on_key(k) => return Ok(()),
            _ => continue,
          }
        };

        if matches!(probe, KeyScanProbe::Stop) {
          return Ok(());
        }
      }

      let overflow_idx = curr_bucket.overflow_index();
      if overflow_idx == 0 {
        break;
      }

      match session.store.index.overflow_pool.get(overflow_idx) {
        Some(next) => curr_bucket = next,
        None => break,
      }
    }
  }

  Ok(())
}

pub trait ObjectCommands<D: Device> {
  /// 读取反序列化指定键的堆对象
  async fn load_object(&self, key: &[u8]) -> Result<Option<GarnetObject>>;

  /// 保存堆对象到存储引擎
  async fn save_object(&self, key: &[u8], obj: GarnetObject) -> Result<()>;

  /// 加载用于读取的集合元数据：
  /// - 若元数据存在且类型匹配且元素总数大于 0，返回 Ok(Some(meta))
  /// - 若元数据不存在、元素总数为 0 或类型不匹配，返回 Ok(None)
  /// - 若元数据不存在（或为 size == 0 的幽灵元记录）但存在同名字符串裸键，
  ///   返回 Err(InvalidCollectionType) 触发 WRONGTYPE（与写路径口径一致）
  async fn load_collection_meta_read(
    &self,
    key: &[u8],
    expected: CollectionType,
  ) -> Result<Option<MetaValue>>;

  /// 加载用于写入或修改的集合元数据：
  /// - 若元数据存在且类型匹配，返回 Ok(Some(meta))
  /// - 若元数据存在但类型不匹配（若旧集合已清空 size == 0 则视为无冲突），返回 Err(InvalidCollectionType)
  /// - 若元数据不存在（或为 size == 0 的幽灵元记录）但存在同名字符串裸键，返回 Err(InvalidCollectionType)
  /// - 若键完全不存在，返回 Ok(None)
  async fn load_collection_meta_write(
    &self,
    key: &[u8],
    expected: CollectionType,
  ) -> Result<Option<MetaValue>>;

  /// 兼容别名，默认执行写语义严格校验
  async fn load_collection_meta(
    &self,
    key: &[u8],
    expected: CollectionType,
  ) -> Result<Option<MetaValue>>;

  /// 返回指定键的数据类型名称（优先探测 Meta 键，若无再检查普通 key，否则返回 "none"）
  async fn type_of(&self, key: &[u8]) -> Result<&'static str>;

  /// 获取当前数据库中有效用户键总数 (DBSIZE)
  async fn dbsize(&self) -> Result<usize>;

  /// 重命名键 (RENAME / RENAMENX)
  async fn rename(&self, key: &[u8], new_key: &[u8], nx: bool) -> Result<RenameResult>;

  /// 原子设置多个键值对数组 (MSET，受两阶段锁保护，严格对标 Garnet TransactionManager.LockAllKeys)
  async fn mset_chunks<T: AsRef<[u8]>>(&self, pairs: &[[T; 2]]) -> Result<()>;

  /// 仅当所有键都不存在时原子设置多个键值对 (MSETNX，严格对标 Garnet 两阶段锁与原子事务检查)
  async fn msetnx<T: AsRef<[u8]>>(&self, pairs: &[[T; 2]]) -> Result<bool>;
}

impl<D: Device> ObjectCommands<D> for StoreSession<D> {
  /// 读取反序列化指定键的堆对象
  async fn load_object(&self, key: &[u8]) -> Result<Option<GarnetObject>> {
    if let Some(meta) = self.load_meta(key).await?
      && meta.size > 0
    {
      return Err(wval::Error::InvalidCollectionType(meta.collection_type.as_u8()).into());
    }
    match self.read(key).await? {
      Some(bytes) => match GarnetObject::deserialize(&bytes) {
        Ok(obj) => Ok(Some(obj)),
        Err(e) => {
          // 反序列化失败对外统一折叠为 WRONGTYPE，但保留 debug 级原始诊断
          // （版本演化/字节损坏时可直接定位失败原因，不在热路径付出格式化成本）
          log::debug!("堆对象反序列化失败，按 WRONGTYPE 折叠: err={e}");
          Err(wedb_object::Error::WrongType.into())
        }
      },
      None => Ok(None),
    }
  }

  /// 保存堆对象到存储引擎
  async fn save_object(&self, key: &[u8], mut obj: GarnetObject) -> Result<()> {
    if obj.is_empty() {
      self.delete(key).await?;
    } else {
      let bytes = obj.to_vec();
      self.upsert(key, &bytes).await?;
    }
    Ok(())
  }

  /// 加载用于读取的集合元数据：
  /// - 若元数据存在且类型匹配且元素总数大于 0，返回 Ok(Some(meta))
  /// - 若元数据不存在、元素总数为 0 或类型不匹配，返回 Ok(None)
  /// - 若元数据不存在（或为 size == 0 的幽灵元记录）但存在同名字符串裸键，
  ///   返回 Err(InvalidCollectionType) 触发 WRONGTYPE（与写路径口径一致）
  #[inline]
  async fn load_collection_meta_read(
    &self,
    key: &[u8],
    expected: CollectionType,
  ) -> Result<Option<MetaValue>> {
    if let Some(meta) = self.load_meta(key).await?
      && meta.size > 0
    {
      if meta.collection_type != expected {
        return Ok(None);
      }
      return Ok(Some(meta));
    }
    // 幽灵元记录（打平集合秒删残留）：继续探测同名裸键以识别 WRONGTYPE
    if self.read(key).await?.is_some() {
      return Err(wval::Error::InvalidCollectionType(0xFF).into());
    }
    Ok(None)
  }

  /// 加载用于写入或修改的集合元数据：
  /// - 若元数据存在且类型匹配，返回 Ok(Some(meta))
  /// - 若元数据存在但类型不匹配（若旧集合已清空 size == 0 则视为无冲突），返回 Err(InvalidCollectionType)
  /// - 若元数据不存在（或为 size == 0 的幽灵元记录）但存在同名字符串裸键，返回 Err(InvalidCollectionType)
  /// - 若键完全不存在，返回 Ok(None)
  #[inline]
  async fn load_collection_meta_write(
    &self,
    key: &[u8],
    expected: CollectionType,
  ) -> Result<Option<MetaValue>> {
    if let Some(meta) = self.load_meta(key).await?
      && meta.size > 0
    {
      if meta.collection_type != expected {
        return Err(wval::Error::InvalidCollectionType(meta.collection_type.as_u8()).into());
      }
      return Ok(Some(meta));
    }
    // 幽灵元记录：继续探测同名裸键，杜绝在活字符串之上静默重建同名集合
    if self.read(key).await?.is_some() {
      return Err(wval::Error::InvalidCollectionType(0xFF).into());
    }
    Ok(None)
  }

  /// 兼容别名，默认执行写语义严格校验
  #[inline]
  async fn load_collection_meta(
    &self,
    key: &[u8],
    expected: CollectionType,
  ) -> Result<Option<MetaValue>> {
    self.load_collection_meta_write(key, expected).await
  }

  /// 返回指定键的数据类型名称（优先探测 Meta 键，若无再检查普通 key，否则返回 "none"）
  async fn type_of(&self, key: &[u8]) -> Result<&'static str> {
    if let Some(meta) = self.load_meta(key).await?
      && meta.size > 0
    {
      return Ok(meta.collection_type.as_str());
    }
    // 幽灵元记录（打平集合秒删残留 size == 0）不直接判定 none：
    // 若同名单名裸键存在（秒删后同名 SET 的合法状态），必须按 string 上报
    let val = self.read(key).await?;
    match val {
      None => Ok("none"),
      Some(bytes) => {
        if matches!(bytes.first(), Some(1..=4))
          && let Ok(obj) = GarnetObject::deserialize(&bytes)
        {
          return Ok(obj.type_name());
        }
        Ok("string")
      }
    }
  }

  /// 获取当前数据库中有效用户键总数 (DBSIZE)
  async fn dbsize(&self) -> Result<usize> {
    let _guard = self.participant.enter();
    let mut count = 0usize;
    for_each_live_user_key(self, |_| {
      count += 1;
      true
    })
    .await?;
    Ok(count)
  }

  /// 重命名键 (RENAME / RENAMENX)
  async fn rename(&self, key: &[u8], new_key: &[u8], nx: bool) -> Result<RenameResult> {
    if key == new_key {
      return if self.contains_key(key).await? {
        Ok(RenameResult::SameKey)
      } else {
        Ok(RenameResult::NoSuchKey)
      };
    }

    // 双键独占两阶段锁（严格对标 C# Garnet RENAME：SaveKeyEntryToLock(old/new, Exclusive)），
    // 杜绝迁移期间源键与目标键被其他事务型多键操作并发修改
    let _guard = self
      .store
      .index
      .acquire_keys_lock_exclusive(&[key, new_key])?;

    let source_meta = self.load_meta(key).await?;
    let source_val = if source_meta.as_ref().is_some_and(|m| m.size > 0) {
      None
    } else {
      self.read(key).await?
    };

    if source_meta.as_ref().is_none_or(|m| m.size == 0) && source_val.is_none() {
      return Ok(RenameResult::NoSuchKey);
    }

    // 预读源键 TTL 以便随键迁移（Redis 语义：RENAME 后 TTL 跟随新键）
    let src_ttl = self.ttl_of(key).await?;

    let target_exists = self.contains_key(new_key).await?;
    if nx && target_exists {
      return Ok(RenameResult::AlreadyExists);
    }

    if target_exists {
      self.delete(new_key).await?;
    }

    if let Some(meta) = source_meta {
      if meta.size > 0 {
        if meta.collection_type == CollectionType::RangeIndex {
          // RangeIndex 迁移：数据文件按"键名哈希前缀"命名无法别名共享，
          // 必须先快照重建新键的树注册，随后彻底清理旧键及其数据文件
          self.rename_range_index(key, new_key).await?;
          self.delete(key).await?;
        } else {
          if let Some(raw_meta_bytes) = self.read_raw(&self.session_meta_key(key)).await? {
            let new_meta_k = self.session_meta_key(new_key);
            self.upsert_raw(&new_meta_k, &raw_meta_bytes).await?;
            self
              .store
              .update_key_id_meta(meta.key_id, meta.version, true);
          } else {
            self.save_meta(new_key, &meta).await?;
          }
          self.delete_raw(&self.session_meta_key(key)).await?;
          self.delete(key).await?;
        }
      } else if let Some(val) = source_val {
        self.upsert(new_key, &val).await?;
        self.delete(key).await?;
      }
    } else if let Some(val) = source_val {
      self.upsert(new_key, &val).await?;
      self.delete(key).await?;
    }

    // TTL 记录随键迁移：源键 TTL 已随 delete(key) 一并清除，此处写入新键；
    // 目标键若原存活，其旧 TTL 已随前置 delete(new_key) 清除，不残留
    if let Some(exp) = src_ttl {
      self.put_ttl(new_key, exp).await?;
    }

    Ok(RenameResult::Success)
  }

  /// 原子设置多个键值对数组 (MSET，受两阶段锁保护，严格对标 Garnet TransactionManager.LockAllKeys)
  async fn mset_chunks<T: AsRef<[u8]>>(&self, pairs: &[[T; 2]]) -> Result<()> {
    if pairs.is_empty() {
      return Ok(());
    }
    let mut stack_keys = [&b""[..]; 16];
    let heap_keys;
    let keys: &[&[u8]] = if pairs.len() <= 16 {
      for (i, p) in pairs.iter().enumerate() {
        stack_keys[i] = p[0].as_ref();
      }
      &stack_keys[..pairs.len()]
    } else {
      heap_keys = pairs.iter().map(|p| p[0].as_ref()).collect::<Vec<_>>();
      &heap_keys
    };

    let _guard = self.store.index.acquire_keys_lock_exclusive(keys)?;
    for pair in pairs {
      self.upsert(pair[0].as_ref(), pair[1].as_ref()).await?;
    }
    Ok(())
  }

  /// 仅当所有键都不存在时原子设置多个键值对 (MSETNX，严格对标 Garnet 两阶段锁与原子事务检查)
  async fn msetnx<T: AsRef<[u8]>>(&self, pairs: &[[T; 2]]) -> Result<bool> {
    if pairs.is_empty() {
      return Ok(true);
    }
    let mut stack_keys = [&b""[..]; 16];
    let heap_keys;
    let keys: &[&[u8]] = if pairs.len() <= 16 {
      for (i, p) in pairs.iter().enumerate() {
        stack_keys[i] = p[0].as_ref();
      }
      &stack_keys[..pairs.len()]
    } else {
      heap_keys = pairs.iter().map(|p| p[0].as_ref()).collect::<Vec<_>>();
      &heap_keys
    };

    let _guard = self.store.index.acquire_keys_lock_exclusive(keys)?;
    for pair in pairs {
      if self.contains_key(pair[0].as_ref()).await? {
        return Ok(false);
      }
    }
    for pair in pairs {
      self.upsert(pair[0].as_ref(), pair[1].as_ref()).await?;
    }
    Ok(true)
  }
}
