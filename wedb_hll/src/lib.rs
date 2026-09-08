//! 针对 Redis / Microsoft Garnet 规范的 HyperLogLog 基数估计算法实现。
//!
//! 双表示设计，与 C# `Garnet server/Resp/HyperLogLog/HyperLogLog.cs` 一致：
//! - Sparse：RLE opcode 流（见 [`sparse`]），初始仅 146 字节，PFADD 小键场景内存占用极低；
//! - Dense：16384 个 6-bit 寄存器，全长 12304 字节（16 字节头 + 12288 字节数据区）。
//!
//! 新建 HLL 从 Sparse 起步，opcode 流越过 4096 字节上限时自动升级 Dense（不可逆）。
//! 基数估计采用 C# 同款 σ/τ 修正最大似然估计（见 [`estimate`]），
//! 为 PFADD / PFCOUNT / PFMERGE 提供底层算法支撑。

#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
mod estimate;
mod sparse;

use std::borrow::Cow;

use bitcode::{Decode, Encode};
pub use error::{Error, Result};
use estimate::{HIST_LEN, finalize, hist_dense};
use sparse::{SPARSE_CAP, sparse_nonzero, validate};
use whasher::fast_hash;

/// HyperLogLog 魔数标识 "HYLL"
pub const HLL_MAGIC: &[u8; 4] = b"HYLL";

/// Sparse 稀疏模式编码标识（C# `HLL_DTYPE.HLL_SPARSE`）
pub const HLL_SPARSE: u8 = 0;

/// Dense 密集模式编码标识（C# `HLL_DTYPE.HLL_DENSE`）
pub const HLL_DENSE: u8 = 1;

/// 头部字节数 (魔数 4B + 编码 1B + 保留 3B + 缓存基数 8B)
/// 对标 C# `HyperLogLog.hll_header_bytes = 16`
pub const HLL_HEADER_SIZE: usize = 16;

/// 寄存器数量 (2^14 = 16384)
/// 对标 C# `HyperLogLog.mcnt = 1 << pbit` (pbit = 14)
pub const HLL_REGISTERS: usize = 16384;

/// 寄存器位宽 (6 bits)
/// 对标 C# `HyperLogLog.reg_bits = 6`
pub const HLL_BITS: usize = 6;

/// Dense 模式数据区字节数 (16384 * 6 / 8 = 12288)
pub const HLL_DATA_SIZE: usize = HLL_REGISTERS * HLL_BITS / 8;

/// Dense 模式完整字节数 (16 + 12288 = 12304)
/// 对标 C# `HyperLogLog.DenseBytes = hll_header_bytes + ((reg_bits * RegCnt) >> 3)`
pub const HLL_DENSE_SIZE: usize = HLL_HEADER_SIZE + HLL_DATA_SIZE;

/// 可容纳的最大基数 (缓存基数借用符号位为失效标记，有效基数占用低 63 位)
pub const HLL_MAX_COUNT: u64 = (1 << 63) - 1;

/// 前导零计数位宽 qbit = 64 - 14（C# `qbit`），寄存器值上限为 qbit + 1
pub(crate) const QBIT: u32 = 50;

/// 失效基数缓存标记（C# `SetCard(long.MinValue)`：负值即失效，正值即已缓存估计）
pub(crate) const CARD_INVALID: i64 = i64::MIN;

/// Dense 数据区内 3 字节块数量（每块 4 个 6-bit 寄存器）
const BLOCKS: usize = HLL_DATA_SIZE / 3;

/// 将 3 个连续字节解包为 4 个 6-bit 寄存器 (r0, r1, r2, r3)
/// 对标 C# `CountDenseNCEstimator` 中的解包逻辑
#[inline(always)]
pub const fn unpack_3bytes(b0: u8, b1: u8, b2: u8) -> (u8, u8, u8, u8) {
  let val = (b0 as u32) | ((b1 as u32) << 8) | ((b2 as u32) << 16);
  (
    (val & 0x3F) as u8,
    ((val >> 6) & 0x3F) as u8,
    ((val >> 12) & 0x3F) as u8,
    ((val >> 18) & 0x3F) as u8,
  )
}

