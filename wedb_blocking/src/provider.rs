use std::{collections::VecDeque, str::from_utf8, sync::Arc};

use bytes::Bytes;
use parking_lot::RwLock;
use wedb_resp::RespCommand;
use whasher::{GxPapayaMap, new_papaya_map};

use crate::{
  error::{Error, Result},
  result::CollectionItemResult,
};

/// 列表操作方向
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
  /// 左端 / 头部 (LEFT / HEAD)
  Left,
  /// 右端 / 尾部 (RIGHT / TAIL)
  Right,
}

impl Direction {
  /// 从字节切片解析方向
  pub fn from_bytes(slice: &[u8]) -> Option<Self> {
    if slice.eq_ignore_ascii_case(b"LEFT") {
      Some(Self::Left)
    } else if slice.eq_ignore_ascii_case(b"RIGHT") {
      Some(Self::Right)
    } else {
      None
    }
  }
}

/// 集合对象存储定义
#[derive(Debug)]
enum CollectionObject {
  List(VecDeque<Bytes>),
  SortedSet(Vec<(f64, Bytes)>),
}

/// 集合数据访问提供者 Trait
///
/// 允许底层存储引擎（如 Garnet / RocksDB / wedb_store）接入阻塞调度中继器。
///
/// # 锁序契约
///
/// 实现须保证单次调用的加锁自包含：broker 会在持有观察者状态锁的临界区内
/// 调用本 trait（对齐 C# 在 ObserverStatusLock 写锁内弹出与投递的原子语义），
/// 因此实现内部不得反向调用 broker 或等待其他持有 broker 内部锁的路径。
pub trait CollectionProvider: Send + Sync + 'static {
  /// 尝试从集合中弹出满足命令语义的元素
  ///
  /// 返回 Ok(Some) 表示成功弹出；Ok(None) 表示键不存在 / 集合为空 / 类型不匹配。
  ///
  /// `fail_on_mismatch`：对齐 C# `TryGetResult` 的 `failOnSrcTypeMismatch`。
  /// 注册探测传 true：类型不符时返回 WRONGTYPE 直接终结观察者；
  /// 等待分配路径传 false：类型不符视为暂不可消费（观察者继续阻塞，对齐 Redis 语义）。
  fn try_pop_item(
    &self,
    key: &[u8],
    command: RespCommand,
    args: &[Bytes],
    fail_on_mismatch: bool,
  ) -> Result<Option<CollectionItemResult>>;

  /// 针对 BLMOVE / BRPOPLPUSH 等跨 Key 原子转移操作
  ///
  /// 源类型不匹配遵循 `fail_on_src_mismatch`（语义同 [Self::try_pop_item]）；
  /// 目标类型不匹配无论何时均返回 WRONGTYPE 且源元素绝不丢失
  /// （对齐 C#「BLMOVE 的 dst 类型不符总是返回 TypeMismatch」）。
  fn try_move_item(
    &self,
    src_key: &[u8],
    dst_key: &[u8],
    src_dir: Direction,
    dst_dir: Direction,
    fail_on_src_mismatch: bool,
  ) -> Result<Option<CollectionItemResult>>;
}

/// 单键弹出归一化结果
enum PopOutcome {
  /// 键不存在或集合为空
  Empty,
  /// 类型不匹配
  Mismatch,
  /// 成功弹出
  Hit(CollectionItemResult),
}

/// 高并发内存集合存储实现
///
/// 既可作为独立模块测试运行，也可作为内嵌轻量级存储提供者。
#[derive(Debug)]
pub struct MemoryCollectionStore {
  collections: GxPapayaMap<Bytes, Arc<RwLock<CollectionObject>>>,
}

impl Default for MemoryCollectionStore {
  fn default() -> Self {
    Self {
      collections: new_papaya_map(),
    }
  }
}

impl MemoryCollectionStore {
  /// 创建新的内存集合存储
  pub fn new() -> Self {
    Self::default()
  }

  /// 清空全部数据
  pub fn clear(&self) {
    self.collections.pin().clear();
  }

