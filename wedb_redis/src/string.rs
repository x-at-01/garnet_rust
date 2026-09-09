use core::str::from_utf8;

use wdev::Device;
use wedb_object::GarnetObject;
use whasher::HashSet;
use wkv::{StoreSession, TtlProbe};

use super::{object::for_each_live_user_key, *};
use crate::error::Result;

pub trait StringCommands<D: Device> {
  /// 纯同步 DRAM 内存直读快路径（尝试零开销直读字符串原始数据）
  /// 纪元预保护下的同步内存字符串直读（对标 Garnet ReadWithUnsafeContext，完全绕过 enter() 原子开销）
  ///
  /// - `Ok(Some(Some(res)))`: 内存直读命中且类型有效
  /// - `Ok(Some(None))`: 确切不存在（墓碑或无候选且无磁盘数据）
  /// - `Ok(None)`: 需回退到异步磁盘读取或需查询集合元数据
  /// - `Err(e)`: 类型错误或解析错误
  fn try_read_string_in_memory_unprotected<R>(
    &self,
    key: &[u8],
    f: impl FnOnce(&[u8]) -> R,
  ) -> Result<Option<Option<R>>>;

  /// 同步内存字符串直读快路径
  fn try_read_string_in_memory<R>(
    &self,
    key: &[u8],
    f: impl FnOnce(&[u8]) -> R,
  ) -> Result<Option<Option<R>>>;

  /// 零拷贝读取字符串原始数据，若命中且非富对象直接执行闭包；仅未命中时探测集合元数据以识别类型错误
  async fn read_string_with<R>(&self, key: &[u8], f: impl FnOnce(&[u8]) -> R) -> Result<Option<R>>;

  /// 读取字符串原始数据，若存在且为集合元数据或富对象则返回 InvalidCollectionType 错误
  async fn read_string(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

  /// 整型数值自增自减 (INCRBY / DECRBY / INCR / DECR)
  async fn incrby(&self, key: &[u8], incr: i64) -> Result<i64>;

  /// 浮点数值自增 (INCRBYFLOAT)
  async fn incrbyfloat(&self, key: &[u8], incr: f64) -> Result<f64>;

  /// 设置新值并返回旧值 (GETSET)
  async fn getset(&self, key: &[u8], val: &[u8]) -> Result<Option<Vec<u8>>>;

  /// 获取并删除键 (GETDEL)
  async fn getdel(&self, key: &[u8]) -> Result<Option<Vec<u8>>>;

  /// 批量零拷贝流式读取字符串 (MGET)
  /// 批量读取字符串并对每个元素调用回调闭包 (MGET Each)
  ///
  /// - 严格对照 C# Garnet ContextReadWithPrefetch 与 MGetReadArgBatch_SG 架构：
  /// - 直接复用底层的 12 项硬件流水线两级预取批量读取方法 `read_batch_with`；
  /// - 若键存在且为普通字符串，向闭包传入 `Some(&[u8])`；
  /// - 若键不存在、已过期、或为富对象/集合类型，向闭包传入 `None`；
  /// - 回调次序严格对位请求键序（严格对照 Redis MGET：结果顺序恒等于请求顺序，
  ///   缺失键为 nil），底座批量读保证按 idx 升序交付，调用方无需携带索引；
  /// - 纯内存批常态零堆内存分配（混批含磁盘冷键时，仅底座冷路径收割与暂存按需分配），
  ///   且对 99.9% 场景免查 `load_meta`；
  /// - 逐 key 惰性过期检查：批量回调内以同步内存探针判定 TTL（不存在即无 TTL，零额外
  ///   I/O），已过期先行回调 None，批量读闭环后再统一物理清除；仅 TTL 记录落盘的罕见
  ///   情形延迟裁决并物化 Vec（唯一可能分配的冷路径），且该键之后的所有交付一律暂存，
  ///   裁决完成后按 idx 升序合流回调（MGET 线上协议按回调序对位写响应，严禁乱序）。
  async fn mget_each<K, F>(&self, keys: &[K], on_item: F) -> Result<()>
  where
    K: AsRef<[u8]>,
    F: FnMut(Option<&[u8]>);

  /// 批量读取字符串 (MGET，类型不匹配或不存在时返回 None)
  async fn mget(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>>;

  /// 批量原子写入字符串 (MSET，受两阶段锁保护，严格对标 Garnet TransactionManager.LockAllKeys)
  async fn mset<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, pairs: &[(K, V)]) -> Result<()>;

  /// 匹配模式扫描所有用户键 (KEYS)
  async fn keys(&self, pattern: &[u8]) -> Result<Vec<Vec<u8>>>;

  /// 追加字符串并返回新长度 (APPEND)
  async fn append(&self, key: &[u8], val: &[u8]) -> Result<usize>;

  /// 返回字符串长度 (STRLEN)
  async fn strlen(&self, key: &[u8]) -> Result<usize>;

  /// 零拷贝获取字符串切片范围 (GETRANGE)
  async fn getrange_with<R>(
    &self,
    key: &[u8],
    start: isize,
    end: isize,
    f: impl FnOnce(&[u8]) -> R,
  ) -> Result<Option<R>>;

  /// 获取字符串切片范围 (GETRANGE)
  async fn getrange(&self, key: &[u8], start: isize, end: isize) -> Result<Vec<u8>>;

  /// 覆盖写入字符串指定偏移量部分 (SETRANGE)
  async fn setrange(&self, key: &[u8], offset: usize, val: &[u8]) -> Result<usize>;
}

impl<D: Device> StringCommands<D> for StoreSession<D> {
  /// 纯同步 DRAM 内存直读快路径（尝试零开销直读字符串原始数据）
  /// 纪元预保护下的同步内存字符串直读（对标 Garnet ReadWithUnsafeContext，完全绕过 enter() 原子开销）
  ///
  /// - `Ok(Some(Some(res)))`: 内存直读命中且类型有效
  /// - `Ok(Some(None))`: 确切不存在（墓碑或无候选且无磁盘数据）
  /// - `Ok(None)`: 需回退到异步磁盘读取或需查询集合元数据
  /// - `Err(e)`: 类型错误或解析错误
  #[inline]
  fn try_read_string_in_memory_unprotected<R>(
    &self,
    key: &[u8],
    f: impl FnOnce(&[u8]) -> R,
  ) -> Result<Option<Option<R>>> {
    let str_k = self.session_string_key(key);
    let first_addr = self.store.index.find_tag(&str_k);
    let res = self.try_read_raw_in_memory_with_addr(&str_k, first_addr, |bytes| {
      if matches!(bytes.first(), Some(1..=4)) && GarnetObject::deserialize(bytes).is_ok() {
        return Err(wval::Error::InvalidCollectionType(TYPE_OBJECT_MARKER).into());
      }
      Ok(f(bytes))
    })?;

    match res {
      Some(Some(Ok(val))) => Ok(Some(Some(val))),
      Some(Some(Err(e))) => Err(e),
      Some(None) => Ok(None),
      None => Ok(None),
    }
  }