/// 将 4 个 6-bit 寄存器打包为 3 个字节 (b0, b1, b2)，与 unpack_3bytes 严格互逆
#[inline(always)]
pub const fn pack_3bytes(r0: u8, r1: u8, r2: u8, r3: u8) -> (u8, u8, u8) {
  let val = (r0 as u32 & 0x3F)
    | ((r1 as u32 & 0x3F) << 6)
    | ((r2 as u32 & 0x3F) << 12)
    | ((r3 as u32 & 0x3F) << 18);
  (val as u8, (val >> 8) as u8, (val >> 16) as u8)
}

/// 从 3 字节块中提取第 `rem` 个寄存器值
#[inline(always)]
fn reg_of(b0: u8, b1: u8, b2: u8, rem: usize) -> u8 {
  match rem {
    0 => b0 & 0x3F,
    1 => (b0 >> 6) | ((b1 & 0x0F) << 2),
    2 => (b1 >> 4) | ((b2 & 0x03) << 4),
    _ => b2 >> 2,
  }
}

/// 解包 3 字节块、替换第 `rem` 个寄存器为 `v` 并重新打包
#[inline(always)]
fn repack(b0: u8, b1: u8, b2: u8, rem: usize, v: u8) -> (u8, u8, u8) {
  let (mut r0, mut r1, mut r2, mut r3) = unpack_3bytes(b0, b1, b2);
  match rem {
    0 => r0 = v,
    1 => r1 = v,
    2 => r2 = v,
    _ => r3 = v,
  }
  pack_3bytes(r0, r1, r2, r3)
}

/// 读取寄存器数据区第 `idx` 个 6-bit 值
/// (idx < 16384 保证块偏移 +2 不越界，get_unchecked 消除边界检查)
#[inline(always)]
fn reg_get(data: &[u8], idx: usize) -> u8 {
  debug_assert!(idx < HLL_REGISTERS);
  let o = (idx >> 2) * 3;
  unsafe {
    let (b0, b1, b2) = (
      *data.get_unchecked(o),
      *data.get_unchecked(o + 1),
      *data.get_unchecked(o + 2),
    );
    reg_of(b0, b1, b2, idx & 3)
  }
}

/// 仅当 `v` 大于旧值时写入，返回是否更新（快速路径：旧值不小于新值立即退出）
#[inline(always)]
fn reg_update(data: &mut [u8], idx: usize, v: u8) -> bool {
  debug_assert!(idx < HLL_REGISTERS);
  let o = (idx >> 2) * 3;
  unsafe {
    let (b0, b1, b2) = (
      *data.get_unchecked(o),
      *data.get_unchecked(o + 1),
      *data.get_unchecked(o + 2),
    );
    if v <= reg_of(b0, b1, b2, idx & 3) {
      return false;
    }
    let (n0, n1, n2) = repack(b0, b1, b2, idx & 3, v);
    *data.get_unchecked_mut(o) = n0;
    *data.get_unchecked_mut(o + 1) = n1;
    *data.get_unchecked_mut(o + 2) = n2;
  }
  true
}

/// 无条件写入第 `idx` 个寄存器
#[inline(always)]
fn reg_set(data: &mut [u8], idx: usize, v: u8) {
  debug_assert!(idx < HLL_REGISTERS);
  let o = (idx >> 2) * 3;
  unsafe {
    let (b0, b1, b2) = (
      *data.get_unchecked(o),
      *data.get_unchecked(o + 1),
      *data.get_unchecked(o + 2),
    );
    let (n0, n1, n2) = repack(b0, b1, b2, idx & 3, v);
    *data.get_unchecked_mut(o) = n0;
    *data.get_unchecked_mut(o + 1) = n1;
    *data.get_unchecked_mut(o + 2) = n2;
  }
}