  /// 对 key 挂载的集合加写锁执行可变操作，集中处理取条目与空回收
  ///
  /// key 不存在时返回 None 且不执行闭包；闭包返回 (输出, 操作后集合是否为空)，
  /// 集合变空时回收该 key 条目（对齐 C# 弹空后 EXPIRE asKey TimeSpan.Zero 的清理语义）
  fn mutate<O>(&self, key: &[u8], f: impl FnOnce(&mut CollectionObject) -> (O, bool)) -> Option<O> {
    let pin = self.collections.pin();
    let entry = pin.get(key)?.clone();
    let mut guard = entry.write();
    let (out, emptied) = f(&mut guard);
    if emptied {
      drop(guard);
      pin.remove(key);
    }
    Some(out)
  }

  /// 获取（或创建）key 对应的 List 条目；类型冲突时返回 WRONGTYPE
  fn list_entry_for_write(&self, key: &[u8]) -> Result<Arc<RwLock<CollectionObject>>> {
    let pin = self.collections.pin();
    let entry = match pin.get(key) {
      Some(e) if matches!(&*e.read(), CollectionObject::SortedSet(_)) => {
        return Err(Error::WrongType);
      }
      Some(e) => e.clone(),
      None => pin
        .get_or_insert_with(Bytes::copy_from_slice(key), || {
          Arc::new(RwLock::new(CollectionObject::List(VecDeque::new())))
        })
        .clone(),
    };
    // 双重检查：与并发类型变更竞争时宁可报 WRONGTYPE，绝不破坏既有数据
    if matches!(&*entry.read(), CollectionObject::SortedSet(_)) {
      return Err(Error::WrongType);
    }
    Ok(entry)
  }

  /// 向列表端部推入一个元素 (LPUSH / RPUSH)
  ///
  /// 若 key 已存在且类型不是 List，返回 WRONGTYPE，既有数据保持不变（对齐 Redis 语义）
  fn push_list_side(&self, key: &[u8], value: Bytes, front: bool) -> Result<usize> {
    let entry = self.list_entry_for_write(key)?;
    let mut guard = entry.write();
    let CollectionObject::List(list) = &mut *guard else {
      return Err(Error::WrongType);
    };
    if front {
      list.push_front(value);
    } else {
      list.push_back(value);
    }
    Ok(list.len())
  }

  /// 向列表左侧推入一个元素 (LPUSH)
  pub fn push_list_left(&self, key: &[u8], value: impl Into<Bytes>) -> Result<usize> {
    self.push_list_side(key, value.into(), true)
  }

  /// 向列表右侧推入一个元素 (RPUSH)
  pub fn push_list_right(&self, key: &[u8], value: impl Into<Bytes>) -> Result<usize> {
    self.push_list_side(key, value.into(), false)
  }

  /// 从列表端部弹出一个元素 (LPOP / RPOP)，弹空后回收条目
  fn pop_list_side(&self, key: &[u8], front: bool) -> Option<Bytes> {
    self
      .mutate(key, |c| match c {
        CollectionObject::List(l) => {
          let item = if front { l.pop_front() } else { l.pop_back() };
          (item, l.is_empty())
        }
        _ => (None, false),
      })
      .flatten()
  }

  /// 从列表左端弹出一个元素 (LPOP)
  pub fn pop_list_left(&self, key: &[u8]) -> Option<Bytes> {
    self.pop_list_side(key, true)
  }

  /// 从列表右端弹出一个元素 (RPOP)
  pub fn pop_list_right(&self, key: &[u8]) -> Option<Bytes> {
    self.pop_list_side(key, false)
  }

