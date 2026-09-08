use std::sync::atomic::{AtomicU64, Ordering};

use whasher::fast_hash;

/// 默认版本映射表槽位容量（65536，必须为 2 的幂）
pub const DEFAULT_VERSION_MAP_CAPACITY: usize = 1 << 16;

/// 全局键监视版本映射表
///
/// 使用定长原子槽位数组配合全局纪元，通过哈希掩码寻址。
/// 具备常数时间复杂度、零动态堆内存分配、无锁原子读写。
/// 全表自增支持 O(1) 全局失效，常数时间单次遍历与广播。
#[derive(Debug)]
pub struct WatchVersionMap {
  /// 原子版本计数器槽位切片
  slots: Box<[AtomicU64]>,
  /// 槽位掩码（容量减一）
  mask: usize,
  /// 全局失效纪元计数器（FLUSH 级联广播时 O(1) 原子递增）
  global_epoch: AtomicU64,
}

impl Default for WatchVersionMap {
  #[inline]
  fn default() -> Self {
    Self::new(DEFAULT_VERSION_MAP_CAPACITY)
  }
}

impl WatchVersionMap {
  /// 创建指定容量的版本映射表（容量将自动向上对齐至 2 的幂）
  pub fn new(capacity: usize) -> Self {
    let cap = capacity.max(2).next_power_of_two();
    let slots = (0..cap).map(|_| AtomicU64::new(0)).collect::<Box<[_]>>();
    Self {
      slots,
      mask: cap - 1,
      global_epoch: AtomicU64::new(0),
    }
  }

  /// 计算键字节序列的 64 位高质量哈希值
  #[inline]
  pub fn hash_key(key: &[u8]) -> u64 {
    fast_hash(key)
  }

  /// 读取指定键哈希对应的当前版本号（融合全局纪元）
  #[inline]
  pub fn read_version(&self, hash: u64) -> u64 {
    let index = (hash as usize) & self.mask;
    // 安全性保证：self.mask 为 cap - 1 且 cap 保证为 2 的幂，
    // (hash as usize) & self.mask 严格小于 self.slots.len()，无越界风险
    let slot_ver = unsafe { self.slots.get_unchecked(index) }.load(Ordering::Acquire);
    let epoch = self.global_epoch.load(Ordering::Acquire);
    slot_ver.wrapping_add(epoch)
  }

  /// 递增指定键哈希的版本号并返回递增后的新版本
  ///
  /// 版本采用 u64 回绕计数：2^64 次递增在实践中不可达，即便回绕，
  /// `wrapping_add` 亦保证读写两侧对回绕语义一致，无逻辑风险。
  #[inline]
  pub fn bump_version(&self, hash: u64) -> u64 {
    let index = (hash as usize) & self.mask;
    // 安全性保证：同上，掩码寻址严格在有效切片索引范围内
    let prev = unsafe { self.slots.get_unchecked(index) }.fetch_add(1, Ordering::AcqRel);
    let epoch = self.global_epoch.load(Ordering::Acquire);
    prev.wrapping_add(1).wrapping_add(epoch)
  }

  /// 全表自增：FLUSHDB / FLUSHALL 等全局变异提交时调用，
  /// 使所有会话的全部受监视键一次性整体失效。
  /// 采用全局纪元原子递增，实现 O(1) 瞬时广播级联失效。
  #[inline]
  pub fn bump_all(&self) {
    self.global_epoch.fetch_add(1, Ordering::AcqRel);
  }

  /// 通过键字节切片读取当前版本号
  #[inline]
  pub fn read_version_key(&self, key: &[u8]) -> u64 {
    self.read_version(Self::hash_key(key))
  }

  /// 通过键字节切片递增版本号并返回新版本
  ///
  /// 非事务写路径（单命令即时执行）修改键后必须调用本方法触发监视失效，
  /// 对标 Garnet 存储函数内的 `watchVersionMap.IncrementVersion` 钩子。
  #[inline]
  pub fn bump_version_key(&self, key: &[u8]) -> u64 {
    self.bump_version(Self::hash_key(key))
  }

  /// 清空重置所有槽位的版本号与全局纪元为 0
  ///
  /// # 契约
  /// 仅允许在不存在并发监视者的场景调用（如启动 / 恢复阶段）。
  /// 若在有会话持有版本基线时调用，基线为 0 的监视将与清零后的新值相同，
  /// 从而错过本次清空所代表的修改（对标 Garnet：版本表无清空接口，随进程重建）。
  pub fn clear(&self) {
    self.global_epoch.store(0, Ordering::Release);
    for slot in &*self.slots {
      slot.store(0, Ordering::Release);
    }
  }

  /// 获取槽位总数
  #[inline]
  pub const fn slot_count(&self) -> usize {
    self.mask + 1
  }

  /// 获取槽位掩码
  #[inline]
  pub const fn mask(&self) -> usize {
    self.mask
  }
}