  /// 同步内存字符串直读快路径
  #[inline]
  fn try_read_string_in_memory<R>(
    &self,
    key: &[u8],
    f: impl FnOnce(&[u8]) -> R,
  ) -> Result<Option<Option<R>>> {
    let _guard = self.participant.enter();
    self.try_read_string_in_memory_unprotected(key, f)
  }

  /// 零拷贝读取字符串原始数据，若命中且非富对象直接执行闭包；仅未命中时探测集合元数据以识别类型错误
  async fn read_string_with<R>(&self, key: &[u8], f: impl FnOnce(&[u8]) -> R) -> Result<Option<R>> {
    let hit = self
      .read_with(key, |bytes| {
        if matches!(bytes.first(), Some(1..=4)) && GarnetObject::deserialize(bytes).is_ok() {
          return Err(wval::Error::InvalidCollectionType(TYPE_OBJECT_MARKER).into());
        }
        Ok(f(bytes))
      })
      .await?;

    match hit {
      Some(Ok(res)) => Ok(Some(res)),
      Some(Err(err)) => Err(err),
      None => {
        if let Some(meta) = self.load_meta(key).await?
          && meta.size > 0
        {
          return Err(wval::Error::InvalidCollectionType(meta.collection_type.as_u8()).into());
        }
        Ok(None)
      }
    }
  }

  /// 读取字符串原始数据，若存在且为集合元数据或富对象则返回 InvalidCollectionType 错误
  async fn read_string(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
    self.read_string_with(key, |b| b.to_vec()).await
  }

