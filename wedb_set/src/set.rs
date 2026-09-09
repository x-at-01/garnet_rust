use core::{cmp::Reverse, iter::FusedIterator, mem, ptr};
use std::collections::hash_set;

use bitcode::{Decode, Encode};
use whasher::{HashSet, hash_set_with_capacity};
use wval::{glob_match, sample_distinct_indices};

use crate::error::{Error, Result};

/// 极简格式版本号（1 字节）
pub const FORMAT_VERSION: u8 = 1;

/// 允许重复采样的单次安全返回上限（防恶意大数内存耗尽与死循环）
pub const MAX_RAND_SAMPLE_LIMIT: usize = 1_000_000;

/// SSCAN 未指定 COUNT 时的默认每页条数（对齐 Redis 默认值 10）
const SSCAN_DEFAULT_COUNT: usize = 10;

/// 重复采样引用表的栈容量上限（超出平滑退化为堆分配）
const STACK_REFS_CAP: usize = 512;

/// 栈模式探针容量上限（超出平滑退化为堆分配）
const STACK_PROBES_CAP: usize = 16;

/// 探针集合容器（支持至多 16 个探针零堆分配，超出则平滑退化为堆分配）
///
/// 栈与堆两存储互斥：堆模式时栈区已清空，栈模式时堆区恒空。
struct Probes<'a> {
  stack: [Option<&'a SetObject>; STACK_PROBES_CAP],
  heap: Vec<&'a SetObject>,
  len: usize,
  is_heap: bool,
}

impl<'a> Probes<'a> {
  /// 按预估探针数创建容器（堆模式按预估值一次性预分配，栈模式堆区恒空）
  #[inline]
  fn new(capacity: usize) -> Self {
    let heap_cap = if capacity > STACK_PROBES_CAP {
      capacity
    } else {
      0
    };
    Self {
      stack: [None; STACK_PROBES_CAP],
      heap: Vec::with_capacity(heap_cap),
      len: 0,
      is_heap: capacity > STACK_PROBES_CAP,
    }
  }

  #[inline]
  fn push(&mut self, item: &'a SetObject) {
    if self.is_heap {
      self.heap.push(item);
    } else if self.len < STACK_PROBES_CAP {
      self.stack[self.len] = Some(item);
      self.len += 1;
    } else {
      // 栈区溢出：整体迁移至堆区并切换模式
      self.heap.reserve_exact(self.len + 1);
      self.heap.extend(self.stack.iter().flatten().copied());
      self.heap.push(item);
      self.stack = [None; STACK_PROBES_CAP];
      self.len = 0;
      self.is_heap = true;
    }
  }

  #[inline]
  fn is_empty(&self) -> bool {
    if self.is_heap {
      self.heap.is_empty()
    } else {
      self.len == 0
    }
  }

  /// 指针级去重判定（同一集合实例只登记一次）
  #[inline]
  fn contains_ptr(&self, target: &SetObject) -> bool {
    if self.is_heap {
      self.heap.iter().any(|p| ptr::eq(*p, target))
    } else {
      self.stack[..self.len]
        .iter()
        .flatten()
        .any(|p| ptr::eq(*p, target))
    }
  }

  /// 任一探针包含目标成员（差集排除判定，命中即短路）
  #[inline]
  fn any_contains(&self, member: &[u8]) -> bool {
    if self.is_heap {
      self.heap.iter().any(|p| p.members.contains(member))
    } else {
      self.stack[..self.len]
        .iter()
        .flatten()
        .any(|p| p.members.contains(member))
    }
  }

  /// 全部探针均包含目标成员（交集命中判定，缺失即短路）
  #[inline]
  fn all_contains(&self, member: &[u8]) -> bool {
    if self.is_heap {
      self.heap.iter().all(|p| p.members.contains(member))
    } else {
      self.stack[..self.len]
        .iter()
        .flatten()
        .all(|p| p.members.contains(member))
    }
  }

  /// 按集合基数升序排序（交集主遍历最短路优先）
  fn sort_by_len(&mut self) {
    if self.is_heap {
      self.heap.sort_unstable_by_key(|s| s.len());
    } else {
      // 栈模式前缀均为 Some，map_or 兜底分支不会命中
      self.stack[..self.len].sort_unstable_by_key(|s| s.map_or(0, SetObject::len));
    }
  }

