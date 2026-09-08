use std::{
  cmp::{Ordering, Reverse},
  collections::BinaryHeap,
  fmt::{self, Debug, Formatter},
  hash::{Hash, Hasher},
  ops::Deref,
};

use bitcode::{Decode, Encode};
use coarsetime::Clock;
use fastrand::Rng;
use whasher::{HashMap, HashSet, hash_map_with_capacity, new_hash_map, new_hash_set};
use wrecord::{decode_order_preserving_f64, encode_order_preserving_f64, sample_distinct_indices};

use crate::{
  error::{Error, Result},
  geo::{
    GeoDistanceUnit, GeoItem, GeoOrder, GeoOrigin, GeoSearchOpt, GeoShape, LATITUDE_MAX,
    LATITUDE_MIN, LONGITUDE_MAX, LONGITUDE_MIN, convert_meters_to_units, convert_value_to_meters,
    decode_geohash, encode_geohash, geo_distance, get_distance_when_in_rectangle, get_geohash_code,
    is_point_within_radius,
  },
  skiplist::{ScoreRange, SkipList},
};

/// 极简格式版本号（1 字节）
pub const FORMAT_VERSION: u8 = 1;

/// 允许重复采样的单次安全返回上限（防恶意大数内存耗尽与死循环）
pub const MAX_RAND_SAMPLE_LIMIT: usize = 1_000_000;

/// 掩码位：最高位用于指示成员是否包含过期时间戳
const EXPIRATION_BIT_MASK: u32 = 1 << 31;

/// 将 f64 转换为保序的大端 8 字节数组（支持 memcmp / 字节序直接比大小）
///
/// 位翻转核心统一收敛到 [`encode_order_preserving_f64`] 单一实现；
/// 对象层在此前置「-0.0 折叠为 +0.0」归一（record 层为位级无损编解码，保留 ±0.0 符号位区分），
/// 保证 ±0.0 分值编码恒等，成员去重与跳表排序口径一致
#[inline(always)]
pub const fn encode_sortable_f64(val: f64) -> [u8; 8] {
  encode_order_preserving_f64(if val == 0.0 { 0.0 } else { val })
}

/// 从保序大端 8 字节数组恢复 f64（位翻转逻辑与 record 层单一实现恒等，直接转发）
#[inline(always)]
pub const fn decode_sortable_f64(bytes: [u8; 8]) -> f64 {
  decode_order_preserving_f64(bytes)
}

/// IEEE 754 浮点数全序保序包装类型
///
/// 通过大端保序映射消除 -0.0 与 +0.0 差异，提供全序关系 (Ord) 与哈希 (Hash)
#[derive(Clone, Copy, Default, Encode, Decode)]
pub struct SortableFloat(pub f64);

impl SortableFloat {
  /// 创建包装对象
  #[inline(always)]
  pub const fn new(val: f64) -> Self {
    Self(val)
  }

  /// 转换为大端保序 8 字节数组
  #[inline(always)]
  pub const fn encode(&self) -> [u8; 8] {
    encode_sortable_f64(self.0)
  }

  /// 从大端保序 8 字节数组还原
  #[inline(always)]
  pub const fn decode(bytes: [u8; 8]) -> Self {
    Self(decode_sortable_f64(bytes))
  }

  /// 获取内部浮点值
  #[inline(always)]
  pub const fn get(&self) -> f64 {
    self.0
  }
}

impl PartialEq for SortableFloat {
  #[inline(always)]
  fn eq(&self, other: &Self) -> bool {
    self.encode() == other.encode()
  }
}

impl Eq for SortableFloat {}

impl PartialOrd for SortableFloat {
  #[inline(always)]
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for SortableFloat {
  #[inline(always)]
  fn cmp(&self, other: &Self) -> Ordering {
    self.encode().cmp(&other.encode())
  }
}

impl Hash for SortableFloat {
  #[inline(always)]
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.encode().hash(state);
  }
}

impl Deref for SortableFloat {
  type Target = f64;

  #[inline(always)]
  fn deref(&self) -> &Self::Target {
    &self.0
  }
}

impl From<f64> for SortableFloat {
  #[inline(always)]
  fn from(val: f64) -> Self {
    Self(val)
  }
}

impl From<SortableFloat> for f64 {
  #[inline(always)]
  fn from(val: SortableFloat) -> Self {
    val.0
  }
}

impl Debug for SortableFloat {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    write!(f, "SortableFloat({})", self.0)
  }
}

impl fmt::Display for SortableFloat {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    let mut buf = zmij::Buffer::new();
    f.write_str(buf.format(self.0))
  }
}

/// 将浮点数高效格式化为字符串 (基于 zmij 纳秒级无分配转换)
#[inline(always)]
pub fn format_f64(val: f64) -> String {
  let mut buf = zmij::Buffer::new();
  buf.format(val).to_owned()
}

/// 将浮点数高效格式化写入 zmij 缓冲区并返回切片
#[inline(always)]
pub fn write_f64(val: f64, buf: &mut zmij::Buffer) -> &str {
  buf.format(val)
}

/// 字典序边界 (ZRANGEBYLEX / ZLEXCOUNT / ZREMRANGEBYLEX)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LexBound {
  /// 闭区间 [value]
  Included(Vec<u8>),
  /// 开区间 (value)
  Excluded(Vec<u8>),
  /// 负无穷 "-"（对标 C# SpecialRanges.InfiniteMin）
  NegInf,
  /// 正无穷 "+"（对标 C# SpecialRanges.InfiniteMax）
  PosInf,
}

impl LexBound {
  /// 解析 Redis 字典序边界表示（如 b"[a", b"(b", b"-", b"+"）
  ///
  /// 对标 C# TryParseLexParameter：min 为 "+" 或 max 为 "-" 时区间恒为空集；
  /// `[-` / `[+` 按负无穷边界处理（C# 同款兼容行为）
  pub fn parse(bytes: &[u8]) -> Result<Self> {
    if bytes.is_empty() {
      return Err(Error::InvalidOpt);
    }
    match bytes[0] {
      b'-' if bytes.len() == 1 => Ok(Self::NegInf),
      b'+' if bytes.len() == 1 => Ok(Self::PosInf),
      b'[' | b'(' => {
        let rest = &bytes[1..];
        if rest.len() == 1 && (rest[0] == b'-' || rest[0] == b'+') {
          return Ok(Self::NegInf);
        }
        if bytes[0] == b'[' {
          Ok(Self::Included(rest.to_vec()))
        } else {
          Ok(Self::Excluded(rest.to_vec()))
        }
      }
      _ => Err(Error::InvalidOpt),
    }
  }

  /// 检查成员是否满足下界 (min bound)：NegInf 全满足，PosInf 全不满足
  #[inline]
  pub fn matches_min(&self, member: &[u8]) -> bool {
    match self {
      Self::NegInf => true,
      Self::PosInf => false,
      Self::Included(b) => member >= b.as_slice(),
      Self::Excluded(b) => member > b.as_slice(),
    }
  }

  /// 检查成员是否满足上界 (max bound)：PosInf 全满足，NegInf 全不满足
  #[inline]
  pub fn matches_max(&self, member: &[u8]) -> bool {
    match self {
      Self::PosInf => true,
      Self::NegInf => false,
      Self::Included(b) => member <= b.as_slice(),
      Self::Excluded(b) => member < b.as_slice(),
    }
  }
}

/// 字段过期优先队列类型别名
pub type ExpirationQueue = BinaryHeap<Reverse<(u64, Vec<u8>)>>;

/// 当前系统时间戳 (毫秒)
#[inline]
fn now_ms() -> u64 {
  Clock::now_since_epoch().as_millis()
}

pub use wrecord::glob_match;

/// ZADD 选项配置
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ZAddOpt {
  /// 仅新增成员，不更新已存在成员 (NX)
  pub nx: bool,
  /// 仅更新已存在成员，不新增成员 (XX)
  pub xx: bool,
  /// 仅当新分数大于当前分数时更新 (GT)
  pub gt: bool,
  /// 仅当新分数小于当前分数时更新 (LT)
  pub lt: bool,
  /// 统计变更总数 (新增 + 分数更新)，而非仅新增数 (CH)
  pub ch: bool,
  /// 执行自增模式 (INCR)
  pub incr: bool,
}

impl ZAddOpt {
  /// 校验 NaN 分数与选项互斥组合 (对标 C# GetOptions 的 XX/NX、GT/LT/NX 冲突检测)
  #[inline]
  pub fn validate(&self, score: f64) -> Result<()> {
    if score.is_nan() {
      return Err(Error::InvalidScore);
    }
    if (self.nx && self.xx) || (self.gt && self.lt) || ((self.gt || self.lt) && self.nx) {
      return Err(Error::InvalidOpt);
    }
    Ok(())
  }
}

/// 成员过期选项 (ZEXPIRE options)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExpireOpt {
  pub nx: bool,
  pub xx: bool,
  pub gt: bool,
  pub lt: bool,
}

/// 成员过期操作返回结果 (对标 Garnet ExpireResult)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum ExpireResult {
  KeyNotFound = -2,
  NoExpirationSet = -1,
  ExpireConditionNotMet = 0,
  Ok = 1,
  KeyAlreadyExpired = 2,
}