/// 写入 blob 头部的基数缓存字段（C# `SetCard`）
#[inline]
fn write_card(blob: &mut [u8], card: i64) {
  blob[8..16].copy_from_slice(&card.to_le_bytes());
}

/// 容纳 HyperLogLog 的统一结构体（内部自动管理 Sparse / Dense 双表示）
#[derive(Clone, Debug, PartialEq, Eq, Encode, Decode)]
pub struct HyperLogLog {
  /// 完整 blob：Dense 为 12304 字节；Sparse 为 18 字节头 + RLE 流
  bytes: Vec<u8>,
}

impl Default for HyperLogLog {
  fn default() -> Self {
    Self::new()
  }
}

impl HyperLogLog {
  /// 创建一个全新的初始 HyperLogLog（Sparse 表示，146 字节，全零寄存器，C# `InitSparse`）
  pub fn new() -> Self {
    Self::sparse_blob()
  }

  /// 校验字节切片是否为合法的 HyperLogLog 格式（Sparse / Dense 均接受，C# `IsValidHYLL`）
  #[inline]
  pub fn is_valid(bytes: &[u8]) -> bool {
    bytes.len() >= HLL_HEADER_SIZE
      && &bytes[0..4] == HLL_MAGIC
      && match bytes[4] {
        HLL_DENSE => bytes.len() == HLL_DENSE_SIZE,
        HLL_SPARSE => validate(bytes),
        _ => false,
      }
  }

