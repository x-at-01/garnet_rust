use core::{marker::PhantomData, slice::from_raw_parts};
use std::{
  cmp::Ordering,
  iter::FusedIterator,
  ptr::{NonNull, null_mut},
};

use fastrand::Rng;

use crate::zset::{LexBound, encode_sortable_f64};

const MAX_LEVEL: usize = 32;
const SKIPLIST_P: f32 = 0.25;

/// 分数范围区间
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScoreRange {
  pub min: f64,
  pub min_inclusive: bool,
  pub max: f64,
  pub max_inclusive: bool,
}

impl ScoreRange {
  #[inline]
  pub const fn new(min: f64, min_inclusive: bool, max: f64, max_inclusive: bool) -> Self {
    Self {
      min,
      min_inclusive,
      max,
      max_inclusive,
    }
  }

  /// 判断区间是否合法 (排除 NaN、倒置区间以及退化开区间)
  #[inline]
  pub const fn is_valid(&self) -> bool {
    if self.min.is_nan() || self.max.is_nan() || self.min > self.max {
      return false;
    }
    if self.min == self.max && (!self.min_inclusive || !self.max_inclusive) {
      return false;
    }
    true
  }

  /// 判断分数是否落在区间内 (闭/开边界，自动校验区间合法性)
  #[inline]
  pub const fn contains(&self, s: f64) -> bool {
    if !self.is_valid() || s.is_nan() {
      return false;
    }
    let ge_min = if self.min_inclusive {
      s >= self.min
    } else {
      s > self.min
    };
    let le_max = if self.max_inclusive {
      s <= self.max
    } else {
      s < self.max
    };
    ge_min && le_max
  }
}

/// 跳表层级链接与跨度
#[derive(Clone, Copy)]
struct Level {
  forward: *mut Node,
  span: usize,
}

/// 跳表节点
pub struct Node {
  pub score: f64,
  /// 保序编码分数缓存：热路径比较零重复编码 (与 encode_sortable_f64(score) 恒等)
  order: [u8; 8],
  pub member: Vec<u8>,
  backward: *mut Node,
  level: Box<[Level]>,
}

impl Node {
  fn new(level: usize, score: f64, member: Vec<u8>) -> Self {
    let order = encode_sortable_f64(score);
    let level = vec![
      Level {
        forward: null_mut(),
        span: 0,
      };
      level
    ]
    .into_boxed_slice();
    Self {
      score,
      order,
      member,
      backward: null_mut(),
      level,
    }
  }

  #[inline]
  fn forward(&self, i: usize) -> *mut Node {
    self.level[i].forward
  }

  #[inline]
  fn set_forward(&mut self, i: usize, fwd: *mut Node) {
    self.level[i].forward = fwd;
  }

  #[inline]
  fn span(&self, i: usize) -> usize {
    self.level[i].span
  }

  #[inline]
  fn set_span(&mut self, i: usize, span: usize) {
    self.level[i].span = span;
  }
}

/// Redis 规范跳表 (zskiplist，带每层 span 跨度，支持 O(log N) 排名查询)
pub struct SkipList {
  header: NonNull<Node>,
  tail: *mut Node,
  length: usize,
  level: usize,
  rng: Rng,
}

impl Default for SkipList {
  fn default() -> Self {
    Self::new()
  }
}

impl SkipList {
  /// 创建空跳表
  pub fn new() -> Self {
    let header_node = Box::into_raw(Box::new(Node::new(MAX_LEVEL, 0.0, Vec::new())));
    Self {
      header: NonNull::new(header_node).unwrap(),
      tail: null_mut(),
      length: 0,
      level: 1,
      rng: Rng::new(),
    }
  }

  /// 获取元素总数
  #[inline]
  pub fn len(&self) -> usize {
    self.length
  }

