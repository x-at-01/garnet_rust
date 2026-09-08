use std::{
  fmt::{Debug, Formatter, Result as FmtResult},
  sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  },
};

use parking_lot::Mutex;

/// 单层定额缓冲池
struct PoolLevel {
  target_capacity: usize,
  items: Mutex<Vec<Vec<u8>>>,
}

/// 2 的幂次分级定额全局网络缓冲池（对标 Garnet `LimitedFixedBufferPool.cs`）
///
/// 架构设计与底层优化：
/// 1. 采用以 `min_allocation_size` 为基准的 2 的幂次分级结构（如 4KB, 8KB, 16KB, 32KB, 64KB, 128KB）；
/// 2. 缓存 `min_shift = min_size.trailing_zeros()`，消除重复的 `next_power_of_two` 位运算，单周期指令定位层级；
/// 3. 每一级独立维护定额互斥复用池（基于轻量自旋/自适应互斥锁 `parking_lot::Mutex`），单级缓存上限为 `max_entries_per_level`；
/// 4. 当请求获取缓冲区时，向上对齐至最近的 2 的幂次层级，命中缓存则直接弹出复用（0 堆内存分配）；
/// 5. 当归还缓冲区时，严格校验容量必须为 2 的幂次且达到对应层级定额容量，杜绝非标容器污染层级池；
/// 6. 提供 `purge` 接口供显式回收所有层级的未用闲置内存，提供完备的统计指标（分配数、命中数、越界分配数）。
#[derive(Debug)]
pub struct LimitedFixedBufferPool {
  min_allocation_size: usize,
  max_allocation_size: usize,
  min_shift: u32,
  max_entries_per_level: usize,
  num_levels: usize,
  levels: Vec<PoolLevel>,
  total_allocations: AtomicUsize,
  total_pool_hits: AtomicUsize,
  total_out_of_bounds: AtomicUsize,
}

impl LimitedFixedBufferPool {
  /// 默认最小分配块大小（4KB）
  pub const DEFAULT_MIN_SIZE: usize = 4096;
  /// 默认每级缓存上限（16 个）
  pub const DEFAULT_MAX_ENTRIES_PER_LEVEL: usize = 16;
  /// 默认层级数（6 层：4KB 到 128KB）
  pub const DEFAULT_NUM_LEVELS: usize = 6;

  /// 构造指定参数的分级定额缓冲池
  pub fn new(min_allocation_size: usize, max_entries_per_level: usize, num_levels: usize) -> Self {
    let min_size = min_allocation_size.next_power_of_two().max(1024);
    let num_levels = num_levels.max(1);
    let max_size = min_size << (num_levels - 1);
    let min_shift = min_size.trailing_zeros();

    let levels = (0..num_levels)
      .map(|i| PoolLevel {
        target_capacity: min_size << i,
        items: Mutex::new(Vec::with_capacity(max_entries_per_level)),
      })
      .collect();

    Self {
      min_allocation_size: min_size,
      max_allocation_size: max_size,
      min_shift,
      max_entries_per_level,
      num_levels,
      levels,
      total_allocations: AtomicUsize::new(0),
      total_pool_hits: AtomicUsize::new(0),
      total_out_of_bounds: AtomicUsize::new(0),
    }
  }

  /// 创建全局默认配置的缓冲池实例（4KB ~ 128KB，每级上限 16）
  pub fn default_pool() -> Arc<Self> {
    Arc::new(Self::new(
      Self::DEFAULT_MIN_SIZE,
      Self::DEFAULT_MAX_ENTRIES_PER_LEVEL,
      Self::DEFAULT_NUM_LEVELS,
    ))
  }

  /// 最小分配尺寸
  #[inline]
  pub fn min_allocation_size(&self) -> usize {
    self.min_allocation_size
  }

  /// 最大受管分配尺寸
  #[inline]
  pub fn max_allocation_size(&self) -> usize {
    self.max_allocation_size
  }

  /// 池化层级总数
  #[inline]
  pub fn num_levels(&self) -> usize {
    self.num_levels
  }

  /// 每层最大缓存容器数
  #[inline]
  pub fn max_entries_per_level(&self) -> usize {
    self.max_entries_per_level
  }