  /// 整型数值自增自减 (INCRBY / DECRBY / INCR / DECR)
  async fn incrby(&self, key: &[u8], incr: i64) -> Result<i64> {
    let mut itoa_buf = itoa::Buffer::new();
    // 惰性过期裁决前移：已过期键视同不存在，原位 RMW 不得复活过期值；
    // 无 TTL 记录时仅一次哈希探针零额外 I/O
    if self.has_ttl_tag(key)? && self.check_expired(key).await? {
      let formatted = itoa_buf.format(incr);
      self.upsert(key, formatted.as_bytes()).await?;
      return Ok(incr);
    }
    let mut candidate_new_val = None;
    let try_res = self.try_modify_in_place(key, |bytes| {
      let s = from_utf8(bytes).ok()?;
      let old_val = s.trim().parse::<i64>().ok()?;
      let new_val = old_val.checked_add(incr)?;
      let formatted = itoa_buf.format(new_val);
      if formatted.len() == bytes.len() {
        bytes.copy_from_slice(formatted.as_bytes());
        Some(new_val)
      } else {
        candidate_new_val = Some(new_val);
        None
      }
    })?;
    if let Some(res) = try_res {
      return Ok(res);
    }

    if let Some(new_val) = candidate_new_val {
      let formatted = itoa_buf.format(new_val);
      if self.try_modify_with_slack(key, formatted.as_bytes())? {
        return Ok(new_val);
      }
    }

    let old_val = self
      .read_string_with(key, |bytes| {
        let s = from_utf8(bytes).map_err(|_| crate::Error::NotInteger)?;
        s.trim()
          .parse::<i64>()
          .map_err(|_| crate::Error::NotInteger)
      })
      .await?
      .transpose()?
      .unwrap_or(0i64);
    let new_val = old_val.checked_add(incr).ok_or(crate::Error::NotInteger)?;
    let formatted = itoa_buf.format(new_val);
    self.upsert(key, formatted.as_bytes()).await?;
    Ok(new_val)
  }

  /// 浮点数值自增 (INCRBYFLOAT)
  async fn incrbyfloat(&self, key: &[u8], incr: f64) -> Result<f64> {
    if incr.is_nan() || incr.is_infinite() {
      return Err(crate::Error::NotFloat);
    }
    let mut zmij_buf = zmij::Buffer::new();
    // 惰性过期裁决前移：已过期键视同不存在，原位 RMW 不得复活过期值
    if self.has_ttl_tag(key)? && self.check_expired(key).await? {
      let formatted = zmij_buf.format(incr);
      self.upsert(key, formatted.as_bytes()).await?;
      return Ok(incr);
    }
    let mut candidate_new_val = None;
    let try_res = self.try_modify_in_place(key, |bytes| {
      let s = from_utf8(bytes).ok()?;
      let parsed = s.trim().parse::<f64>().ok()?;
      if parsed.is_nan() || parsed.is_infinite() {
        return None;
      }
      let new_val = parsed + incr;
      if new_val.is_nan() || new_val.is_infinite() {
        return None;
      }
      let formatted = zmij_buf.format(new_val);
      if formatted.len() == bytes.len() {
        bytes.copy_from_slice(formatted.as_bytes());
        Some(new_val)
      } else {
        candidate_new_val = Some(new_val);
        None
      }
    })?;
    if let Some(res) = try_res {
      return Ok(res);
    }

    if let Some(new_val) = candidate_new_val {
      let formatted = zmij_buf.format(new_val);
      if self.try_modify_with_slack(key, formatted.as_bytes())? {
        return Ok(new_val);
      }
    }

    let old_val = self
      .read_string_with(key, |bytes| {
        let s = from_utf8(bytes).map_err(|_| crate::Error::NotFloat)?;
        let parsed = s
          .trim()
          .parse::<f64>()
          .map_err(|_| crate::Error::NotFloat)?;
        if parsed.is_nan() || parsed.is_infinite() {
          return Err(crate::Error::NotFloat);
        }
        Ok(parsed)
      })
      .await?
      .transpose()?
      .unwrap_or(0.0f64);
    let new_val = old_val + incr;
    if new_val.is_nan() || new_val.is_infinite() {
      return Err(crate::Error::NanOrInfinity);
    }
    let formatted = zmij_buf.format(new_val);
    self.upsert(key, formatted.as_bytes()).await?;
    Ok(new_val)
  }

  /// 设置新值并返回旧值 (GETSET)
  async fn getset(&self, key: &[u8], val: &[u8]) -> Result<Option<Vec<u8>>> {
    let old = self.read_string(key).await?;
    self.upsert(key, val).await?;
    Ok(old)
  }

  /// 获取并删除键 (GETDEL)
  async fn getdel(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
    let old = self.read_string(key).await?;
    if old.is_some() {
      self.delete(key).await?;
    }
    Ok(old)
  }