  /// 向有序集合添加一个元素 (ZADD)
  ///
  /// 若 key 已存在且类型不是 SortedSet，返回 WRONGTYPE，既有数据保持不变（对齐 Redis 语义）
  pub fn zadd(&self, key: &[u8], score: f64, member: impl Into<Bytes>) -> Result<usize> {
    let mem_bytes = member.into();
    let pin = self.collections.pin();
    let entry = match pin.get(key) {
      Some(e) if matches!(&*e.read(), CollectionObject::List(_)) => {
        return Err(Error::WrongType);
      }
      Some(e) => e.clone(),
      None => pin
        .get_or_insert_with(Bytes::copy_from_slice(key), || {
          Arc::new(RwLock::new(CollectionObject::SortedSet(Vec::new())))
        })
        .clone(),
    };

    let mut guard = entry.write();
    let CollectionObject::SortedSet(zset) = &mut *guard else {
      return Err(Error::WrongType);
    };
    let is_new = if let Some(pos) = zset.iter().position(|(_, m)| m == &mem_bytes) {
      if zset[pos].0.total_cmp(&score).is_eq() {
        return Ok(0);
      }
      zset.remove(pos);
      false
    } else {
      true
    };
    let insert_idx = zset
      .binary_search_by(|(s, m)| s.total_cmp(&score).then_with(|| m.as_ref().cmp(&mem_bytes)))
      .unwrap_or_else(|e| e);
    zset.insert(insert_idx, (score, mem_bytes));
    Ok(if is_new { 1 } else { 0 })
  }

  /// 弹出有序集合最低 / 最高分成员 (ZPOPMIN / ZPOPMAX)，弹空后回收条目
  fn zpop_side(&self, key: &[u8], max: bool) -> Option<(f64, Bytes)> {
    self
      .mutate(key, |c| {
        let CollectionObject::SortedSet(z) = c else {
          return (None, false);
        };
        let item = if max {
          z.pop()
        } else if z.is_empty() {
          None
        } else {
          Some(z.remove(0))
        };
        (item, z.is_empty())
      })
      .flatten()
  }

  /// 从有序集合弹出最小分数元素 (ZPOPMIN)
  pub fn zpop_min(&self, key: &[u8]) -> Option<(f64, Bytes)> {
    self.zpop_side(key, false)
  }

  /// 从有序集合弹出最大分数元素 (ZPOPMAX)
  pub fn zpop_max(&self, key: &[u8]) -> Option<(f64, Bytes)> {
    self.zpop_side(key, true)
  }

  /// 获取指定集合当前元素数量
  pub fn len(&self, key: &[u8]) -> usize {
    let pin = self.collections.pin();
    match pin.get(key) {
      Some(entry) => match &*entry.read() {
        CollectionObject::List(l) => l.len(),
        CollectionObject::SortedSet(z) => z.len(),
      },
      None => 0,
    }
  }

  /// [Self::mutate] 的弹出归一化包装：键不存在时归一化为 [PopOutcome::Empty]
  fn pop_with(
    &self,
    key: &[u8],
    f: impl FnOnce(&mut CollectionObject) -> (PopOutcome, bool),
  ) -> Result<PopOutcome> {
    Ok(self.mutate(key, f).unwrap_or(PopOutcome::Empty))
  }

