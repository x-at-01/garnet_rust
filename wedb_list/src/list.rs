use std::{
  collections::VecDeque,
  mem::{size_of, take},
  ops::RangeInclusive,
  ptr,
};

use bitcode::{Decode, Encode};

use crate::{
  error::{Error, Result},
  page::{DEFAULT_PAGE_CAPACITY, LinkedPage},
};

/// 列表分页别名 (每页容量上限为 [`DEFAULT_PAGE_CAPACITY`])
type Page = LinkedPage<Vec<u8>>;

/// 列表插入方向 (LINSERT 命令参数)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Encode, Decode)]
pub enum InsertPosition {
  /// 插入到目标元素之前
  Before,
  /// 插入到目标元素之后
  After,
}

/// 列表对象 (对标 Redis List / Garnet ListObject)
///
/// quicklist 式分页存储 (结构性重设计): 元素保存在定容 [`LinkedPage`] 分页中,
/// 页目录为 [`VecDeque`] 以支持页级双端 O(1) 推入弹出。
///
/// - LPUSH/RPUSH/LPOP/RPOP: O(1) 均摊 (端页满则开新页, 页空即时回收)
/// - LINDEX/LSET: O(P) 页扫描 (P = 页数, 就近端扫描)
/// - LINSERT/LREM: 元素搬移限定在单页容量内, 免除 C# LinkedList 的逐节点堆分配
///
/// 不变量: 页目录中不存在空页; `len` 恒等于所有页长之和; 列表变空时页目录随之释放。
#[derive(Debug, Clone, Default, Eq, Encode, Decode)]
pub struct ListObject {
  /// 页目录 (每页至多 DEFAULT_PAGE_CAPACITY 个元素)
  pages: VecDeque<Page>,
  /// 元素总数缓存 (保证 LLEN O(1))
  len: usize,
}

/// 页容量不得超过 u64 位宽 (LREM 反向删除的页内位图依赖此约束)
const _: () = assert!(DEFAULT_PAGE_CAPACITY <= u64::BITS as usize);

impl PartialEq for ListObject {
  /// 按元素内容比较 (与页划分无关)
  fn eq(&self, other: &Self) -> bool {
    self.len == other.len && self.iter().eq(other.iter())
  }
}

impl ListObject {
  /// 创建空列表
  #[inline]
  pub fn new() -> Self {
    Self::default()
  }