/// 集合运算聚合类型 (对标 Redis/Garnet AGGREGATE {SUM, MIN, MAX})
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortedSetAggregate {
  #[default]
  Sum,
  Min,
  Max,
}

/// 有序集合对象 (对应 Redis SortedSet / Garnet SortedSetObject)
pub struct SortedSetObject {
  /// 成员字典映射 (Member -> Score) 提供 O(1) 分数反查
  dict: HashMap<Vec<u8>, f64>,
  /// 跳表维护按 (Score, Member) 严格有序排列，支持 O(log N) 排名与范围操作
  skiplist: SkipList,
  /// 成员过期时间戳字典 (毫秒)
  expiration_times: Option<HashMap<Vec<u8>, u64>>,
  /// 成员过期优先队列 (最小堆，按时间戳升序快速淘汰)
  expiration_queue: Option<ExpirationQueue>,
}

impl Debug for SortedSetObject {
  fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
    f.debug_struct("SortedSetObject")
      .field("len", &self.dict.len())
      .finish()
  }
}

impl Clone for SortedSetObject {
  fn clone(&self) -> Self {
    let mut new_obj = Self::new();
    for (m, &s) in &self.dict {
      let _ = new_obj.zadd(s, m.clone(), Default::default());
    }
    new_obj.expiration_times = self.expiration_times.clone();
    new_obj.expiration_queue = self.expiration_queue.clone();
    new_obj
  }
}

impl Default for SortedSetObject {
  fn default() -> Self {
    Self::new()
  }
}

impl SortedSetObject {
  /// 创建空有序集合
  pub fn new() -> Self {
    Self {
      dict: new_hash_map(),
      skiplist: SkipList::new(),
      expiration_times: None,
      expiration_queue: None,
    }
  }