  /// 基于已对齐为 2 的幂次的大小直接通过纯位运算计算层级索引
  ///
  /// pow2_size 必须为 2 的幂次。0 循环、0 浮点除法、消除重复 next_power_of_two 开销。
  #[inline(always)]
  pub fn level_index_pow2(&self, pow2_size: usize) -> Option<usize> {
    if pow2_size < self.min_allocation_size || pow2_size > self.max_allocation_size {
      return None;
    }
    let idx = (pow2_size.trailing_zeros().wrapping_sub(self.min_shift)) as usize;
    if idx < self.num_levels {
      Some(idx)
    } else {
      None
    }
  }

  /// 计算任意大小对齐至 2 的幂次后的层级索引与目标对齐容量
  #[inline(always)]
  pub fn align_and_index(&self, size: usize) -> (usize, Option<usize>) {
    let target = size.next_power_of_two().max(self.min_allocation_size);
    let idx = self.level_index_pow2(target);
    (target, idx)
  }

  /// 获取指定最小容量的网络缓冲区
  ///
  /// 若层级池中有可用缓存项，直接弹出复用；否则从堆中新分配对齐容量的容器。
  pub fn get(&self, min_size: usize) -> Vec<u8> {
    self.total_allocations.fetch_add(1, Ordering::Relaxed);
    let (target_size, lvl_idx) = self.align_and_index(min_size);

    if let Some(idx) = lvl_idx {
      // SAFETY: level_index_pow2 严格保证 idx < self.num_levels == self.levels.len()
      let level = unsafe { self.levels.get_unchecked(idx) };
      let mut queue = level.items.lock();
      if let Some(mut buf) = queue.pop() {
        self.total_pool_hits.fetch_add(1, Ordering::Relaxed);
        buf.clear();
        return buf;
      }
    } else {
      self.total_out_of_bounds.fetch_add(1, Ordering::Relaxed);
    }

    Vec::with_capacity(target_size)
  }

  /// 归还已使用完毕的缓冲区
  ///
  /// 严格保证容量必须为 2 的幂次且大于等于层级定额容量，杜绝非标容器污染层级池；
  /// 若容量落在池化层级内且未达到定额上限，则存入池中复用；否则直接丢弃交由系统回收。
  pub fn return_buffer(&self, mut buf: Vec<u8>) {
    let cap = buf.capacity();
    if !cap.is_power_of_two() {
      return;
    }
    if let Some(lvl_idx) = self.level_index_pow2(cap) {
      // SAFETY: level_index_pow2 严格保证 lvl_idx < self.num_levels == self.levels.len()
      let level = unsafe { self.levels.get_unchecked(lvl_idx) };
      if cap >= level.target_capacity {
        let mut queue = level.items.lock();
        if queue.len() < self.max_entries_per_level {
          buf.clear();
          queue.push(buf);
        }
      }
    }
  }

  /// 释放所有层级中的闲置缓冲区（对标 Garnet `Purge`）
  pub fn purge(&self) {
    for level in &self.levels {
      let mut queue = level.items.lock();
      queue.clear();
      queue.shrink_to_fit();
    }
  }

  /// 获取当前池中各层级闲置缓存总数
  pub fn current_cached_count(&self) -> usize {
    self.levels.iter().map(|lvl| lvl.items.lock().len()).sum()
  }

  /// 获取累计命中次数
  #[inline]
  pub fn pool_hits(&self) -> usize {
    self.total_pool_hits.load(Ordering::Relaxed)
  }

  /// 获取累计分配请求数
  #[inline]
  pub fn total_allocations(&self) -> usize {
    self.total_allocations.load(Ordering::Relaxed)
  }

  /// 获取累计越界分配请求数
  #[inline]
  pub fn total_out_of_bounds(&self) -> usize {
    self.total_out_of_bounds.load(Ordering::Relaxed)
  }
}

impl Default for LimitedFixedBufferPool {
  fn default() -> Self {
    Self::new(
      Self::DEFAULT_MIN_SIZE,
      Self::DEFAULT_MAX_ENTRIES_PER_LEVEL,
      Self::DEFAULT_NUM_LEVELS,
    )
  }
}

impl Debug for PoolLevel {
  fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
    f.debug_struct("PoolLevel")
      .field("target_capacity", &self.target_capacity)
      .field("cached_items", &self.items.lock().len())
      .finish()
  }
}