  /// 创建空列表并预留页目录容量 (每 DEFAULT_PAGE_CAPACITY 个元素一页)
  #[inline]
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      pages: VecDeque::with_capacity(capacity.div_ceil(DEFAULT_PAGE_CAPACITY)),
      len: 0,
    }
  }

  /// 获取列表长度 (LLEN)
  #[inline]
  pub fn len(&self) -> usize {
    self.len
  }

  /// 判断列表是否为空
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// 获取当前已分配的总槽位容量 (页数 × 页容量)
  #[inline]
  pub fn capacity(&self) -> usize {
    self.pages.len() * DEFAULT_PAGE_CAPACITY
  }

  /// 估算列表对象占用的总堆内存大小（字节数）
  pub fn byte_size(&self) -> usize {
    // 页目录环形缓冲 + 各页槽位 + 各元素自身堆载荷
    let dir_bytes = self.pages.capacity() * size_of::<Page>();
    let slot_bytes = self.pages.len() * DEFAULT_PAGE_CAPACITY * size_of::<Vec<u8>>();
    let payload_bytes: usize = self
      .pages
      .iter()
      .map(|page| page.iter().map(Vec::capacity).sum::<usize>())
      .sum();
    size_of::<Self>() + dir_bytes + slot_bytes + payload_bytes
  }

  /// 物理清理：若列表为空，主动释放页目录与全部分页
  #[inline]
  pub fn clear_physical(&mut self) {
    if self.len == 0 {
      self.pages = VecDeque::new();
    }
  }

  /// 清空列表并完全释放底层堆内存
  #[inline]
  pub fn clear(&mut self) {
    self.pages = VecDeque::new();
    self.len = 0;
  }

  /// 压缩页目录与各页缓冲区至刚好容纳现有元素
  pub fn shrink_to_fit(&mut self) {
    self.pages.shrink_to_fit();
    for page in self.pages.iter_mut() {
      page.shrink_to_fit();
    }
  }

  /// 将负数或正数索引规整化为 0..len 的有效绝对索引
  #[inline]
  fn normalize_index(&self, index: isize) -> Option<usize> {
    let len = self.len as isize;
    if len == 0 {
      return None;
    }
    let absolute = if index >= 0 {
      index
    } else {
      len.checked_add(index)?
    };
    if absolute >= 0 && absolute < len {
      Some(absolute as usize)
    } else {
      None
    }
  }

  /// 规整化闭区间范围 [start, stop] 为合法的绝对索引区间
  ///
  /// 遵循 Redis/Garnet 语义：负 start 越界钳制到 0；负 stop 越界（len+stop < 0）
  /// 保持负值不做钳制，由 start > stop 判空（对应 C# 端 `end < 0` 与 `start > end` 判定）
  #[inline]
  fn normalize_range(&self, start: isize, stop: isize) -> Option<RangeInclusive<usize>> {
    let len = self.len as isize;
    if len == 0 {
      return None;
    }

    let actual_start = if start < 0 {
      len.saturating_add(start).max(0)
    } else {
      start
    };
    let actual_stop = if stop < 0 {
      len.saturating_add(stop)
    } else {
      stop.min(len - 1)
    };

    if actual_start > actual_stop {
      return None;
    }

    Some(actual_start as usize..=actual_stop as usize)
  }

  /// 定位全局索引所在的 (页下标, 页内偏移)，从更近一端扫描
  fn locate(&self, idx: usize) -> Option<(usize, usize)> {
    if idx >= self.len {
      return None;
    }
    if idx * 2 <= self.len {
      let mut acc = 0;
      for (pi, page) in self.pages.iter().enumerate() {
        let plen = page.len();
        if idx < acc + plen {
          return Some((pi, idx - acc));
        }
        acc += plen;
      }
    } else {
      let mut acc = self.len;
      for (pi, page) in self.pages.iter().enumerate().rev() {
        acc -= page.len();
        if idx >= acc {
          return Some((pi, idx - acc));
        }
      }
    }
    // 不变量被破坏时优雅降级 (理论上不可达)
    None
  }

  /// 页级头部推入 (O(1) 均摊; 头页满则开新页)
  fn push_front_inner(&mut self, val: Vec<u8>) {
    match self.pages.front_mut() {
      Some(page) if page.remaining_capacity() > 0 => {
        let _ = page.push_front(val); // 剩余容量 > 0, 必然成功
      }
      _ => {
        let mut page = Page::new();
        let _ = page.push_front(val); // 空页必然成功
        self.pages.push_front(page);
      }
    }
    self.len += 1;
  }

  /// 页级尾部推入 (O(1) 均摊; 尾页满则开新页)
  fn push_back_inner(&mut self, val: Vec<u8>) {
    match self.pages.back_mut() {
      Some(page) if page.remaining_capacity() > 0 => {
        let _ = page.push_back(val); // 剩余容量 > 0, 必然成功
      }
      _ => {
        let mut page = Page::new();
        let _ = page.push_back(val); // 空页必然成功
        self.pages.push_back(page);
      }
    }
    self.len += 1;
  }

  /// 页级头部弹出 (O(1); 页空即时回收)
  ///
  /// 列表弹空时显式重置页目录, 连同环形缓冲区分配一并释放 (byte_size 彻底归零)
  fn pop_front_inner(&mut self) -> Option<Vec<u8>> {
    let val = self.pages.front_mut()?.pop_front()?;
    self.len -= 1;
    if self.pages.front().is_some_and(Page::is_empty) {
      self.pages.pop_front();
    }
    if self.len == 0 {
      self.pages = VecDeque::new();
    }
    Some(val)
  }

  /// 页级尾部弹出 (O(1); 页空即时回收; 弹空时页目录连同缓冲区一并释放)
  fn pop_back_inner(&mut self) -> Option<Vec<u8>> {
    let val = self.pages.back_mut()?.pop_back()?;
    self.len -= 1;
    if self.pages.back().is_some_and(Page::is_empty) {
      self.pages.pop_back();
    }
    if self.len == 0 {
      self.pages = VecDeque::new();
    }
    Some(val)
  }

  /// 删除指定全局索引处的元素 (搬移限定在单页内; 页空即时回收; 删空时彻底释放页目录)
  fn remove_at(&mut self, idx: usize) -> Option<Vec<u8>> {
    let (pi, local) = self.locate(idx)?;
    let val = self.pages.get_mut(pi)?.remove(local)?;
    self.len -= 1;
    if self.pages.get(pi).is_some_and(Page::is_empty) {
      self.pages.remove(pi);
    }
    if self.len == 0 {
      self.pages = VecDeque::new();
    }
    Some(val)
  }

  /// 在页 (pi, local) 处插入元素; 满页分裂为前后两半 (对应 C# AddBefore/AddAfter)
  ///
  /// 前置约束: `local <= pages[pi].len()` 且 `pi` 为有效页下标
  fn insert_into_page(&mut self, pi: usize, local: usize, val: Vec<u8>) {
    let page = &mut self.pages[pi];
    if !page.is_full() {
      page.insert(local, val);
    } else {
      // 满页分裂: 前半保留 [0..half), 后半迁移至新页, 新值落入正确的一半
      // half 与 LinkedPage::split 的中点同源 (len/2), 消除两处独立常量的隐性耦合
      let half = page.len() / 2;
      let mut right = page.split();
      if local < half {
        page.insert(local, val);
      } else {
        right.insert(local - half, val);
      }
      self.pages.insert(pi + 1, right);
    }
    self.len += 1;
  }

  /// 页回收: 剔除空页, 并合并合计不超过半容量的相邻稀疏页 (防止大删除后页碎片化)
  ///
  /// 单趟重建页目录: 每页仅尝试并入前一保留页, 合并无需搬移目录中已就位的页,
  /// 时间 O(P) (P = 页数), 元素搬移总量 O(合并页元素数) 有界。
  fn reclaim(&mut self) {
    let old = take(&mut self.pages);
    let half = DEFAULT_PAGE_CAPACITY / 2;
    let mut merged: VecDeque<Page> = VecDeque::with_capacity(old.len());
    for page in old {
      // 页空即时剔除 (页内 retain 全删后遗留; 列表变空时页目录随之清空)
      if page.is_empty() {
        continue;
      }
      match merged.back_mut() {
        // 与前一保留页合并 (合计不超过半容量才合并)
        Some(head) if head.len() + page.len() <= half => {
          if let Err(page) = head.try_merge(page) {
            merged.push_back(page); // 容量上限兜底 (合计 <= half <= CAP, 理论不可达)
          }
        }
        _ => merged.push_back(page),
      }
    }
    self.pages = merged;
    // 全删时重置页目录, 连同环形缓冲区分配一并释放 (byte_size 彻底归零)
    if self.len == 0 {
      self.pages = VecDeque::new();
    }
  }

  /// 构造全局切片 [start, end) 的双向迭代器
  fn slices(&self, start: usize, end: usize) -> Slices<'_> {
    if start >= end {
      return Slices {
        pages: &self.pages,
        front: (0, 0),
        back: (0, 0),
        remaining: 0,
      };
    }
    let front = self.locate(start).unwrap_or((0, 0));
    let back = self.locate(end - 1).unwrap_or((0, 0));
    Slices {
      pages: &self.pages,
      front,
      back,
      remaining: end - start,
    }
  }

  /// 从左侧 (头部) 推入一个或多个元素 (LPUSH)
  ///
  /// 按照 Redis 语义，元素依次从头部推入，后推入的元素位于更靠前的位置。
  /// 返回操作后列表的总长度。
  pub fn lpush(&mut self, values: impl IntoIterator<Item = impl Into<Vec<u8>>>) -> usize {
    for val in values {
      self.push_front_inner(val.into());
    }
    self.len
  }

  /// 从右侧 (尾部) 推入一个或多个元素 (RPUSH)
  ///
  /// 返回操作后列表的总长度。
  pub fn rpush(&mut self, values: impl IntoIterator<Item = impl Into<Vec<u8>>>) -> usize {
    for val in values {
      self.push_back_inner(val.into());
    }
    self.len
  }

  /// 仅当列表非空时从头部推入元素 (LPUSHX)
  ///
  /// 若列表为空，不执行任何操作并返回 0；否则推入并返回新长度。
  pub fn lpushx(&mut self, values: impl IntoIterator<Item = impl Into<Vec<u8>>>) -> usize {
    if self.is_empty() {
      return 0;
    }
    self.lpush(values)
  }

  /// 仅当列表非空时从尾部推入元素 (RPUSHX)
  ///
  /// 若列表为空，不执行任何操作并返回 0；否则推入并返回新长度。
  pub fn rpushx(&mut self, values: impl IntoIterator<Item = impl Into<Vec<u8>>>) -> usize {
    if self.is_empty() {
      return 0;
    }
    self.rpush(values)
  }

  /// 从左侧 (头部) 弹出一个元素 (LPOP 单元素模式)
  #[inline]
  pub fn lpop_one(&mut self) -> Option<Vec<u8>> {
    self.pop_front_inner()
  }

  /// 从右侧 (尾部) 弹出一个元素 (RPOP 单元素模式)
  #[inline]
  pub fn rpop_one(&mut self) -> Option<Vec<u8>> {
    self.pop_back_inner()
  }

  /// 从左侧 (头部) 弹出至多 count 个元素 (LPOP)
  pub fn lpop(&mut self, count: usize) -> Vec<Vec<u8>> {
    if count == 0 || self.is_empty() {
      return Vec::new();
    }
    self.drain_left(count).collect()
  }

  /// 从右侧 (尾部) 弹出至多 count 个元素 (RPOP)，按弹出顺序返回
  pub fn rpop(&mut self, count: usize) -> Vec<Vec<u8>> {
    if count == 0 || self.is_empty() {
      return Vec::new();
    }
    self.drain_right(count).rev().collect()
  }

  /// 从头部流式弹出元素的双向迭代器 (按需消费，零中间集合分配)
  ///
  /// 注: 未消费完即丢弃迭代器时，剩余元素保留在列表中。
  #[inline]
  pub fn drain_left(&mut self, count: usize) -> impl DoubleEndedIterator<Item = Vec<u8>> + '_ {
    let remaining = count.min(self.len);
    Drain {
      list: self,
      start: 0,
      remaining,
    }
  }

  /// 从尾部流式弹出元素的双向迭代器 (按需消费，零中间集合分配)
  ///
  /// 迭代区间为列表的最后 min(count, len) 个元素 (按列表顺序)。
  #[inline]
  pub fn drain_right(&mut self, count: usize) -> impl DoubleEndedIterator<Item = Vec<u8>> + '_ {
    let remaining = count.min(self.len);
    let start = self.len - remaining;
    Drain {
      list: self,
      start,
      remaining,
    }
  }

  /// 获取指定范围的切片 (LRANGE)
  ///
  /// 支持负数索引 (-1 表示最后一个元素，-2 表示倒数第二个元素)。
  /// 索引区间为闭区间 [start, stop]。
  pub fn lrange(&self, start: isize, stop: isize) -> Vec<Vec<u8>> {
    match self.normalize_range(start, stop) {
      Some(range) => self
        .slices(*range.start(), *range.end() + 1)
        .map(<[u8]>::to_vec)
        .collect(),
      None => Vec::new(),
    }
  }

  /// 获取闭区间 [start, stop] 范围的双向切片迭代器 (零拷贝流式读取)
  pub fn iter_range(
    &self,
    start: isize,
    stop: isize,
  ) -> impl DoubleEndedIterator<Item = &[u8]> + ExactSizeIterator {
    match self.normalize_range(start, stop) {
      Some(range) => self.slices(*range.start(), *range.end() + 1),
      None => self.slices(0, 0),
    }
  }

  /// 获取列表所有元素的切片双向迭代器
  #[inline]
  pub fn iter(&self) -> impl DoubleEndedIterator<Item = &[u8]> + ExactSizeIterator {
    self.slices(0, self.len)
  }

  /// 规整化用户索引并定位 (页下标, 页内偏移)
  #[inline]
  fn locate_normalized(&self, index: isize) -> Option<(usize, usize)> {
    self.normalize_index(index).and_then(|idx| self.locate(idx))
  }

  /// 获取指定索引的元素切片 (LINDEX, 对应 C# ListIndex)
  #[inline]
  pub fn lindex(&self, index: isize) -> Option<&[u8]> {
    let (pi, local) = self.locate_normalized(index)?;
    self.pages.get(pi)?.get(local).map(Vec::as_slice)
  }

  /// 获取指定索引的元素可变引用 (支持原地覆写修改)
  #[inline]
  pub fn lindex_mut(&mut self, index: isize) -> Option<&mut Vec<u8>> {
    let (pi, local) = self.locate_normalized(index)?;
    self.pages.get_mut(pi)?.get_mut(local)
  }

  /// 设置指定索引的元素 (LSET, 对应 C# ListSet; 越界返回 IndexOutOfRange)
  pub fn lset(&mut self, index: isize, val: impl Into<Vec<u8>>) -> Result<()> {
    *self.lindex_mut(index).ok_or(Error::IndexOutOfRange)? = val.into();
    Ok(())
  }

  /// 修剪列表，仅保留 [start, stop] 闭区间范围内的元素 (LTRIM)
  pub fn ltrim(&mut self, start: isize, stop: isize) {
    if self.len == 0 {
      return;
    }

    match self.normalize_range(start, stop) {
      Some(range) => {
        let keep_start = *range.start();
        let keep_len = *range.end() + 1 - keep_start;

        // 头部裁剪: 整页弹出, 边界页页内裁剪 (单次借用定位边界页)
        let mut head = keep_start;
        let mut head_removed = 0;
        while head > 0 {
          let Some(page) = self.pages.front_mut() else {
            break;
          };
          let plen = page.len();
          if plen > head {
            page.drain_front(head);
            head_removed += head;
            break;
          }
          head -= plen;
          head_removed += plen;
          self.pages.pop_front();
        }

        // 尾部裁剪: 整页弹出, 边界页页内裁剪 (需扣除头部已裁剪量)
        let mut tail = self.len - head_removed - keep_len;
        while tail > 0 {
          let Some(page) = self.pages.back_mut() else {
            break;
          };
          let plen = page.len();
          if plen > tail {
            page.drain_back(tail);
            break;
          }
          self.pages.pop_back();
          tail -= plen;
        }

        self.len = keep_len;
      }
      None => {
        // 区间为空: 全删并连同页目录一并释放
        self.pages = VecDeque::new();
        self.len = 0;
      }
    }
  }

  /// 单页头部限额删除: 就地删除页内至多 quota 个匹配元素, 返回实际删除数
  fn remove_from_page_head(page: &mut Page, quota: usize, val: &[u8]) -> usize {
    let mut removed = 0;
    page.retain(|item| {
      if removed < quota && item.as_slice() == val {
        removed += 1;
        false
      } else {
        true
      }
    });
    removed
  }

  /// 单页尾部限额删除: 从页尾起删除至多 quota 个匹配元素, 返回实际删除数
  ///
  /// 页容量上限为 64, 用 u64 位图标记待删元素, 零堆分配完成逆向定点删除
  fn remove_from_page_tail(page: &mut Page, mut quota: usize, val: &[u8]) -> usize {
    let mut mask = 0u64;
    let mut removed = 0;
    for (local, item) in page.iter().enumerate().rev() {
      if quota == 0 {
        break;
      }
      if item.as_slice() == val {
        mask |= 1 << local;
        quota -= 1;
        removed += 1;
      }
    }
    if removed > 0 {
      // 位图自低位向高位对应 local 0..len, 移位逐位判定是否保留
      page.retain(|_| {
        let keep = mask & 1 == 0;
        mask >>= 1;
        keep
      });
    }
    removed
  }

  /// 全量删除与 val 相等的元素, 返回删除数 (单趟逐页 retain)
  fn retain_not_eq(&mut self, val: &[u8]) -> usize {
    let mut removed = 0;
    for page in self.pages.iter_mut() {
      let before = page.len();
      page.retain(|item| item.as_slice() != val);
      removed += before - page.len();
    }
    removed
  }

  /// 删除与指定值相等的元素 (LREM)
  ///
  /// count > 0: 从头到尾删除至多 count 个
  /// count < 0: 从尾到头删除至多 |count| 个
  /// count == 0: 删除所有匹配元素
  /// 返回实际删除的元素个数
  pub fn lrem(&mut self, count: isize, val: &[u8]) -> usize {
    if self.is_empty() {
      return 0;
    }

    // count == 0 与 |count| >= len 等价于全量匹配删除 (单趟 retain)
    let removed = match count {
      0 => self.retain_not_eq(val),
      c if c > 0 => {
        let mut quota = c as usize;
        let mut removed = 0;
        for page in self.pages.iter_mut() {
          if quota == 0 {
            break;
          }
          let r = Self::remove_from_page_head(page, quota, val);
          quota -= r;
          removed += r;
        }
        removed
      }
      c => {
        let limit = c.unsigned_abs();
        if limit >= self.len {
          self.retain_not_eq(val)
        } else {
          // 从尾向头限额删除: 逐页自尾锚定, 保证删除的是全局最后 limit 个匹配
          let mut quota = limit;
          let mut removed = 0;
          for page in self.pages.iter_mut().rev() {
            if quota == 0 {
              break;
            }
            let r = Self::remove_from_page_tail(page, quota, val);
            quota -= r;
            removed += r;
          }
          removed
        }
      }
    };

    self.len -= removed;
    self.reclaim();
    removed
  }

  /// 在 pivot 元素前或后插入新元素 (LINSERT, 对应 C# ListInsert)
  ///
  /// 找到首个与 pivot 匹配的元素并插入。若找到并插入成功返回插入后的列表长度，若未找到返回 -1。
  pub fn linsert(&mut self, pivot: &[u8], val: impl Into<Vec<u8>>, pos: InsertPosition) -> isize {
    // 单趟扫描定位首个 pivot 的 (页下标, 页内偏移), 免去二次全局 locate
    let found = self.pages.iter().enumerate().find_map(|(pi, page)| {
      page
        .iter()
        .position(|item| item.as_slice() == pivot)
        .map(|local| (pi, local))
    });

    let Some((pi, local)) = found else {
      return -1;
    };
    let offset = match pos {
      InsertPosition::Before => local,
      InsertPosition::After => local + 1,
    };
    self.insert_into_page(pi, offset, val.into());
    self.len as isize
  }

  /// 查找元素在列表中的匹配位置索引 (LPOS, 对应 C# ListPosition)
  ///
  /// - `rank`: 指定匹配第几次出现 (默认 1，正数从头到尾，负数从尾到头；跳过前 |rank|-1 个匹配)
  /// - `count`: None 返回首个匹配; Some(0) 返回全部匹配; Some(n) 返回至多 n 个匹配
  /// - `maxlen`: 最多比较的元素个数 (0 表示全部)
  pub fn lpos(
    &self,
    element: &[u8],
    rank: isize,
    count: Option<usize>,
    maxlen: usize,
  ) -> Vec<usize> {
    if rank == 0 || self.len == 0 {
      return Vec::new();
    }

    let max_comparisons = if maxlen == 0 {
      self.len
    } else {
      maxlen.min(self.len)
    };

    let target_count = match count {
      Some(0) => usize::MAX,
      Some(c) => c,
      None => 1,
    };

    let mut matches = Vec::with_capacity(target_count.min(16));

    if rank > 0 {
      let mut skip_matches = (rank - 1) as usize;
      for (idx, item) in self.iter().enumerate().take(max_comparisons) {
        if item == element {
          if skip_matches > 0 {
            skip_matches -= 1;
          } else {
            matches.push(idx);
            if matches.len() >= target_count {
              break;
            }
          }
        }
      }
    } else {
      let mut skip_matches = rank.unsigned_abs().saturating_sub(1);
      for (offset, item) in self.iter().rev().enumerate().take(max_comparisons) {
        if item == element {
          let idx = self.len - 1 - offset;
          if skip_matches > 0 {
            skip_matches -= 1;
          } else {
            matches.push(idx);
            if matches.len() >= target_count {
              break;
            }
          }
        }
      }
    }

    matches
  }

  /// 从源列表弹出并推入目标列表 (LMOVE)
  ///
  /// `from_left`: true 从源列表头部弹出 (LEFT)，false 从尾部弹出 (RIGHT)
  /// `to_left`: true 推入目标列表头部 (LEFT)，false 推入尾部 (RIGHT)
  pub fn lmove(
    source: &mut Self,
    destination: &mut Self,
    from_left: bool,
    to_left: bool,
  ) -> Option<Vec<u8>> {
    let val = if from_left {
      source.pop_front_inner()?
    } else {
      source.pop_back_inner()?
    };
    if to_left {
      destination.push_front_inner(val.clone());
    } else {
      destination.push_back_inner(val.clone());
    }
    Some(val)
  }

  /// 原子弹出尾部元素并推入目标列表头部 (RPOPLPUSH)
  #[inline]
  pub fn rpoplpush(source: &mut Self, destination: &mut Self) -> Option<Vec<u8>> {
    Self::lmove(source, destination, false, true)
  }

  /// 列表内循环旋转移动 (同键 LMOVE 场景)
  ///
  /// 同键 LMOVE 退化为页级首尾弹出推入, O(1) 且零元素搬移 (对比环形缓冲整体旋转的 O(n))
  pub fn rotate(&mut self, from_left: bool, to_left: bool) -> Option<&[u8]> {
    if self.len == 0 {
      return None;
    }
    match (from_left, to_left) {
      (true, false) => {
        let val = self.pop_front_inner()?;
        self.push_back_inner(val);
      }
      (false, true) => {
        let val = self.pop_back_inner()?;
        self.push_front_inner(val);
      }
      // 同向移动为纯 peek 语义 (pop 后立即推回同端)
      (true, true) | (false, false) => {}
    }
    let page = if to_left {
      self.pages.front()
    } else {
      self.pages.back()
    };
    page
      .and_then(|page| if to_left { page.front() } else { page.back() })
      .map(Vec::as_slice)
  }

  /// 批量从源列表移动元素到目标列表（零中间集合分配的流式转移）
  ///
  /// 语义严格等价于连续执行 `count` 次 `lmove(source, destination, from_left, to_left)`。
  /// 支持源和目标为同一列表的自转批量移动 (退化为全局旋转)。
  pub fn transfer_to(
    &mut self,
    destination: &mut Self,
    count: usize,
    from_left: bool,
    to_left: bool,
  ) -> usize {
    let take_count = count.min(self.len);
    if take_count == 0 {
      return 0;
    }

    // 源目标相同时的自旋优化 (等价于保留顺序的整体旋转, 零中间集合分配)
    if ptr::eq(self, destination) {
      match (from_left, to_left) {
        // 左旋: 头部元素依次转入尾部
        (true, false) => {
          for _ in 0..take_count {
            if let Some(val) = self.pop_front_inner() {
              self.push_back_inner(val);
            }
          }
        }
        // 右旋: 尾部元素依次转入头部
        (false, true) => {
          for _ in 0..take_count {
            if let Some(val) = self.pop_back_inner() {
              self.push_front_inner(val);
            }
          }
        }
        // 同向移动为无操作 (pop 后立即 push 回同端)
        (true, true) | (false, false) => {}
      }
      return take_count;
    }

    if from_left {
      if to_left {
        for val in self.drain_left(take_count) {
          destination.push_front_inner(val);
        }
      } else {
        for val in self.drain_left(take_count) {
          destination.push_back_inner(val);
        }
      }
    } else {
      // 弹出顺序为自尾向头
      let drain = self.drain_right(take_count).rev();
      if to_left {
        for val in drain {
          destination.push_front_inner(val);
        }
      } else {
        for val in drain {
          destination.push_back_inner(val);
        }
      }
    }

    take_count
  }

  /// 使用 bitcode 极速序列化为字节向量
  #[inline]
  pub fn to_bitcode(&self) -> Vec<u8> {
    bitcode::encode(self)
  }

  /// 从 bitcode 字节切片反序列化 (解码后校验页级不变量)
  #[inline]
  pub fn from_bitcode(bytes: &[u8]) -> Result<Self> {
    let obj: Self = bitcode::decode(bytes).map_err(Error::from)?;
    // 防御恶意构造的数据: len 与实际页长一致, 且页目录无空页、单页不超容量
    // (超容量页会使 LREM 反向删除的 u64 位图 `1 << local` 移位越界)
    if obj.len != obj.pages.iter().map(Page::len).sum::<usize>()
      || obj
        .pages
        .iter()
        .any(|p| p.is_empty() || p.len() > p.capacity())
    {
      return Err(Error::CorruptedData);
    }
    Ok(obj)
  }

  /// 序列化为 Garnet 1:1 二进制存储格式
  ///
  /// 格式：
  /// - count: i32 (小端)
  /// - 遍历每个元素：
  ///   - item_len: i32 (小端)
  ///   - item 字节切片
  pub fn serialize(&self, buf: &mut Vec<u8>) {
    // Garnet 1:1 格式的 count/item_len 均为 i32 (C# ListObject.Count 本身即 int):
    // 编码侧开发期断言防 usize->i32 静默位截断, 与解码侧 deserialize/from_bitcode
    // 的损坏数据校验对称
    debug_assert!(self.len <= i32::MAX as usize);
    let count = self.len as i32;
    let payload_bytes: usize = self
      .pages
      .iter()
      .map(|page| page.iter().map(|item| 4 + item.len()).sum::<usize>())
      .sum();
    buf.reserve(4 + payload_bytes);

    buf.extend_from_slice(&count.to_le_bytes());

    for page in &self.pages {
      for item in page {
        debug_assert!(item.len() <= i32::MAX as usize);
        let item_len = item.len() as i32;
        buf.extend_from_slice(&item_len.to_le_bytes());
        buf.extend_from_slice(item);
      }
    }
  }

  /// 从二进制数据反序列化（Garnet 1:1 格式）
  pub fn deserialize(buf: &[u8]) -> Result<Self> {
    if buf.len() < 4 {
      return Err(Error::BufferTooShort);
    }

    let count = i32::from_le_bytes(buf[..4].try_into().map_err(|_| Error::BufferTooShort)?);
    if count < 0 {
      return Err(Error::CorruptedData);
    }

    // 每个元素至少占 4 字节的 item_len 头部，防止恶意大数导致巨量预分配 OOM
    let count_usize = count as usize;
    if count_usize > (buf.len() - 4) / 4 {
      return Err(Error::CorruptedData);
    }

    let mut list = Self::with_capacity(count_usize);
    let mut rest = &buf[4..];

    for _ in 0..count {
      if rest.len() < 4 {
        return Err(Error::BufferTooShort);
      }
      let (head, payload) = rest.split_at(4);
      let item_len_raw = i32::from_le_bytes(head.try_into().map_err(|_| Error::BufferTooShort)?);
      if item_len_raw < 0 {
        return Err(Error::CorruptedData);
      }

      let item_len = item_len_raw as usize;
      if payload.len() < item_len {
        return Err(Error::BufferTooShort);
      }
      let (item, remain) = payload.split_at(item_len);
      list.push_back_inner(item.to_vec());
      rest = remain;
    }

    Ok(list)
  }
}