  /// 按集合基数降序排序（差集探测大集合优先命中排除）
  fn sort_by_len_desc(&mut self) {
    if self.is_heap {
      self.heap.sort_unstable_by_key(|s| Reverse(s.len()));
    } else {
      self.stack[..self.len].sort_unstable_by_key(|s| s.map_or(Reverse(0), |v| Reverse(v.len())));
    }
  }
}

/// 集合对象 (对应 Redis Set / Garnet SetObject)
#[derive(Debug, Clone, Default, PartialEq, Eq, Encode, Decode)]
pub struct SetObject {
  /// 哈希集合存储唯一成员
  pub members: HashSet<Vec<u8>>,
}

impl SetObject {
  /// 创建空集合
  #[inline]
  pub fn new() -> Self {
    Self::default()
  }

  /// 创建指定容量的空集合
  #[inline]
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      members: hash_set_with_capacity(capacity),
    }
  }

  /// 获取集合成员数量 (SCARD)
  #[inline]
  pub fn len(&self) -> usize {
    self.members.len()
  }

  /// 判断集合是否为空
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.members.is_empty()
  }

  /// 获取底层哈希集合当前已分配容量
  #[inline]
  pub fn capacity(&self) -> usize {
    self.members.capacity()
  }

  /// 收缩底层哈希集合内存以释放冗余空间
  #[inline]
  pub fn shrink_to_fit(&mut self) {
    self.members.shrink_to_fit();
  }

  /// 清空集合并即刻物理收敛（释放哈希表底层所有内存）
  #[inline]
  pub fn clear(&mut self) {
    self.members = HashSet::default();
  }

  /// 零拷贝只读借用迭代器
  #[inline]
  pub fn iter(&self) -> Iter<'_> {
    Iter {
      inner: self.members.iter(),
    }
  }

  /// 零拷贝获取所有成员只读切片列表
  #[inline]
  pub fn members_ref(&self) -> Vec<&[u8]> {
    self.members.iter().map(|m| m.as_slice()).collect()
  }

  /// 向集合添加一个或多个成员 (SADD)
  ///
  /// 先行探测 `contains` 避免为已存在成员进行不必要的堆内存分配。
  /// 返回成功添加的新成员数量。
  pub fn sadd<T>(&mut self, items: impl IntoIterator<Item = T>) -> usize
  where
    T: Into<Vec<u8>> + AsRef<[u8]>,
  {
    let iter = items.into_iter();
    let (lower, _) = iter.size_hint();
    if lower > 0 {
      self.members.reserve(lower);
    }
    let mut added = 0;
    for item in iter {
      if self.members.contains(item.as_ref()) {
        continue;
      }
      self.members.insert(item.into());
      added += 1;
    }
    added
  }

  /// 从集合中移除一个或多个成员 (SREM)
  ///
  /// 返回实际被移除的成员数量。当集合清空时即刻物理收敛释放内存。
  pub fn srem(&mut self, items: &[&[u8]]) -> usize {
    if self.members.is_empty() {
      return 0;
    }
    let mut removed = 0;
    for &item in items {
      if self.members.remove(item) {
        removed += 1;
      }
    }
    if self.members.is_empty() {
      self.members = HashSet::default();
    }
    removed
  }

  /// 判断指定成员是否存在于集合中 (SISMEMBER)
  #[inline]
  pub fn sismember(&self, member: &[u8]) -> bool {
    self.members.contains(member)
  }

  /// 批量判断多个成员是否存在于集合中 (SMISMEMBER)
  pub fn smismember(&self, members: &[&[u8]]) -> Vec<bool> {
    if self.members.is_empty() {
      return vec![false; members.len()];
    }
    members.iter().map(|&m| self.members.contains(m)).collect()
  }

  /// 获取集合中的所有成员 (SMEMBERS)
  pub fn smembers(&self) -> Vec<Vec<u8>> {
    self.members.iter().cloned().collect()
  }

  /// 随机弹出并移除至多 count 个成员 (SPOP)
  ///
  /// - `count >= total`: 原位 `mem::take` 整表接管，零重复遍历且集合即刻物理收敛释放内存；
  /// - `0 < count < total`: 升序无重复抽样下标 + `extract_if` 单趟原位摘除，
  ///   零克隆、零指针强转、零中间集合，摘除后按剩余基数物理收敛。
  pub fn spop(&mut self, count: usize) -> Vec<Vec<u8>> {
    let total = self.members.len();
    if total == 0 || count == 0 {
      return Vec::new();
    }

    if count >= total {
      return mem::take(&mut self.members).into_iter().collect();
    }

    // 桶遍历序与迭代器序一致，抽样下标语义稳定；扫描越过全部抽样点后谓词恒假
    let picks = sample_distinct_indices(total, count);
    let (mut next, mut idx) = (0usize, 0usize);
    let mut popped = Vec::with_capacity(count);
    popped.extend(self.members.extract_if(|_| {
      let hit = next < picks.len() && idx == picks[next];
      idx += 1;
      if hit {
        next += 1;
      }
      hit
    }));

    if self.members.is_empty() {
      self.members = HashSet::default();
    } else if self.members.len() * 2 <= total {
      self.members.shrink_to_fit();
    }
    popped
  }

  /// 随机获取单个成员的只读切片借用 (SRANDMEMBER 单元素零拷贝)
  #[inline]
  pub fn random_member(&self) -> Option<&[u8]> {
    let total = self.members.len();
    if total == 0 {
      return None;
    }
    let idx = fastrand::usize(0..total);
    self.members.iter().nth(idx).map(|m| m.as_slice())
  }

  /// 随机获取至多 count 个互不相同的成员只读切片借用 (SRANDMEMBER 零拷贝切片版本)
  pub fn srandmember_ref(&self, count: usize) -> Vec<&[u8]> {
    let total = self.members.len();
    if total == 0 || count == 0 {
      return Vec::new();
    }
    let pick_count = count.min(total);
    if pick_count == total {
      return self.members.iter().map(|m| m.as_slice()).collect();
    }

    let sorted_indices = sample_distinct_indices(total, pick_count);
    let mut res = Vec::with_capacity(pick_count);
    let mut cur_pick = 0;
    for (i, item) in self.members.iter().enumerate() {
      if i == sorted_indices[cur_pick] {
        res.push(item.as_slice());
        cur_pick += 1;
        if cur_pick == pick_count {
          break;
        }
      }
    }
    res
  }

  /// 随机获取 count 个成员的只读切片借用（允许重复采样，零多余堆分配）
  pub fn srandmember_dup_ref(&self, count: usize) -> Vec<&[u8]> {
    let total = self.members.len();
    if total == 0 || count == 0 {
      return Vec::new();
    }
    let pick_count = count.min(MAX_RAND_SAMPLE_LIMIT);
    if total == 1 {
      // 安全性保证：total == 1 必存在唯一元素
      let elem = unsafe { self.members.iter().next().unwrap_unchecked() }.as_slice();
      return vec![elem; pick_count];
    }
    let mut stack_refs = [&[][..]; STACK_REFS_CAP];
    let heap_refs: Vec<&[u8]>;
    let member_refs: &[&[u8]] = if total <= STACK_REFS_CAP {
      for (slot, m) in stack_refs.iter_mut().zip(self.members.iter()) {
        *slot = m.as_slice();
      }
      &stack_refs[..total]
    } else {
      heap_refs = self.members.iter().map(|m| m.as_slice()).collect();
      &heap_refs
    };

    let mut res = Vec::with_capacity(pick_count);
    for _ in 0..pick_count {
      let idx = fastrand::usize(0..total);
      // 安全性保证：idx < total == member_refs.len()
      res.push(*unsafe { member_refs.get_unchecked(idx) });
    }
    res
  }

  /// 随机获取成员而不移除 (SRANDMEMBER)
  ///
  /// count > 0: 返回至多 count 个互不相同的成员
  /// count < 0: 返回 |count| 个成员，可能包含重复项
  pub fn srandmember(&self, count: isize) -> Vec<Vec<u8>> {
    if self.members.is_empty() || count == 0 {
      return Vec::new();
    }
    let refs = if count > 0 {
      self.srandmember_ref((count as usize).min(self.members.len()))
    } else {
      self.srandmember_dup_ref(count.unsigned_abs())
    };
    refs.into_iter().map(|s| s.to_vec()).collect()
  }

  /// 将成员从当前集合移动到目标集合 (SMOVE，零拷贝所有权转移，零多余堆分配)
  ///
  /// 若成员存在于源集合并成功移动返回 true；若源集合中不存在该成员返回 false。
  /// 若源集合与目标集合为同一实例，直接返回 false（对标 Garnet SetMove 语义）。
  /// 源集合被清空时即刻物理收敛释放底层内存。
  pub fn smove(&mut self, dest: &mut SetObject, member: &[u8]) -> bool {
    if ptr::eq(self, dest) {
      return false;
    }
    if let Some(owned_member) = self.members.take(member) {
      dest.members.insert(owned_member);
      if self.members.is_empty() {
        self.members = HashSet::default();
      }
      true
    } else {
      false
    }
  }

  /// 计算当前集合与其他集合的差集 (SDIFF)
  ///
  /// 自适应优化：
  /// 1. 若当前集合为空，或 others 中包含与当前集合相同的引用，直接返回空集合；
  /// 2. 过滤空集合，探针集合按基数降序排序，使判定以最高概率最快命中排除；
  /// 3. 小集合栈上零堆分配收集探针；
  /// 4. 结果为空时即刻物理收敛底层内存。
  pub fn diff(&self, others: &[&SetObject]) -> SetObject {
    if self.is_empty() || others.is_empty() {
      return self.clone();
    }
    if others.iter().any(|o| ptr::eq(*o, self)) {
      return SetObject::new();
    }

    let mut probes = Probes::new(others.len());
    for other in others {
      if !other.is_empty() && !probes.contains_ptr(other) {
        probes.push(other);
      }
    }
    if probes.is_empty() {
      return self.clone();
    }
    probes.sort_by_len_desc();

    let mut res = SetObject::with_capacity(self.len());
    for item in &self.members {
      if !probes.any_contains(item) {
        res.members.insert(item.clone());
      }
    }
    if res.is_empty() {
      res.members = HashSet::default();
    } else if res.len() * 2 < self.len() {
      res.members.shrink_to_fit();
    }
    res
  }

  /// 选取 `self` 与 `others` 中基数最小的集合作为主遍历集（SINTER/SINTERCARD 共用）
  ///
  /// 返回 `(主集, 探针容器)`：探针为其余参与集合（指针去重并排除主集自身），按基数升序排序以最快短路。
  fn min_set_probes<'a, 'b>(&'b self, others: &[&'a SetObject]) -> (&'a SetObject, Probes<'a>)
  where
    'b: 'a,
  {
    let mut min_set: &SetObject = self;
    for other in others {
      if other.len() < min_set.len() {
        min_set = other;
      }
    }

    let mut probes = Probes::new(1 + others.len());
    if !ptr::eq(self, min_set) {
      probes.push(self);
    }
    for other in others {
      if !ptr::eq(*other, min_set) && !probes.contains_ptr(other) {
        probes.push(other);
      }
    }
    probes.sort_by_len();
    (min_set, probes)
  }

  /// 计算当前集合与其他集合的交集 (SINTER)
  ///
  /// 性能自适应：
  /// 1. 遇到任一空集合立即短路退出，返回空集合；
  /// 2. 选取最小基数集合作为主遍历集（探针按基数升序、指针去重，栈优先零堆分配）；
  /// 3. 单次遍历主集判定全探针命中；结果为空时即刻物理收敛底层内存。
  pub fn inter(&self, others: &[&SetObject]) -> SetObject {
    if self.is_empty() || others.iter().any(|o| o.is_empty()) {
      return SetObject::new();
    }
    if others.is_empty() {
      return self.clone();
    }

    let (min_set, probes) = self.min_set_probes(others);
    if probes.is_empty() {
      return min_set.clone();
    }

    let mut res = SetObject::with_capacity(min_set.len());
    for item in &min_set.members {
      if probes.all_contains(item) {
        res.members.insert(item.clone());
      }
    }
    if res.is_empty() {
      res.members = HashSet::default();
    } else if res.len() * 2 < min_set.len() {
      res.members.shrink_to_fit();
    }
    res
  }

  /// 计算当前集合与其他集合的交集基数 (SINTERCARD)
  ///
  /// 若 limit 为 0，表示不设上限，计算完整交集基数；
  /// 若 limit > 0，当交集基数达到 limit 时提前终止计算，实现零额外堆分配的高效短路统计。
  pub fn intercard(&self, others: &[&SetObject], limit: usize) -> usize {
    if self.is_empty() || others.iter().any(|o| o.is_empty()) {
      return 0;
    }
    if others.is_empty() {
      // limit == 0 表示不设上限，返回完整基数
      return if limit > 0 {
        self.len().min(limit)
      } else {
        self.len()
      };
    }

    let (min_set, probes) = self.min_set_probes(others);
    if probes.is_empty() {
      // limit == 0 表示不设上限，返回完整基数
      return if limit > 0 {
        min_set.len().min(limit)
      } else {
        min_set.len()
      };
    }

    let mut count = 0;
    for item in &min_set.members {
      if probes.all_contains(item) {
        count += 1;
        if limit > 0 && count >= limit {
          return count;
        }
      }
    }
    count
  }

  /// 计算当前集合与其他集合的并集 (SUNION)
  ///
  /// 自适应基数优化：优先以最大基数集合为主体克隆，其余集合增量插入去重，
  /// 显著减少哈希探测与内存重分配开销。
  /// 空间复杂度严格 O(结果基数)：依赖哈希表摊还在线增长，
  /// 刻意不做按输入总量的一次性预 reserve，避免多键高度重叠时内存放大。
  pub fn union(&self, others: &[&SetObject]) -> SetObject {
    if others.is_empty() {
      return self.clone();
    }

    let mut max_set = self;
    for other in others {
      if other.len() > max_set.len() {
        max_set = other;
      }
    }

    let mut res = max_set.clone();

    if !ptr::eq(self, max_set) {
      for item in &self.members {
        if !res.members.contains(item) {
          res.members.insert(item.clone());
        }
      }
    }

    for other in others {
      if !ptr::eq(*other, max_set) {
        for item in &other.members {
          if !res.members.contains(item) {
            res.members.insert(item.clone());
          }
        }
      }
    }

    res
  }

  /// 计算交集并直接存储至当前集合 (SINTERSTORE)
  ///
  /// 返回存储后的集合基数。若结果集为空，立即物理收敛释放内存。
  pub fn inter_store(&mut self, src: &SetObject, others: &[&SetObject]) -> usize {
    let result = src.inter(others);
    *self = result;
    self.len()
  }

  /// 就地计算交集并覆盖当前集合 (SINTERSTORE 目标键与源键相同)
  ///
  /// 遇到空集合立即短路清空；当 self 基数较小或相等时原地 `retain` 零分配过滤，
  /// 当存在基数显著更小的集合时自适应切换为小集合驱动并替换。
  pub fn inter_store_in_place(&mut self, others: &[&SetObject]) -> usize {
    if self.is_empty() || others.iter().any(|o| o.is_empty()) {
      self.clear();
      return 0;
    }
    if others.is_empty() {
      return self.len();
    }

    let mut min_other = others[0];
    for other in &others[1..] {
      if other.len() < min_other.len() {
        min_other = other;
      }
    }

    // 若 self 就是最小集合或基数相当，直接原地 retain，零堆分配
    if self.len() <= min_other.len() * 2 {
      let mut probes = Probes::new(others.len());
      for other in others {
        if !ptr::eq(*other, self) && !probes.contains_ptr(other) {
          probes.push(other);
        }
      }
      probes.sort_by_len();
      let initial_len = self.members.len();
      self.members.retain(|item| probes.all_contains(item));
      if self.members.is_empty() {
        self.members = HashSet::default();
      } else if self.members.len() * 2 < initial_len {
        self.members.shrink_to_fit();
      }
      self.members.len()
    } else {
      // 若存在显著更小的 min_other，基于 min_other 构建新集合以最小化探测次数
      let mut probes = Probes::new(others.len());
      for other in others {
        if !ptr::eq(*other, min_other) && !ptr::eq(*other, self) && !probes.contains_ptr(other) {
          probes.push(other);
        }
      }
      probes.sort_by_len();
      let mut res = SetObject::with_capacity(min_other.len());
      for item in &min_other.members {
        if self.members.contains(item) && probes.all_contains(item) {
          res.members.insert(item.clone());
        }
      }
      if res.is_empty() {
        self.clear();
      } else {
        *self = res;
      }
      self.len()
    }
  }

  /// 计算并集并直接存储至当前集合 (SUNIONSTORE)
  ///
  /// 返回存储后的集合基数。若结果集为空，立即物理收敛释放内存。
  pub fn union_store(&mut self, src: &SetObject, others: &[&SetObject]) -> usize {
    let result = src.union(others);
    *self = result;
    self.len()
  }

  /// 就地计算并集并覆盖当前集合 (SUNIONSTORE 目标键与源键相同)
  ///
  /// 原位增量插入去重，避免重复克隆当前集合元素，大幅降低哈希探测与分配开销。
  /// 空间复杂度严格 O(结果基数)：依赖哈希表摊还在线增长，避免高度重叠时的预 reserve 内存放大。
  pub fn union_store_in_place(&mut self, others: &[&SetObject]) -> usize {
    if others.is_empty() {
      return self.len();
    }

    for other in others {
      if ptr::eq(*other, self) {
        continue;
      }
      for item in &other.members {
        if !self.members.contains(item) {
          self.members.insert(item.clone());
        }
      }
    }

    self.len()
  }

  /// 计算差集并直接存储至当前集合 (SDIFFSTORE)
  ///
  /// 返回存储后的集合基数。若结果集为空，立即物理收敛释放内存。
  pub fn diff_store(&mut self, src: &SetObject, others: &[&SetObject]) -> usize {
    let result = src.diff(others);
    *self = result;
    self.len()
  }

  /// 就地计算差集并覆盖当前集合 (SDIFFSTORE 目标键与源键相同)
  ///
  /// 采用原地 `retain` 过滤，零克隆、零多余堆分配，当集合清空时即刻物理收敛。
  pub fn diff_store_in_place(&mut self, others: &[&SetObject]) -> usize {
    if self.is_empty() {
      return 0;
    }
    if others.is_empty() {
      return self.len();
    }
    // 自别名检查：若 others 中包含 self，差集必为空集，直接物理释放清空
    if others.iter().any(|o| ptr::eq(*o, self)) {
      self.clear();
      return 0;
    }

    let mut probes = Probes::new(others.len());
    for other in others {
      if !other.is_empty() && !probes.contains_ptr(other) {
        probes.push(other);
      }
    }
    if probes.is_empty() {
      return self.len();
    }
    probes.sort_by_len_desc();

    let initial_len = self.members.len();
    self.members.retain(|item| !probes.any_contains(item));

    if self.members.is_empty() {
      self.members = HashSet::default();
    } else if self.members.len() * 2 < initial_len {
      self.members.shrink_to_fit();
    }
    self.members.len()
  }

  /// 游标扫描 (SSCAN) 零拷贝切片版本
  pub fn sscan_ref(
    &self,
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> (usize, Vec<&[u8]>) {
    let total = self.members.len();
    if cursor >= total {
      return (0, Vec::new());
    }

    let limit = if count == 0 {
      SSCAN_DEFAULT_COUNT
    } else {
      count
    };
    let mut items = Vec::with_capacity(limit.min(total - cursor));
    let mut next_cursor = cursor;

    for m in self.members.iter().skip(cursor) {
      next_cursor += 1;
      let matches = match pattern {
        Some(p) => glob_match(p, m),
        None => true,
      };
      if matches {
        items.push(m.as_slice());
        if items.len() >= limit {
          break;
        }
      }
    }

    if next_cursor >= total {
      next_cursor = 0;
    }
    (next_cursor, items)
  }

  /// 游标扫描 (SSCAN)
  pub fn sscan(
    &self,
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> (usize, Vec<Vec<u8>>) {
    let (next_cur, refs) = self.sscan_ref(cursor, count, pattern);
    (next_cur, refs.into_iter().map(|m| m.to_vec()).collect())
  }

  /// 极简可扩展二进制序列化
  ///
  /// 格式（极简主义，零多余，零幻数）：
  /// - version: u8 (当前 FORMAT_VERSION = 1)
  /// - count: u32 (小端)
  /// - 遍历每个成员：
  ///   - item_len: u32 (小端)
  ///   - item 字节切片
  pub fn serialize(&self, buf: &mut Vec<u8>) {
    let payload_len: usize = self.members.iter().map(|m| 4 + m.len()).sum();
    buf.reserve(1 + 4 + payload_len);

    buf.push(FORMAT_VERSION);
    debug_assert!(self.members.len() <= u32::MAX as usize);
    let count = self.members.len() as u32;
    buf.extend_from_slice(&count.to_le_bytes());

    for item in &self.members {
      let item_len = item.len() as u32;
      buf.extend_from_slice(&item_len.to_le_bytes());
      buf.extend_from_slice(item);
    }
  }

  /// 极简二进制序列化输出为新建 Vec
  #[inline]
  pub fn to_vec(&self) -> Vec<u8> {
    let mut buf = Vec::new();
    self.serialize(&mut buf);
    buf
  }

  /// bitcode 极速编码（Rust 专用紧凑格式，不与 C# 序列化互通）
  #[inline]
  pub fn encode_bitcode(&self) -> Vec<u8> {
    bitcode::encode(self)
  }

  /// bitcode 极速解码（损坏/恶意输入一律返回 Err，内部分配规模受输入长度严格约束，无 panic/OOM 风险）
  #[inline]
  pub fn decode_bitcode(bytes: &[u8]) -> Result<Self> {
    bitcode::decode(bytes).map_err(Error::from)
  }

  /// 极简可扩展二进制反序列化（单次切片划分，零多余边界计算）
  pub fn deserialize(buf: &[u8]) -> Result<Self> {
    let (header, mut rest) = buf.split_first_chunk::<5>().ok_or(Error::BufferTooShort)?;

    let version = header[0];
    if version != FORMAT_VERSION {
      return Err(Error::UnsupportedVersion(version));
    }

    let count = u32::from_le_bytes([header[1], header[2], header[3], header[4]]);
    let max_possible = rest.len() / 4;
    if (count as usize) > max_possible {
      return Err(Error::CorruptedData);
    }
    let mut set = Self::with_capacity(count as usize);

    for _ in 0..count {
      let (len_bytes, payload) = rest.split_first_chunk::<4>().ok_or(Error::BufferTooShort)?;
      let item_len = u32::from_le_bytes(*len_bytes) as usize;
      if payload.len() < item_len {
        return Err(Error::BufferTooShort);
      }
      let (item, remain) = payload.split_at(item_len);
      set.members.insert(item.to_vec());
      rest = remain;
    }

    if !rest.is_empty() {
      return Err(Error::CorruptedData);
    }

    Ok(set)
  }
}

/// 集合对象只读借用迭代器
pub struct Iter<'a> {
  inner: hash_set::Iter<'a, Vec<u8>>,
}