  /// 批量零拷贝流式读取字符串 (MGET)
  /// 批量读取字符串并对每个元素调用回调闭包 (MGET Each)
  ///
  /// - 严格对照 C# Garnet ContextReadWithPrefetch 与 MGetReadArgBatch_SG 架构：
  /// - 直接复用底层的 12 项硬件流水线两级预取批量读取方法 `read_batch_with`；
  /// - 若键存在且为普通字符串，向闭包传入 `Some(&[u8])`；
  /// - 若键不存在、已过期、或为富对象/集合类型，向闭包传入 `None`；
  /// - 回调次序严格对位请求键序（严格对照 Redis MGET：结果顺序恒等于请求顺序，
  ///   缺失键为 nil），底座批量读保证按 idx 升序交付，调用方无需携带索引；
  /// - 纯内存批常态零堆内存分配（混批含磁盘冷键时，仅底座冷路径收割与暂存按需分配），
  ///   且对 99.9% 场景免查 `load_meta`；
  /// - 逐 key 惰性过期检查：批量回调内以同步内存探针判定 TTL（不存在即无 TTL，零额外
  ///   I/O），已过期先行回调 None，批量读闭环后再统一物理清除；仅 TTL 记录落盘的罕见
  ///   情形延迟裁决并物化 Vec（唯一可能分配的冷路径），且该键之后的所有交付一律暂存，
  ///   裁决完成后按 idx 升序合流回调（MGET 线上协议按回调序对位写响应，严禁乱序）。
  async fn mget_each<K, F>(&self, keys: &[K], mut on_item: F) -> Result<()>
  where
    K: AsRef<[u8]>,
    F: FnMut(Option<&[u8]>),
  {
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let mut due: Vec<usize> = Vec::new();
    let mut deferred: Vec<(usize, Vec<u8>)> = Vec::new();
    // 延迟裁决保序暂存：一旦出现 Deferred（TTL 记录落盘冷路径），其后所有交付
    // （含命中/对象类型/缺失/Due 的 None）必须先物化再暂存，待延迟项裁决完成后
    // 按 idx 升序合流回调（MGET 线上协议按回调序对位写响应，严禁乱序）
    let mut held: Vec<(usize, Option<Vec<u8>>)> = Vec::new();
    let mut holding = false;
    self
      .read_batch_with(keys, |idx, val_opt| {
        // 暂存期交付（值切片仅在回调存活期有效，必须物化所有权值）
        macro_rules! deliver {
          ($v:expr) => {
            if holding {
              held.push((idx, $v.map(<[u8]>::to_vec)));
            } else {
              on_item($v);
            }
          };
        }
        match val_opt {
          Some(bytes) => {
            if matches!(bytes.first(), Some(1..=4)) && GarnetObject::deserialize(bytes).is_ok() {
              deliver!(None);
              return;
            }
            match self.probe_ttl(keys[idx].as_ref(), now) {
              TtlProbe::Pass => deliver!(Some(bytes)),
              TtlProbe::Due => {
                deliver!(None);
                due.push(idx);
              }
              TtlProbe::Deferred => {
                deferred.push((idx, bytes.to_vec()));
                holding = true;
              }
            }
          }
          None => deliver!(None),
        }
      })
      .await?;

    // 延迟裁决项与暂存项按 idx 升序合流回调（两个序列各自升序，归并保序），
    // check_expired 内含完整磁盘路径裁决与物理清除
    let mut held_iter = held.into_iter().peekable();
    for (d_idx, d_bytes) in deferred {
      while let Some((_, v)) = held_iter.next_if(|&(h_idx, _)| h_idx < d_idx) {
        on_item(v.as_deref());
      }
      if self.check_expired(keys[d_idx].as_ref()).await? {
        on_item(None);
      } else {
        on_item(Some(&d_bytes));
      }
    }
    for (_, v) in held_iter {
      on_item(v.as_deref());
    }

    // 物理清除已判定过期的键（数据 + TTL 记录，走统一 DEL 路径）；
    // 清除前经 check_expired 按 TTL 记录最新态双检：探针判定与清除间隙内
    // 可能被并发续期，不得误删（与 gc sweep 双检口径一致）
    for idx in due {
      self.check_expired(keys[idx].as_ref()).await?;
    }
    Ok(())
  }

  /// 批量读取字符串 (MGET，类型不匹配或不存在时返回 None)
  async fn mget(&self, keys: &[&[u8]]) -> Result<Vec<Option<Vec<u8>>>> {
    let mut res = Vec::with_capacity(keys.len());
    self
      .mget_each(keys, |val_opt| {
        res.push(val_opt.map(|b| b.to_vec()));
      })
      .await?;
    Ok(res)
  }

