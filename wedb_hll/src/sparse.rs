//! Sparse 稀疏表示：RLE（行程编码）opcode 流。
//!
//! 对应 C# `HyperLogLog.cs` 的 `InitSparse` / `UpdateSparseReg` / `SparseToDense` /
//! `SparseToSparse` / `IsValidSparseStream`：
//!
//! - `0vvv vvvv`：非零 opcode，覆盖 1 个寄存器，寄存器值 = v + 1 ∈ [1, qbit+1]
//! - `1xxx xxxx`：零段 opcode，覆盖 x + 1 个连续全零寄存器（1..=128）
//!
//! 布局 = 通用 16 字节头 + 2 字节 RLE 长度（LE u16，C# `SetSparseRLESize`）+ opcode 流。
//! 初始 18 + 128 字节（128 个 `0xFF` 零段恰好覆盖 16384 个寄存器）；
//! 总长超过 4096 字节上限（C# `SparseSizeMaxCap`）即不可逆升级为 Dense。

use crate::{
  CARD_INVALID, HLL_DENSE, HLL_DENSE_SIZE, HLL_HEADER_SIZE, HLL_MAGIC, HLL_REGISTERS, HLL_SPARSE,
  HyperLogLog, QBIT, estimate::HIST_LEN, reg_set, write_card,
};

/// Sparse 头部字节数 = 通用头 16 + RLE 长度 2（C# `SparseHeaderSize`）
pub(super) const SPARSE_HEADER: usize = 18;

/// Sparse 表示总长上限，超出升级 Dense（C# `SparseSizeMaxCap = 1 << 12`）
pub(super) const SPARSE_CAP: usize = 4096;

/// 非零 opcode 可表达的最大寄存器值 = qbit + 1（C# 非零值域校验上限）
const MAX_REG_VAL: u8 = QBIT as u8 + 1;

/// 初始零段 opcode 数量 = 寄存器数 >> 7（C# `SparseZeroRanges`），每段覆盖 128 个零寄存器
const SPARSE_INIT_RANGES: usize = HLL_REGISTERS >> 7;

/// opcode 覆盖的寄存器数：零段为 `x + 1`，非零恒为 1（C# `ZeroRangeLen`）
#[inline]
fn opcode_len(b: u8) -> usize {
  if b & 0x80 != 0 {
    (b & 0x7F) as usize + 1
  } else {
    1
  }
}

/// 校验 Sparse blob 结构（C# `IsValidHLLLength` 的 Sparse 分支 + `IsValidSparseStream`）：
/// 长度落在 [头, 上限] 内、RLE 长度与负载严格一致、非零值域合法、且流恰好覆盖全部寄存器一次
pub(super) fn validate(bytes: &[u8]) -> bool {
  if bytes.len() < SPARSE_HEADER || bytes.len() > SPARSE_CAP {
    return false;
  }
  let rle = u16::from_le_bytes([bytes[16], bytes[17]]) as usize;
  if rle != bytes.len() - SPARSE_HEADER {
    return false;
  }
  let mut covered = 0usize;
  for &b in &bytes[SPARSE_HEADER..] {
    // 非零 opcode 值域必须落在 [1, qbit+1]（C# `nonZero > qbit + 1` 校验）
    if b & 0x80 == 0 && b & 0x7F >= MAX_REG_VAL {
      return false;
    }
    covered += opcode_len(b);
    // 越过寄存器空间即非法
    if covered > HLL_REGISTERS {
      return false;
    }
  }
  covered == HLL_REGISTERS
}

/// 遍历字节流中的全部非零寄存器 `(idx, val)`（C# SparseToDense / SparseToSparse 的流遍历）
pub(super) fn sparse_nonzero(bytes: &[u8]) -> impl Iterator<Item = (usize, u8)> + '_ {
  let rle = u16::from_le_bytes([bytes[16], bytes[17]]) as usize;
  bytes[SPARSE_HEADER..SPARSE_HEADER + rle]
    .iter()
    .copied()
    .scan(0usize, |off, b| {
      let start = *off;
      *off += opcode_len(b);
      Some((start, b))
    })
    .filter_map(|(idx, b)| (b & 0x80 == 0).then_some((idx, (b & 0x7F) + 1)))
}

impl HyperLogLog {
  /// 是否为 Dense 表示
  #[inline]
  pub(super) fn is_dense(&self) -> bool {
    self.bytes[4] == HLL_DENSE
  }

  /// 读取 RLE 流长度（字节），C# `GetSparseRLESize`
  #[inline]
  pub(super) fn rle_size(&self) -> usize {
    u16::from_le_bytes([self.bytes[16], self.bytes[17]]) as usize
  }