  /// 从既有字节切片解析，若格式不匹配则返回错误
  pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
    if !Self::is_valid(bytes) {
      return Err(Error::InvalidHyperLogLog);
    }
    Ok(Self {
      bytes: bytes.to_vec(),
    })
  }

  /// 将当前实例序列化为 Bitcode 二进制字节流
  pub fn to_bitcode(&self) -> Vec<u8> {
    bitcode::encode(self)
  }

  /// 从 Bitcode 二进制字节流反序列化并严格校验有效性
  pub fn from_bitcode(bytes: &[u8]) -> Result<Self> {
    let hll: Self = bitcode::decode(bytes).map_err(|_| Error::InvalidHyperLogLog)?;
    if !Self::is_valid(&hll.bytes) {
      return Err(Error::InvalidHyperLogLog);
    }
    Ok(hll)
  }

  /// 获取底层字节切片引用
  #[inline(always)]
  pub fn as_bytes(&self) -> &[u8] {
    &self.bytes
  }

  /// 获取底层可变字节切片引用 (主动使基数缓存失效)
  #[inline(always)]
  pub fn as_bytes_mut(&mut self) -> &mut [u8] {
    self.invalidate_cache();
    &mut self.bytes
  }

  /// 读取指定索引处寄存器的值 (0..=16383)
  /// 对标 C# `HyperLogLog._get_register`
  #[inline]
  pub fn get_register(&self, idx: usize) -> u8 {
    assert!(idx < HLL_REGISTERS, "register index {idx} out of bounds");
    if self.is_dense() {
      reg_get(&self.bytes[HLL_HEADER_SIZE..], idx)
    } else {
      self.sparse_get(idx)
    }
  }

  /// 直接设置指定索引处寄存器的 6-bit 编码值 (同时使基数缓存失效)。
  /// Sparse 无固定寄存器写入语义，先升级 Dense 再写（测试 / 调试用，对标 C# `_set_register`）
  #[inline]
  pub fn set_register(&mut self, idx: usize, val: u8) {
    assert!(idx < HLL_REGISTERS, "register index {idx} out of bounds");
    debug_assert!(val < 64, "register value {val} exceeds 6-bit range");
    if !self.is_dense() {
      self.upgrade_dense();
    }
    reg_set(&mut self.bytes[HLL_HEADER_SIZE..], idx, val);
    self.invalidate_cache();
  }

  /// 写入指定索引处寄存器的值 (若新值大于旧值则更新并使缓存失效，返回是否更新)
  /// 对标 C# `UpdateDenseRegister`
  #[inline]
  pub fn update_register(&mut self, idx: usize, val: u8) -> bool {
    assert!(idx < HLL_REGISTERS, "register index {idx} out of bounds");
    let v = val & 0x3F;
    let updated = if self.is_dense() {
      reg_update(&mut self.bytes[HLL_HEADER_SIZE..], idx, v)
    } else if v == 0 {
      // 0 不可能大于任何旧值，Sparse 路径直接短路
      false
    } else if v <= QBIT as u8 + 1 {
      self.sparse_update(idx, v)
    } else {
      // Sparse opcode 表达不了 > qbit+1 的值，升级 Dense 后写入
      self.upgrade_dense();
      reg_update(&mut self.bytes[HLL_HEADER_SIZE..], idx, v)
    };
    if updated {
      self.invalidate_cache();
    }
    updated
  }

  /// 使基数缓存失效
  /// 对标 C# `SetCard(ptr, long.MinValue)`
  #[inline(always)]
  pub fn invalidate_cache(&mut self) {
    write_card(&mut self.bytes, CARD_INVALID);
  }

  /// 获取缓存中的基数 (若有效则返回 Some(count)，C# `IsValidCard` 符号位语义)
  #[inline(always)]
  pub fn get_cached_count(&self) -> Option<u64> {
    let card = i64::from_le_bytes(self.bytes[8..16].try_into().unwrap());
    (card >= 0).then_some(card as u64)
  }

  /// 写入基数缓存 (估计值 ≤ HLL_MAX_COUNT，恒为正)
  #[inline(always)]
  fn set_cached_count(&mut self, est: u64) {
    write_card(&mut self.bytes, est as i64);
  }

  /// 向 HyperLogLog 添加一个元素，若任何寄存器被更新则返回 true
  pub fn add(&mut self, element: &[u8]) -> bool {
    self.add_hash(fast_hash(element))
  }

  /// 向 HyperLogLog 添加预计算的 64 位哈希值
  /// 对标 C# `UpdateDense` / `UpdateSparse`：
  /// - `RegIdx(hv)`: 低 14 位作为寄存器索引
  /// - `clz(hv)`: 64 位哈希前导零个数 + 1，封顶 qbit+1
  #[inline]
  pub fn add_hash(&mut self, hash: u64) -> bool {
    let idx = (hash & (HLL_REGISTERS as u64 - 1)) as usize;
    let cnt = hash.leading_zeros().min(QBIT) as u8 + 1;

    let was_dense = self.is_dense();
    let updated = if was_dense {
      reg_update(&mut self.bytes[HLL_HEADER_SIZE..], idx, cnt)
    } else {
      self.sparse_update(idx, cnt)
    };
    if updated {
      // Sparse 流越过上限时升级 Dense（C# `UpdateGrow` 超限转 Dense 同语义），否则仅失效缓存
      if was_dense || self.bytes.len() < SPARSE_CAP {
        self.invalidate_cache();
      } else {
        self.upgrade_dense();
      }
    }
    updated
  }

  /// 估算并返回基数 (若无缓存则估算并更新缓存)
  /// 对标 C# `HyperLogLog.Count`
  pub fn count(&mut self) -> u64 {
    if let Some(cached) = self.get_cached_count() {
      return cached;
    }
    let est = self.count_readonly();
    self.set_cached_count(est);
    est
  }

  /// 只读方式估算基数 (不更新缓存)
  pub fn count_readonly(&self) -> u64 {
    if let Some(cached) = self.get_cached_count() {
      return cached;
    }
    let mut hist = [0u32; HIST_LEN];
    if self.is_dense() {
      hist_dense(&self.bytes[HLL_HEADER_SIZE..], &mut hist);
    } else {
      self.sparse_hist(&mut hist);
    }
    finalize(&hist)
  }

  /// 合并另一个 HyperLogLog 实例到当前实例（寄存器逐位取 max）
  pub fn merge(&mut self, other: &Self) {
    self.merge_valid(&other.bytes);
  }

  /// 合并外部字节切片数据 (严格校验数据格式，Sparse / Dense 均可)
  pub fn merge_raw(&mut self, other_bytes: &[u8]) -> Result<()> {
    if !Self::is_valid(other_bytes) {
      return Err(Error::InvalidHyperLogLog);
    }
    self.merge_valid(other_bytes);
    Ok(())
  }

  /// 内部合并：按 (目标, 源) 表示分发
  /// （C# `DenseToDense` / `SparseToDense` / `SparseToSparse` / `MergeGrow`）
  fn merge_valid(&mut self, src: &[u8]) {
    // 密集源合并到稀疏目标：先升级 Dense（C# `MergeGrow` 对该组合返回 DenseBytes）
    if !self.is_dense() && src[4] == HLL_DENSE {
      self.upgrade_dense();
    }
    let updated = if self.is_dense() {
      if src[4] == HLL_SPARSE {
        // 稀疏源 → 密集目标（C# `SparseToDense`：流遍历逐寄存器更新）
        let dst = &mut self.bytes[HLL_HEADER_SIZE..];
        sparse_nonzero(src).fold(false, |up, (idx, v)| reg_update(dst, idx, v) | up)
      } else {
        self.merge_dense(&src[HLL_HEADER_SIZE..])
      }
    } else {
      // 稀疏源 → 稀疏目标（C# SparseToSparse）
      let mut updated = false;
      for (idx, v) in sparse_nonzero(src) {
        updated |= self.sparse_update(idx, v);
      }
      // 仅在发生更新且越过 Sparse 上限时升级，寄存器内容与 C# 预扩容路径完全一致
      if updated && self.bytes.len() >= SPARSE_CAP {
        self.upgrade_dense();
      }
      updated
    };
    // 仅在寄存器变化时失效缓存（C# `DenseToDense` 同语义）
    if updated {
      self.invalidate_cache();
    }
  }

  /// Dense 目标数据区逐 3 字节块取 max（C# `DenseToDense`），返回是否发生更新
  fn merge_dense(&mut self, src_data: &[u8]) -> bool {
    let mut updated = false;
    let dst = &mut self.bytes[HLL_HEADER_SIZE..];
    for (d, s) in dst
      .as_chunks_mut::<3>()
      .0
      .iter_mut()
      .zip(src_data.as_chunks::<3>().0)
    {
      // 全等块或全零源块均无需更新
      if *d == *s || s == &[0; 3] {
        continue;
      }
      let (d0, d1, d2, d3) = unpack_3bytes(d[0], d[1], d[2]);
      let (s0, s1, s2, s3) = unpack_3bytes(s[0], s[1], s[2]);
      let m = (d0.max(s0), d1.max(s1), d2.max(s2), d3.max(s3));
      if m != (d0, d1, d2, d3) {
        let (n0, n1, n2) = pack_3bytes(m.0, m.1, m.2, m.3);
        *d = [n0, n1, n2];
        updated = true;
      }
    }
    updated
  }

  /// 寄存器数据区的统一 3 字节块视图：Dense 直接借用，Sparse 按需展开为 12288 字节
  fn packed_data(&self) -> Cow<'_, [u8]> {
    if self.is_dense() {
      Cow::Borrowed(&self.bytes[HLL_HEADER_SIZE..])
    } else {
      let mut data = vec![0u8; HLL_DATA_SIZE];
      for (idx, v) in sparse_nonzero(&self.bytes) {
        reg_set(&mut data, idx, v);
      }
      Cow::Owned(data)
    }
  }

  /// 估算多个 HyperLogLog 实例合并后的联合基数
  /// (全 Dense 时零堆分配；结果与逐一 merge 后 count 严格等价)
  pub fn count_multiple(hlls: &[&Self]) -> u64 {
    match hlls {
      [] => 0,
      [single] => single.count_readonly(),
      _ => {
        // 先物化各实例的寄存器数据视图 (Cow)：Dense 零拷贝借用，Sparse 按需展开
        let rows: Vec<Cow<'_, [u8]>> = hlls.iter().map(|h| h.packed_data()).collect();
        let mut hist = [0u32; HIST_LEN];
        for b in 0..BLOCKS {
          // 跨实例对 4 个 lane 逐位取 max，全零块快速跳过
          let (mut r0, mut r1, mut r2, mut r3) = (0u8, 0u8, 0u8, 0u8);
          for row in &rows {
            let c = row.as_chunks::<3>().0[b];
            if c == [0; 3] {
              continue;
            }
            let (a0, a1, a2, a3) = unpack_3bytes(c[0], c[1], c[2]);
            r0 = r0.max(a0);
            r1 = r1.max(a1);
            r2 = r2.max(a2);
            r3 = r3.max(a3);
          }
          hist[r0 as usize] += 1;
          hist[r1 as usize] += 1;
          hist[r2 as usize] += 1;
          hist[r3 as usize] += 1;
        }
        finalize(&hist)
      }
    }
  }
}