impl<'a> Iterator for Iter<'a> {
  type Item = &'a [u8];

  #[inline]
  fn next(&mut self) -> Option<Self::Item> {
    self.inner.next().map(|m| m.as_slice())
  }

  #[inline]
  fn size_hint(&self) -> (usize, Option<usize>) {
    self.inner.size_hint()
  }
}

impl ExactSizeIterator for Iter<'_> {
  #[inline]
  fn len(&self) -> usize {
    self.inner.len()
  }
}

impl FusedIterator for Iter<'_> {}

/// 集合对象所有权消费迭代器
pub struct IntoIter {
  inner: hash_set::IntoIter<Vec<u8>>,
}

impl Iterator for IntoIter {
  type Item = Vec<u8>;

  #[inline]
  fn next(&mut self) -> Option<Self::Item> {
    self.inner.next()
  }

  #[inline]
  fn size_hint(&self) -> (usize, Option<usize>) {
    self.inner.size_hint()
  }
}

impl ExactSizeIterator for IntoIter {
  #[inline]
  fn len(&self) -> usize {
    self.inner.len()
  }
}

impl FusedIterator for IntoIter {}

impl<'a> IntoIterator for &'a SetObject {
  type Item = &'a [u8];
  type IntoIter = Iter<'a>;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    self.iter()
  }
}

impl IntoIterator for SetObject {
  type Item = Vec<u8>;
  type IntoIter = IntoIter;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    IntoIter {
      inner: self.members.into_iter(),
    }
  }
}

impl<T: Into<Vec<u8>>> FromIterator<T> for SetObject {
  fn from_iter<I: IntoIterator<Item = T>>(iter: I) -> Self {
    let iter = iter.into_iter();
    let (lower, _) = iter.size_hint();
    let mut set = Self::with_capacity(lower);
    for item in iter {
      set.members.insert(item.into());
    }
    set
  }
}
