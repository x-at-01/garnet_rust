use core::str::from_utf8;
use std::{cmp::Reverse, collections::BinaryHeap};

use bitcode::{Decode, Encode};
use coarsetime::Clock;
use whasher::{Entry, HashMap, hash_map_with_capacity};
use wval::sample_distinct_indices;

use crate::{
  error::{Error, Result},
  expire::{ExpireOpt, ExpireResult},
};

/// 极简格式版本号（1 字节）
pub const FORMAT_VERSION: u8 = 1;

/// 允许重复采样的单次安全返回上限（防恶意大数内存耗尽与死循环）
pub const MAX_RAND_SAMPLE_LIMIT: usize = 1_000_000;

/// HSCAN 单次调用默认返回条目数 (对标 Redis/Garnet 默认 COUNT 10)
const DEFAULT_HSCAN_COUNT: usize = 10;

/// 掩码位：最高位用于指示字段是否包含过期时间戳
pub const EXPIRATION_BIT_MASK: u32 = 1 << 31;

/// 哈希键值对条目 (field, value)
pub type HashEntry = (Vec<u8>, Vec<u8>);

/// HSCAN 游标扫描结果类型: (next_cursor, matched_entries)
pub type HScanResult = (usize, Vec<HashEntry>);

/// HSCAN 零拷贝借用扫描结果类型: (next_cursor, matched_borrowed_entries)
pub type HScanBorrowedResult<'a> = (usize, Vec<(&'a [u8], &'a [u8])>);

/// 哈希条目 Bitcode 紧凑表示（公开类型，支持独立编解码）
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct HashEntryBitcode {
  /// 字段键
  pub field: Vec<u8>,
  /// 字段值
  pub val: Vec<u8>,
  /// 绝对过期时间戳 (毫秒)
  pub expire_at: Option<u64>,
}

/// 字段过期优先队列类型别名 (仅内部过期结构使用)
pub(crate) type ExpirationQueue = BinaryHeap<Reverse<(u64, Vec<u8>)>>;

/// 当前系统时间戳 (毫秒)
#[inline]
fn now_ms() -> u64 {
  Clock::now_since_epoch().as_millis()
}

pub use wval::glob_match;

/// 哈希对象 (对应 Redis Hash / Garnet HashObject)
#[derive(Debug, Clone, Default)]
pub struct HashObject {
  /// 字段字典
  fields: HashMap<Vec<u8>, Vec<u8>>,
  /// 字段过期时间戳字典 (毫秒)
  expiration_times: Option<HashMap<Vec<u8>, u64>>,
  /// 字段过期优先队列 (最小堆，按过期时间升序淘汰)
  expiration_queue: Option<ExpirationQueue>,
}

impl HashObject {
  /// 创建空哈希对象
  #[inline]
  pub fn new() -> Self {
    Self::default()
  }

