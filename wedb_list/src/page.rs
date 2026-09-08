use std::collections::{
  VecDeque,
  vec_deque::{IntoIter, Iter, IterMut},
};

use bitcode::{Decode, Encode};

/// 默认分页容量
pub const DEFAULT_PAGE_CAPACITY: usize = 64;

/// 分块分页节点 (LinkedPage)
///
/// 具备固定容量上限的环形分页缓冲区，支持头尾 O(1) 推入弹出、局部索引访问、分页分裂与合并。
/// 面向 QuickList 分块双向链表的分页节点存储。
#[derive(Debug, Clone, PartialEq, Eq, Encode, Decode)]
pub struct LinkedPage<T, const CAP: usize = DEFAULT_PAGE_CAPACITY> {
  /// 底层环形缓冲区
  items: VecDeque<T>,
}

impl<T, const CAP: usize> LinkedPage<T, CAP> {
  /// 创建空分页
  #[inline]
  pub fn new() -> Self {
    Self {
      items: VecDeque::with_capacity(CAP),
    }
  }

  /// 获取分页最大容量上限
  #[inline]
  pub const fn capacity(&self) -> usize {
    CAP
  }

  /// 获取分页当前元素数量
  #[inline]
  pub fn len(&self) -> usize {
    self.items.len()
  }

  /// 判断分页是否为空
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.items.is_empty()
  }

  /// 判断分页是否已满
  #[inline]
  pub fn is_full(&self) -> bool {
    self.items.len() >= CAP
  }

  /// 获取分页剩余可用容量
  #[inline]
  pub fn remaining_capacity(&self) -> usize {
    CAP.saturating_sub(self.items.len())
  }

  /// 向头部推入元素，若分页已满返回 Err(val)
  pub fn push_front(&mut self, val: T) -> Result<(), T> {
    if self.is_full() {
      Err(val)
    } else {
      self.items.push_front(val);
      Ok(())
    }
  }

  /// 向尾部推入元素，若分页已满返回 Err(val)
  pub fn push_back(&mut self, val: T) -> Result<(), T> {
    if self.is_full() {
      Err(val)
    } else {
      self.items.push_back(val);
      Ok(())
    }
  }

  /// 从头部弹出一个元素
  #[inline]
  pub fn pop_front(&mut self) -> Option<T> {
    self.items.pop_front()
  }

  /// 从尾部弹出一个元素
  #[inline]
  pub fn pop_back(&mut self) -> Option<T> {
    self.items.pop_back()
  }

  /// 获取页首元素引用
  #[inline]
  pub fn front(&self) -> Option<&T> {
    self.items.front()
  }

  /// 获取页尾元素引用
  #[inline]
  pub fn back(&self) -> Option<&T> {
    self.items.back()
  }

  /// 按分页局部索引读取元素
  #[inline]
  pub fn get(&self, index: usize) -> Option<&T> {
    self.items.get(index)
  }

  /// 按分页局部索引读取可变引用
  #[inline]
  pub fn get_mut(&mut self, index: usize) -> Option<&mut T> {
    self.items.get_mut(index)
  }

  /// 在页内指定局部索引处插入元素 (搬移限定在单页内)
  #[inline]
  pub fn insert(&mut self, index: usize, val: T) {
    self.items.insert(index, val);
  }

  /// 移除并返回页内指定局部索引处的元素
  #[inline]
  pub fn remove(&mut self, index: usize) -> Option<T> {
    self.items.remove(index)
  }

  /// 就地过滤页内元素 (保持相对顺序)
  #[inline]
  pub fn retain(&mut self, f: impl FnMut(&T) -> bool) {
    self.items.retain(f);
  }

  /// 丢弃页首 count 个元素
  #[inline]
  pub fn drain_front(&mut self, count: usize) {
    drop(self.items.drain(..count.min(self.items.len())));
  }

  /// 丢弃页尾 count 个元素
  #[inline]
  pub fn drain_back(&mut self, count: usize) {
    let start = self.items.len().saturating_sub(count);
    drop(self.items.drain(start..));
  }

  /// 压缩页内缓冲区至刚好容纳现有元素
  #[inline]
  pub fn shrink_to_fit(&mut self) {
    self.items.shrink_to_fit();
  }

  /// 分裂当前分页为前后两半，后半部分构成新分页返回
  pub fn split(&mut self) -> Self {
    let mid = self.items.len() / 2;
    let mut right_items = VecDeque::with_capacity(CAP);
    right_items.extend(self.items.drain(mid..));
    Self { items: right_items }
  }

  /// 尝试合并另一个分页（若合并后元素总数不超过容量上限）
  pub fn try_merge(&mut self, mut other: Self) -> Result<(), Self> {
    if self.len() + other.len() <= CAP {
      self.items.reserve(other.len());
      self.items.extend(other.items.drain(..));
      Ok(())
    } else {
      Err(other)
    }
  }

  /// 获取分页元素的双向引用迭代器
  #[inline]
  pub fn iter(&self) -> impl DoubleEndedIterator<Item = &T> + ExactSizeIterator {
    self.items.iter()
  }

  /// 获取分页元素的可变双向引用迭代器
  #[inline]
  pub fn iter_mut(&mut self) -> impl DoubleEndedIterator<Item = &mut T> + ExactSizeIterator {
    self.items.iter_mut()
  }

  /// 清空分页并保留容量
  #[inline]
  pub fn clear(&mut self) {
    self.items.clear();
  }
}

impl<T, const CAP: usize> Default for LinkedPage<T, CAP> {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(test)]
impl<T, const CAP: usize> LinkedPage<T, CAP> {
  /// 仅测试用: 无视容量上限构造页, 用于校验反序列化对恶意数据的防御逻辑
  pub(crate) fn from_items_unchecked(items: VecDeque<T>) -> Self {
    Self { items }
  }
}

impl<T, const CAP: usize> IntoIterator for LinkedPage<T, CAP> {
  type Item = T;
  type IntoIter = IntoIter<T>;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    self.items.into_iter()
  }
}

impl<'a, T, const CAP: usize> IntoIterator for &'a LinkedPage<T, CAP> {
  type Item = &'a T;
  type IntoIter = Iter<'a, T>;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    self.items.iter()
  }
}

impl<'a, T, const CAP: usize> IntoIterator for &'a mut LinkedPage<T, CAP> {
  type Item = &'a mut T;
  type IntoIter = IterMut<'a, T>;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    self.items.iter_mut()
  }
}