  /// 创建全新 Sparse blob（C# `InitSparse`：128 个 `0xFF` 零段覆盖全部寄存器，缓存失效）
  pub(super) fn sparse_blob() -> Self {
    let mut bytes = vec![0u8; SPARSE_HEADER + SPARSE_INIT_RANGES];
    bytes[0..4].copy_from_slice(HLL_MAGIC);
    bytes[4] = HLL_SPARSE;
    write_card(&mut bytes, CARD_INVALID);
    bytes[16..SPARSE_HEADER].copy_from_slice(&(SPARSE_INIT_RANGES as u16).to_le_bytes());
    bytes[SPARSE_HEADER..].fill(0xFF);
    Self { bytes }
  }

  /// 定位覆盖第 `idx` 个寄存器的 opcode：`(流内偏移, 起始寄存器号, 覆盖长度)`。
  /// 仅当流被 `as_bytes_mut` 人为破坏时返回 None（安全降级为无操作）
  fn locate(&self, idx: usize) -> Option<(usize, usize, usize)> {
    let rle = self.rle_size();
    let mut offset = 0usize;
    for (pos, &b) in self.bytes[SPARSE_HEADER..SPARSE_HEADER + rle]
      .iter()
      .enumerate()
    {
      let clen = opcode_len(b);
      if idx < offset + clen {
        return Some((pos, offset, clen));
      }
      offset += clen;
    }
    None
  }

  /// 读取第 `idx` 个寄存器值（零段返回 0）
  pub(super) fn sparse_get(&self, idx: usize) -> u8 {
    match self.locate(idx) {
      Some((pos, ..)) if self.bytes[SPARSE_HEADER + pos] & 0x80 == 0 => {
        (self.bytes[SPARSE_HEADER + pos] & 0x7F) + 1
      }
      _ => 0,
    }
  }

  /// Sparse 寄存器更新：仅 `val` 大于旧值时生效（C# `UpdateSparseReg`）。
  /// 返回是否发生更新；缓存失效由调用方统一处理
  pub(super) fn sparse_update(&mut self, idx: usize, val: u8) -> bool {
    debug_assert!((1..=MAX_REG_VAL).contains(&val));
    let Some((pos, offset, clen)) = self.locate(idx) else {
      return false;
    };
    let base = SPARSE_HEADER + pos;
    if self.bytes[base] & 0x80 == 0 {
      // 命中非零 opcode：原值不小于新值则无更新，否则原地改写（C# 分支 2）
      if val <= (self.bytes[base] & 0x7F) + 1 {
        return false;
      }
      self.bytes[base] = val - 1;
    } else {
      // 零段内拆分为 [左零段][值][右零段]，净增长 0..=2 字节（C# 分支 3）
      let mut ops = [0u8; 3];
      let mut n = 0;
      if offset != idx {
        ops[n] = ((idx - offset - 1) as u8) | 0x80;
        n += 1;
      }
      ops[n] = val - 1;
      n += 1;
      if offset + clen - 1 != idx {
        ops[n] = ((offset + clen - idx - 2) as u8) | 0x80;
        n += 1;
      }
      // 单个 opcode 字节原位替换为 n 个字节（C# 以 memmove 后移后缀等价实现）
      let grown = n - 1;
      let size = self.rle_size() + grown;
      self.bytes.splice(base..base + 1, ops[..n].iter().copied());
      self.bytes[16..SPARSE_HEADER].copy_from_slice(&(size as u16).to_le_bytes());
    }
    true
  }

  /// 累计 Sparse 流的寄存器直方图（C# `CountSparseNCEstimator` 的流扫描，零段计入 rhisto[0]）
  pub(super) fn sparse_hist(&self, hist: &mut [u32; HIST_LEN]) {
    let rle = self.rle_size();
    for &b in &self.bytes[SPARSE_HEADER..SPARSE_HEADER + rle] {
      if b & 0x80 != 0 {
        hist[0] += (b & 0x7F) as u32 + 1;
      } else {
        // 值域经 validate 保证 ≤ 51，min 兜底防御 as_bytes_mut 破坏导致的直方图越界
        hist[((b & 0x7F) + 1).min(63) as usize] += 1;
      }
    }
  }

  /// Sparse → Dense 原地升级（C# `InitDense` + `SparseToDense`），新 blob 缓存失效
  pub(super) fn upgrade_dense(&mut self) {
    let mut bytes = vec![0u8; HLL_DENSE_SIZE];
    bytes[0..4].copy_from_slice(HLL_MAGIC);
    bytes[4] = HLL_DENSE;
    write_card(&mut bytes, CARD_INVALID);
    for (idx, v) in sparse_nonzero(&self.bytes) {
      reg_set(&mut bytes[HLL_HEADER_SIZE..], idx, v);
    }
    self.bytes = bytes;
  }
}