  /// 创建指定初始容量的哈希对象
  #[inline]
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      fields: hash_map_with_capacity(capacity),
      expiration_times: None,
      expiration_queue: None,
    }
  }

  /// 获取当前字段数量 (自动清理过期字段，HLEN)
  #[inline]
  pub fn len(&mut self) -> usize {
    self.delete_expired();
    self.fields.len()
  }

  /// 获取在指定时间戳下的有效字段数量 (只读引用版本)
  #[inline]
  pub fn len_ref_at(&self, now: u64) -> usize {
    if let Some(times) = &self.expiration_times {
      let expired_count = times.values().filter(|&&exp| exp <= now).count();
      self.fields.len().saturating_sub(expired_count)
    } else {
      self.fields.len()
    }
  }

  /// 获取当前有效字段数量 (只读引用版本，自动排除已过期字段)
  #[inline]
  pub fn len_ref(&self) -> usize {
    self.len_ref_at(now_ms())
  }

  /// 判断哈希对象是否为空 (自动清理过期字段)
  pub fn is_empty(&mut self) -> bool {
    self.len() == 0
  }

  /// 判断哈希对象是否为空 (只读引用版本)
  pub fn is_empty_ref(&self) -> bool {
    self.len_ref() == 0
  }

  /// 获取底层哈希表当前已分配容量
  #[inline]
  pub fn capacity(&self) -> usize {
    self.fields.capacity()
  }

  /// 收缩底层哈希表与过期时间存储以节约内存
  pub fn shrink_to_fit(&mut self) {
    self.delete_expired();
    self.fields.shrink_to_fit();
    if let (Some(times), Some(queue)) = (&mut self.expiration_times, &mut self.expiration_queue) {
      times.shrink_to_fit();
      // 若堆中积压的历史废弃幽灵条目超过有效过期键数的 2 倍，重构最小堆彻底释放内存
      if queue.len() > times.len().saturating_mul(2) {
        let mut new_queue = BinaryHeap::with_capacity(times.len());
        for (field, &expire) in times.iter() {
          new_queue.push(Reverse((expire, field.clone())));
        }
        *queue = new_queue;
      }
      queue.shrink_to_fit();
    }
    self.cleanup_expiration_if_empty();
  }

  /// 初始化过期结构
  #[inline]
  fn init_expiration(&mut self) {
    if self.expiration_times.is_none() {
      self.expiration_times = Some(HashMap::default());
      self.expiration_queue = Some(BinaryHeap::new());
    }
  }

  /// 当过期字典为空或所有字段均已被删除时，回收过期结构释放内存 (对标 Garnet CleanupExpirationStructuresIfEmpty)
  #[inline]
  fn cleanup_expiration_if_empty(&mut self) {
    if self.fields.is_empty() {
      self.expiration_times = None;
      self.expiration_queue = None;
      if self.fields.capacity() > 128 {
        self.fields.shrink_to_fit();
      }
      return;
    }
    if let Some(times) = &self.expiration_times
      && times.is_empty()
    {
      self.expiration_times = None;
      self.expiration_queue = None;
    }
  }

  /// 物理清理所有已过期字段并回收空间，返回清理的字段数
  #[inline]
  pub fn purge_expired(&mut self) -> usize {
    self.delete_expired()
  }

  /// 清理已过期的所有字段，返回被清理的字段数量
  pub fn delete_expired(&mut self) -> usize {
    let (Some(queue), Some(times)) = (&mut self.expiration_queue, &mut self.expiration_times)
    else {
      return 0;
    };
    let mut purged = 0;
    let now = now_ms();
    while let Some(Reverse((expire_time, _))) = queue.peek() {
      if *expire_time > now {
        break;
      }
      let (expire_time, field) = queue.pop().unwrap().0;
      // 检查当前记录的时间戳是否与堆中一致 (防止字段被更新后残留的旧堆条目误删新字段)
      if let Some(&current_expire) = times.get(&field)
        && current_expire == expire_time
      {
        times.remove(&field);
        if self.fields.remove(&field).is_some() {
          purged += 1;
        }
      }
    }
    self.cleanup_expiration_if_empty();
    purged
  }

  /// 检查特定字段是否已过期；若已过期就地将其清理并返回 true
  #[inline]
  fn check_and_purge_expired(&mut self, field: &[u8]) -> bool {
    if let Some(times) = &mut self.expiration_times
      && let Some(&expire_time) = times.get(field)
      && expire_time <= now_ms()
    {
      times.remove(field);
      self.fields.remove(field);
      self.cleanup_expiration_if_empty();
      return true;
    }
    false
  }

  /// 写入键值对 (HSET)
  ///
  /// 若字段已存在则覆盖值，且遵循 Redis 规范清除该字段此前设置的过期时间。
  /// 返回 true 表示新增字段（含覆盖此前已过期的字段），false 表示覆盖未过期旧字段。
  pub fn hset(&mut self, field: impl Into<Vec<u8>>, val: impl Into<Vec<u8>>) -> bool {
    let field = field.into();
    let val = val.into();

    let was_expired = self.check_and_purge_expired(&field);

    // 覆盖存活字段时清除其过期时间（字段持久化语义）
    if !was_expired
      && let Some(times) = &mut self.expiration_times
      && times.remove(&field).is_some()
    {
      self.cleanup_expiration_if_empty();
    }

    // 过期字段已被就地清除，insert 必为新增；存活字段覆盖则 is_new = false
    self.fields.insert(field, val).is_none()
  }

  /// 仅当字段不存在时写入 (HSETNX)
  ///
  /// 返回 true 表示成功写入，false 表示字段已存在未作修改。
  pub fn hsetnx(&mut self, field: impl Into<Vec<u8>>, val: impl Into<Vec<u8>>) -> bool {
    let field = field.into();
    if self.check_and_purge_expired(&field) {
      self.fields.insert(field, val.into());
      true
    } else {
      match self.fields.entry(field) {
        Entry::Vacant(e) => {
          e.insert(val.into());
          true
        }
        Entry::Occupied(_) => false,
      }
    }
  }

  /// 批量写入键值对 (HMSET)
  pub fn hmset(&mut self, kvs: impl IntoIterator<Item = HashEntry>) -> usize {
    let mut count = 0;
    for (k, v) in kvs {
      if self.hset(k, v) {
        count += 1;
      }
    }
    count
  }

  /// 判断指定字段自 `now` 起是否已过期 (内部高频辅助，时钟由调用方提升，避免逐键取时)
  #[inline]
  fn expired_since(&self, field: &[u8], now: u64) -> bool {
    self
      .expiration_times
      .as_ref()
      .is_some_and(|times| times.get(field).is_some_and(|&expire| expire <= now))
  }

  /// 判断指定字段是否已过期 (只读检查)
  #[inline]
  pub fn is_expired(&self, field: &[u8]) -> bool {
    self.expired_since(field, now_ms())
  }

  /// 读取字段值 (HGET)
  pub fn hget(&mut self, field: &[u8]) -> Option<&[u8]> {
    if self.check_and_purge_expired(field) {
      return None;
    }
    self.fields.get(field).map(|v| v.as_slice())
  }

  /// 只读读取字段值 (HGET，零克隆)
  pub fn hget_ref(&self, field: &[u8]) -> Option<&[u8]> {
    if self.is_expired(field) {
      return None;
    }
    self.fields.get(field).map(|v| v.as_slice())
  }

  /// 批量读取字段值 (HMGET)
  ///
  /// 先物理清理过期字段，随后委托 [`Self::hmget_ref`] 直查（清理后 TTL 表为空时判定零开销）。
  pub fn hmget(&mut self, fields: &[&[u8]]) -> Vec<Option<Vec<u8>>> {
    self.delete_expired();
    self
      .hmget_ref(fields)
      .into_iter()
      .map(|v| v.map(<[u8]>::to_vec))
      .collect()
  }

  /// 批量只读读取字段值 (HMGET，零克隆)
  pub fn hmget_ref<'a>(&'a self, fields: &[&[u8]]) -> Vec<Option<&'a [u8]>> {
    let now = now_ms();
    fields
      .iter()
      .map(|&f| {
        if self.expired_since(f, now) {
          None
        } else {
          self.fields.get(f).map(|v| v.as_slice())
        }
      })
      .collect()
  }

  /// 获取所有字段与值 (HGETALL)
  pub fn hgetall(&mut self) -> Vec<HashEntry> {
    self.delete_expired();
    self.hgetall_ref()
  }

  /// 获取所有有效字段与值的零拷贝借用迭代器 (HGETALL 零拷贝遍历，排除已过期字段)
  #[inline]
  pub fn iter_valid(&self) -> impl Iterator<Item = (&[u8], &[u8])> {
    let now = now_ms();
    self
      .fields
      .iter()
      .filter(move |(k, _)| !self.expired_since(k, now))
      .map(|(k, v)| (k.as_slice(), v.as_slice()))
  }

  /// 获取所有字段与值 (HGETALL，只读引用版本)
  pub fn hgetall_ref(&self) -> Vec<HashEntry> {
    self
      .iter_valid()
      .map(|(k, v)| (k.to_vec(), v.to_vec()))
      .collect()
  }

  /// 获取所有字段名 (HKEYS)
  pub fn hkeys(&mut self) -> Vec<Vec<u8>> {
    self.delete_expired();
    self.hkeys_ref()
  }

  /// 获取所有有效字段名的零拷贝借用迭代器 (HKEYS 零拷贝遍历)
  #[inline]
  pub fn hkeys_iter(&self) -> impl Iterator<Item = &[u8]> {
    self.iter_valid().map(|(k, _)| k)
  }

  /// 获取所有字段名 (HKEYS，只读引用版本)
  pub fn hkeys_ref(&self) -> Vec<Vec<u8>> {
    self.hkeys_iter().map(|k| k.to_vec()).collect()
  }

  /// 获取所有值 (HVALS)
  pub fn hvals(&mut self) -> Vec<Vec<u8>> {
    self.delete_expired();
    self.hvals_ref()
  }

  /// 获取所有有效字段值的零拷贝借用迭代器 (HVALS 零拷贝遍历)
  #[inline]
  pub fn hvals_iter(&self) -> impl Iterator<Item = &[u8]> {
    self.iter_valid().map(|(_, v)| v)
  }

  /// 获取所有值 (HVALS，只读引用版本)
  pub fn hvals_ref(&self) -> Vec<Vec<u8>> {
    self.hvals_iter().map(|v| v.to_vec()).collect()
  }

  /// 检查字段是否存在 (HEXISTS)
  pub fn hexists(&mut self, field: &[u8]) -> bool {
    if self.check_and_purge_expired(field) {
      return false;
    }
    self.fields.contains_key(field)
  }

  /// 只读检查字段是否存在 (HEXISTS，零克隆)
  pub fn hexists_ref(&self, field: &[u8]) -> bool {
    if self.is_expired(field) {
      return false;
    }
    self.fields.contains_key(field)
  }

  /// 删除指定字段 (HDEL)
  ///
  /// 返回成功删除的未过期字段总数；已过期字段视作不存在，仅物理清除不计入。
  pub fn hdel(&mut self, fields: &[&[u8]]) -> usize {
    let now = now_ms();
    let mut removed = 0;
    for &field in fields {
      // 先依据 TTL 表判定存活性，再移除记录，过期字段视作不存在不计入删除数
      let expired = self.expired_since(field, now);
      if let Some(times) = &mut self.expiration_times {
        times.remove(field);
      }
      if self.fields.remove(field).is_some() && !expired {
        removed += 1;
      }
    }
    self.cleanup_expiration_if_empty();
    removed
  }

  /// 获取字段值的字节长度 (HSTRLEN)
  pub fn hstrlen(&mut self, field: &[u8]) -> usize {
    if self.check_and_purge_expired(field) {
      return 0;
    }
    self.fields.get(field).map(|v| v.len()).unwrap_or(0)
  }

  /// 获取字段值的字节长度 (HSTRLEN，只读引用版本)
  pub fn hstrlen_ref(&self, field: &[u8]) -> usize {
    if self.is_expired(field) {
      return 0;
    }
    self.fields.get(field).map(|v| v.len()).unwrap_or(0)
  }

  /// 将格式化后的数值写回字段 (存在则原地复用缓冲区，不存在则插入，对标 C# HashIncrement 的原地复用)
  #[inline]
  fn store_numeric(&mut self, field: &[u8], val_bytes: &[u8]) {
    match self.fields.get_mut(field) {
      Some(val) => {
        val.clear();
        val.extend_from_slice(val_bytes);
      }
      None => {
        self.fields.insert(field.to_vec(), val_bytes.to_vec());
      }
    }
  }

  /// 整数自增 (HINCRBY，对标 Garnet HashObjectImpl.HashIncrement)
  ///
  /// 若字段不存在或已过期，则以自增量作为初始值。
  /// 注意：根据 Redis/Garnet 语义，自增操作保留字段既有的过期时间（不清除 TTL）。
  pub fn hincrby(&mut self, field: &[u8], incr: i64) -> Result<i64> {
    self.check_and_purge_expired(field);

    // 计算新值：既有值解析失败或加法溢出立即报错，不产生部分写入
    let new_val = match self.fields.get(field) {
      Some(val) => {
        let cur = from_utf8(val)
          .ok()
          .and_then(|s| s.parse::<i64>().ok())
          .ok_or(Error::InvalidNumber)?;
        cur.checked_add(incr).ok_or(Error::InvalidNumber)?
      }
      None => incr,
    };

    let mut buf = itoa::Buffer::new();
    let val_bytes = buf.format(new_val).as_bytes();
    self.store_numeric(field, val_bytes);
    Ok(new_val)
  }

  /// 浮点数自增 (HINCRBYFLOAT，对标 Garnet HashObjectImpl.HashIncrementFloat)
  ///
  /// 若字段不存在或已过期，则从 0.0 开始自增。
  /// 注意：根据 Redis/Garnet 语义，自增操作保留字段既有的过期时间（不清除 TTL）。
  pub fn hincrbyfloat(&mut self, field: &[u8], incr: f64) -> Result<f64> {
    if !incr.is_finite() {
      return Err(Error::InvalidNumber);
    }

    self.check_and_purge_expired(field);

    // 计算新值：既有值解析失败或加法溢出至非有限数立即报错，不产生部分写入
    let new_val = match self.fields.get(field) {
      Some(val) => {
        let cur = from_utf8(val)
          .ok()
          .and_then(|s| s.parse::<f64>().ok())
          .ok_or(Error::InvalidNumber)?;
        if !cur.is_finite() {
          return Err(Error::InvalidNumber);
        }
        let sum = cur + incr;
        if !sum.is_finite() {
          return Err(Error::InvalidNumber);
        }
        sum + 0.0 // IEEE754: -0.0 + 0.0 = +0.0，归一化负零
      }
      // 新字段从 0.0 起算，等价于 0.0 + incr；+ 0.0 同步归一化负零
      None => incr + 0.0,
    };

    // 整数值以整数格式写回 (对齐 Redis ld2string / C# double.TryFormat，如 0.0 → "0"、5.0 → "5")，
    // 保证后续 HINCRBY 可在结果上继续操作；`< i64::MAX as f64` (即 2^63) 确保转 i64 无损
    if new_val.trunc() == new_val && (i64::MIN as f64..i64::MAX as f64).contains(&new_val) {
      let mut buf = itoa::Buffer::new();
      let val_bytes = buf.format(new_val as i64).as_bytes();
      self.store_numeric(field, val_bytes);
    } else {
      let mut buf = zmij::Buffer::new();
      let val_bytes = buf.format_finite(new_val).as_bytes();
      self.store_numeric(field, val_bytes);
    }
    Ok(new_val)
  }

  /// 随机获取字段 (HRANDFIELD)
  ///
  /// count > 0: 返回至多 count 个互不相同的字段
  /// count < 0: 返回 |count| 个字段，可能包含重复项
  pub fn hrandfield(&mut self, count: isize, with_values: bool) -> Vec<(Vec<u8>, Option<Vec<u8>>)> {
    self.delete_expired();
    self
      .hrandfield_ref(count, with_values)
      .into_iter()
      .map(|(k, v)| (k.to_vec(), v.map(<[u8]>::to_vec)))
      .collect()
  }

  /// 随机获取字段 (HRANDFIELD，只读引用版本，零拷贝借用)
  ///
  /// count > 0: 返回至多 count 个互不相同的字段与可选值
  /// count < 0: 返回 |count| 个字段与可选值，可能包含重复项
  pub fn hrandfield_ref<'a>(
    &'a self,
    count: isize,
    with_values: bool,
  ) -> Vec<(&'a [u8], Option<&'a [u8]>)> {
    let now = now_ms();
    let total = self.len_ref_at(now);
    if total == 0 || count == 0 {
      return Vec::new();
    }

    if count.unsigned_abs() == 1 {
      let target_idx = fastrand::usize(0..total);
      if let Some((k, v)) = self
        .fields
        .iter()
        .filter(|(k, _)| !self.expired_since(k, now))
        .nth(target_idx)
      {
        return vec![(k.as_slice(), with_values.then_some(v.as_slice()))];
      }
      return Vec::new();
    }

    if count > 0 {
      let pick_count = (count as usize).min(total);
      if pick_count == total {
        return self
          .fields
          .iter()
          .filter(|(k, _)| !self.expired_since(k, now))
          .map(|(k, v)| (k.as_slice(), with_values.then_some(v.as_slice())))
          .collect();
      }

      let sorted_indices = sample_distinct_indices(total, pick_count);

      let mut res = Vec::with_capacity(pick_count);
      let mut cur_pick = 0;
      for (i, (k, v)) in self
        .fields
        .iter()
        .filter(|(k, _)| !self.expired_since(k, now))
        .enumerate()
      {
        if i == sorted_indices[cur_pick] {
          res.push((k.as_slice(), with_values.then_some(v.as_slice())));
          cur_pick += 1;
          if cur_pick == pick_count {
            break;
          }
        }
      }
      res
    } else {
      let pick_count = count.unsigned_abs().min(MAX_RAND_SAMPLE_LIMIT);
      let mut pairs: Vec<(usize, usize)> = (0..pick_count)
        .map(|orig_idx| (fastrand::usize(0..total), orig_idx))
        .collect();
      pairs.sort_unstable_by_key(|&(sample_idx, _)| sample_idx);

      let mut result: Vec<(&'a [u8], Option<&'a [u8]>)> = vec![(&[], None); pick_count];
      let mut cur_pair = 0;

      for (i, (k, v)) in self
        .fields
        .iter()
        .filter(|(k, _)| !self.expired_since(k, now))
        .enumerate()
      {
        while cur_pair < pick_count && pairs[cur_pair].0 == i {
          let orig_idx = pairs[cur_pair].1;
          result[orig_idx] = (k.as_slice(), with_values.then_some(v.as_slice()));
          cur_pair += 1;
        }
        if cur_pair == pick_count {
          break;
        }
      }
      result
    }
  }

  /// 设置字段绝对过期时间戳 (对标 Garnet HashObject.SetExpiration，即 HPEXPIREAT 语义)
  ///
  /// expire_at_ms: 目标绝对过期时间戳 (毫秒)；HEXPIRE/HPEXPIRE/HEXPIREAT 的
  /// 相对时长或秒级时间戳由调用方换算为绝对毫秒后传入 (见 hexpireat/hpexpireat)。
  ///
  /// 与 C# 的差异：NX/XX/GT/LT 条件校验先于「过去时间戳删除」执行，
  /// 条件不满足时绝不误删字段 (对齐 Redis 7.4 键级 EXPIRE 语义，Garnet C# 会先删)。
  pub fn hexpire(&mut self, field: &[u8], expire_at_ms: u64, option: ExpireOpt) -> ExpireResult {
    if self.check_and_purge_expired(field) || !self.fields.contains_key(field) {
      return ExpireResult::KeyNotFound;
    }

    let curr_expire = self
      .expiration_times
      .as_ref()
      .and_then(|times| times.get(field).copied());

    // 校验选项冲突 (对标 Redis 7.4+ / Garnet SetExpiration: line 551-573)
    if let Some(curr) = curr_expire {
      if option.nx {
        return ExpireResult::ExpireConditionNotMet;
      }
      if option.gt && expire_at_ms <= curr {
        return ExpireResult::ExpireConditionNotMet;
      }
      if option.lt && expire_at_ms >= curr {
        return ExpireResult::ExpireConditionNotMet;
      }
    } else if option.xx || option.gt {
      return ExpireResult::ExpireConditionNotMet;
    }

    let now = now_ms();
    if expire_at_ms <= now {
      // 过期时间小于等于当前时间，且已通过选项校验，立即删除该字段
      if let Some(times) = &mut self.expiration_times {
        times.remove(field);
      }
      self.fields.remove(field);
      self.cleanup_expiration_if_empty();
      return ExpireResult::KeyAlreadyExpired;
    }

    // 若该字段已在过期表中，直接原地更新时间戳，免除哈希表二次插入与键克隆
    if let Some(times) = &mut self.expiration_times
      && let Some(val) = times.get_mut(field)
    {
      *val = expire_at_ms;
      let queue = self.expiration_queue.as_mut().unwrap();
      queue.push(Reverse((expire_at_ms, field.to_vec())));
      return ExpireResult::Ok;
    }

    self.init_expiration();
    let times = self.expiration_times.as_mut().unwrap();
    let queue = self.expiration_queue.as_mut().unwrap();

    let field_vec = field.to_vec();
    times.insert(field_vec.clone(), expire_at_ms);
    queue.push(Reverse((expire_at_ms, field_vec)));

    ExpireResult::Ok
  }

  /// 设置字段在指定 UNIX 秒级时间戳过期 (HEXPIREAT，对标 Garnet RespCommand.HEXPIREAT)
  #[inline]
  pub fn hexpireat(&mut self, field: &[u8], unix_time_sec: u64, option: ExpireOpt) -> ExpireResult {
    self.hexpire(field, unix_time_sec.saturating_mul(1000), option)
  }

  /// 设置字段在指定 UNIX 毫秒级时间戳过期 (HPEXPIREAT，对标 Garnet RespCommand.HPEXPIREAT)
  #[inline]
  pub fn hpexpireat(&mut self, field: &[u8], unix_time_ms: u64, option: ExpireOpt) -> ExpireResult {
    self.hexpire(field, unix_time_ms, option)
  }

  /// 查询字段剩余过期毫秒数 (HTTL / HPTTL)
  ///
  /// -2: 字段不存在或已过期
  /// -1: 字段存在但未设置过期
  /// >=0: 剩余毫秒数
  pub fn httl(&mut self, field: &[u8]) -> i64 {
    self.check_and_purge_expired(field);
    self.httl_ref(field)
  }

  /// 只读查询字段剩余过期毫秒数 (HTTL / HPTTL，只读引用版本)
  pub fn httl_ref(&self, field: &[u8]) -> i64 {
    if !self.fields.contains_key(field) {
      return ExpireResult::KeyNotFound as i64;
    }

    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(field)
    {
      let now = now_ms();
      if expire_at <= now {
        return ExpireResult::KeyNotFound as i64;
      }
      return (expire_at.saturating_sub(now)) as i64;
    }

    ExpireResult::NoExpirationSet as i64
  }

  /// 查询字段绝对过期 UNIX 时间戳 (秒) (HEXPIRETIME，对标 Garnet RespCommand.HEXPIRETIME)
  ///
  /// -2: 字段不存在或已过期
  /// -1: 字段存在但未设置过期
  /// >=0: 绝对过期时间戳 (秒)
  pub fn hexpiretime(&mut self, field: &[u8]) -> i64 {
    self.check_and_purge_expired(field);
    self.hexpiretime_ref(field)
  }

  /// 只读查询字段绝对过期 UNIX 时间戳 (秒) (HEXPIRETIME，只读引用版本)
  pub fn hexpiretime_ref(&self, field: &[u8]) -> i64 {
    if !self.fields.contains_key(field) {
      return ExpireResult::KeyNotFound as i64;
    }

    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(field)
    {
      if expire_at <= now_ms() {
        return ExpireResult::KeyNotFound as i64;
      }
      return (expire_at / 1000) as i64;
    }

    ExpireResult::NoExpirationSet as i64
  }

  /// 查询字段绝对过期 UNIX 时间戳 (毫秒) (HPEXPIRETIME，对标 Garnet RespCommand.HPEXPIRETIME)
  ///
  /// -2: 字段不存在或已过期
  /// -1: 字段存在但未设置过期
  /// >=0: 绝对过期时间戳 (毫秒)
  pub fn hpexpiretime(&mut self, field: &[u8]) -> i64 {
    self.check_and_purge_expired(field);
    self.hpexpiretime_ref(field)
  }

  /// 只读查询字段绝对过期 UNIX 时间戳 (毫秒) (HPEXPIRETIME，只读引用版本)
  pub fn hpexpiretime_ref(&self, field: &[u8]) -> i64 {
    if !self.fields.contains_key(field) {
      return ExpireResult::KeyNotFound as i64;
    }

    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(field)
    {
      if expire_at <= now_ms() {
        return ExpireResult::KeyNotFound as i64;
      }
      return expire_at as i64;
    }

    ExpireResult::NoExpirationSet as i64
  }

  /// 移除字段的过期时间 (HPERSIST，对标 Garnet HashObject.Persist 三态语义)
  ///
  /// 返回 Ok 表示成功移除；NoExpirationSet (-1) 表示字段存在但无过期时间；
  /// KeyNotFound (-2) 表示字段不存在或已过期。
  pub fn hpersist(&mut self, field: &[u8]) -> ExpireResult {
    if self.check_and_purge_expired(field) || !self.fields.contains_key(field) {
      return ExpireResult::KeyNotFound;
    }

    if let Some(times) = &mut self.expiration_times
      && times.remove(field).is_some()
    {
      self.cleanup_expiration_if_empty();
      return ExpireResult::Ok;
    }
    ExpireResult::NoExpirationSet
  }

  /// 游标扫描 (HSCAN)
  ///
  /// 先物理清理过期字段，再委托 [`Self::hscan_borrowed`] 扫描并克隆结果。
  pub fn hscan(&mut self, cursor: usize, count: usize, pattern: Option<&[u8]>) -> HScanResult {
    self.delete_expired();
    let (cursor, items) = self.hscan_borrowed(cursor, count, pattern);
    (
      cursor,
      items
        .into_iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect(),
    )
  }

  /// 游标扫描 (HSCAN，只读引用版本)
  ///
  /// 委托 [`Self::hscan_borrowed`] 并克隆结果，游标语义以「存活条目序数」推进。
  pub fn hscan_ref(&self, cursor: usize, count: usize, pattern: Option<&[u8]>) -> HScanResult {
    let (cursor, items) = self.hscan_borrowed(cursor, count, pattern);
    (
      cursor,
      items
        .into_iter()
        .map(|(k, v)| (k.to_vec(), v.to_vec()))
        .collect(),
    )
  }

  /// 游标扫描 (HSCAN，唯一实现，零拷贝借用)
  ///
  /// 游标语义对标 C# HashObject.Scan：以「存活条目序数」推进，
  /// 无论是否命中 pattern 均计入游标；返回 0 表示扫描终止。
  pub fn hscan_borrowed(
    &self,
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> HScanBorrowedResult<'_> {
    let now = now_ms();
    let total_valid = self.len_ref_at(now);
    if cursor >= total_valid {
      return (0, Vec::new());
    }
    let limit = if count == 0 {
      DEFAULT_HSCAN_COUNT
    } else {
      count
    };
    let mut items = Vec::with_capacity(limit.min(total_valid - cursor));
    for (idx, (k, v)) in (cursor..).zip(
      self
        .fields
        .iter()
        .filter(|(k, _)| !self.expired_since(k, now))
        .skip(cursor),
    ) {
      if pattern.is_none_or(|p| glob_match(p, k)) {
        items.push((k.as_slice(), v.as_slice()));
        if items.len() >= limit {
          let next_cur = if idx + 1 >= total_valid { 0 } else { idx + 1 };
          return (next_cur, items);
        }
      }
    }
    (0, items)
  }

  /// 极简可扩展二进制序列化（只读版本，自动排除已过期字段）
  ///
  /// 预估容量与存活计数在单次遍历中同时完成，序列化全程零扩容零重拷贝。
  pub fn serialize_ref(&self, buf: &mut Vec<u8>) {
    let now = now_ms();

    let mut valid_count = 0usize;
    let est_len = 5
      + self
        .fields
        .iter()
        .filter(|(k, _)| !self.expired_since(k, now))
        .map(|(k, v)| {
          valid_count += 1;
          8 + k.len()
            + v.len()
            + usize::from(
              self
                .expiration_times
                .as_ref()
                .is_some_and(|t| t.contains_key(k)),
            ) * 8
        })
        .sum::<usize>();
    // 与 wedb_list 同类防御：count/长度头均以 u32 落盘，编码侧断言无静默截断
    debug_assert!(valid_count <= u32::MAX as usize);
    buf.reserve(est_len);

    buf.push(FORMAT_VERSION);
    buf.extend_from_slice(&(valid_count as u32).to_le_bytes());

    for (k, v) in &self.fields {
      if self.expired_since(k, now) {
        continue;
      }
      let expire = self
        .expiration_times
        .as_ref()
        .and_then(|t| t.get(k).copied());
      debug_assert!(k.len() <= u32::MAX as usize);
      debug_assert!(v.len() <= u32::MAX as usize);
      let mut key_len = k.len() as u32;
      if expire.is_some() {
        key_len |= EXPIRATION_BIT_MASK;
      }

      buf.extend_from_slice(&key_len.to_le_bytes());
      buf.extend_from_slice(k);
      buf.extend_from_slice(&(v.len() as u32).to_le_bytes());
      buf.extend_from_slice(v);

      if let Some(exp) = expire {
        buf.extend_from_slice(&exp.to_le_bytes());
      }
    }
  }

  /// 极简可扩展二进制序列化（对标 C# HashObject.DoSerialize）
  ///
  /// 格式（极简主义，零多余，零幻数）：
  /// - version: u8 (当前 FORMAT_VERSION = 1)
  /// - count: u32 (小端)
  /// - 遍历每个键值对：
  ///   - key_len: u32 (若含过期时间，最高位置 1)
  ///   - key 字节切片
  ///   - val_len: u32
  ///   - val 字节切片
  ///   - 若含过期时间，写入 expiration: u64 (毫秒时间戳)
  pub fn serialize(&mut self, buf: &mut Vec<u8>) {
    self.delete_expired();
    self.serialize_ref(buf);
  }

  /// 极简可扩展二进制反序列化
  ///
  /// 已过期的条目在恢复时直接丢弃（对齐 C# 反序列化语义）。
  pub fn deserialize(buf: &[u8]) -> Result<Self> {
    if buf.len() < 5 {
      return Err(Error::BufferTooShort);
    }
    if buf[0] != FORMAT_VERSION {
      return Err(Error::UnsupportedVersion(buf[0]));
    }

    let count = le_u32(buf, 1).ok_or(Error::BufferTooShort)? as usize;
    // 每条目至少 8 字节 (key_len + val_len 双前缀)，提前拒绝伪造超大 count
    if count > (buf.len() - 5) / 8 {
      return Err(Error::CorruptedData);
    }

    let mut cur = 5;
    let now = now_ms();
    let mut hash = Self::with_capacity(count);

    for _ in 0..count {
      let raw_key_len = le_u32(buf, cur).ok_or(Error::BufferTooShort)?;
      cur += 4;

      let has_expiration = raw_key_len & EXPIRATION_BIT_MASK != 0;
      let key_end = cur
        .checked_add((raw_key_len & !EXPIRATION_BIT_MASK) as usize)
        .ok_or(Error::CorruptedData)?;
      // 一次校验 key 边界与 val_len 前缀完整性
      let val_len = le_u32(buf, key_end).ok_or(Error::BufferTooShort)? as usize;

      let val_end = key_end
        .checked_add(4)
        .and_then(|e| e.checked_add(val_len))
        .ok_or(Error::CorruptedData)?;
      if val_end > buf.len() {
        return Err(Error::BufferTooShort);
      }
      let key = &buf[cur..key_end];
      let val = &buf[key_end + 4..val_end];
      cur = val_end;

      if has_expiration {
        let expire = le_u64(buf, val_end).ok_or(Error::BufferTooShort)?;
        cur = val_end.checked_add(8).ok_or(Error::CorruptedData)?;
        if expire > now {
          let key_vec = key.to_vec();
          hash.init_expiration();
          let times = hash.expiration_times.as_mut().unwrap();
          let queue = hash.expiration_queue.as_mut().unwrap();
          times.insert(key_vec.clone(), expire);
          queue.push(Reverse((expire, key_vec.clone())));
          hash.fields.insert(key_vec, val.to_vec());
        } else {
          // 已过期条目：后到记录覆盖一切，直接移除同名字段 (与 from_bitcode 语义对齐)
          if let Some(times) = &mut hash.expiration_times {
            times.remove(key);
          }
          hash.fields.remove(key);
        }
      } else {
        // 重复键覆盖：清除早前条目残留的过期记录，避免恢复后携带脏 TTL 被误清理 (与 from_bitcode 语义对齐)
        if let Some(times) = &mut hash.expiration_times {
          times.remove(key);
        }
        hash.fields.insert(key.to_vec(), val.to_vec());
      }
    }

    hash.cleanup_expiration_if_empty();
    Ok(hash)
  }

  /// 使用 Bitcode 进行极速序列化 (自动清理已过期字段，零拷贝键值切片借用)
  pub fn to_bitcode(&mut self) -> Result<Vec<u8>> {
    self.delete_expired();
    self.to_bitcode_ref()
  }

  /// 使用 Bitcode 进行只读极速序列化 (自动排除已过期字段)
  pub fn to_bitcode_ref(&self) -> Result<Vec<u8>> {
    let now = now_ms();
    let mut entries = Vec::with_capacity(self.len_ref_at(now));
    for (k, v) in &self.fields {
      if !self.expired_since(k, now) {
        let expire_at = self
          .expiration_times
          .as_ref()
          .and_then(|t| t.get(k).copied());
        entries.push(HashEntryBitcode {
          field: k.clone(),
          val: v.clone(),
          expire_at,
        });
      }
    }
    Ok(bitcode::encode(&entries))
  }

  /// 从 Bitcode 缓冲区极速恢复哈希对象 (自动滤除已过期数据并重建最小堆)
  pub fn from_bitcode(slice: &[u8]) -> Result<Self> {
    let entries: Vec<HashEntryBitcode> = bitcode::decode(slice)?;
    let mut hash = Self::with_capacity(entries.len());
    let now = now_ms();
    for entry in entries {
      if let Some(expire) = entry.expire_at {
        if expire > now {
          hash.init_expiration();
          let times = hash.expiration_times.as_mut().unwrap();
          let queue = hash.expiration_queue.as_mut().unwrap();
          times.insert(entry.field.clone(), expire);
          queue.push(Reverse((expire, entry.field.clone())));
          hash.fields.insert(entry.field, entry.val);
        } else {
          // 已过期条目：后到记录覆盖一切，直接移除同名字段 (与 deserialize 语义对齐)
          if let Some(times) = &mut hash.expiration_times {
            times.remove(&entry.field);
          }
          hash.fields.remove(&entry.field);
        }
      } else {
        if let Some(times) = &mut hash.expiration_times {
          times.remove(&entry.field);
        }
        hash.fields.insert(entry.field, entry.val);
      }
    }
    hash.cleanup_expiration_if_empty();
    Ok(hash)
  }
}

/// 小端 u32 读取（越界返回 None）
#[inline]
fn le_u32(buf: &[u8], at: usize) -> Option<u32> {
  let chunk: [u8; 4] = buf.get(at..at + 4)?.try_into().ok()?;
  Some(u32::from_le_bytes(chunk))
}

/// 小端 u64 读取（越界返回 None）
#[inline]
fn le_u64(buf: &[u8], at: usize) -> Option<u64> {
  let chunk: [u8; 8] = buf.get(at..at + 8)?.try_into().ok()?;
  Some(u64::from_le_bytes(chunk))
}