  /// 批量原子写入字符串 (MSET，受两阶段锁保护，严格对标 Garnet TransactionManager.LockAllKeys)
  async fn mset<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, pairs: &[(K, V)]) -> Result<()> {
    if pairs.is_empty() {
      return Ok(());
    }
    let mut stack_keys = [&b""[..]; 16];
    let heap_keys;
    let keys: &[&[u8]] = if pairs.len() <= 16 {
      for (i, (k, _)) in pairs.iter().enumerate() {
        stack_keys[i] = k.as_ref();
      }
      &stack_keys[..pairs.len()]
    } else {
      heap_keys = pairs.iter().map(|(k, _)| k.as_ref()).collect::<Vec<_>>();
      &heap_keys
    };

    let _guard = self.store.index.acquire_keys_lock_exclusive(keys)?;
    for (k, v) in pairs {
      self.upsert(k.as_ref(), v.as_ref()).await?;
    }
    Ok(())
  }

  /// 匹配模式扫描所有用户键 (KEYS)
  async fn keys(&self, pattern: &[u8]) -> Result<Vec<Vec<u8>>> {
    let _guard = self.participant.enter();
    let mut seen: HashSet<u128> = HashSet::default();
    let mut res = Vec::new();

    for_each_live_user_key(self, |k| {
      if glob_match(pattern, k) && seen.insert(whasher::fast_hash128(k)) {
        res.push(k.to_vec());
      }
      true
    })
    .await?;
    Ok(res)
  }

  /// 追加字符串并返回新长度 (APPEND)
  async fn append(&self, key: &[u8], val: &[u8]) -> Result<usize> {
    match self.read_string(key).await? {
      None => {
        self.upsert(key, val).await?;
        Ok(val.len())
      }
      Some(mut bytes) => {
        bytes.extend_from_slice(val);
        self.upsert(key, &bytes).await?;
        Ok(bytes.len())
      }
    }
  }

  /// 返回字符串长度 (STRLEN)
  async fn strlen(&self, key: &[u8]) -> Result<usize> {
    Ok(self.read_string_with(key, |b| b.len()).await?.unwrap_or(0))
  }

  /// 零拷贝获取字符串切片范围 (GETRANGE)
  async fn getrange_with<R>(
    &self,
    key: &[u8],
    start: isize,
    end: isize,
    f: impl FnOnce(&[u8]) -> R,
  ) -> Result<Option<R>> {
    self
      .read_string_with(key, |bytes| {
        let slice = match normalize_range(bytes.len(), start, end) {
          Some((s, e)) => &bytes[s..=e],
          None => b"",
        };
        f(slice)
      })
      .await
  }

  /// 获取字符串切片范围 (GETRANGE)
  async fn getrange(&self, key: &[u8], start: isize, end: isize) -> Result<Vec<u8>> {
    let res = self
      .getrange_with(key, start, end, |slice| slice.to_vec())
      .await?;
    Ok(res.unwrap_or_default())
  }

  /// 覆盖写入字符串指定偏移量部分 (SETRANGE)
  async fn setrange(&self, key: &[u8], offset: usize, val: &[u8]) -> Result<usize> {
    if val.is_empty() {
      return self.strlen(key).await;
    }
    let needed_len = match offset.checked_add(val.len()) {
      Some(l) => l,
      None => return Err(wval::Error::ValueLengthOverflow(offset).into()),
    };
    if needed_len > 536_870_912 {
      return Err(wval::Error::ValueLengthOverflow(needed_len).into());
    }

    // 1. 惰性过期裁决前移 + 尝试原位覆写（严格对标 C# Garnet InPlaceUpdaterWorker）：
    //    原位覆写不得复活已过期值，无 TTL 记录时仅一次哈希探针零额外 I/O
    if !(self.has_ttl_tag(key)? && self.check_expired(key).await?) {
      let try_res = self.try_modify_in_place(key, |bytes| {
        if bytes.len() >= needed_len {
          bytes[offset..needed_len].copy_from_slice(val);
          Some(bytes.len())
        } else {
          None
        }
      })?;
      if let Some(len) = try_res {
        return Ok(len);
      }
    }

    // 2. 需要扩容或非可变区，降级走标准路径
    let mut bytes = self.read_string(key).await?.unwrap_or_default();
    if bytes.len() < needed_len {
      bytes.resize(needed_len, 0);
    }
    bytes[offset..needed_len].copy_from_slice(val);
    self.upsert(key, &bytes).await?;
    Ok(bytes.len())
  }
}