  /// 创建指定初始容量的有序集合
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      dict: hash_map_with_capacity(capacity),
      skiplist: SkipList::new(),
      expiration_times: None,
      expiration_queue: None,
    }
  }

  /// 清空集合中所有数据与过期元数据
  pub fn clear(&mut self) {
    self.dict.clear();
    self.skiplist = SkipList::new();
    self.expiration_times = None;
    self.expiration_queue = None;
  }

  /// 获取有序集合基数 (ZCARD，自动清理过期成员)
  pub fn len(&mut self) -> usize {
    self.delete_expired();
    self.dict.len()
  }

  /// 获取有序集合基数 (只读引用版本)
  pub fn len_ref(&self) -> usize {
    if let Some(times) = &self.expiration_times
      && !times.is_empty()
    {
      let now = now_ms();
      let expired_count = times.values().filter(|&&exp| exp <= now).count();
      self.dict.len().saturating_sub(expired_count)
    } else {
      self.dict.len()
    }
  }

  /// 判断有序集合是否为空
  pub fn is_empty(&mut self) -> bool {
    self.len() == 0
  }

  /// 获取底层哈希表当前已分配容量
  #[inline]
  pub fn capacity(&self) -> usize {
    self.dict.capacity()
  }

  /// 收缩底层哈希表与过期存储以节约内存
  pub fn shrink_to_fit(&mut self) {
    self.delete_expired();
    self.dict.shrink_to_fit();
    if let Some(times) = &mut self.expiration_times {
      times.shrink_to_fit();
    }
    if let Some(queue) = &mut self.expiration_queue {
      queue.shrink_to_fit();
    }
  }

  /// 物理清理所有已过期成员并回收空间，返回清理的成员数 (delete_expired 别名)
  #[inline]
  pub fn purge_expired(&mut self) -> usize {
    self.delete_expired()
  }

  /// 初始化过期数据结构
  #[inline]
  fn init_expiration(&mut self) {
    if self.expiration_times.is_none() {
      self.expiration_times = Some(new_hash_map());
      self.expiration_queue = Some(BinaryHeap::new());
    }
  }

  /// 清理已过期的所有成员，返回清理的成员数量
  pub fn delete_expired(&mut self) -> usize {
    let now = now_ms();
    let mut purged = 0;
    if let (Some(queue), Some(times)) = (&mut self.expiration_queue, &mut self.expiration_times) {
      while let Some(Reverse((expire_time, _))) = queue.peek() {
        if *expire_time > now {
          break;
        }
        let (expire_time, member) = queue.pop().unwrap().0;
        if let Some(&current_expire) = times.get(&member)
          && current_expire == expire_time
        {
          times.remove(&member);
          if let Some(score) = self.dict.remove(&member) {
            self.skiplist.delete(score, &member);
            purged += 1;
          }
        }
      }
      self.cleanup_expiration_if_empty();
    }
    purged
  }

  /// 若过期数据结构为空则清理并重置为 None (消除后续只读路径冗余判断与内存占用)
  #[inline]
  fn cleanup_expiration_if_empty(&mut self) {
    if let Some(times) = &self.expiration_times
      && times.is_empty()
    {
      self.expiration_times = None;
      self.expiration_queue = None;
    }
  }

  /// 检查特定成员是否过期；若已过期就地清理并返回 true
  #[inline]
  fn check_and_purge_expired(&mut self, member: &[u8]) -> bool {
    self.purge_expired_at(member, now_ms())
  }

  /// 检查成员在给定时刻是否已过期；若已过期就地清理并返回 true
  #[inline]
  fn purge_expired_at(&mut self, member: &[u8], now: u64) -> bool {
    if self.is_expired_at(member, now) {
      if let Some(times) = &mut self.expiration_times {
        times.remove(member);
      }
      if let Some(score) = self.dict.remove(member) {
        self.skiplist.delete(score, member);
      }
      self.cleanup_expiration_if_empty();
      return true;
    }
    false
  }

  /// 添加或更新成员分数 (ZADD)
  ///
  /// 返回 (added_or_changed, current_score):
  /// - 若 ch = false: 返回 1 表示新增成员，0 表示已存在仅更新或未改动
  /// - 若 ch = true: 返回 1 表示新增或分数发生变更，0 表示未作任何修改
  pub fn zadd(
    &mut self,
    score: f64,
    member: impl Into<Vec<u8>>,
    options: ZAddOpt,
  ) -> Result<(usize, f64)> {
    options.validate(score)?;
    let member = member.into();
    self.check_and_purge_expired(&member);

    match self.dict.get(&member).copied() {
      Some(old_score) => {
        // 先计算 INCR 结果（对标 C# SortedSetObjectImpl：NaN 在任何条件判断前报错）
        let new_score = if options.incr {
          let s = old_score + score;
          if s.is_nan() {
            return Err(Error::InvalidScore);
          }
          s
        } else {
          score
        };

        // NX / GT / LT 条件判定：若条件未达成，成员状态与 TTL 严格保持不动
        if options.nx {
          return Ok((0, old_score));
        }
        if options.gt && new_score <= old_score {
          return Ok((0, old_score));
        }
        if options.lt && new_score >= old_score {
          return Ok((0, old_score));
        }

        // 分值相等：通过条件校验后的同分写操作视为未变更分值，但按 Redis 规范清除成员 TTL
        if new_score == old_score {
          if let Some(times) = &mut self.expiration_times {
            times.remove(&member);
          }
          self.cleanup_expiration_if_empty();
          return Ok((0, new_score));
        }

        // 实际发生覆盖更新：清除成员过期 (对标 C# TryRemoveExpiration)
        if let Some(times) = &mut self.expiration_times {
          times.remove(&member);
        }
        self.cleanup_expiration_if_empty();

        self.skiplist.delete(old_score, &member);
        *self.dict.get_mut(&member).unwrap() = new_score;
        self.skiplist.insert(new_score, member);

        Ok((if options.ch || options.incr { 1 } else { 0 }, new_score))
      }
      None => {
        if options.xx {
          return Ok((0, score));
        }

        self.skiplist.insert(score, member.clone());
        self.dict.insert(member, score);
        Ok((1, score))
      }
    }
  }

  /// 分数自增 (ZINCRBY)
  ///
  /// 对标 C# SortedSetObjectImpl.Zincrby：只更新分值，**不触碰成员 TTL**
  /// (与 ZADD 不同，C# ZINCRBY 全程不调用 TryRemoveExpiration)
  pub fn zincrby(&mut self, member: impl Into<Vec<u8>>, incr: f64) -> Result<f64> {
    if incr.is_nan() {
      return Err(Error::InvalidScore);
    }
    let member = member.into();
    self.check_and_purge_expired(&member);

    let new_score = match self.dict.get(&member).copied() {
      Some(old_score) => {
        let s = old_score + incr;
        if s.is_nan() {
          return Err(Error::InvalidScore);
        }
        self.skiplist.delete(old_score, &member);
        *self.dict.get_mut(&member).unwrap() = s;
        self.skiplist.insert(s, member);
        s
      }
      None => {
        self.skiplist.insert(incr, member.clone());
        self.dict.insert(member, incr);
        incr
      }
    };
    Ok(new_score)
  }

  /// 判断指定成员是否已过期 (只读检查)
  #[inline]
  pub fn is_expired(&self, member: &[u8]) -> bool {
    self.is_expired_at(member, now_ms())
  }

  /// 判断成员是否已过期 (传入缓存时间戳，避免循环内重复取时钟)
  #[inline]
  fn is_expired_at(&self, member: &[u8], now: u64) -> bool {
    match &self.expiration_times {
      Some(times) => times.get(member).is_some_and(|&exp| exp <= now),
      None => false,
    }
  }

  /// 当前是否存在已到期但尚未清退的成员 (决定只读路径是否需要过期过滤)
  #[inline]
  fn has_expired(&self, now: u64) -> bool {
    match &self.expiration_times {
      Some(times) => times.values().any(|&exp| exp <= now),
      None => false,
    }
  }

  /// 获取成员分数 (ZSCORE)
  pub fn zscore(&mut self, member: &[u8]) -> Option<f64> {
    if self.check_and_purge_expired(member) {
      return None;
    }
    self.dict.get(member).copied()
  }

  /// 获取成员分数 (只读引用版本)
  pub fn zscore_ref(&self, member: &[u8]) -> Option<f64> {
    if self.is_expired(member) {
      return None;
    }
    self.dict.get(member).copied()
  }

  /// 批量获取成员分数 (ZMSCORE)
  pub fn zmscore(&mut self, members: &[&[u8]]) -> Vec<Option<f64>> {
    self.delete_expired();
    members.iter().map(|&m| self.dict.get(m).copied()).collect()
  }

  /// 批量获取成员分数 (只读引用版本)
  pub fn zmscore_ref(&self, members: &[&[u8]]) -> Vec<Option<f64>> {
    let now = now_ms();
    members
      .iter()
      .map(|&m| {
        if self.is_expired_at(m, now) {
          None
        } else {
          self.dict.get(m).copied()
        }
      })
      .collect()
  }

  /// 获取成员的 0-based 升序排名 (ZRANK)
  pub fn zrank(&mut self, member: &[u8]) -> Option<usize> {
    if self.check_and_purge_expired(member) {
      return None;
    }
    let score = self.dict.get(member).copied()?;
    self.skiplist.get_rank(score, member)
  }

  /// 获取成员的 0-based 升序排名 (只读引用版本，扣除已到期未清退的前置成员)
  pub fn zrank_ref(&self, member: &[u8]) -> Option<usize> {
    let now = now_ms();
    if self.is_expired_at(member, now) {
      return None;
    }
    let score = self.dict.get(member).copied()?;
    let mut rank = self.skiplist.get_rank(score, member)?;

    // 对标 C# SortedSetRank：排名只在存活成员中累计，
    // 尚未物理清退的过期成员若 (score, member) 排在目标之前则扣除
    if self.has_expired(now) {
      let target_order = encode_sortable_f64(score);
      rank -= self
        .expiration_times
        .as_ref()
        .unwrap()
        .iter()
        .filter(|(m, exp)| {
          **exp <= now
            && self.dict.get(*m).is_some_and(|&s| {
              encode_sortable_f64(s)
                .cmp(&target_order)
                .then_with(|| m.as_slice().cmp(member))
                == Ordering::Less
            })
        })
        .count();
    }
    Some(rank)
  }

  /// 获取成员的 0-based 降序排名 (ZREVRANK)
  pub fn zrevrank(&mut self, member: &[u8]) -> Option<usize> {
    self.delete_expired();
    let rank = self.zrank(member)?;
    Some(self.dict.len() - 1 - rank)
  }

  /// 获取成员的 0-based 降序排名 (只读引用版本)
  pub fn zrevrank_ref(&self, member: &[u8]) -> Option<usize> {
    let rank = self.zrank_ref(member)?;
    let len = self.len_ref();
    if rank >= len {
      None
    } else {
      Some(len - 1 - rank)
    }
  }

  /// 按 0-based 排名区间获取成员与分数 (ZRANGE)
  ///
  /// 支持负数排名：-1 为最后一个，-2 为倒数第二个。
  pub fn zrange(&mut self, start: isize, stop: isize, reverse: bool) -> Vec<(Vec<u8>, f64)> {
    self.delete_expired();
    self.zrange_ref(start, stop, reverse)
  }

  /// 按 0-based 排名区间获取成员切片借用与分数 (ZRANGE 零拷贝借用)
  pub fn zrange_borrowed(&self, start: isize, stop: isize, reverse: bool) -> Vec<(&[u8], f64)> {
    let len = self.len_ref() as isize;
    if len == 0 {
      return Vec::new();
    }

    let actual_start = if start < 0 {
      len.saturating_add(start).max(0)
    } else {
      start
    };
    // 负索引换算后仍为负说明越界过头：保持负值参与比较，
    // 与 Redis/C# 一致以 start(>=0) > stop(<0) 判空，而非钳位到 0
    let actual_stop = if stop < 0 {
      len.saturating_add(stop)
    } else {
      stop
    };

    if actual_start >= len || actual_start > actual_stop {
      return Vec::new();
    }
    let actual_stop = actual_stop.min(len - 1);

    let now = now_ms();

    // 无待清退过期成员时，物理排名与逻辑排名一致，直接走跳表原生排名窗口
    if !self.has_expired(now) {
      return self.skiplist.range_by_rank_borrowed(
        actual_start as usize,
        actual_stop as usize,
        reverse,
      );
    }

    // 只读路径存在已到期未清退成员：逻辑排名以存活成员为序
    // (对标 C# SortedSetRange 的 Where(!IsExpired)→Skip→Take 次序)
    let (lo, hi) = if reverse {
      (
        (len - 1 - actual_stop) as usize,
        (len - 1 - actual_start) as usize,
      )
    } else {
      (actual_start as usize, actual_stop as usize)
    };
    let mut res: Vec<_> = self
      .skiplist
      .iter()
      .filter(|(m, _)| !self.is_expired_at(m, now))
      .skip(lo)
      .take(hi - lo + 1)
      .collect();
    if reverse {
      res.reverse();
    }
    res
  }

  /// 按 0-based 排名区间获取成员与分数 (只读引用版本)
  pub fn zrange_ref(&self, start: isize, stop: isize, reverse: bool) -> Vec<(Vec<u8>, f64)> {
    self
      .zrange_borrowed(start, stop, reverse)
      .into_iter()
      .map(|(m, s)| (m.to_vec(), s))
      .collect()
  }

  /// 按 0-based 降序排名区间获取成员与分数 (ZREVRANGE)
  #[inline]
  pub fn zrevrange(&mut self, start: isize, stop: isize) -> Vec<(Vec<u8>, f64)> {
    self.zrange(start, stop, true)
  }

  /// 按 0-based 降序排名区间获取成员与分数 (只读引用版本)
  #[inline]
  pub fn zrevrange_ref(&self, start: isize, stop: isize) -> Vec<(Vec<u8>, f64)> {
    self.zrange_ref(start, stop, true)
  }

  /// 按 0-based 降序排名区间获取成员切片借用与分数 (零拷贝借用)
  #[inline]
  pub fn zrevrange_borrowed(&self, start: isize, stop: isize) -> Vec<(&[u8], f64)> {
    self.zrange_borrowed(start, stop, true)
  }

  /// 按分数区间获取成员与分数 (ZRANGEBYSCORE)
  pub fn zrangebyscore(
    &mut self,
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self.delete_expired();
    self.zrangebyscore_ref(range, reverse, offset, count)
  }

  /// 按分数区间获取成员切片借用与分数 (零拷贝借用)
  pub fn zrangebyscore_borrowed(
    &self,
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(&[u8], f64)> {
    if count == 0 {
      return Vec::new();
    }
    let now = now_ms();
    if !self.has_expired(now) {
      return self
        .skiplist
        .range_by_score_borrowed(range, reverse, offset, count);
    }
    self
      .skiplist
      .range_by_score_borrowed(range, reverse, 0, usize::MAX)
      .into_iter()
      .filter(|(m, _)| !self.is_expired_at(m, now))
      .skip(offset)
      .take(count)
      .collect()
  }

  /// 按分数区间获取成员与分数 (只读引用版本)
  pub fn zrangebyscore_ref(
    &self,
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self
      .zrangebyscore_borrowed(range, reverse, offset, count)
      .into_iter()
      .map(|(m, s)| (m.to_vec(), s))
      .collect()
  }

  /// 按分数降序区间获取成员与分数 (ZREVRANGEBYSCORE)
  #[inline]
  pub fn zrevrangebyscore(
    &mut self,
    range: ScoreRange,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self.zrangebyscore(range, true, offset, count)
  }

  /// 按分数降序区间获取成员与分数 (只读引用版本)
  #[inline]
  pub fn zrevrangebyscore_ref(
    &self,
    range: ScoreRange,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self.zrangebyscore_ref(range, true, offset, count)
  }

  /// 按分数降序区间获取成员切片借用与分数 (零拷贝借用)
  #[inline]
  pub fn zrevrangebyscore_borrowed(
    &self,
    range: ScoreRange,
    offset: usize,
    count: usize,
  ) -> Vec<(&[u8], f64)> {
    self.zrangebyscore_borrowed(range, true, offset, count)
  }

  /// 统计分数区间内的成员数量 (ZCOUNT)
  pub fn zcount(&mut self, range: ScoreRange) -> usize {
    self.delete_expired();
    self.zcount_ref(range)
  }

  /// 统计分数区间内的成员数量 (只读引用版本，扣除已到期未清退成员)
  pub fn zcount_ref(&self, range: ScoreRange) -> usize {
    let total = self.skiplist.count_by_score(range);
    let now = now_ms();
    if !self.has_expired(now) {
      return total;
    }
    // 对标 C# SortedSetCount：过期成员不计入，即使尚未物理清退
    let expired_in_range = self
      .expiration_times
      .as_ref()
      .unwrap()
      .iter()
      .filter(|(m, exp)| **exp <= now && self.dict.get(*m).is_some_and(|&s| range.contains(s)))
      .count();
    total.saturating_sub(expired_in_range)
  }

  /// 移除一个或多个成员 (ZREM)
  ///
  /// 已过期未清退成员视同不存在：就地物理清退但不计入删除数（与读路径过期语义一致）
  pub fn zrem(&mut self, members: &[&[u8]]) -> usize {
    let now = now_ms();
    let mut removed = 0;
    for &m in members {
      if self.purge_expired_at(m, now) {
        continue;
      }
      if let Some(score) = self.dict.remove(m) {
        self.skiplist.delete(score, m);
        removed += 1;
      }
    }
    self.cleanup_expiration_if_empty();
    removed
  }

  /// 按排名范围移除成员 (ZREMRANGEBYRANK)
  pub fn zremrangebyrank(&mut self, start: isize, stop: isize) -> usize {
    self.delete_expired();
    let len = self.dict.len();
    if len == 0 {
      return 0;
    }
    let actual_start = if start < 0 {
      (len as isize).saturating_add(start).max(0)
    } else {
      start
    };
    // 负索引换算后仍为负时保持负值：与 Redis/C# 一致以 start > stop 判空，而非钳位到 0
    let actual_stop = if stop < 0 {
      (len as isize).saturating_add(stop)
    } else {
      stop
    };
    if actual_start > actual_stop || actual_start >= len as isize {
      return 0;
    }
    let stop = (actual_stop as usize).min(len - 1);
    let start = actual_start as usize;

    let items = self.skiplist.range_by_rank(start, stop, false);
    for (m, _) in &items {
      if let Some(times) = &mut self.expiration_times {
        times.remove(m);
      }
      if let Some(score) = self.dict.remove(m) {
        self.skiplist.delete(score, m);
      }
    }
    self.cleanup_expiration_if_empty();
    items.len()
  }

  /// 按分数范围移除成员 (ZREMRANGEBYSCORE)
  pub fn zremrangebyscore(&mut self, range: ScoreRange) -> usize {
    self.delete_expired();
    let items = self.skiplist.range_by_score(range, false, 0, usize::MAX);
    for (m, _) in &items {
      if let Some(times) = &mut self.expiration_times {
        times.remove(m);
      }
      if let Some(score) = self.dict.remove(m) {
        self.skiplist.delete(score, m);
      }
    }
    self.cleanup_expiration_if_empty();
    items.len()
  }

  /// 字典序范围查询成员 (ZRANGEBYLEX)
  pub fn zrangebylex(
    &mut self,
    min: &LexBound,
    max: &LexBound,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self.delete_expired();
    self.zrangebylex_ref(min, max, reverse, offset, count)
  }

  /// 字典序范围查询成员切片借用与分数 (零拷贝借用)
  pub fn zrangebylex_borrowed<'a>(
    &'a self,
    min: &LexBound,
    max: &LexBound,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(&'a [u8], f64)> {
    if count == 0 {
      return Vec::new();
    }
    let now = now_ms();
    if !self.has_expired(now) {
      return self
        .skiplist
        .range_by_lex_borrowed(min, max, reverse, offset, count);
    }
    // 存在已到期未清退成员：先过滤再应用 LIMIT (对标 C# GetElementsInRangeByLex 语义)
    self
      .skiplist
      .range_by_lex_borrowed(min, max, reverse, 0, usize::MAX)
      .into_iter()
      .filter(|(m, _)| !self.is_expired_at(m, now))
      .skip(offset)
      .take(count)
      .collect()
  }

  /// 字典序范围查询成员 (只读引用版本)
  pub fn zrangebylex_ref(
    &self,
    min: &LexBound,
    max: &LexBound,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self
      .zrangebylex_borrowed(min, max, reverse, offset, count)
      .into_iter()
      .map(|(m, s)| (m.to_vec(), s))
      .collect()
  }

  /// 字典序降序范围查询成员与分数 (ZREVRANGEBYLEX)
  #[inline]
  pub fn zrevrangebylex(
    &mut self,
    min: &LexBound,
    max: &LexBound,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self.zrangebylex(min, max, true, offset, count)
  }

  /// 字典序降序范围查询成员与分数 (只读引用版本)
  #[inline]
  pub fn zrevrangebylex_ref(
    &self,
    min: &LexBound,
    max: &LexBound,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self.zrangebylex_ref(min, max, true, offset, count)
  }

  /// 字典序降序范围查询成员切片借用与分数 (零拷贝借用)
  #[inline]
  pub fn zrevrangebylex_borrowed<'a>(
    &'a self,
    min: &LexBound,
    max: &LexBound,
    offset: usize,
    count: usize,
  ) -> Vec<(&'a [u8], f64)> {
    self.zrangebylex_borrowed(min, max, true, offset, count)
  }

  /// 字典序范围成员计数 (ZLEXCOUNT)
  pub fn zlexcount(&mut self, min: &LexBound, max: &LexBound) -> usize {
    self.delete_expired();
    self.zlexcount_ref(min, max)
  }

  /// 字典序范围成员计数 (只读引用版本，扣除已到期未清退成员)
  pub fn zlexcount_ref(&self, min: &LexBound, max: &LexBound) -> usize {
    let total = self.skiplist.count_by_lex(min, max);
    let now = now_ms();
    if !self.has_expired(now) {
      return total;
    }
    // 对标 C# ZLEXCOUNT：过期成员不计入
    let expired_in_range = self
      .expiration_times
      .as_ref()
      .unwrap()
      .iter()
      .filter(|(m, exp)| {
        **exp <= now
          && self.dict.contains_key(*m)
          && min.matches_min(m.as_slice())
          && max.matches_max(m.as_slice())
      })
      .count();
    total.saturating_sub(expired_in_range)
  }

  /// 字典序范围删除成员 (ZREMRANGEBYLEX)
  pub fn zremrangebylex(&mut self, min: &LexBound, max: &LexBound) -> usize {
    self.delete_expired();
    let items = self.skiplist.range_by_lex(min, max, false, 0, usize::MAX);
    for (m, _) in &items {
      if let Some(times) = &mut self.expiration_times {
        times.remove(m);
      }
      if let Some(score) = self.dict.remove(m) {
        self.skiplist.delete(score, m);
      }
    }
    self.cleanup_expiration_if_empty();
    items.len()
  }

  /// 获取指定成员的 11 位 Base32 geohash 字符串 (GEOHASH)
  pub fn geohash(&mut self, members: &[&[u8]]) -> Vec<Option<String>> {
    self.delete_expired();
    self.geohash_ref(members)
  }

  /// 获取指定成员的 11 位 Base32 geohash 字符串 (只读引用版本)
  pub fn geohash_ref(&self, members: &[&[u8]]) -> Vec<Option<String>> {
    let now = now_ms();
    members
      .iter()
      .map(|&m| {
        if self.is_expired_at(m, now) {
          None
        } else {
          self
            .dict
            .get(m)
            .map(|&score| get_geohash_code(score as u64))
        }
      })
      .collect()
  }

  /// 游标扫描 (ZSCAN)
  pub fn zscan(
    &mut self,
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> (usize, Vec<(Vec<u8>, f64)>) {
    self.delete_expired();
    self.zscan_ref(cursor, count, pattern)
  }

  /// 游标扫描切片借用版本 (零成员克隆)
  pub fn zscan_borrowed<'a>(
    &'a self,
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> (usize, Vec<(&'a [u8], f64)>) {
    let total = self.len_ref();
    if cursor >= total {
      return (0, Vec::new());
    }

    let limit = if count == 0 { 10 } else { count };
    let mut items = Vec::new();
    let mut index = 0;
    let now = now_ms();

    for (m, &s) in &self.dict {
      if self.is_expired_at(m, now) {
        continue;
      }
      if index < cursor {
        index += 1;
        continue;
      }
      let matches = match pattern {
        Some(p) => glob_match(p, m),
        None => true,
      };
      if matches {
        items.push((m.as_slice(), s));
      }
      index += 1;
      if items.len() >= limit {
        break;
      }
    }

    // 终止判定以存活成员数为基准：index 只累计非过期成员，
    // 若用含过期成员的 dict.len() 比较，游标将永不归零导致调用方死循环
    let next_cursor = if index >= total { 0 } else { index };
    (next_cursor, items)
  }

  /// 游标扫描 (只读引用版本)
  ///
  /// 游标基准 = 非过期成员的遍历序位置（与 pattern 匹配无关，对标 C# SortedSetObject.Scan：
  /// 过期成员计入 expiredKeysCount 不推进游标，`index + expiredKeysCount == dict.Count` 时归零）
  pub fn zscan_ref(
    &self,
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> (usize, Vec<(Vec<u8>, f64)>) {
    let (next_cursor, borrowed) = self.zscan_borrowed(cursor, count, pattern);
    let items = borrowed.into_iter().map(|(m, s)| (m.to_vec(), s)).collect();
    (next_cursor, items)
  }

  /// 弹出分数最小的单个成员 (ZPOPMIN 1 / BZPOPMIN 立即命中路径，零向量分配)
  pub fn pop_min(&mut self) -> Option<(Vec<u8>, f64)> {
    self.delete_expired();
    let (m, s) = self.skiplist.pop_first()?;
    if let Some(times) = &mut self.expiration_times {
      times.remove(&m);
    }
    self.dict.remove(&m);
    self.cleanup_expiration_if_empty();
    Some((m, s))
  }

  /// 弹出分数最大的单个成员 (ZPOPMAX 1 / BZPOPMAX 立即命中路径，零向量分配)
  pub fn pop_max(&mut self) -> Option<(Vec<u8>, f64)> {
    self.delete_expired();
    let (m, s) = self.skiplist.pop_last()?;
    if let Some(times) = &mut self.expiration_times {
      times.remove(&m);
    }
    self.dict.remove(&m);
    self.cleanup_expiration_if_empty();
    Some((m, s))
  }

  /// 弹出分数最小的至多 count 个成员 (ZPOPMIN，零全表搜索，常数时间均摊弹出)
  pub fn zpopmin(&mut self, count: usize) -> Vec<(Vec<u8>, f64)> {
    self.delete_expired();
    let take = count.min(self.dict.len());
    if take == 0 {
      return Vec::new();
    }
    let mut items = Vec::with_capacity(take);
    for _ in 0..take {
      if let Some((m, s)) = self.skiplist.pop_first() {
        if let Some(times) = &mut self.expiration_times {
          times.remove(&m);
        }
        self.dict.remove(&m);
        items.push((m, s));
      } else {
        break;
      }
    }
    self.cleanup_expiration_if_empty();
    items
  }

  /// 弹出分数最大的至多 count 个成员 (ZPOPMAX)
  pub fn zpopmax(&mut self, count: usize) -> Vec<(Vec<u8>, f64)> {
    self.delete_expired();
    let take = count.min(self.dict.len());
    if take == 0 {
      return Vec::new();
    }
    let mut items = Vec::with_capacity(take);
    for _ in 0..take {
      if let Some((m, s)) = self.skiplist.pop_last() {
        if let Some(times) = &mut self.expiration_times {
          times.remove(&m);
        }
        self.dict.remove(&m);
        items.push((m, s));
      } else {
        break;
      }
    }
    self.cleanup_expiration_if_empty();
    items
  }

  /// 随机获取成员 (只读引用版本，自动排除已过期未清退的幽灵条目)
  pub fn zrandmember_ref(&self, count: isize, with_scores: bool) -> Vec<(Vec<u8>, Option<f64>)> {
    let now = now_ms();
    let total = self.len_ref();
    if total == 0 || count == 0 {
      return Vec::new();
    }

    let mut rng = Rng::new();

    if !self.has_expired(now) {
      // 无待清退过期成员：直接走跳表 O(log N) 排名索引，零中间分配
      Self::zrandmember_impl(total, count, with_scores, &mut rng, |i| {
        // SAFETY: idx < total ≤ 跳表长度，跳表与字典严格同步，排名必然命中
        unsafe { self.skiplist.get_by_rank(i).unwrap_unchecked() }
      })
    } else {
      // 对标 C# ElementAt：以存活成员为基准先收集，再按存活序号访问
      let alive: Vec<(&[u8], f64)> = self
        .skiplist
        .iter()
        .filter(|(m, _)| !self.is_expired_at(m, now))
        .collect();
      if alive.is_empty() {
        return Vec::new();
      }
      Self::zrandmember_impl(alive.len(), count, with_scores, &mut rng, |i| alive[i])
    }
  }

  /// 随机抽样核心：按排名访问器 `at` 执行采样 (对标 C# RandomUtils.PickKRandomIndexes)
  ///
  /// - count = 1：单次均匀采样
  /// - count > 0：不重复抽样 (部分 Fisher-Yates，过半时反向排除集优化)
  /// - count < 0：允许重复抽样，上限 MAX_RAND_SAMPLE_LIMIT 防内存耗尽
  fn zrandmember_impl<'a, A>(
    total: usize,
    count: isize,
    with_scores: bool,
    rng: &mut Rng,
    at: A,
  ) -> Vec<(Vec<u8>, Option<f64>)>
  where
    A: Fn(usize) -> (&'a [u8], f64),
  {
    let to_item = |(m, s): (&[u8], f64)| (m.to_vec(), if with_scores { Some(s) } else { None });

    if count == 1 {
      return vec![to_item(at(rng.usize(0..total)))];
    }

    if count > 0 {
      let pick = (count as usize).min(total);
      if pick == total {
        return (0..total).map(|i| to_item(at(i))).collect();
      }

      let picked_indices = sample_distinct_indices(total, pick);
      picked_indices.into_iter().map(|i| to_item(at(i))).collect()
    } else {
      let pick = count.unsigned_abs().min(MAX_RAND_SAMPLE_LIMIT);
      (0..pick)
        .map(|_| to_item(at(rng.usize(0..total))))
        .collect()
    }
  }

  /// 随机获取成员 (ZRANDMEMBER)
  pub fn zrandmember(&mut self, count: isize, with_scores: bool) -> Vec<(Vec<u8>, Option<f64>)> {
    self.delete_expired();
    self.zrandmember_ref(count, with_scores)
  }

  /// 设置成员过期时间戳 (ZEXPIRE)
  ///
  /// 判序对齐 `HashObject::hexpire` 与 Redis 7.4：先选项校验，后过去时间戳删除
  pub fn zexpire(&mut self, member: &[u8], expire_at_ms: u64, option: ExpireOpt) -> ExpireResult {
    // 已过期成员视同不存在（就地清理后按 KeyNotFound 处理）
    if self.check_and_purge_expired(member) || !self.dict.contains_key(member) {
      return ExpireResult::KeyNotFound;
    }

    let curr_expire = self
      .expiration_times
      .as_ref()
      .and_then(|times| times.get(member).copied());

    // 校验选项冲突 (对标 C# SortedSetObject.SetExpiration：无 TTL 时 XX 与 GT 均拒绝)
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
      // 过期时间小于等于当前时间，且已通过选项校验，立即删除该成员
      if let Some(times) = &mut self.expiration_times {
        times.remove(member);
      }
      self.cleanup_expiration_if_empty();
      if let Some(score) = self.dict.remove(member) {
        self.skiplist.delete(score, member);
      }
      return ExpireResult::KeyAlreadyExpired;
    }

    self.init_expiration();
    let times = self.expiration_times.as_mut().unwrap();
    let queue = self.expiration_queue.as_mut().unwrap();

    times.insert(member.to_vec(), expire_at_ms);
    queue.push(Reverse((expire_at_ms, member.to_vec())));
    ExpireResult::Ok
  }

  /// 获取成员剩余存活毫秒数 (ZTTL)
  pub fn zttl(&mut self, member: &[u8]) -> i64 {
    if self.check_and_purge_expired(member) || !self.dict.contains_key(member) {
      return ExpireResult::KeyNotFound as i64;
    }

    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(member)
    {
      let now = now_ms();
      return if expire_at > now {
        (expire_at - now) as i64
      } else {
        ExpireResult::KeyNotFound as i64
      };
    }

    ExpireResult::NoExpirationSet as i64
  }

  /// 只读查询成员剩余存活毫秒数 (ZTTL，只读引用版本)
  pub fn zttl_ref(&self, member: &[u8]) -> i64 {
    if !self.dict.contains_key(member) {
      return ExpireResult::KeyNotFound as i64;
    }

    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(member)
    {
      let now = now_ms();
      if expire_at <= now {
        return ExpireResult::KeyNotFound as i64;
      }
      return (expire_at - now) as i64;
    }

    ExpireResult::NoExpirationSet as i64
  }

  /// 查询成员剩余存活毫秒数 (ZPTTL 别名)
  #[inline]
  pub fn zpttl(&mut self, member: &[u8]) -> i64 {
    self.zttl(member)
  }

  /// 只读查询成员剩余存活毫秒数 (ZPTTL 别名，只读引用版本)
  #[inline]
  pub fn zpttl_ref(&self, member: &[u8]) -> i64 {
    self.zttl_ref(member)
  }

  /// 设置成员在指定 UNIX 秒级时间戳过期 (ZEXPIREAT)
  #[inline]
  pub fn zexpireat(
    &mut self,
    member: &[u8],
    unix_time_sec: u64,
    option: ExpireOpt,
  ) -> ExpireResult {
    self.zexpire(member, unix_time_sec.saturating_mul(1000), option)
  }

  /// 设置成员在指定 UNIX 毫秒级时间戳过期 (ZPEXPIREAT)
  #[inline]
  pub fn zpexpireat(
    &mut self,
    member: &[u8],
    unix_time_ms: u64,
    option: ExpireOpt,
  ) -> ExpireResult {
    self.zexpire(member, unix_time_ms, option)
  }

  /// 设置成员相对存活毫秒数过期 (ZPEXPIRE)
  #[inline]
  pub fn zpexpire(&mut self, member: &[u8], milliseconds: u64, option: ExpireOpt) -> ExpireResult {
    let now = now_ms();
    let expire_at_ms = now.saturating_add(milliseconds);
    self.zexpire(member, expire_at_ms, option)
  }

  /// 查询成员绝对过期 UNIX 时间戳 (秒) (ZEXPIRETIME)
  pub fn zexpiretime(&mut self, member: &[u8]) -> i64 {
    if self.check_and_purge_expired(member) || !self.dict.contains_key(member) {
      return ExpireResult::KeyNotFound as i64;
    }
    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(member)
    {
      return (expire_at / 1000) as i64;
    }
    ExpireResult::NoExpirationSet as i64
  }

  /// 只读查询成员绝对过期 UNIX 时间戳 (秒) (ZEXPIRETIME，只读引用版本)
  pub fn zexpiretime_ref(&self, member: &[u8]) -> i64 {
    if !self.dict.contains_key(member) {
      return ExpireResult::KeyNotFound as i64;
    }
    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(member)
    {
      if expire_at <= now_ms() {
        return ExpireResult::KeyNotFound as i64;
      }
      return (expire_at / 1000) as i64;
    }
    ExpireResult::NoExpirationSet as i64
  }

  /// 查询成员绝对过期 UNIX 时间戳 (毫秒) (ZPEXPIRETIME)
  pub fn zpexpiretime(&mut self, member: &[u8]) -> i64 {
    if self.check_and_purge_expired(member) || !self.dict.contains_key(member) {
      return ExpireResult::KeyNotFound as i64;
    }
    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(member)
    {
      return expire_at as i64;
    }
    ExpireResult::NoExpirationSet as i64
  }

  /// 只读查询成员绝对过期 UNIX 时间戳 (毫秒) (ZPEXPIRETIME，只读引用版本)
  pub fn zpexpiretime_ref(&self, member: &[u8]) -> i64 {
    if !self.dict.contains_key(member) {
      return ExpireResult::KeyNotFound as i64;
    }
    if let Some(times) = &self.expiration_times
      && let Some(&expire_at) = times.get(member)
    {
      if expire_at <= now_ms() {
        return ExpireResult::KeyNotFound as i64;
      }
      return expire_at as i64;
    }
    ExpireResult::NoExpirationSet as i64
  }

  /// 移除成员过期时间 (ZPERSIST)
  pub fn zpersist(&mut self, member: &[u8]) -> bool {
    if self.check_and_purge_expired(member) || !self.dict.contains_key(member) {
      return false;
    }

    if let Some(times) = &mut self.expiration_times {
      let removed = times.remove(member).is_some();
      self.cleanup_expiration_if_empty();
      removed
    } else {
      false
    }
  }

  // --- Geospatial API (GEOADD, GEODIST, GEOPOS, GEOHASH) ---

  /// 添加地理位置成员 (GEOADD)
  pub fn geoadd(&mut self, lat: f64, lon: f64, member: impl Into<Vec<u8>>) -> Result<bool> {
    Ok(self.geoadd_opts(lat, lon, member, false, false)?.0 > 0)
  }

  /// 添加地理位置成员 (GEOADD 完整选项形态)
  ///
  /// 对标 C# GeoAddOpt：`nx` = 仅新增成员 (NX)，`xx` = 仅更新成员 (XX)
  /// 返回 `(新增数, 变更数)`，调用方按 CH 标志自行选择回写哪一个
  pub fn geoadd_opts(
    &mut self,
    lat: f64,
    lon: f64,
    member: impl Into<Vec<u8>>,
    nx: bool,
    xx: bool,
  ) -> Result<(usize, usize)> {
    let hash = encode_geohash(lat, lon)?;
    let score = hash as f64;
    let member = member.into();
    self.check_and_purge_expired(&member);

    match self.dict.get(&member).copied() {
      Some(old_score) => {
        if nx || old_score == score {
          return Ok((0, 0));
        }
        self.skiplist.delete(old_score, &member);
        self.skiplist.insert(score, member.clone());
        self.dict.insert(member.clone(), score);
        // 对标 C# GeoAdd：覆盖更新仅改分值，不触碰成员过期 (区别于 ZADD 的 TryRemoveExpiration)
        Ok((0, 1))
      }
      None => {
        if xx {
          return Ok((0, 0));
        }
        self.skiplist.insert(score, member.clone());
        self.dict.insert(member.clone(), score);
        Ok((1, 1))
      }
    }
  }

  /// 计算两位置成员之间的距离 (GEODIST，单位：米)
  pub fn geodist(&mut self, member1: &[u8], member2: &[u8]) -> Option<f64> {
    self.geodist_ref(member1, member2)
  }

  /// 计算两位置成员之间的距离 (只读引用版本)
  pub fn geodist_ref(&self, member1: &[u8], member2: &[u8]) -> Option<f64> {
    let score1 = self.zscore_ref(member1)?;
    let score2 = self.zscore_ref(member2)?;

    let (lat1, lon1) = decode_geohash(score1 as u64);
    let (lat2, lon2) = decode_geohash(score2 as u64);

    Some(geo_distance(lat1, lon1, lat2, lon2))
  }

  /// 查询成员的经纬度位置 (GEOPOS)
  pub fn geopos(&mut self, member: &[u8]) -> Option<(f64, f64)> {
    self.geopos_ref(member)
  }

  /// 批量查询成员经纬度 (GEOPOS 多成员形态，对标 C# GeoPosition 循环)
  pub fn geopos_many(&mut self, members: &[&[u8]]) -> Vec<Option<(f64, f64)>> {
    self.delete_expired();
    let now = now_ms();
    members
      .iter()
      .map(|&m| {
        if self.is_expired_at(m, now) {
          None
        } else {
          self.dict.get(m).map(|&score| decode_geohash(score as u64))
        }
      })
      .collect()
  }

  /// 查询成员的经纬度位置 (只读引用版本)
  pub fn geopos_ref(&self, member: &[u8]) -> Option<(f64, f64)> {
    let score = self.zscore_ref(member)?;
    Some(decode_geohash(score as u64))
  }

  /// 地理位置检索 (GEOSEARCH，只读引用版本，支持 RADIUS / BOX，COORD / MEMBER 原点)
  pub fn geosearch_ref(
    &self,
    origin: GeoOrigin,
    shape: GeoShape,
    opts: GeoSearchOpt,
  ) -> Result<Vec<GeoItem>> {
    let (center_lon, center_lat) = match origin {
      GeoOrigin::Coord { lon, lat } => {
        if !(LONGITUDE_MIN..=LONGITUDE_MAX).contains(&lon)
          || !(LATITUDE_MIN..=LATITUDE_MAX).contains(&lat)
        {
          return Err(Error::InvalidCoordinates);
        }
        (lon, lat)
      }
      GeoOrigin::Member(ref m) => {
        let score = self.zscore_ref(m).ok_or(Error::MemberNotFound)?;
        let (lat, lon) = decode_geohash(score as u64);
        (lon, lat)
      }
    };

    let (unit, is_box, radius_m, width_m, height_m) = match shape {
      GeoShape::ByRadius { radius, unit } => {
        if radius < 0.0 {
          return Err(Error::InvalidDistance);
        }
        (unit, false, convert_value_to_meters(radius, unit), 0.0, 0.0)
      }
      GeoShape::ByBox {
        width,
        height,
        unit,
      } => {
        if width < 0.0 || height < 0.0 {
          return Err(Error::InvalidDistance);
        }
        (
          unit,
          true,
          0.0,
          convert_value_to_meters(width, unit),
          convert_value_to_meters(height, unit),
        )
      }
    };

    let now = now_ms();
    let mut candidates: Vec<(GeoItem, f64)> = Vec::new();

    for (m, &score) in &self.dict {
      if self.is_expired_at(m, now) {
        continue;
      }
      let (m_lat, m_lon) = decode_geohash(score as u64);

      let maybe_dist_m = if is_box {
        get_distance_when_in_rectangle(width_m, height_m, center_lat, center_lon, m_lat, m_lon)
      } else {
        is_point_within_radius(radius_m, center_lat, center_lon, m_lat, m_lon)
      };

      if let Some(dist_m) = maybe_dist_m {
        let dist = convert_meters_to_units(dist_m, unit);
        let coord = if opts.with_coord {
          Some((m_lon, m_lat))
        } else {
          None
        };
        let hash = if opts.with_hash {
          Some(score as u64)
        } else {
          None
        };
        let dist_opt = if opts.with_dist { Some(dist) } else { None };

        candidates.push((
          GeoItem {
            member: m.clone(),
            dist: dist_opt,
            hash,
            coord,
          },
          dist_m,
        ));

        // 若指定 ANY，收集满 COUNT 即提前退出，不计排序方向
        // (对标 C# withCountAny：先到先得，排序仅作用于已收集子集)
        if opts.any
          && let Some(cnt) = opts.count
          && candidates.len() >= cnt
        {
          break;
        }
      }
    }

    // 排序逻辑 (对标 Garnet / Redis)：
    // 1. ASC / DESC 显式排序
    // 2. 若 order == None 但指定了 COUNT 且没有 ANY，按 Redis 规范默认按距离 ASC 排序再截断
    match opts.order {
      GeoOrder::Asc => {
        candidates.sort_unstable_by(|a, b| {
          a.1
            .partial_cmp(&b.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.member.cmp(&b.0.member))
        });
      }
      GeoOrder::Desc => {
        candidates.sort_unstable_by(|a, b| {
          b.1
            .partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.member.cmp(&b.0.member))
        });
      }
      GeoOrder::None => {
        if opts.count.is_some() && !opts.any {
          candidates.sort_unstable_by(|a, b| {
            a.1
              .partial_cmp(&b.1)
              .unwrap_or(Ordering::Equal)
              .then_with(|| a.0.member.cmp(&b.0.member))
          });
        }
      }
    }

    if let Some(cnt) = opts.count {
      candidates.truncate(cnt);
    }

    Ok(candidates.into_iter().map(|(item, _)| item).collect())
  }

  /// 地理位置检索 (GEOSEARCH，可变版本，自动清理过期条目)
  pub fn geosearch(
    &mut self,
    origin: GeoOrigin,
    shape: GeoShape,
    opts: GeoSearchOpt,
  ) -> Result<Vec<GeoItem>> {
    self.delete_expired();
    self.geosearch_ref(origin, shape, opts)
  }

  /// 地理位置检索并存储结果到目标有序集合 (GEOSEARCHSTORE)
  ///
  /// store_dist = true 时以指定单位的距离作为分值；false 时以 52 位整数 GeoHash 作为分值
  pub fn geosearch_store(
    &self,
    dest: &mut Self,
    origin: GeoOrigin,
    shape: GeoShape,
    opts: GeoSearchOpt,
    store_dist: bool,
  ) -> Result<usize> {
    let mut search_opts = opts;
    if store_dist {
      search_opts.with_dist = true;
    } else {
      search_opts.with_hash = true;
    }

    let items = self.geosearch_ref(origin, shape, search_opts)?;
    dest.clear();

    for item in &items {
      let score = if store_dist {
        item.dist.unwrap_or(0.0)
      } else {
        item.hash.unwrap_or(0) as f64
      };
      dest.zadd(score, item.member.clone(), ZAddOpt::default())?;
    }

    Ok(items.len())
  }

  /// 搜索给定经纬度与半径范围内的地理位置 (GEORADIUS)
  pub fn georadius(
    &mut self,
    lon: f64,
    lat: f64,
    radius: f64,
    unit: GeoDistanceUnit,
    opts: GeoSearchOpt,
  ) -> Result<Vec<GeoItem>> {
    self.delete_expired();
    self.georadius_ref(lon, lat, radius, unit, opts)
  }

  /// 搜索给定经纬度与半径范围内的地理位置 (只读引用版本)
  pub fn georadius_ref(
    &self,
    lon: f64,
    lat: f64,
    radius: f64,
    unit: GeoDistanceUnit,
    opts: GeoSearchOpt,
  ) -> Result<Vec<GeoItem>> {
    self.geosearch_ref(
      GeoOrigin::Coord { lon, lat },
      GeoShape::ByRadius { radius, unit },
      opts,
    )
  }

  /// 搜索给定成员为中心、指定半径范围内的地理位置 (GEORADIUSBYMEMBER)
  pub fn georadiusbymember(
    &mut self,
    member: &[u8],
    radius: f64,
    unit: GeoDistanceUnit,
    opts: GeoSearchOpt,
  ) -> Result<Vec<GeoItem>> {
    self.delete_expired();
    self.georadiusbymember_ref(member, radius, unit, opts)
  }

  /// 搜索给定成员为中心、指定半径范围内的地理位置 (只读引用版本)
  pub fn georadiusbymember_ref(
    &self,
    member: &[u8],
    radius: f64,
    unit: GeoDistanceUnit,
    opts: GeoSearchOpt,
  ) -> Result<Vec<GeoItem>> {
    self.geosearch_ref(
      GeoOrigin::Member(member.to_vec()),
      GeoShape::ByRadius { radius, unit },
      opts,
    )
  }

  // --- 集合运算 API (ZUNION, ZINTER, ZDIFF, ZINTERCARD) ---

  /// 多有序集合并集运算 (ZUNION / ZUNIONSTORE 纯内存计算)
  ///
  /// 支持权重与 SUM/MIN/MAX 聚合，自动排除已过期但尚未物理清退的幽灵条目
  pub fn union(sources: &[(&Self, f64)], aggregate: SortedSetAggregate) -> Self {
    if sources.is_empty() {
      return Self::new();
    }
    let now = now_ms();
    let mut map = new_hash_map::<Vec<u8>, f64>();

    for &(zset, weight) in sources {
      for (member, &score) in &zset.dict {
        if zset.is_expired_at(member, now) {
          continue;
        }
        let w_score = score * weight;
        if w_score.is_nan() {
          continue;
        }
        // 命中已有键时零克隆聚合，仅首次插入才克隆成员
        if let Some(s) = map.get_mut(member) {
          match aggregate {
            SortedSetAggregate::Sum => *s += w_score,
            SortedSetAggregate::Min => *s = s.min(w_score),
            SortedSetAggregate::Max => *s = s.max(w_score),
          }
        } else {
          map.insert(member.clone(), w_score);
        }
      }
    }

    let mut result = Self::with_capacity(map.len());
    for (m, s) in map {
      // SUM 聚合可合成 NaN (+inf + -inf)：与单条加权 NaN 一致显式剔除
      if !s.is_nan() {
        let _ = result.zadd(s, m, ZAddOpt::default());
      }
    }
    result
  }

  /// 多有序集合交集运算 (ZINTER / ZINTERSTORE 纯内存计算)
  ///
  /// ACM 启发式基准集合优化：优先选择有效基数最小的集合构建基准字典，最小化哈希查找开销；
  /// 任意集合为空时提前短路返回
  pub fn inter(sources: &[(&Self, f64)], aggregate: SortedSetAggregate) -> Self {
    if sources.is_empty() {
      return Self::new();
    }
    let now = now_ms();

    // 检查是否有任何一个集合为空
    let mut counts = Vec::with_capacity(sources.len());
    for &(zset, _) in sources {
      let count = zset.len_ref();
      if count == 0 {
        return Self::new();
      }
      counts.push(count);
    }

    // 找到存活基数最小的集合索引作为初始基准
    let mut min_idx = 0;
    let mut min_cnt = counts[0];
    for (i, &cnt) in counts.iter().enumerate().skip(1) {
      if cnt < min_cnt {
        min_cnt = cnt;
        min_idx = i;
      }
    }

    let (base_zset, base_weight) = sources[min_idx];
    let mut map = new_hash_map::<Vec<u8>, f64>();
    for (member, &score) in &base_zset.dict {
      if base_zset.is_expired_at(member, now) {
        continue;
      }
      let w_score = score * base_weight;
      if !w_score.is_nan() {
        map.insert(member.clone(), w_score);
      }
    }

    // 与其余集合逐一求交并聚合分值 (内联过期判定与字典查找，避免逐成员重复取时钟)
    for (i, &(zset, weight)) in sources.iter().enumerate() {
      if i == min_idx {
        continue;
      }
      map.retain(|member, s| {
        if let Some(&other_score) = zset.dict.get(member)
          && !zset.is_expired_at(member, now)
        {
          let w_score = other_score * weight;
          if !w_score.is_nan() {
            match aggregate {
              SortedSetAggregate::Sum => *s += w_score,
              SortedSetAggregate::Min => *s = s.min(w_score),
              SortedSetAggregate::Max => *s = s.max(w_score),
            }
            return true;
          }
        }
        false
      });

      if map.is_empty() {
        break;
      }
    }

    let mut result = Self::with_capacity(map.len());
    for (m, s) in map {
      // SUM 聚合可合成 NaN (+inf + -inf)：与单条加权 NaN 一致显式剔除
      if !s.is_nan() {
        let _ = result.zadd(s, m, ZAddOpt::default());
      }
    }
    result
  }

  /// 多有序集合差集运算 (ZDIFF / ZDIFFSTORE 纯内存计算)
  ///
  /// 保留仅属于首个集合且不存在于后续任何集合中的成员，若首集合为空或结果集被剔除为空则提前退出。
  /// 过期成员视同不存在：首集合过期成员被剔除，后续集合过期成员不参与排除。
  /// 排除集仅借用后续集合的成员键切片（零克隆）构建，成员判等与 dict/跳表一致按字节精确匹配；
  /// 结果按 (score, member) 由跳表重排，与输入遍历序（哈希序）无关
  pub fn diff(first: &Self, others: &[&Self]) -> Self {
    let now = now_ms();

    // 收集后续集合存活成员的排除集：仅借用键切片，无逐成员堆分配
    let mut exclude: HashSet<&[u8]> = new_hash_set();
    for other in others {
      for member in other.dict.keys() {
        if !other.is_expired_at(member, now) {
          exclude.insert(member.as_slice());
        }
      }
    }

    // 首集合单次遍历直出结果：跳过自身过期与被排除成员，仅存活成员克隆一次
    let mut result = Self::with_capacity(first.len_ref());
    for (member, &score) in &first.dict {
      if !first.is_expired_at(member, now) && !exclude.contains(&member[..]) {
        let _ = result.zadd(score, member.clone(), ZAddOpt::default());
      }
    }
    result
  }

  /// 多有序集合交集基数统计 (ZINTERCARD 纯内存计算)
  ///
  /// 零额外哈希表堆内存分配；以最小集合为基准，在达到 limit（若 > 0）时立即短路返回
  pub fn inter_card(sources: &[&Self], limit: usize) -> usize {
    if sources.is_empty() {
      return 0;
    }
    let now = now_ms();
    let mut min_idx = 0;
    let mut min_len = sources[0].len_ref();
    if min_len == 0 {
      return 0;
    }

    for (i, &s) in sources.iter().enumerate().skip(1) {
      let len = s.len_ref();
      if len == 0 {
        return 0;
      }
      if len < min_len {
        min_len = len;
        min_idx = i;
      }
    }

    let base_zset = sources[min_idx];
    let mut count = 0;

    for member in base_zset.dict.keys() {
      if base_zset.is_expired_at(member, now) {
        continue;
      }
      let present_in_all = sources.iter().enumerate().all(|(i, &other)| {
        i == min_idx || (other.dict.contains_key(member) && !other.is_expired_at(member, now))
      });

      if present_in_all {
        count += 1;
        if limit > 0 && count >= limit {
          return limit;
        }
      }
    }

    count
  }

  /// 多有序集合并集并写入当前集合 (ZUNIONSTORE)
  ///
  /// sources 为源集合切片，weights 可选自定义权重 (默认为 1.0)
  pub fn zunionstore(
    &mut self,
    sources: &[&Self],
    weights: Option<&[f64]>,
    aggregate: SortedSetAggregate,
  ) -> Result<usize> {
    let pairs: Vec<(&Self, f64)> = sources
      .iter()
      .enumerate()
      .map(|(i, &s)| {
        let w = weights.and_then(|ws| ws.get(i).copied()).unwrap_or(1.0);
        (s, w)
      })
      .collect();
    let res = Self::union(&pairs, aggregate);
    let count = res.len_ref();
    *self = res;
    Ok(count)
  }

  /// 多有序集合交集并写入当前集合 (ZINTERSTORE)
  pub fn zinterstore(
    &mut self,
    sources: &[&Self],
    weights: Option<&[f64]>,
    aggregate: SortedSetAggregate,
  ) -> Result<usize> {
    let pairs: Vec<(&Self, f64)> = sources
      .iter()
      .enumerate()
      .map(|(i, &s)| {
        let w = weights.and_then(|ws| ws.get(i).copied()).unwrap_or(1.0);
        (s, w)
      })
      .collect();
    let res = Self::inter(&pairs, aggregate);
    let count = res.len_ref();
    *self = res;
    Ok(count)
  }

  /// 多有序集合差集并写入当前集合 (ZDIFFSTORE)
  pub fn zdiffstore(&mut self, sources: &[&Self]) -> Result<usize> {
    if sources.is_empty() {
      self.clear();
      return Ok(0);
    }
    let res = Self::diff(sources[0], &sources[1..]);
    let count = res.len_ref();
    *self = res;
    Ok(count)
  }

  /// 极简可扩展二进制序列化
  ///
  /// 格式（极简主义，零多余，零幻数）：
  /// - version: u8 (当前 FORMAT_VERSION = 1)
  /// - count: u32 (小端)
  /// - 遍历每个元素：
  ///   - score: [u8; 8] (大端保序浮点数)
  ///   - key_len: u32 (最高位 1 表示包含过期时间戳)
  ///   - item 字节切片
  ///   - 若含过期时间，写入 expiration: u64 (毫秒时间戳)
  pub fn serialize(&mut self, buf: &mut Vec<u8>) {
    self.delete_expired();

    buf.push(FORMAT_VERSION);
    let count = self.dict.len() as u32;
    buf.extend_from_slice(&count.to_le_bytes());

    for (member, &score) in &self.dict {
      buf.extend_from_slice(&encode_sortable_f64(score));

      let mut key_len = member.len() as u32;
      let expire = self
        .expiration_times
        .as_ref()
        .and_then(|t| t.get(member).copied());

      if expire.is_some() {
        key_len |= EXPIRATION_BIT_MASK;
      }

      buf.extend_from_slice(&key_len.to_le_bytes());
      buf.extend_from_slice(member);

      if let Some(exp) = expire {
        buf.extend_from_slice(&exp.to_le_bytes());
      }
    }
  }

  /// 极简可扩展二进制反序列化
  pub fn deserialize(buf: &[u8]) -> Result<Self> {
    if buf.len() < 5 {
      return Err(Error::BufferTooShort);
    }

    let version = buf[0];
    if version != FORMAT_VERSION {
      return Err(Error::UnsupportedVersion(version));
    }

    let count = u32::from_le_bytes(*buf[1..5].first_chunk().unwrap());
    // 每个元素至少占 8B 分数 + 4B 长度 = 12B，防止恶意大数导致巨量预分配 OOM
    let max_possible = (buf.len() - 5) / 12;
    if (count as usize) > max_possible {
      return Err(Error::CorruptedData);
    }
    let mut cursor = 5;

    let now = now_ms();
    let mut zset = Self::with_capacity(count as usize);

    for _ in 0..count {
      if cursor + 8 + 4 > buf.len() {
        return Err(Error::BufferTooShort);
      }

      let score_bytes: [u8; 8] = *buf[cursor..cursor + 8].first_chunk().unwrap();
      let score = decode_sortable_f64(score_bytes);
      cursor += 8;

      let raw_key_len = u32::from_le_bytes(*buf[cursor..cursor + 4].first_chunk().unwrap());
      cursor += 4;

      let has_expiration = (raw_key_len & EXPIRATION_BIT_MASK) != 0;
      let key_len = (raw_key_len & !EXPIRATION_BIT_MASK) as usize;

      let key_end = cursor.checked_add(key_len).ok_or(Error::CorruptedData)?;
      if key_end > buf.len() {
        return Err(Error::BufferTooShort);
      }
      let member = buf[cursor..key_end].to_vec();
      cursor = key_end;

      if has_expiration {
        if cursor + 8 > buf.len() {
          return Err(Error::BufferTooShort);
        }
        let expire = u64::from_le_bytes(*buf[cursor..cursor + 8].first_chunk().unwrap());
        cursor += 8;

        if expire > now {
          zset.zadd(score, member.clone(), ZAddOpt::default())?;
          zset.zexpire(&member, expire, ExpireOpt::default());
        }
      } else {
        zset.zadd(score, member, ZAddOpt::default())?;
      }
    }

    if cursor != buf.len() {
      // 尾部存在未声明的多余字节：视为数据损坏，与 CompactZSet 校验保持一致
      return Err(Error::CorruptedData);
    }

    Ok(zset)
  }

  /// 将有序集合编码为 bitcode 字节流 (排除已过期条目)
  pub fn to_bitcode(&self) -> Vec<u8> {
    let now = now_ms();
    let entries: Vec<SortedSetEntryBitcode> = self
      .skiplist
      .iter()
      .filter_map(|(member, score)| {
        let expire = self
          .expiration_times
          .as_ref()
          .and_then(|exp| exp.get(member).copied());
        if expire.is_some_and(|exp| exp <= now) {
          return None;
        }
        Some(SortedSetEntryBitcode {
          member: member.to_vec(),
          score,
          expire,
        })
      })
      .collect();
    bitcode::encode(&entries)
  }

  /// 从 bitcode 字节流解码出 SortedSetObject (自动滤除已过期条目)
  pub fn from_bitcode(bytes: &[u8]) -> Result<Self> {
    let entries: Vec<SortedSetEntryBitcode> = bitcode::decode(bytes).map_err(Error::Bitcode)?;
    let mut zset = Self::with_capacity(entries.len());
    let now = now_ms();
    for entry in entries {
      if let Some(exp) = entry.expire {
        if exp <= now {
          continue;
        }
        zset.zadd(entry.score, entry.member.clone(), ZAddOpt::default())?;
        zset.zexpire(&entry.member, exp, ExpireOpt::default());
      } else {
        zset.zadd(entry.score, entry.member, ZAddOpt::default())?;
      }
    }
    Ok(zset)
  }
}

/// 成员条目 bitcode 序列化结构
#[derive(Debug, Clone, PartialEq, Encode, Decode)]
pub struct SortedSetEntryBitcode {
  pub member: Vec<u8>,
  pub score: f64,
  pub expire: Option<u64>,
}