/// 跨页全局切片双向迭代器 (页游标推进, 单次遍历 O(区间长 + 触及页数))
struct Slices<'a> {
  /// 页目录只读引用
  pages: &'a VecDeque<Page>,
  /// 前端游标 (页下标, 页内偏移)
  front: (usize, usize),
  /// 后端游标 (页下标, 页内偏移)
  back: (usize, usize),
  /// 剩余元素数
  remaining: usize,
}

impl<'a> Slices<'a> {
  /// 前游标后移一格
  fn advance_front(&mut self) {
    if let Some(page) = self.pages.get(self.front.0) {
      self.front.1 += 1;
      if self.front.1 >= page.len() {
        self.front = (self.front.0 + 1, 0);
      }
    }
  }

  /// 后游标前移一格
  fn advance_back(&mut self) {
    if self.back.1 == 0 {
      let pi = self.back.0.wrapping_sub(1);
      let local = self
        .pages
        .get(pi)
        .map_or(0, |page| page.len().saturating_sub(1));
      self.back = (pi, local);
    } else {
      self.back.1 -= 1;
    }
  }
}

impl<'a> Iterator for Slices<'a> {
  type Item = &'a [u8];

  fn next(&mut self) -> Option<&'a [u8]> {
    if self.remaining == 0 {
      return None;
    }
    let item = self.pages.get(self.front.0)?.get(self.front.1)?.as_slice();
    self.remaining -= 1;
    if self.remaining > 0 {
      self.advance_front();
    }
    Some(item)
  }

  fn size_hint(&self) -> (usize, Option<usize>) {
    (self.remaining, Some(self.remaining))
  }
}