  /// 按命令语义尝试单键弹出（对齐 C# `CollectionItemBroker.TryGetResult` 的单对象路径）
  fn pop_outcome(&self, key: &[u8], command: RespCommand, args: &[Bytes]) -> Result<PopOutcome> {
    match command {
      RespCommand::Blpop | RespCommand::Brpop => {
        let front = command == RespCommand::Blpop;
        self.pop_with(key, |c| match c {
          CollectionObject::List(l) => {
            let item = if front { l.pop_front() } else { l.pop_back() };
            let out = match item {
              Some(v) => {
                PopOutcome::Hit(CollectionItemResult::single(Bytes::copy_from_slice(key), v))
              }
              None => PopOutcome::Empty,
            };
            (out, l.is_empty())
          }
          _ => (PopOutcome::Mismatch, false),
        })
      }
      RespCommand::Blmpop => {
        let front = args.first().and_then(|b| Direction::from_bytes(b)) != Some(Direction::Right);
        let count = parse_count(args.get(1));
        self.pop_with(key, |c| match c {
          CollectionObject::List(l) => {
            let take = count.min(l.len());
            let mut items = Vec::with_capacity(take);
            for _ in 0..take {
              match if front { l.pop_front() } else { l.pop_back() } {
                Some(v) => items.push(v),
                None => break,
              }
            }
            let out = if items.is_empty() {
              PopOutcome::Empty
            } else {
              PopOutcome::Hit(CollectionItemResult::multi(
                Bytes::copy_from_slice(key),
                items,
              ))
            };
            (out, l.is_empty())
          }
          _ => (PopOutcome::Mismatch, false),
        })
      }
      RespCommand::Bzpopmin | RespCommand::Bzpopmax => {
        let max = command == RespCommand::Bzpopmax;
        self.pop_with(key, |c| match c {
          CollectionObject::SortedSet(z) => {
            let item = if max {
              z.pop()
            } else if z.is_empty() {
              None
            } else {
              Some(z.remove(0))
            };
            let out = match item {
              Some((score, m)) => PopOutcome::Hit(CollectionItemResult::single_scored(
                Bytes::copy_from_slice(key),
                score,
                m,
              )),
              None => PopOutcome::Empty,
            };
            (out, z.is_empty())
          }
          _ => (PopOutcome::Mismatch, false),
        })
      }
      RespCommand::Bzmpop => {
        let is_max = args.first().is_some_and(|b| b.eq_ignore_ascii_case(b"MAX"));
        let count = parse_count(args.get(1));
        self.pop_with(key, |c| match c {
          CollectionObject::SortedSet(z) => {
            let take = count.min(z.len());
            let mut scores = Vec::with_capacity(take);
            let mut items = Vec::with_capacity(take);
            if is_max {
              for _ in 0..take {
                if let Some((s, m)) = z.pop() {
                  scores.push(s);
                  items.push(m);
                }
              }
            } else {
              for (s, m) in z.drain(0..take) {
                scores.push(s);
                items.push(m);
              }
            }
            let out = if items.is_empty() {
              PopOutcome::Empty
            } else {
              PopOutcome::Hit(CollectionItemResult::multi_scored(
                Bytes::copy_from_slice(key),
                scores,
                items,
              ))
            };
            (out, z.is_empty())
          }
          _ => (PopOutcome::Mismatch, false),
        })
      }
      other => Err(Error::UnsupportedCommand(other)),
    }
  }
}

impl CollectionProvider for MemoryCollectionStore {
  fn try_pop_item(
    &self,
    key: &[u8],
    command: RespCommand,
    args: &[Bytes],
    fail_on_mismatch: bool,
  ) -> Result<Option<CollectionItemResult>> {
    match self.pop_outcome(key, command, args)? {
      // 类型不匹配：等待分配路径（fail_on_mismatch = false）视为暂不可消费，
      // 观察者保持阻塞（对齐 C# TryAssignItemFromKey 以 failOnSrcTypeMismatch: false 调用）
      PopOutcome::Mismatch if !fail_on_mismatch => Ok(None),
      PopOutcome::Mismatch => Ok(Some(CollectionItemResult::type_mismatch())),
      PopOutcome::Empty => Ok(None),
      PopOutcome::Hit(result) => Ok(Some(result)),
    }
  }

  fn try_move_item(
    &self,
    src_key: &[u8],
    dst_key: &[u8],
    src_dir: Direction,
    dst_dir: Direction,
    fail_on_src_mismatch: bool,
  ) -> Result<Option<CollectionItemResult>> {
    let pin = self.collections.pin();
    let Some(src_entry) = pin.get(src_key).cloned() else {
      return Ok(None);
    };

    let same_key = src_key == dst_key;
    // 预取/预建目标条目，保证下方按 key 字节序加锁，跨 key 转移绝无锁序死锁
    let mut created_dst = false;
    let dst_entry = if same_key {
      src_entry.clone()
    } else {
      match pin.get(dst_key) {
        Some(e) => e.clone(),
        None => {
          created_dst = true;
          pin
            .get_or_insert_with(Bytes::copy_from_slice(dst_key), || {
              Arc::new(RwLock::new(CollectionObject::List(VecDeque::new())))
            })
            .clone()
        }
      }
    };

    let outcome = if same_key {
      // 同 key 转移：单写锁内完成（如 BLMOVE k k LEFT RIGHT 的旋转语义）
      let mut guard = src_entry.write();
      move_within(&mut guard, src_dir, dst_dir)
    } else if src_key < dst_key {
      let mut sg = src_entry.write();
      let mut dg = dst_entry.write();
      move_across(&mut sg, &mut dg, src_dir, dst_dir)
    } else {
      let mut dg = dst_entry.write();
      let mut sg = src_entry.write();
      move_across(&mut sg, &mut dg, src_dir, dst_dir)
    };

    // 回收预建却未被使用的目标条目，防泄漏空壳
    let reap_dst = || {
      if created_dst && is_empty_list(&dst_entry) {
        pin.remove(dst_key);
      }
    };

    match outcome {
      MoveOutcome::NotFound => {
        reap_dst();
        Ok(None)
      }
      // 源类型不匹配：等待分配路径（fail_on_src_mismatch = false）视为暂不可消费，
      // 观察者保持阻塞（对齐 C# TryGetResult 的 failOnSrcTypeMismatch 语义）
      MoveOutcome::SrcMismatch if !fail_on_src_mismatch => {
        reap_dst();
        Ok(None)
      }
      MoveOutcome::SrcMismatch => {
        reap_dst();
        Ok(Some(CollectionItemResult::type_mismatch()))
      }
      // 目标类型不匹配：无论何时都返回 WRONGTYPE，且源元素绝不丢失
      // （对齐 C# 先 GET dstKey 校验、总是返回 TypeMismatch）
      MoveOutcome::DstMismatch => {
        reap_dst();
        Ok(Some(CollectionItemResult::type_mismatch()))
      }
      MoveOutcome::Moved { item, src_emptied } => {
        if src_emptied {
          pin.remove(src_key);
        }
        Ok(Some(CollectionItemResult::single(
          Bytes::copy_from_slice(src_key),
          item,
        )))
      }
    }
  }
}