#[cfg(test)]
mod tests {
  use aok::{OK, Void};

  use super::*;

  #[ctor::ctor(unsafe)]
  fn init() {
    log_init::init();
  }

  #[test]
  fn test_pack_unpack_roundtrip() -> Void {
    for r0 in 0..=63u8 {
      for r1 in [0, 1, 15, 31, 63] {
        for r2 in [0, 7, 23, 42, 63] {
          for r3 in [0, 5, 17, 33, 63] {
            let (b0, b1, b2) = pack_3bytes(r0, r1, r2, r3);
            let (u0, u1, u2, u3) = unpack_3bytes(b0, b1, b2);
            assert_eq!((r0, r1, r2, r3), (u0, u1, u2, u3));
          }
        }
      }
    }
    OK
  }

  #[test]
  fn test_hll_basic() -> Void {
    let mut hll = HyperLogLog::new();
    assert_eq!(hll.count(), 0);

    for i in 0..1000 {
      let mut s = String::new();
      s.push_str("item_");
      s.push_str(itoa::Buffer::new().format(i));
      hll.add(s.as_bytes());
    }

    let count = hll.count();
    let err = (count as f64 - 1000.0).abs() / 1000.0;
    assert!(err < 0.05, "误差过大: count={count}, err={err}");
    OK
  }

  #[test]
  fn test_hll_merge() -> Void {
    let mut hll1 = HyperLogLog::new();
    let mut hll2 = HyperLogLog::new();

    for i in 0..500 {
      let mut key = String::new();
      key.push_str("key_");
      key.push_str(itoa::Buffer::new().format(i));
      hll1.add(key.as_bytes());
    }
    for i in 400..900 {
      let mut key = String::new();
      key.push_str("key_");
      key.push_str(itoa::Buffer::new().format(i));
      hll2.add(key.as_bytes());
    }

    let multi_count = HyperLogLog::count_multiple(&[&hll1, &hll2]);
    let err = (multi_count as f64 - 900.0).abs() / 900.0;
    assert!(err < 0.05, "联合估算误差过大: multi_count={multi_count}");

    hll1.merge(&hll2);
    let merged_count = hll1.count();
    let err2 = (merged_count as f64 - 900.0).abs() / 900.0;
    assert!(
      err2 < 0.05,
      "合并后估算误差过大: merged_count={merged_count}"
    );
    assert_eq!(multi_count, merged_count);
    OK
  }
}