  /// 升序零克隆迭代 (成员切片, 分数)
  pub fn iter(&self) -> SkipListIter<'_> {
    SkipListIter {
      curr: unsafe { (&*self.header.as_ptr()).forward(0) },
      remaining: self.length,
      marker: PhantomData,
    }
  }

  /// 判断是否为空
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.length == 0
  }

  /// 生成随机层数 (几何分布，p = 0.25)
  fn random_level(&mut self) -> usize {
    let mut lvl = 1;
    while lvl < MAX_LEVEL && self.rng.f32() < SKIPLIST_P {
      lvl += 1;
    }
    lvl
  }

  /// 插入新节点 (score, member)
  pub fn insert(&mut self, score: f64, member: Vec<u8>) {
    let order = encode_sortable_f64(score);
    let mut update = [null_mut(); MAX_LEVEL];
    let mut rank = [0usize; MAX_LEVEL];

    let mut curr = self.header.as_ptr();
    for i in (0..self.level).rev() {
      rank[i] = if i == self.level - 1 { 0 } else { rank[i + 1] };
      unsafe {
        while !(&*curr).forward(i).is_null() {
          let next = (&*curr).forward(i);
          let next_ref = &*next;
          if next_ref
            .order
            .cmp(&order)
            .then_with(|| next_ref.member.cmp(&member))
            == Ordering::Less
          {
            rank[i] += (&*curr).span(i);
            curr = next;
          } else {
            break;
          }
        }
      }
      update[i] = curr;
    }

    let lvl = self.random_level();
    if lvl > self.level {
      for i in self.level..lvl {
        rank[i] = 0;
        update[i] = self.header.as_ptr();
        unsafe {
          (&mut *self.header.as_ptr()).set_span(i, self.length);
        }
      }
      self.level = lvl;
    }

    let new_node = Box::into_raw(Box::new(Node::new(lvl, score, member)));

    for i in 0..lvl {
      unsafe {
        let update_ref = &mut *update[i];
        let new_ref = &mut *new_node;

        new_ref.set_forward(i, update_ref.forward(i));
        update_ref.set_forward(i, new_node);

        new_ref.set_span(i, update_ref.span(i) - (rank[0] - rank[i]));
        update_ref.set_span(i, (rank[0] - rank[i]) + 1);
      }
    }

    for (i, update_node) in update
      .iter()
      .copied()
      .enumerate()
      .take(self.level)
      .skip(lvl)
    {
      unsafe {
        let update_ref = &mut *update_node;
        let current_span = update_ref.span(i);
        update_ref.set_span(i, current_span + 1);
      }
    }

    unsafe {
      (&mut *new_node).backward = if update[0] == self.header.as_ptr() {
        null_mut()
      } else {
        update[0]
      };

      let forward0 = (&*new_node).forward(0);
      if !forward0.is_null() {
        (&mut *forward0).backward = new_node;
      } else {
        self.tail = new_node;
      }
    }

    self.length += 1;
  }

  /// 删除节点 (score, member)，若删除成功返回其所有权 (member, score)
  pub fn delete_entry(&mut self, score: f64, member: &[u8]) -> Option<(Vec<u8>, f64)> {
    let order = encode_sortable_f64(score);
    let mut update = [null_mut(); MAX_LEVEL];

    let mut curr = self.header.as_ptr();
    for i in (0..self.level).rev() {
      unsafe {
        while !(&*curr).forward(i).is_null() {
          let next = (&*curr).forward(i);
          let next_ref = &*next;
          if next_ref
            .order
            .cmp(&order)
            .then_with(|| next_ref.member.as_slice().cmp(member))
            == Ordering::Less
          {
            curr = next;
          } else {
            break;
          }
        }
      }
      update[i] = curr;
    }

    unsafe {
      let target = (&*curr).forward(0);
      // order 相等即视为分数相等 (保序编码已折叠 ±0.0)，配合成员字节精确匹配
      if !target.is_null() && (*target).order == order && (*target).member.as_slice() == member {
        let target_ref = &*target;
        for (i, update_node) in update.iter().copied().enumerate().take(self.level) {
          let update_ref = &mut *update_node;
          if update_ref.forward(i) == target {
            let new_span = update_ref.span(i) + target_ref.span(i) - 1;
            update_ref.set_span(i, new_span);
            update_ref.set_forward(i, target_ref.forward(i));
          } else {
            let current_span = update_ref.span(i);
            update_ref.set_span(i, current_span - 1);
          }
        }

        if !target_ref.forward(0).is_null() {
          (&mut *target_ref.forward(0)).backward = target_ref.backward;
        } else {
          self.tail = target_ref.backward;
        }

        while self.level > 1 && (&*self.header.as_ptr()).forward(self.level - 1).is_null() {
          self.level -= 1;
        }

        self.length -= 1;
        let boxed = Box::from_raw(target);
        Some((boxed.member, boxed.score))
      } else {
        None
      }
    }
  }

  /// 删除节点 (score, member)，若删除成功返回 true
  #[inline]
  pub fn delete(&mut self, score: f64, member: &[u8]) -> bool {
    self.delete_entry(score, member).is_some()
  }

  /// 获取指定 (score, member) 的排名 (0-based)
  pub fn get_rank(&self, score: f64, member: &[u8]) -> Option<usize> {
    let order = encode_sortable_f64(score);
    let mut rank = 0;
    let mut curr = self.header.as_ptr();

    for i in (0..self.level).rev() {
      unsafe {
        while !(&*curr).forward(i).is_null() {
          let next = (&*curr).forward(i);
          let next_ref = &*next;
          match next_ref
            .order
            .cmp(&order)
            .then_with(|| next_ref.member.as_slice().cmp(member))
          {
            Ordering::Less => {
              rank += (&*curr).span(i);
              curr = next;
            }
            Ordering::Equal => {
              rank += (&*curr).span(i);
              return Some(rank - 1); // 转换为 0-based
            }
            Ordering::Greater => break,
          }
        }
      }
    }
    None
  }

  /// 获取指定排名 (0-based) 对应的节点引用
  pub fn get_by_rank(&self, rank: usize) -> Option<(&[u8], f64)> {
    if rank >= self.length {
      return None;
    }
    let target_rank = rank + 1; // 转换为 1-based
    let mut curr_rank = 0;
    let mut curr = self.header.as_ptr();

    for i in (0..self.level).rev() {
      unsafe {
        while !(&*curr).forward(i).is_null() && curr_rank + (&*curr).span(i) <= target_rank {
          curr_rank += (&*curr).span(i);
          curr = (&*curr).forward(i);
        }
        if curr_rank == target_rank {
          return Some((&(*curr).member, (*curr).score));
        }
      }
    }
    None
  }

  /// 按 0-based 排名区间获取元素借用切片 (闭区间 [start, stop]，零成员堆分配)
  pub fn range_by_rank_borrowed(
    &self,
    start: usize,
    stop: usize,
    reverse: bool,
  ) -> Vec<(&[u8], f64)> {
    if self.length == 0 || start > stop || start >= self.length {
      return Vec::new();
    }

    let actual_stop = stop.min(self.length - 1);
    let count = actual_stop - start + 1;
    let mut res = Vec::with_capacity(count);

    if !reverse {
      let target_rank = start + 1;
      let mut curr_rank = 0;
      let mut curr = self.header.as_ptr();

      for i in (0..self.level).rev() {
        unsafe {
          while !(&*curr).forward(i).is_null() && curr_rank + (&*curr).span(i) <= target_rank {
            curr_rank += (&*curr).span(i);
            curr = (&*curr).forward(i);
          }
          if curr_rank == target_rank {
            break;
          }
        }
      }

      unsafe {
        for _ in 0..count {
          if curr.is_null() || curr == self.header.as_ptr() {
            break;
          }
          res.push(((*curr).member.as_slice(), (*curr).score));
          curr = (&*curr).forward(0);
        }
      }
    } else {
      let rev_start = self.length - 1 - start;
      let target_rank = rev_start + 1;
      let mut curr_rank = 0;
      let mut curr = self.header.as_ptr();

      for i in (0..self.level).rev() {
        unsafe {
          while !(&*curr).forward(i).is_null() && curr_rank + (&*curr).span(i) <= target_rank {
            curr_rank += (&*curr).span(i);
            curr = (&*curr).forward(i);
          }
          if curr_rank == target_rank {
            break;
          }
        }
      }

      unsafe {
        for _ in 0..count {
          if curr.is_null() || curr == self.header.as_ptr() {
            break;
          }
          res.push(((*curr).member.as_slice(), (*curr).score));
          curr = (*curr).backward;
        }
      }
    }

    res
  }

  /// 按 0-based 排名区间获取元素 (闭区间 [start, stop])
  pub fn range_by_rank(&self, start: usize, stop: usize, reverse: bool) -> Vec<(Vec<u8>, f64)> {
    self
      .range_by_rank_borrowed(start, stop, reverse)
      .into_iter()
      .map(|(m, s)| (m.to_vec(), s))
      .collect()
  }

  /// 按分数区间获取元素借用切片 (零成员堆分配)
  pub fn range_by_score_borrowed(
    &self,
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(&[u8], f64)> {
    if self.length == 0 || count == 0 || !range.is_valid() {
      return Vec::new();
    }

    let mut res = Vec::new();

    if !reverse {
      let mut curr = self.header.as_ptr();
      for i in (0..self.level).rev() {
        unsafe {
          while !(&*curr).forward(i).is_null() {
            let next = (&*curr).forward(i);
            let next_ref = &*next;
            let condition = if range.min_inclusive {
              next_ref.score < range.min
            } else {
              next_ref.score <= range.min
            };
            if condition {
              curr = next;
            } else {
              break;
            }
          }
        }
      }

      unsafe {
        curr = (&*curr).forward(0);
        let mut skipped = 0;
        while !curr.is_null() {
          let s = (*curr).score;
          let within_max = if range.max_inclusive {
            s <= range.max
          } else {
            s < range.max
          };
          if !within_max {
            break;
          }
          if skipped < offset {
            skipped += 1;
          } else {
            res.push(((*curr).member.as_slice(), s));
            if res.len() >= count {
              break;
            }
          }
          curr = (&*curr).forward(0);
        }
      }
    } else {
      let mut curr = self.header.as_ptr();
      for i in (0..self.level).rev() {
        unsafe {
          while !(&*curr).forward(i).is_null() {
            let next = (&*curr).forward(i);
            let next_ref = &*next;
            let condition = if range.max_inclusive {
              next_ref.score <= range.max
            } else {
              next_ref.score < range.max
            };
            if condition {
              curr = next;
            } else {
              break;
            }
          }
        }
      }

      unsafe {
        let mut skipped = 0;
        while !curr.is_null() && curr != self.header.as_ptr() {
          let s = (*curr).score;
          let within_min = if range.min_inclusive {
            s >= range.min
          } else {
            s > range.min
          };
          if !within_min {
            break;
          }
          if skipped < offset {
            skipped += 1;
          } else {
            res.push(((*curr).member.as_slice(), s));
            if res.len() >= count {
              break;
            }
          }
          curr = (*curr).backward;
        }
      }
    }

    res
  }

  /// 按分数区间获取元素
  pub fn range_by_score(
    &self,
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self
      .range_by_score_borrowed(range, reverse, offset, count)
      .into_iter()
      .map(|(m, s)| (m.to_vec(), s))
      .collect()
  }

  /// 弹出首个节点 (分值最小的节点)，O(1) 均摊复杂度，零成员克隆
  pub fn pop_first(&mut self) -> Option<(Vec<u8>, f64)> {
    if self.length == 0 {
      return None;
    }

    unsafe {
      let first = (&*self.header.as_ptr()).forward(0);
      if first.is_null() {
        return None;
      }
      let first_ref = &*first;

      for i in 0..self.level {
        let update_ref = &mut *self.header.as_ptr();
        if update_ref.forward(i) == first {
          update_ref.set_span(i, update_ref.span(i) + first_ref.span(i) - 1);
          update_ref.set_forward(i, first_ref.forward(i));
        } else {
          update_ref.set_span(i, update_ref.span(i) - 1);
        }
      }

      if !first_ref.forward(0).is_null() {
        (&mut *first_ref.forward(0)).backward = null_mut();
      } else {
        self.tail = null_mut();
      }

      while self.level > 1 && (&*self.header.as_ptr()).forward(self.level - 1).is_null() {
        self.level -= 1;
      }

      self.length -= 1;

      let boxed = Box::from_raw(first);
      Some((boxed.member, boxed.score))
    }
  }

  /// 弹出末尾节点 (分值最大的节点)，O(log N) 复杂度，零成员克隆
  pub fn pop_last(&mut self) -> Option<(Vec<u8>, f64)> {
    if self.length == 0 || self.tail.is_null() {
      return None;
    }
    let (score, member_ptr, member_len) = unsafe {
      let tail = &*self.tail;
      (tail.score, tail.member.as_ptr(), tail.member.len())
    };
    // SAFETY: 在 delete_entry 查找并解开 target 节点前，tail 节点及其 member 缓冲区在堆上保持有效且未修改
    let member_slice = unsafe { from_raw_parts(member_ptr, member_len) };
    self.delete_entry(score, member_slice)
  }

  /// 按字典序范围获取元素借用切片 (闭/开区间查询，ZRANGEBYLEX，零成员堆分配)
  pub fn range_by_lex_borrowed<'a>(
    &'a self,
    min: &LexBound,
    max: &LexBound,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(&'a [u8], f64)> {
    if self.length == 0 || count == 0 {
      return Vec::new();
    }

    let mut res = Vec::new();

    if !reverse {
      // O(log N) 导航到首个满足下界的节点，后续节点必然全部满足下界
      let Some(mut curr) = self.lower_bound(min) else {
        return Vec::new();
      };
      let mut skipped = 0;

      while !curr.is_null() {
        let member = unsafe { &(*curr).member };
        if !max.matches_max(member) {
          break;
        }
        if skipped < offset {
          skipped += 1;
        } else {
          let score = unsafe { (*curr).score };
          res.push((member.as_slice(), score));
          if res.len() >= count {
            break;
          }
        }
        curr = unsafe { (&*curr).forward(0) };
      }
    } else {
      // 从满足上界的末节点沿 backward 逆序遍历，仅需再校验下界
      let Some(mut curr) = self.upper_bound(max) else {
        return Vec::new();
      };
      let mut skipped = 0;

      while !curr.is_null() && curr != self.header.as_ptr() {
        let member = unsafe { &(*curr).member };
        if !min.matches_min(member) {
          break;
        }
        if skipped < offset {
          skipped += 1;
        } else {
          let score = unsafe { (*curr).score };
          res.push((member.as_slice(), score));
          if res.len() >= count {
            break;
          }
        }
        curr = unsafe { (*curr).backward };
      }
    }

    res
  }

  /// 按字典序范围获取元素 (闭/开区间查询，ZRANGEBYLEX)
  pub fn range_by_lex(
    &self,
    min: &LexBound,
    max: &LexBound,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Vec<(Vec<u8>, f64)> {
    self
      .range_by_lex_borrowed(min, max, reverse, offset, count)
      .into_iter()
      .map(|(m, s)| (m.to_vec(), s))
      .collect()
  }

  /// 利用高层索引 O(log N) 导航到首个满足字典序下界 (member ≥ min，开区间则 > min) 的节点
  ///
  /// 返回 `None` 表示区间恒空：min 为 "+" (正无穷下界) 时无任何成员可满足
  /// (对标 C# minValueInfinity == InfiniteMax 短路)
  fn lower_bound(&self, min: &LexBound) -> Option<*mut Node> {
    let curr = match min {
      // "-" 无下界：直接从头遍历
      LexBound::NegInf => self.header.as_ptr(),
      // 闭下界 [b：前进条件 member < b，停在首个 member ≥ b
      LexBound::Included(b) => self.nav_lex(b, true).0,
      // 开下界 (b：前进条件 member ≤ b，停在首个 member > b
      LexBound::Excluded(b) => self.nav_lex(b, false).0,
      LexBound::PosInf => return None,
    };
    let next = unsafe { (&*curr).forward(0) };
    if next.is_null() { None } else { Some(next) }
  }

  /// 利用高层索引 O(log N) 导航到最后一个满足字典序上界 (member ≤ max，开区间则 < max) 的节点
  ///
  /// 返回 `None` 表示区间恒空：max 为 "-" (负无穷上界)；PosInf → 尾节点
  fn upper_bound(&self, max: &LexBound) -> Option<*mut Node> {
    match max {
      LexBound::PosInf => {
        if self.tail.is_null() {
          None
        } else {
          Some(self.tail)
        }
      }
      LexBound::NegInf => None,
      // 闭上界 [b：前进条件 member ≤ b，停在末个 member ≤ b
      LexBound::Included(b) => {
        let (node, _) = self.nav_lex(b, false);
        if node == self.header.as_ptr() || node.is_null() {
          None
        } else {
          Some(node)
        }
      }
      // 开上界 (b：前进条件 member < b，停在末个 member < b
      LexBound::Excluded(b) => {
        let (node, _) = self.nav_lex(b, true);
        if node == self.header.as_ptr() || node.is_null() {
          None
        } else {
          Some(node)
        }
      }
    }
  }

  /// 字典序导航核心：基于高层索引 O(log N) 前进至边界处
  ///
  /// 前进条件：`lt_only` 为 true 时 `member < bound`，否则 `member <= bound`；
  /// 返回 (停留节点, 严格位于该位置之前的成员数，即停留节点的 0-based 排名)
  fn nav_lex(&self, bound: &[u8], lt_only: bool) -> (*mut Node, usize) {
    let mut curr = self.header.as_ptr();
    let mut rank = 0;
    for i in (0..self.level).rev() {
      unsafe {
        while !(&*curr).forward(i).is_null() {
          let next = (&*curr).forward(i);
          let m = (&*next).member.as_slice();
          if if lt_only { m < bound } else { m <= bound } {
            rank += (&*curr).span(i);
            curr = next;
          } else {
            break;
          }
        }
      }
    }
    (curr, rank)
  }

  /// 统计处于指定字典序区间内的成员数量 (ZLEXCOUNT)
  ///
  /// 基于跳表跨度 (span) 实现 O(log N) 排名相减计算：
  /// `count = 末个满足 max 上界的节点排名 - 严格位于 min 下界之前的成员数`
  pub fn count_by_lex(&self, min: &LexBound, max: &LexBound) -> usize {
    if self.length == 0 {
      return 0;
    }

    // 1. 严格位于下界之前的成员数；min = "+" 时区间恒空 (对标 C# InfiniteMax 短路)
    let (first_node, before_min) = match min {
      LexBound::NegInf => (unsafe { (&*self.header.as_ptr()).forward(0) }, 0),
      LexBound::PosInf => return 0,
      LexBound::Included(b) => {
        let (curr, r) = self.nav_lex(b, true);
        (unsafe { (&*curr).forward(0) }, r)
      }
      LexBound::Excluded(b) => {
        let (curr, r) = self.nav_lex(b, false);
        (unsafe { (&*curr).forward(0) }, r)
      }
    };

    // 首个满足 min 的节点必须仍落在 max 上界内，否则区间为空
    if first_node.is_null() || !max.matches_max(unsafe { &(*first_node).member }) {
      return 0;
    }

    // 2. 满足 max 上界的末节点 1-based 排名；max = "-" 恒空，"+" 时为全表长度
    let last_rank = match max {
      LexBound::NegInf => return 0,
      LexBound::PosInf => self.length,
      LexBound::Included(b) => self.nav_lex(b, false).1,
      LexBound::Excluded(b) => self.nav_lex(b, true).1,
    };

    last_rank.saturating_sub(before_min)
  }

  /// 统计处于指定分数区间内的成员数量 (O(log N) 跨度计算，零堆分配)
  pub fn count_by_score(&self, range: ScoreRange) -> usize {
    if self.length == 0 || !range.is_valid() {
      return 0;
    }

    // 1. 查找首个满足 min 边界条件的节点及其 1-based rank
    let mut curr = self.header.as_ptr();
    let mut first_rank = 0;
    for i in (0..self.level).rev() {
      unsafe {
        while !(&*curr).forward(i).is_null() {
          let next = (&*curr).forward(i);
          let next_ref = &*next;
          let condition = if range.min_inclusive {
            next_ref.score < range.min
          } else {
            next_ref.score <= range.min
          };
          if condition {
            first_rank += (&*curr).span(i);
            curr = next;
          } else {
            break;
          }
        }
      }
    }

    let first_node = unsafe { (&*curr).forward(0) };
    if first_node.is_null() {
      return 0;
    }
    let first_score = unsafe { (*first_node).score };
    let first_in_max = if range.max_inclusive {
      first_score <= range.max
    } else {
      first_score < range.max
    };
    if !first_in_max {
      return 0;
    }
    first_rank += 1;

    // 2. 查找最后一个满足 max 边界条件的节点及其 1-based rank
    let mut curr = self.header.as_ptr();
    let mut last_rank = 0;
    for i in (0..self.level).rev() {
      unsafe {
        while !(&*curr).forward(i).is_null() {
          let next = (&*curr).forward(i);
          let next_ref = &*next;
          let condition = if range.max_inclusive {
            next_ref.score <= range.max
          } else {
            next_ref.score < range.max
          };
          if condition {
            last_rank += (&*curr).span(i);
            curr = next;
          } else {
            break;
          }
        }
      }
    }

    if last_rank < first_rank {
      0
    } else {
      last_rank - first_rank + 1
    }
  }
}

impl Drop for SkipList {
  fn drop(&mut self) {
    let mut curr = unsafe { (&*self.header.as_ptr()).forward(0) };
    while !curr.is_null() {
      unsafe {
        let next = (&*curr).forward(0);
        drop(Box::from_raw(curr));
        curr = next;
      }
    }
    unsafe {
      drop(Box::from_raw(self.header.as_ptr()));
    }
  }
}

/// 跳表零克隆升序迭代器 (沿 level-0 链遍历)
pub struct SkipListIter<'a> {
  curr: *mut Node,
  remaining: usize,
  marker: PhantomData<&'a SkipList>,
}

impl<'a> Iterator for SkipListIter<'a> {
  type Item = (&'a [u8], f64);

  #[inline]
  fn next(&mut self) -> Option<Self::Item> {
    if self.remaining == 0 || self.curr.is_null() {
      return None;
    }
    let node = unsafe { &*self.curr };
    self.curr = node.forward(0);
    self.remaining -= 1;
    Some((node.member.as_slice(), node.score))
  }

  #[inline(always)]
  fn size_hint(&self) -> (usize, Option<usize>) {
    (self.remaining, Some(self.remaining))
  }
}

impl FusedIterator for SkipListIter<'_> {}

// 确保 SkipList 具备 Send 与 Sync
unsafe impl Send for SkipList {}
unsafe impl Sync for SkipList {}
