//! 基数估计：对齐 C# `Garnet server/Resp/HyperLogLog/HyperLogLog.cs` 的
//! `CountDenseNCEstimator` / `CountSparseNCEstimator`。
//!
//! 两者共用同一 σ/τ 修正估计公式（Heule et al. "New cardinality estimation
//! algorithms for HyperLogLog sketches"，arXiv:1702.01284）：以寄存器取值直方图
//! 代替浮点调和平均累加，小基数由 σ 修正（零寄存器占比）、大基数由 τ 修正（饱和尾部），
//! 对小基数天然精确（k 个非零寄存器估计 ≈ k），无需额外的线性计数分支。

use crate::{HLL_MAX_COUNT, HLL_REGISTERS, QBIT, unpack_3bytes};

/// 直方图槽位数（C# `stackalloc int[64]`）
pub(super) const HIST_LEN: usize = 64;

/// Flajolet 调和平均系数 α（C# `alpha = 0.721347520444481703680`，无 1.079/m 修正）
const ALPHA: f64 = 0.721_347_520_444_481_7;

/// 统计 Dense 寄存器数据区（12288 字节纯寄存器）的取值直方图
/// （C# `CountDenseNCEstimator` 中按 12 字节 16 寄存器解包累加的循环）
#[inline]
pub(super) fn hist_dense(data: &[u8], hist: &mut [u32; HIST_LEN]) {
  for chunk in data.as_chunks::<3>().0 {
    // 全零块快速路径：4 个寄存器全为 0
    if chunk == &[0; 3] {
      hist[0] += 4;
      continue;
    }
    let (r0, r1, r2, r3) = unpack_3bytes(chunk[0], chunk[1], chunk[2]);
    hist[r0 as usize] += 1;
    hist[r1 as usize] += 1;
    hist[r2 as usize] += 1;
    hist[r3 as usize] += 1;
  }
}

/// 依据寄存器直方图计算最终基数估计（C# 两个 `Count*NCEstimator` 共用的 σ/τ 公式）
pub(super) fn finalize(hist: &[u32; HIST_LEN]) -> u64 {
  let m = HLL_REGISTERS as f64;
  // τ 修正大基数尾部：仅统计取值恰为 qbit+1 的寄存器（C# 同式，52..=63 视为不可见）
  let mut z = m * tau((m - f64::from(hist[QBIT as usize + 1])) / m);
  // Σ r_j·2^-j（秦九韶逐步折半：z = (z + r_j) / 2，j 自 qbit 降至 1）
  z = hist[1..=QBIT as usize]
    .iter()
    .rev()
    .fold(z, |acc, &r| (acc + f64::from(r)) * 0.5);
  // σ 修正小基数零寄存器占比
  z += m * sigma(f64::from(hist[0]) / m);

  let e = ALPHA * m * m / z;
  // C# 在 z == 0（如全部寄存器饱和）时会得到 +inf 并在 long 转换中产生垃圾值，
  // 此处防御性饱和为 HLL_MAX_COUNT（本 crate 测试语义，绝对防 NaN / 防回绕）
  if z == 0.0 || e >= HLL_MAX_COUNT as f64 {
    return HLL_MAX_COUNT;
  }
  e.round() as u64
}

/// C# `cTau`：大基数尾部修正函数（牛顿式迭代逼近）
fn tau(mut x: f64) -> f64 {
  if x == 0.0 || x >= 1.0 {
    return 0.0;
  }
  let (mut y, mut z) = (1.0, 1.0 - x);
  loop {
    x = x.sqrt();
    let prev = z;
    y *= 0.5;
    z -= (1.0 - x) * (1.0 - x) * y;
    if prev == z {
      break;
    }
  }
  z / 3.0
}

/// C# `cSigma`：小基数修正函数（`Σ 2^k·x^2^k`；x == 1 时发散为 +inf，全空 HLL 估计为 0）
fn sigma(x: f64) -> f64 {
  if x >= 1.0 {
    return f64::INFINITY;
  }
  let (mut t, mut y, mut z) = (x, 1.0, x);
  loop {
    t *= t;
    let prev = z;
    z += t * y;
    y += y;
    if prev == z {
      break;
    }
  }
  z
}