impl<'a> DoubleEndedIterator for Slices<'a> {
  fn next_back(&mut self) -> Option<&'a [u8]> {
    if self.remaining == 0 {
      return None;
    }
    let item = self.pages.get(self.back.0)?.get(self.back.1)?.as_slice();
    self.remaining -= 1;
    if self.remaining > 0 {
      self.advance_back();
    }
    Some(item)
  }
}

impl ExactSizeIterator for Slices<'_> {}

/// 跨页区间弹出迭代器 (流式产出, 零中间集合分配)
///
/// 创建时即锁定列表的 [start, start + remaining) 区间;
/// 未消费完即丢弃迭代器时, 剩余元素保留在列表中。
struct Drain<'a> {
  /// 被弹出区间的宿主列表
  list: &'a mut ListObject,
  /// 区间起始全局索引 (固定不变)
  start: usize,
  /// 尚未产出的元素数
  remaining: usize,
}

impl Iterator for Drain<'_> {
  type Item = Vec<u8>;

  #[inline]
  fn next(&mut self) -> Option<Vec<u8>> {
    if self.remaining == 0 {
      return None;
    }
    let val = if self.start == 0 {
      // 处于列表最前端: 直接 O(1) 头部弹出, 零 locate 开销
      self.list.pop_front_inner()?
    } else {
      self.list.remove_at(self.start)?
    };
    self.remaining -= 1;
    Some(val)
  }

  #[inline(always)]
  fn size_hint(&self) -> (usize, Option<usize>) {
    (self.remaining, Some(self.remaining))
  }
}