/// 解析 BLMPOP / BZMPOP 的数量参数（缺省或非法时按 1 处理）
fn parse_count(arg: Option<&Bytes>) -> usize {
  arg
    .and_then(|b| from_utf8(b).ok())
    .and_then(|s| s.parse::<usize>().ok())
    .unwrap_or(1)
    .max(1)
}

/// BLMOVE 转移结果
enum MoveOutcome {
  /// 源列表为空，无可转移元素
  NotFound,
  /// 源对象类型不匹配
  SrcMismatch,
  /// 目标对象类型不匹配
  DstMismatch,
  /// 转移成功：元素与其弹出后源列表是否为空
  Moved { item: Bytes, src_emptied: bool },
}

/// 同 key 转移：弹出后立即推回同一列表
fn move_within(c: &mut CollectionObject, src_dir: Direction, dst_dir: Direction) -> MoveOutcome {
  let CollectionObject::List(list) = c else {
    return MoveOutcome::SrcMismatch;
  };
  let popped = match src_dir {
    Direction::Left => list.pop_front(),
    Direction::Right => list.pop_back(),
  };
  let Some(item) = popped else {
    return MoveOutcome::NotFound;
  };
  match dst_dir {
    Direction::Left => list.push_front(item.clone()),
    Direction::Right => list.push_back(item.clone()),
  }
  MoveOutcome::Moved {
    item,
    src_emptied: list.is_empty(),
  }
}

/// 跨 key 转移：先校验双方类型，再弹出并推入（绝不丢失元素）
fn move_across(
  src: &mut CollectionObject,
  dst: &mut CollectionObject,
  src_dir: Direction,
  dst_dir: Direction,
) -> MoveOutcome {
  let (src_list, dst_list) = match (src, dst) {
    (CollectionObject::List(s), CollectionObject::List(d)) => (s, d),
    // 源为 List 而目标类型不符：目标不匹配
    (CollectionObject::List(_), _) => return MoveOutcome::DstMismatch,
    // 源自身类型不符
    _ => return MoveOutcome::SrcMismatch,
  };
  let popped = match src_dir {
    Direction::Left => src_list.pop_front(),
    Direction::Right => src_list.pop_back(),
  };
  let Some(item) = popped else {
    return MoveOutcome::NotFound;
  };
  let ret = item.clone();
  match dst_dir {
    Direction::Left => dst_list.push_front(item),
    Direction::Right => dst_list.push_back(item),
  }
  MoveOutcome::Moved {
    item: ret,
    src_emptied: src_list.is_empty(),
  }
}

/// 判断条目是否为空列表
fn is_empty_list(entry: &Arc<RwLock<CollectionObject>>) -> bool {
  matches!(
    &*entry.read(),
    CollectionObject::List(list) if list.is_empty()
  )
}