impl DoubleEndedIterator for Drain<'_> {
  #[inline]
  fn next_back(&mut self) -> Option<Vec<u8>> {
    if self.remaining == 0 {
      return None;
    }
    let idx = self.start + self.remaining - 1;
    let val = if idx + 1 == self.list.len {
      // 处于列表最末端: 直接 O(1) 尾部弹出, 零 locate 开销
      self.list.pop_back_inner()?
    } else {
      self.list.remove_at(idx)?
    };
    self.remaining -= 1;
    Some(val)
  }
}

impl ExactSizeIterator for Drain<'_> {}

#[cfg(test)]
mod tests {
  use super::*;

  /// from_bitcode 必须拒绝破坏页级不变量的恶意数据
  /// (空页 / 页长超容量; 后者会使 LREM 位图移位越界, 无法经公共 API 构造, 故模块内测试)
  #[test]
  fn from_bitcode_rejects_broken_page_invariants() {
    // 空页混入页目录
    let empty_paged = ListObject {
      pages: VecDeque::from([Page::new()]),
      len: 0,
    };
    assert_eq!(
      ListObject::from_bitcode(&empty_paged.to_bitcode()),
      Err(Error::CorruptedData)
    );

    // 单页长度超出容量上限 (len 总和校验无法拦截)
    let over: Page =
      Page::from_items_unchecked((0..=DEFAULT_PAGE_CAPACITY).map(|i| vec![i as u8]).collect());
    let oversized = ListObject {
      pages: VecDeque::from([over]),
      len: DEFAULT_PAGE_CAPACITY + 1,
    };
    assert_eq!(
      ListObject::from_bitcode(&oversized.to_bitcode()),
      Err(Error::CorruptedData)
    );

    // 合法数据不受影响
    let mut ok = ListObject::new();
    ok.rpush([b"a", b"b"]);
    assert_eq!(ListObject::from_bitcode(&ok.to_bitcode()), Ok(ok));
  }
}
