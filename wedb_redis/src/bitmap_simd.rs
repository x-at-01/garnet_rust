//! 基于 fearless_simd 的硬件加速位图操作与位计数模块
//!
//! 1:1 对标 Microsoft Garnet:
//! 1. `IBinaryOperator.cs` 与 `BitmapManagerBitOp.cs`:
//!    - 批量位操作: AND, OR, XOR, NOT, DIFF (ANDNOT)
//!    - 支持 1 个源 (NOT, COPY) 或 N 个源 (AND/OR/XOR/DIFF)
//!    - 256 位 (8x32 u8x32)、128 位 (8x16 u8x16)、64/32/16/8 位标量梯级展开与尾部对齐处理
//! 2. `BitmapManagerBitCount.cs`:
//!    - 位计数 (BITCOUNT): 短字节 (< 128 字节) 采用 4x64-bit 标量展开与 Cache Line 对齐优化
//!    - 大内存块 (>= 128 字节) 采用 SIMD 向量分块硬件加速 (AVX2 / NEON / SSE4.2)
//!    - 支持 BYTE 字节区间与 BIT 位区间统计及部分字节掩码计算

use core::hint::unreachable_unchecked;

use fearless_simd::{Level, Simd, dispatch, prelude::*, u8x16, u8x32};

/// 256 位 SIMD 批处理块大小（8 个 32 字节向量，对标 Garnet 8x32）
const BATCH_256: usize = 256;

/// 单个 256 位向量字节大小
const VEC_32: usize = 32;

/// 单个 128 位向量字节大小
const VEC_16: usize = 16;

/// 64 位标量 4x 展开批处理大小（4 个 8 字节 u64，对标 Garnet 4x8）
const BATCH_SCALAR_32: usize = 32;

/// 单个 64 位标量字节大小
const SCALAR_8: usize = 8;

/// SIMD 位计数启用阈值（小于 128 字节使用 4x64 展开计数，对标 Garnet 128 阈值）
const SIMD_BITCOUNT_THRESHOLD: usize = 128;

/// 位图位操作类型（对标 Garnet `BitmapOperation`）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum BitmapOp {
  And = 0,
  Or = 1,
  Xor = 2,
  Not = 3,
  Diff = 4,
}

/// 位图操作相关错误
#[derive(thiserror::Error, Debug, Clone, PartialEq, Eq)]
pub enum BitmapError {
  #[error("BITOP NOT 操作仅支持单一源位图")]
  NotRequiresSingleSource,
  #[error("BITOP DIFF 操作至少需要两个源位图")]
  DiffRequiresMultipleSources,
  #[error("目标缓冲区长度不足")]
  DestinationBufferTooSmall,
}

/// 二进制运算器接口（对标 Garnet `IBinaryOperator`）
pub trait BinaryOperator: Copy + Send + Sync + 'static {
  /// 对 256 位 SIMD 向量执行二元运算
  fn apply_simd32<S: Simd>(a: u8x32<S>, b: u8x32<S>) -> u8x32<S>;
  /// 对 128 位 SIMD 向量执行二元运算
  fn apply_simd16<S: Simd>(a: u8x16<S>, b: u8x16<S>) -> u8x16<S>;
  /// 对 64 位标量执行二元运算
  fn apply_u64(a: u64, b: u64) -> u64;
  /// 对 32 位标量执行二元运算
  fn apply_u32(a: u32, b: u32) -> u32;
  /// 对 16 位标量执行二元运算
  fn apply_u16(a: u16, b: u16) -> u16;
  /// 对 8 位标量执行二元运算
  fn apply_u8(a: u8, b: u8) -> u8;
  /// 是否为 AND 运算（对于 AND，若某源流耗尽补 0，则结果恒为 0）
  const IS_AND: bool;
}

/// 按位与运算器（对标 `BitwiseAndOperator`）
#[derive(Clone, Copy, Debug)]
pub struct BitwiseAnd;
impl BinaryOperator for BitwiseAnd {
  #[inline(always)]
  fn apply_simd32<S: Simd>(a: u8x32<S>, b: u8x32<S>) -> u8x32<S> {
    a & b
  }
  #[inline(always)]
  fn apply_simd16<S: Simd>(a: u8x16<S>, b: u8x16<S>) -> u8x16<S> {
    a & b
  }
  #[inline(always)]
  fn apply_u64(a: u64, b: u64) -> u64 {
    a & b
  }
  #[inline(always)]
  fn apply_u32(a: u32, b: u32) -> u32 {
    a & b
  }
  #[inline(always)]
  fn apply_u16(a: u16, b: u16) -> u16 {
    a & b
  }
  #[inline(always)]
  fn apply_u8(a: u8, b: u8) -> u8 {
    a & b
  }
  const IS_AND: bool = true;
}

/// 按位或运算器（对标 `BitwiseOrOperator`）
#[derive(Clone, Copy, Debug)]
pub struct BitwiseOr;
impl BinaryOperator for BitwiseOr {
  #[inline(always)]
  fn apply_simd32<S: Simd>(a: u8x32<S>, b: u8x32<S>) -> u8x32<S> {
    a | b
  }
  #[inline(always)]
  fn apply_simd16<S: Simd>(a: u8x16<S>, b: u8x16<S>) -> u8x16<S> {
    a | b
  }
  #[inline(always)]
  fn apply_u64(a: u64, b: u64) -> u64 {
    a | b
  }
  #[inline(always)]
  fn apply_u32(a: u32, b: u32) -> u32 {
    a | b
  }
  #[inline(always)]
  fn apply_u16(a: u16, b: u16) -> u16 {
    a | b
  }
  #[inline(always)]
  fn apply_u8(a: u8, b: u8) -> u8 {
    a | b
  }
  const IS_AND: bool = false;
}

/// 按位异或运算器（对标 `BitwiseXorOperator`）
#[derive(Clone, Copy, Debug)]
pub struct BitwiseXor;
impl BinaryOperator for BitwiseXor {
  #[inline(always)]
  fn apply_simd32<S: Simd>(a: u8x32<S>, b: u8x32<S>) -> u8x32<S> {
    a ^ b
  }
  #[inline(always)]
  fn apply_simd16<S: Simd>(a: u8x16<S>, b: u8x16<S>) -> u8x16<S> {
    a ^ b
  }
  #[inline(always)]
  fn apply_u64(a: u64, b: u64) -> u64 {
    a ^ b
  }
  #[inline(always)]
  fn apply_u32(a: u32, b: u32) -> u32 {
    a ^ b
  }
  #[inline(always)]
  fn apply_u16(a: u16, b: u16) -> u16 {
    a ^ b
  }
  #[inline(always)]
  fn apply_u8(a: u8, b: u8) -> u8 {
    a ^ b
  }
  const IS_AND: bool = false;
}

/// 按位差运算器（对标 `BitwiseAndNotOperator`: a & ~b）
#[derive(Clone, Copy, Debug)]
pub struct BitwiseAndNot;
impl BinaryOperator for BitwiseAndNot {
  #[inline(always)]
  fn apply_simd32<S: Simd>(a: u8x32<S>, b: u8x32<S>) -> u8x32<S> {
    a & !b
  }
  #[inline(always)]
  fn apply_simd16<S: Simd>(a: u8x16<S>, b: u8x16<S>) -> u8x16<S> {
    a & !b
  }
  #[inline(always)]
  fn apply_u64(a: u64, b: u64) -> u64 {
    a & !b
  }
  #[inline(always)]
  fn apply_u32(a: u32, b: u32) -> u32 {
    a & !b
  }
  #[inline(always)]
  fn apply_u16(a: u16, b: u16) -> u16 {
    a & !b
  }
  #[inline(always)]
  fn apply_u8(a: u8, b: u8) -> u8 {
    a & !b
  }
  const IS_AND: bool = false;
}

/// 从裸指针安全无越界检查开销加载 32 字节 SIMD 向量
#[inline(always)]
unsafe fn load_u8x32<S: Simd>(simd: S, ptr: *const u8) -> u8x32<S> {
  // SAFETY: 由调用方保证 ptr..ptr+32 在源缓冲区有效生命周期内且可读；
  // [u8; 32] 内存对齐为 1，无未对齐未定义行为
  unsafe { u8x32::load_array_ref(simd, &*(ptr as *const [u8; 32])) }
}

/// 向裸指针安全无越界检查开销存储 32 字节 SIMD 向量
#[inline(always)]
unsafe fn store_u8x32<S: Simd>(val: u8x32<S>, ptr: *mut u8) {
  // SAFETY: 由调用方保证 ptr..ptr+32 在目标缓冲区有效生命周期内且可写；
  // [u8; 32] 内存对齐为 1，无未对齐未定义行为
  unsafe { val.store_array(&mut *(ptr as *mut [u8; 32])) }
}

/// 从裸指针安全无越界检查开销加载 16 字节 SIMD 向量
#[inline(always)]
unsafe fn load_u8x16<S: Simd>(simd: S, ptr: *const u8) -> u8x16<S> {
  // SAFETY: 由调用方保证 ptr..ptr+16 在源缓冲区有效生命周期内且可读；
  // [u8; 16] 内存对齐为 1，无未对齐未定义行为
  unsafe { u8x16::load_array_ref(simd, &*(ptr as *const [u8; 16])) }
}

/// 向裸指针安全无越界检查开销存储 16 字节 SIMD 向量
#[inline(always)]
unsafe fn store_u8x16<S: Simd>(val: u8x16<S>, ptr: *mut u8) {
  // SAFETY: 由调用方保证 ptr..ptr+16 在目标缓冲区有效生命周期内且可写；
  // [u8; 16] 内存对齐为 1，无未对齐未定义行为
  unsafe { val.store_array(&mut *(ptr as *mut [u8; 16])) }
}

/// 单源按位取反（NOT），高度展开的向量与标量实现
#[inline(always)]
fn bitop_not_simd<S: Simd>(simd: S, dst: &mut [u8], src: &[u8]) {
  let len = src.len();
  let mut offset = 0;
  let src_ptr = src.as_ptr();
  let dst_ptr = dst.as_mut_ptr();

  // 1. 256 字节分块（8x 32 字节 u8x32 循环展开，对应 4 个 Cache Line）
  while offset + BATCH_256 <= len {
    // SAFETY: offset + 256 <= len，src 与 dst 在 offset..offset+256 范围内均有效
    unsafe {
      let d0 = !load_u8x32(simd, src_ptr.add(offset));
      let d1 = !load_u8x32(simd, src_ptr.add(offset + 32));
      let d2 = !load_u8x32(simd, src_ptr.add(offset + 64));
      let d3 = !load_u8x32(simd, src_ptr.add(offset + 96));
      let d4 = !load_u8x32(simd, src_ptr.add(offset + 128));
      let d5 = !load_u8x32(simd, src_ptr.add(offset + 160));
      let d6 = !load_u8x32(simd, src_ptr.add(offset + 192));
      let d7 = !load_u8x32(simd, src_ptr.add(offset + 224));

      store_u8x32(d0, dst_ptr.add(offset));
      store_u8x32(d1, dst_ptr.add(offset + 32));
      store_u8x32(d2, dst_ptr.add(offset + 64));
      store_u8x32(d3, dst_ptr.add(offset + 96));
      store_u8x32(d4, dst_ptr.add(offset + 128));
      store_u8x32(d5, dst_ptr.add(offset + 160));
      store_u8x32(d6, dst_ptr.add(offset + 192));
      store_u8x32(d7, dst_ptr.add(offset + 224));
    }
    offset += BATCH_256;
  }

  // 2. 单个 32 字节 u8x32（处理 0~7 个 32 字节块）
  while offset + VEC_32 <= len {
    // SAFETY: offset + 32 <= len，32 字节在内存有效范围内
    unsafe {
      let d0 = !load_u8x32(simd, src_ptr.add(offset));
      store_u8x32(d0, dst_ptr.add(offset));
    }
    offset += VEC_32;
  }

  // 3. 单个 16 字节 u8x16（由于前步已消化 >= 32 字节，剩余必严格小于 32 字节，至多 1 次）
  if offset + VEC_16 <= len {
    // SAFETY: offset + 16 <= len，16 字节在内存有效范围内
    unsafe {
      let d0 = !load_u8x16(simd, src_ptr.add(offset));
      store_u8x16(d0, dst_ptr.add(offset));
    }
    offset += VEC_16;
  }

  // 4. 64 位标量 1x（8 字节）
  if offset + SCALAR_8 <= len {
    // SAFETY: offset + 8 <= len，8 字节在内存有效范围内
    unsafe {
      let u0 = (src_ptr.add(offset) as *const u64).read_unaligned();
      (dst_ptr.add(offset) as *mut u64).write_unaligned(!u0);
    }
    offset += SCALAR_8;
  }

  // 5. 32 位标量 1x（4 字节）
  if offset + 4 <= len {
    // SAFETY: offset + 4 <= len，4 字节在内存有效范围内
    unsafe {
      let u0 = (src_ptr.add(offset) as *const u32).read_unaligned();
      (dst_ptr.add(offset) as *mut u32).write_unaligned(!u0);
    }
    offset += 4;
  }

  // 6. 16 位标量 1x（2 字节）
  if offset + 2 <= len {
    // SAFETY: offset + 2 <= len，2 字节在内存有效范围内
    unsafe {
      let u0 = (src_ptr.add(offset) as *const u16).read_unaligned();
      (dst_ptr.add(offset) as *mut u16).write_unaligned(!u0);
    }
    offset += 2;
  }

  // 7. 尾部最后 1 个字节
  if offset < len {
    // SAFETY: offset < len，单字节访问安全有效
    unsafe {
      *dst.get_unchecked_mut(offset) = !*src.get_unchecked(offset);
    }
  }
}

/// 双源（2 个输入）位运算核心优化特化实现（消除多源遍历循环开销与冗余切片检查）
#[inline(always)]
fn bitop_simd_core_2<S: Simd, OP: BinaryOperator>(
  simd: S,
  dst: &mut [u8],
  s0: &[u8],
  s1: &[u8],
  shortest_len: usize,
) {
  let mut offset = 0;
  let s0_ptr = s0.as_ptr();
  let s1_ptr = s1.as_ptr();
  let dst_ptr = dst.as_mut_ptr();

  // 1. 256 字节分块（8x 32 字节 u8x32 展开，对应 4 个 Cache Line）
  while offset + BATCH_256 <= shortest_len {
    // SAFETY: offset + 256 <= shortest_len，s0, s1 和 dst 的 offset..offset+256 均有效
    unsafe {
      let d0 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset)),
        load_u8x32(simd, s1_ptr.add(offset)),
      );
      let d1 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset + 32)),
        load_u8x32(simd, s1_ptr.add(offset + 32)),
      );
      let d2 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset + 64)),
        load_u8x32(simd, s1_ptr.add(offset + 64)),
      );
      let d3 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset + 96)),
        load_u8x32(simd, s1_ptr.add(offset + 96)),
      );
      let d4 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset + 128)),
        load_u8x32(simd, s1_ptr.add(offset + 128)),
      );
      let d5 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset + 160)),
        load_u8x32(simd, s1_ptr.add(offset + 160)),
      );
      let d6 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset + 192)),
        load_u8x32(simd, s1_ptr.add(offset + 192)),
      );
      let d7 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset + 224)),
        load_u8x32(simd, s1_ptr.add(offset + 224)),
      );

      store_u8x32(d0, dst_ptr.add(offset));
      store_u8x32(d1, dst_ptr.add(offset + 32));
      store_u8x32(d2, dst_ptr.add(offset + 64));
      store_u8x32(d3, dst_ptr.add(offset + 96));
      store_u8x32(d4, dst_ptr.add(offset + 128));
      store_u8x32(d5, dst_ptr.add(offset + 160));
      store_u8x32(d6, dst_ptr.add(offset + 192));
      store_u8x32(d7, dst_ptr.add(offset + 224));
    }
    offset += BATCH_256;
  }

  // 2. 单个 32 字节 u8x32
  while offset + VEC_32 <= shortest_len {
    // SAFETY: offset + 32 <= shortest_len，32 字节在内存有效范围内
    unsafe {
      let d0 = OP::apply_simd32(
        load_u8x32(simd, s0_ptr.add(offset)),
        load_u8x32(simd, s1_ptr.add(offset)),
      );
      store_u8x32(d0, dst_ptr.add(offset));
    }
    offset += VEC_32;
  }

  // 3. 单个 16 字节 u8x16
  if offset + VEC_16 <= shortest_len {
    // SAFETY: offset + 16 <= shortest_len，16 字节在内存有效范围内
    unsafe {
      let d0 = OP::apply_simd16(
        load_u8x16(simd, s0_ptr.add(offset)),
        load_u8x16(simd, s1_ptr.add(offset)),
      );
      store_u8x16(d0, dst_ptr.add(offset));
    }
    offset += VEC_16;
  }

  // 4. 64 位标量 1x（8 字节）
  if offset + SCALAR_8 <= shortest_len {
    // SAFETY: offset + 8 <= shortest_len，8 字节在内存有效范围内
    unsafe {
      let u0 = (s0_ptr.add(offset) as *const u64).read_unaligned();
      let u1 = (s1_ptr.add(offset) as *const u64).read_unaligned();
      let res = OP::apply_u64(u0, u1);
      (dst_ptr.add(offset) as *mut u64).write_unaligned(res);
    }
    offset += SCALAR_8;
  }

  // 5. 32 位标量 1x（4 字节）
  if offset + 4 <= shortest_len {
    // SAFETY: offset + 4 <= shortest_len，4 字节在内存有效范围内
    unsafe {
      let u0 = (s0_ptr.add(offset) as *const u32).read_unaligned();
      let u1 = (s1_ptr.add(offset) as *const u32).read_unaligned();
      let res = OP::apply_u32(u0, u1);
      (dst_ptr.add(offset) as *mut u32).write_unaligned(res);
    }
    offset += 4;
  }

  // 6. 16 位标量 1x（2 字节）
  if offset + 2 <= shortest_len {
    // SAFETY: offset + 2 <= shortest_len，2 字节在内存有效范围内
    unsafe {
      let u0 = (s0_ptr.add(offset) as *const u16).read_unaligned();
      let u1 = (s1_ptr.add(offset) as *const u16).read_unaligned();
      let res = OP::apply_u16(u0, u1);
      (dst_ptr.add(offset) as *mut u16).write_unaligned(res);
    }
    offset += 2;
  }

  // 7. 尾部最后 1 个字节
  if offset < shortest_len {
    // SAFETY: offset < shortest_len，单字节访问完全在源与目标有效切片内
    unsafe {
      *dst.get_unchecked_mut(offset) =
        OP::apply_u8(*s0.get_unchecked(offset), *s1.get_unchecked(offset));
    }
    offset += 1;
  }

  // 8. 处理超出 shortest_len 的尾部字节（基于代数恒等式的块级内存拷贝与填零）
  let dst_len = dst.len();
  if offset < dst_len {
    if OP::IS_AND {
      // AND 运算：任何数与缺失项（补 0）相与均为 0
      dst[offset..dst_len].fill(0);
    } else if s0.len() >= s1.len() {
      // s0 较长：OR(s0, 0)=s0, XOR(s0, 0)=s0, DIFF(s0, 0)=s0
      dst[offset..dst_len].copy_from_slice(&s0[offset..dst_len]);
    } else {
      // s1 较长：s0 已结束（补 0）
      // DIFF(0, s1) = 0 & !s1 = 0；而 OR/XOR 为 s1
      // DIFF 算子首项为 0 时结果恒为 0
      if OP::apply_u8(0, 0xFF) == 0 {
        dst[offset..dst_len].fill(0);
      } else {
        dst[offset..dst_len].copy_from_slice(&s1[offset..dst_len]);
      }
    }
  }
}

/// N 源（>= 3 个输入）通用多源位运算实现（对标 Garnet `InvokeNaryBitwiseOperation`）
#[inline(always)]
fn bitop_simd_core_n<S: Simd, OP: BinaryOperator>(
  simd: S,
  dst: &mut [u8],
  sources: &[&[u8]],
  shortest_len: usize,
) {
  let mut offset = 0;
  let dst_ptr = dst.as_mut_ptr();

  // 1. 256 字节分块（8x 32 字节 u8x32 展开）
  while offset + BATCH_256 <= shortest_len {
    // SAFETY: offset + 256 <= shortest_len，所有 sources 与 dst 在当前区间合法有效
    unsafe {
      let mut d0 = load_u8x32(simd, sources[0].as_ptr().add(offset));
      let mut d1 = load_u8x32(simd, sources[0].as_ptr().add(offset + 32));
      let mut d2 = load_u8x32(simd, sources[0].as_ptr().add(offset + 64));
      let mut d3 = load_u8x32(simd, sources[0].as_ptr().add(offset + 96));
      let mut d4 = load_u8x32(simd, sources[0].as_ptr().add(offset + 128));
      let mut d5 = load_u8x32(simd, sources[0].as_ptr().add(offset + 160));
      let mut d6 = load_u8x32(simd, sources[0].as_ptr().add(offset + 192));
      let mut d7 = load_u8x32(simd, sources[0].as_ptr().add(offset + 224));

      for src in &sources[1..] {
        let s_ptr = src.as_ptr();
        d0 = OP::apply_simd32(d0, load_u8x32(simd, s_ptr.add(offset)));
        d1 = OP::apply_simd32(d1, load_u8x32(simd, s_ptr.add(offset + 32)));
        d2 = OP::apply_simd32(d2, load_u8x32(simd, s_ptr.add(offset + 64)));
        d3 = OP::apply_simd32(d3, load_u8x32(simd, s_ptr.add(offset + 96)));
        d4 = OP::apply_simd32(d4, load_u8x32(simd, s_ptr.add(offset + 128)));
        d5 = OP::apply_simd32(d5, load_u8x32(simd, s_ptr.add(offset + 160)));
        d6 = OP::apply_simd32(d6, load_u8x32(simd, s_ptr.add(offset + 192)));
        d7 = OP::apply_simd32(d7, load_u8x32(simd, s_ptr.add(offset + 224)));
      }

      store_u8x32(d0, dst_ptr.add(offset));
      store_u8x32(d1, dst_ptr.add(offset + 32));
      store_u8x32(d2, dst_ptr.add(offset + 64));
      store_u8x32(d3, dst_ptr.add(offset + 96));
      store_u8x32(d4, dst_ptr.add(offset + 128));
      store_u8x32(d5, dst_ptr.add(offset + 160));
      store_u8x32(d6, dst_ptr.add(offset + 192));
      store_u8x32(d7, dst_ptr.add(offset + 224));
    }
    offset += BATCH_256;
  }

  // 2. 单个 32 字节 u8x32
  while offset + VEC_32 <= shortest_len {
    // SAFETY: offset + 32 <= shortest_len，32 字节在内存有效范围内
    unsafe {
      let mut d0 = load_u8x32(simd, sources[0].as_ptr().add(offset));
      for src in &sources[1..] {
        d0 = OP::apply_simd32(d0, load_u8x32(simd, src.as_ptr().add(offset)));
      }
      store_u8x32(d0, dst_ptr.add(offset));
    }
    offset += VEC_32;
  }

  // 3. 单个 16 字节 u8x16
  if offset + VEC_16 <= shortest_len {
    // SAFETY: offset + 16 <= shortest_len，16 字节在内存有效范围内
    unsafe {
      let mut d0 = load_u8x16(simd, sources[0].as_ptr().add(offset));
      for src in &sources[1..] {
        d0 = OP::apply_simd16(d0, load_u8x16(simd, src.as_ptr().add(offset)));
      }
      store_u8x16(d0, dst_ptr.add(offset));
    }
    offset += VEC_16;
  }

  // 4. 64 位标量 1x（8 字节）
  if offset + SCALAR_8 <= shortest_len {
    // SAFETY: offset + 8 <= shortest_len，8 字节在内存有效范围内
    unsafe {
      let mut u0 = (sources[0].as_ptr().add(offset) as *const u64).read_unaligned();
      for src in &sources[1..] {
        let u = (src.as_ptr().add(offset) as *const u64).read_unaligned();
        u0 = OP::apply_u64(u0, u);
      }
      (dst_ptr.add(offset) as *mut u64).write_unaligned(u0);
    }
    offset += SCALAR_8;
  }

  // 5. 32 位标量 1x（4 字节）
  if offset + 4 <= shortest_len {
    // SAFETY: offset + 4 <= shortest_len，4 字节在内存有效范围内
    unsafe {
      let mut u0 = (sources[0].as_ptr().add(offset) as *const u32).read_unaligned();
      for src in &sources[1..] {
        let u = (src.as_ptr().add(offset) as *const u32).read_unaligned();
        u0 = OP::apply_u32(u0, u);
      }
      (dst_ptr.add(offset) as *mut u32).write_unaligned(u0);
    }
    offset += 4;
  }

  // 6. 16 位标量 1x（2 字节）
  if offset + 2 <= shortest_len {
    // SAFETY: offset + 2 <= shortest_len，2 字节在内存有效范围内
    unsafe {
      let mut u0 = (sources[0].as_ptr().add(offset) as *const u16).read_unaligned();
      for src in &sources[1..] {
        let u = (src.as_ptr().add(offset) as *const u16).read_unaligned();
        u0 = OP::apply_u16(u0, u);
      }
      (dst_ptr.add(offset) as *mut u16).write_unaligned(u0);
    }
    offset += 2;
  }

  // 7. 最短对齐边界内的最后 1 个字节
  if offset < shortest_len {
    // SAFETY: offset < shortest_len，单字节在源与目标有效切片内
    unsafe {
      let mut d0 = *sources[0].get_unchecked(offset);
      for src in &sources[1..] {
        d0 = OP::apply_u8(d0, *src.get_unchecked(offset));
      }
      *dst.get_unchecked_mut(offset) = d0;
    }
    offset += 1;
  }

  // 8. 处理超出 shortest_len 的尾部字节
  let dst_len = dst.len();
  if offset < dst_len {
    if OP::IS_AND {
      // AND 运算中，只要有任意源耗尽（补 0），结果恒为 0
      dst[offset..dst_len].fill(0);
    } else if OP::apply_u8(0, 0xFF) == 0 {
      // DIFF 运算中，如果首项 sources[0] 耗尽（首项为 0），0 & !src 恒为 0
      if offset >= sources[0].len() {
        dst[offset..dst_len].fill(0);
      } else {
        while offset < dst_len {
          let mut d0 = if offset < sources[0].len() {
            sources[0][offset]
          } else {
            0
          };
          for src in &sources[1..] {
            if offset < src.len() {
              d0 = OP::apply_u8(d0, src[offset]);
            }
          }
          dst[offset] = d0;
          offset += 1;
        }
      }
    } else {
      while offset < dst_len {
        let mut d0 = if offset < sources[0].len() {
          sources[0][offset]
        } else {
          0
        };

        for src in &sources[1..] {
          if offset < src.len() {
            d0 = OP::apply_u8(d0, src[offset]);
          }
        }

        dst[offset] = d0;
        offset += 1;
      }
    }
  }
}

/// 执行单源取反位运算并将结果写入目标切片 (BITOP NOT)
#[inline]
pub fn simd_bitop_not(src: &[u8], dst: &mut [u8]) -> Result<usize, BitmapError> {
  if dst.len() < src.len() {
    return Err(BitmapError::DestinationBufferTooSmall);
  }
  let level = Level::new();
  dispatch!(level, simd => bitop_not_simd(simd, dst, src));
  Ok(src.len())
}

/// 执行双源位运算并将结果写入目标切片 (BITOP AND/OR/XOR/DIFF)
#[inline]
pub fn simd_bitop_binary(
  op: BitmapOp,
  s0: &[u8],
  s1: &[u8],
  dst: &mut [u8],
) -> Result<usize, BitmapError> {
  if op == BitmapOp::Not {
    return Err(BitmapError::NotRequiresSingleSource);
  }
  let l0 = s0.len();
  let l1 = s1.len();
  let shortest_len = l0.min(l1);
  let max_len = l0.max(l1);
  if dst.len() < max_len {
    return Err(BitmapError::DestinationBufferTooSmall);
  }

  let level = Level::new();
  dispatch!(level, simd => {
    match op {
      BitmapOp::And => bitop_simd_core_2::<_, BitwiseAnd>(simd, dst, s0, s1, shortest_len),
      BitmapOp::Or => bitop_simd_core_2::<_, BitwiseOr>(simd, dst, s0, s1, shortest_len),
      BitmapOp::Xor => bitop_simd_core_2::<_, BitwiseXor>(simd, dst, s0, s1, shortest_len),
      BitmapOp::Diff => bitop_simd_core_2::<_, BitwiseAndNot>(simd, dst, s0, s1, shortest_len),
      BitmapOp::Not => unreachable!(),
    }
  });

  Ok(max_len)
}

/// 执行位图运算并将结果写入目标切片 (BITOP)
///
/// 对标 Garnet `BitmapManager.InvokeBitOperationUnsafe`:
/// - `op`: 位操作类型 (AND, OR, XOR, NOT, DIFF)
/// - `sources`: 1 个或多个源位图切片
/// - `dst`: 目标位图缓冲区，其长度必须至少为 max(sources.len())
///
/// 返回写入的有效字节长度。
pub fn simd_bitop(op: BitmapOp, sources: &[&[u8]], dst: &mut [u8]) -> Result<usize, BitmapError> {
  match sources {
    [] => Ok(0),
    [src] => {
      if op == BitmapOp::Not {
        simd_bitop_not(src, dst)
      } else if op == BitmapOp::Diff {
        Err(BitmapError::DiffRequiresMultipleSources)
      } else {
        if dst.len() < src.len() {
          return Err(BitmapError::DestinationBufferTooSmall);
        }
        dst[..src.len()].copy_from_slice(src);
        Ok(src.len())
      }
    }
    [s0, s1] => simd_bitop_binary(op, s0, s1, dst),
    _ => {
      if op == BitmapOp::Not {
        return Err(BitmapError::NotRequiresSingleSource);
      }
      let (shortest_len, max_len) = sources.iter().fold((usize::MAX, 0usize), |(min, max), s| {
        let l = s.len();
        (min.min(l), max.max(l))
      });
      if dst.len() < max_len {
        return Err(BitmapError::DestinationBufferTooSmall);
      }

      let level = Level::new();
      dispatch!(level, simd => {
        match op {
          BitmapOp::And => bitop_simd_core_n::<_, BitwiseAnd>(simd, dst, sources, shortest_len),
          BitmapOp::Or => bitop_simd_core_n::<_, BitwiseOr>(simd, dst, sources, shortest_len),
          BitmapOp::Xor => bitop_simd_core_n::<_, BitwiseXor>(simd, dst, sources, shortest_len),
          BitmapOp::Diff => {
            bitop_simd_core_n::<_, BitwiseAndNot>(simd, dst, sources, shortest_len)
          }
          BitmapOp::Not => unreachable!(),
        }
      });

      Ok(max_len)
    }
  }
}

/// 执行位图操作并返回新分配的 Vec<u8>
#[inline]
pub fn simd_bitop_alloc(op: BitmapOp, sources: &[&[u8]]) -> Result<Vec<u8>, BitmapError> {
  if sources.is_empty() {
    return Ok(Vec::new());
  }
  let max_len = sources.iter().map(|s| s.len()).max().unwrap_or(0);
  let mut dst = vec![0u8; max_len];
  simd_bitop(op, sources, &mut dst)?;
  Ok(dst)
}

// =========================================================================
// 位计数 (BITCOUNT) 硬件加速与分块算法实现（对标 BitmapManagerBitCount.cs）
// =========================================================================

/// 4x 64 位标量循环展开与硬件缓存行对齐计数（对标 Garnet `__scalar_popc`）
#[inline(always)]
pub fn scalar_popcount_4x64(bytes: &[u8]) -> usize {
  let len = bytes.len();
  if len == 0 {
    return 0;
  }
  let mut count = 0usize;
  let ptr = bytes.as_ptr();

  // 1. 缓存行与内存对齐剥离：若首地址未 8 字节对齐，先剥离前导非对齐字节
  // 使后续所有 64 位宽字读取天然落入单个 64 字节 Cache Line 内部，杜绝跨 Cache Line 分割读取开销
  let align_offset = (8 - (ptr as usize & 7)) & 7;
  let peel = align_offset.min(len);
  for i in 0..peel {
    // SAFETY: i < peel <= len，bytes 切片在此索引处必定合法
    unsafe {
      count += bytes.get_unchecked(i).count_ones() as usize;
    }
  }
  let mut offset = peel;

  // 2. 32 字节批次（4 个 8 字节 u64 循环展开）
  while offset + BATCH_SCALAR_32 <= len {
    // SAFETY: offset + 32 <= len，ptr.add(offset) 开始的 32 字节在 bytes 切片内均有效，
    // 且前面已对齐到 8 字节边界，read_unaligned 将生成单条硬件快速字加载指令
    unsafe {
      let u0 = (ptr.add(offset) as *const u64)
        .read_unaligned()
        .count_ones() as usize;
      let u1 = (ptr.add(offset + 8) as *const u64)
        .read_unaligned()
        .count_ones() as usize;
      let u2 = (ptr.add(offset + 16) as *const u64)
        .read_unaligned()
        .count_ones() as usize;
      let u3 = (ptr.add(offset + 24) as *const u64)
        .read_unaligned()
        .count_ones() as usize;
      count += (u0 + u1) + (u2 + u3);
    }
    offset += BATCH_SCALAR_32;
  }

  // 3. 8 字节单个 u64
  while offset + SCALAR_8 <= len {
    // SAFETY: offset + 8 <= len，8 字节在内存有效范围内
    unsafe {
      let u = (ptr.add(offset) as *const u64)
        .read_unaligned()
        .count_ones() as usize;
      count += u;
    }
    offset += SCALAR_8;
  }

  // 4. 4 字节单个 u32（单条硬件指令直接计算，无需逐字节遍历）
  if offset + 4 <= len {
    // SAFETY: offset + 4 <= len，4 字节在内存有效范围内
    unsafe {
      let u = (ptr.add(offset) as *const u32)
        .read_unaligned()
        .count_ones() as usize;
      count += u;
    }
    offset += 4;
  }

  // 5. 2 字节单个 u16
  if offset + 2 <= len {
    // SAFETY: offset + 2 <= len，2 字节在内存有效范围内
    unsafe {
      let u = (ptr.add(offset) as *const u16)
        .read_unaligned()
        .count_ones() as usize;
      count += u;
    }
    offset += 2;
  }

  // 6. 尾部最后 1 个字节
  if offset < len {
    // SAFETY: offset < len，单字节访问完全在有效切片内
    unsafe {
      count += bytes.get_unchecked(offset).count_ones() as usize;
    }
  }

  count
}

#[cfg(target_arch = "aarch64")]
fearless_simd::kernel!(
  /// ARM64 NEON 硬件向量位图计数（采用 vcntq_u8 向量指令、树状无进位累加与零流水线阻塞优化）
  #[inline]
  fn neon_popcount(_neon: Neon, bytes: &[u8]) -> usize {
    use core::arch::aarch64::*;
    let len = bytes.len();
    let mut curr = bytes.as_ptr();
    let batch_size = 16 * 8; // 128 字节批次（8 个 128 位向量）
    let tail_len = len & (batch_size - 1);
    // SAFETY: curr 指向 bytes 切片首地址，len - tail_len <= len，指针合法且在 bytes 范围内
    let batch_end = unsafe { curr.add(len - tail_len) };
    // 初始化 128 位全零 u32 向量累加器，单 lane 最大可容纳 42 亿，永不溢出
    let mut acc = vdupq_n_u32(0);

    while curr < batch_end {
      // SAFETY: batch_end 确保当前循环内连续读取 128 字节（curr..curr+128）在 bytes 有效生命周期内
      unsafe {
        let v0 = vcntq_u8(vld1q_u8(curr));
        let v1 = vcntq_u8(vld1q_u8(curr.add(16)));
        let v2 = vcntq_u8(vld1q_u8(curr.add(32)));
        let v3 = vcntq_u8(vld1q_u8(curr.add(48)));
        let v4 = vcntq_u8(vld1q_u8(curr.add(64)));
        let v5 = vcntq_u8(vld1q_u8(curr.add(80)));
        let v6 = vcntq_u8(vld1q_u8(curr.add(96)));
        let v7 = vcntq_u8(vld1q_u8(curr.add(112)));

        // 树状两两并行累加：每个 byte lane 最大值为 8，8 个相加最大为 64 <= 255，绝无 u8 溢出
        let sum01 = vaddq_u8(v0, v1);
        let sum23 = vaddq_u8(v2, v3);
        let sum45 = vaddq_u8(v4, v5);
        let sum67 = vaddq_u8(v6, v7);

        let sum03 = vaddq_u8(sum01, sum23);
        let sum47 = vaddq_u8(sum45, sum67);

        let sum_all = vaddq_u8(sum03, sum47);

        // 拓宽累加：将 16 个 u8 向量元素拓宽折叠为 8 个 u16 元素（每个 u16 最大值为 128）
        let sum_u16 = vpaddlq_u8(sum_all);

        // 向量级累加进 u32 累加器（消除循环内部的跨向量折叠标量同步停顿）
        acc = vpadalq_u16(acc, sum_u16);

        curr = curr.add(batch_size);
      }
    }

    // 处理 16 字节整数倍的剩余块
    // SAFETY: bytes.as_ptr() + len 在切片合法边界内
    let end = unsafe { bytes.as_ptr().add(len) };
    while unsafe { curr.add(16) } <= end {
      // SAFETY: curr + 16 <= end 保证连续读取 16 字节有效
      unsafe {
        let v0 = vcntq_u8(vld1q_u8(curr));
        let sum_u16 = vpaddlq_u8(v0);
        acc = vpadalq_u16(acc, sum_u16);
        curr = curr.add(16);
      }
    }

    // 统一在循环外部执行一次跨通道规约（Reduction），避免 CPU 向量流水线停顿
    let mut total = vaddvq_u32(acc) as usize;

    let offset = (curr as usize) - (bytes.as_ptr() as usize);
    if offset < len {
      total += scalar_popcount_4x64(&bytes[offset..]);
    }

    total
  }
);

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fearless_simd::kernel!(
  /// x86/x86_64 AVX2 256 位位计数（对标 Garnet `__simd_popcX256`）
  #[inline]
  fn avx2_popcount(_avx2: Avx2, bytes: &[u8]) -> usize {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    let len = bytes.len();
    let mut curr = bytes.as_ptr();
    let batch_size = 32 * 8; // 256 字节批次
    let tail_len = len & (batch_size - 1);
    // SAFETY: curr 指向 bytes 首地址，len - tail_len <= len，指针合法
    let batch_end = unsafe { curr.add(len - tail_len) };

    unsafe {
      let lookup = _mm256_setr_epi8(
        0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4, 0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3,
        3, 4,
      );
      let mask = _mm256_set1_epi8(0x0f);
      let mut acc = _mm256_setzero_si256();

      while curr < batch_end {
        // SAFETY: batch_end 确保当前连续读取 256 字节均在 bytes 切片合法内存内
        let x0 = _mm256_loadu_si256(curr as *const __m256i);
        let x1 = _mm256_loadu_si256(curr.add(32) as *const __m256i);
        let x2 = _mm256_loadu_si256(curr.add(64) as *const __m256i);
        let x3 = _mm256_loadu_si256(curr.add(96) as *const __m256i);
        let x4 = _mm256_loadu_si256(curr.add(128) as *const __m256i);
        let x5 = _mm256_loadu_si256(curr.add(160) as *const __m256i);
        let x6 = _mm256_loadu_si256(curr.add(192) as *const __m256i);
        let x7 = _mm256_loadu_si256(curr.add(224) as *const __m256i);

        let popc_step = |x: __m256i| -> __m256i {
          let low = _mm256_shuffle_epi8(lookup, _mm256_and_si256(x, mask));
          let high = _mm256_shuffle_epi8(lookup, _mm256_and_si256(_mm256_srli_epi16(x, 4), mask));
          _mm256_add_epi8(low, high)
        };

        let p0 = popc_step(x0);
        let p1 = popc_step(x1);
        let p2 = popc_step(x2);
        let p3 = popc_step(x3);
        let p4 = popc_step(x4);
        let p5 = popc_step(x5);
        let p6 = popc_step(x6);
        let p7 = popc_step(x7);

        let sum01 = _mm256_add_epi8(p0, p1);
        let sum23 = _mm256_add_epi8(p2, p3);
        let sum45 = _mm256_add_epi8(p4, p5);
        let sum67 = _mm256_add_epi8(p6, p7);

        let sum03 = _mm256_add_epi8(sum01, sum23);
        let sum47 = _mm256_add_epi8(sum45, sum67);

        let sum_all = _mm256_add_epi8(sum03, sum47);
        let sad = _mm256_sad_epu8(sum_all, _mm256_setzero_si256());
        acc = _mm256_add_epi64(acc, sad);

        curr = curr.add(batch_size);
      }

      let end = bytes.as_ptr().add(len);
      // 64 字节梯级（2x 32 字节向量）
      while curr.add(64) <= end {
        // SAFETY: curr + 64 <= end 保证连续读取 64 字节有效
        let x0 = _mm256_loadu_si256(curr as *const __m256i);
        let x1 = _mm256_loadu_si256(curr.add(32) as *const __m256i);
        let p0 = _mm256_add_epi8(
          _mm256_shuffle_epi8(lookup, _mm256_and_si256(x0, mask)),
          _mm256_shuffle_epi8(lookup, _mm256_and_si256(_mm256_srli_epi16(x0, 4), mask)),
        );
        let p1 = _mm256_add_epi8(
          _mm256_shuffle_epi8(lookup, _mm256_and_si256(x1, mask)),
          _mm256_shuffle_epi8(lookup, _mm256_and_si256(_mm256_srli_epi16(x1, 4), mask)),
        );
        let sum = _mm256_add_epi8(p0, p1);
        let sad = _mm256_sad_epu8(sum, _mm256_setzero_si256());
        acc = _mm256_add_epi64(acc, sad);
        curr = curr.add(64);
      }
      // 32 字节梯级（1x 32 字节向量）
      if curr.add(32) <= end {
        // SAFETY: curr + 32 <= end 保证连续读取 32 字节有效
        let x0 = _mm256_loadu_si256(curr as *const __m256i);
        let low = _mm256_shuffle_epi8(lookup, _mm256_and_si256(x0, mask));
        let high = _mm256_shuffle_epi8(lookup, _mm256_and_si256(_mm256_srli_epi16(x0, 4), mask));
        let p0 = _mm256_add_epi8(low, high);
        let sad = _mm256_sad_epu8(p0, _mm256_setzero_si256());
        acc = _mm256_add_epi64(acc, sad);
        curr = curr.add(32);
      }

      let mut res = [0u64; 4];
      // SAFETY: res 数组有 4 个 u64（32 字节），完全匹配 __m256i 大小
      _mm256_storeu_si256(res.as_mut_ptr() as *mut __m256i, acc);
      let mut total = (res[0] + res[1] + res[2] + res[3]) as usize;

      let offset = (curr as usize) - (bytes.as_ptr() as usize);
      if offset < len {
        total += scalar_popcount_4x64(&bytes[offset..]);
      }

      total
    }
  }
);

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fearless_simd::kernel!(
  /// x86/x86_64 SSE4.2 / SSSE3 128 位位计数（对标 Garnet `__simd_popcX128`）
  #[inline]
  fn sse4_2_popcount(_sse4_2: Sse4_2, bytes: &[u8]) -> usize {
    #[cfg(target_arch = "x86")]
    use core::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use core::arch::x86_64::*;

    let len = bytes.len();
    let mut curr = bytes.as_ptr();
    let batch_size = 16 * 8; // 128 字节批次
    let tail_len = len & (batch_size - 1);
    // SAFETY: curr 指向 bytes 首地址，len - tail_len <= len，指针合法
    let batch_end = unsafe { curr.add(len - tail_len) };

    unsafe {
      let lookup = _mm_setr_epi8(0, 1, 1, 2, 1, 2, 2, 3, 1, 2, 2, 3, 2, 3, 3, 4);
      let mask = _mm_set1_epi8(0x0f);
      let mut acc = _mm_setzero_si128();

      while curr < batch_end {
        // SAFETY: batch_end 确保当前连续读取 128 字节均在合法范围内
        let x0 = _mm_loadu_si128(curr as *const __m128i);
        let x1 = _mm_loadu_si128(curr.add(16) as *const __m128i);
        let x2 = _mm_loadu_si128(curr.add(32) as *const __m128i);
        let x3 = _mm_loadu_si128(curr.add(48) as *const __m128i);
        let x4 = _mm_loadu_si128(curr.add(64) as *const __m128i);
        let x5 = _mm_loadu_si128(curr.add(80) as *const __m128i);
        let x6 = _mm_loadu_si128(curr.add(96) as *const __m128i);
        let x7 = _mm_loadu_si128(curr.add(112) as *const __m128i);

        let popc_step = |x: __m128i| -> __m128i {
          let low = _mm_shuffle_epi8(lookup, _mm_and_si128(x, mask));
          let high = _mm_shuffle_epi8(lookup, _mm_and_si128(_mm_srli_epi16(x, 4), mask));
          _mm_add_epi8(low, high)
        };

        let p0 = popc_step(x0);
        let p1 = popc_step(x1);
        let p2 = popc_step(x2);
        let p3 = popc_step(x3);
        let p4 = popc_step(x4);
        let p5 = popc_step(x5);
        let p6 = popc_step(x6);
        let p7 = popc_step(x7);

        let sum01 = _mm_add_epi8(p0, p1);
        let sum23 = _mm_add_epi8(p2, p3);
        let sum45 = _mm_add_epi8(p4, p5);
        let sum67 = _mm_add_epi8(p6, p7);

        let sum03 = _mm_add_epi8(sum01, sum23);
        let sum47 = _mm_add_epi8(sum45, sum67);

        let sum_all = _mm_add_epi8(sum03, sum47);
        let sad = _mm_sad_epu8(sum_all, _mm_setzero_si128());
        acc = _mm_add_epi64(acc, sad);

        curr = curr.add(batch_size);
      }

      let end = bytes.as_ptr().add(len);
      // 32 字节梯级（2x 16 字节向量）
      while curr.add(32) <= end {
        // SAFETY: curr + 32 <= end 保证连续读取 32 字节有效
        let x0 = _mm_loadu_si128(curr as *const __m128i);
        let x1 = _mm_loadu_si128(curr.add(16) as *const __m128i);
        let p0 = _mm_add_epi8(
          _mm_shuffle_epi8(lookup, _mm_and_si128(x0, mask)),
          _mm_shuffle_epi8(lookup, _mm_and_si128(_mm_srli_epi16(x0, 4), mask)),
        );
        let p1 = _mm_add_epi8(
          _mm_shuffle_epi8(lookup, _mm_and_si128(x1, mask)),
          _mm_shuffle_epi8(lookup, _mm_and_si128(_mm_srli_epi16(x1, 4), mask)),
        );
        let sum = _mm_add_epi8(p0, p1);
        let sad = _mm_sad_epu8(sum, _mm_setzero_si128());
        acc = _mm_add_epi64(acc, sad);
        curr = curr.add(32);
      }
      // 16 字节梯级（1x 16 字节向量）
      if curr.add(16) <= end {
        // SAFETY: curr + 16 <= end 保证连续读取 16 字节有效
        let x0 = _mm_loadu_si128(curr as *const __m128i);
        let low = _mm_shuffle_epi8(lookup, _mm_and_si128(x0, mask));
        let high = _mm_shuffle_epi8(lookup, _mm_and_si128(_mm_srli_epi16(x0, 4), mask));
        let p0 = _mm_add_epi8(low, high);
        let sad = _mm_sad_epu8(p0, _mm_setzero_si128());
        acc = _mm_add_epi64(acc, sad);
        curr = curr.add(16);
      }

      let mut res = [0u64; 2];
      // SAFETY: res 数组有 2 个 u64（16 字节），完全匹配 __m128i 大小
      _mm_storeu_si128(res.as_mut_ptr() as *mut __m128i, acc);
      let mut total = (res[0] + res[1]) as usize;

      let offset = (curr as usize) - (bytes.as_ptr() as usize);
      if offset < len {
        total += scalar_popcount_4x64(&bytes[offset..]);
      }

      total
    }
  }
);

/// 统计字节切片中所有置 1 的位数 (BITCOUNT)
///
/// 对标 Garnet `BitmapManagerBitCount.cs`:
/// - 短字节 (< 128 字节): 使用 4x64-bit 展开与 Cache Line 对齐计数
/// - 大内存块 (>= 128 字节): 利用硬件向量加速（AVX2 / NEON / SSE4.2 / 标量展开）
#[inline]
pub fn simd_bit_count(bytes: &[u8]) -> usize {
  if bytes.len() < SIMD_BITCOUNT_THRESHOLD {
    return scalar_popcount_4x64(bytes);
  }

  #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
  {
    let level = Level::new();
    if let Some(avx2) = level.as_avx2() {
      return avx2_popcount(avx2, bytes);
    }
    if let Some(sse4_2) = level.as_sse4_2() {
      return sse4_2_popcount(sse4_2, bytes);
    }
  }

  #[cfg(target_arch = "aarch64")]
  {
    let level = Level::new();
    if let Some(neon) = level.as_neon() {
      return neon_popcount(neon, bytes);
    }
  }

  scalar_popcount_4x64(bytes)
}

/// 在单个字节中统计 [start_bit, end_bit) 范围内的 1 的个数
///
/// bit 0 对应最高有效位 (MSB 0x80)，bit 7 对应最低有效位 (LSB 0x01)（对标 Redis/Garnet `BitIndexCount`）
///
/// 纯算术位移掩码实现，零 reverse_bits 指令开销，零 panic 风险。
#[inline(always)]
pub fn bit_index_count_byte(byte: u8, start_bit: usize, end_bit: usize) -> usize {
  let start_bit = start_bit.min(8);
  let end_bit = end_bit.min(8);
  if start_bit >= end_bit {
    return 0;
  }
  let k = end_bit - start_bit;
  let val = byte << start_bit;
  let mask = !0u8 << (8 - k);
  (val & mask).count_ones() as usize
}

/// 按照字节或位区间范围统计位图中置 1 的位数 (BITCOUNT [start end [BYTE|BIT]])
///
/// 对标 Garnet `BitCountDriver`。
pub fn simd_bit_count_range(
  bytes: &[u8],
  start_offset: isize,
  end_offset: isize,
  is_bit_index: bool,
) -> usize {
  if bytes.is_empty() {
    return 0;
  }

  if !is_bit_index {
    // 字节模式 (BYTE)
    let len = bytes.len() as isize;
    let mut s = if start_offset < 0 {
      start_offset.saturating_add(len)
    } else {
      start_offset
    };
    let mut e = if end_offset < 0 {
      end_offset.saturating_add(len)
    } else {
      end_offset
    };
    if s < 0 {
      s = 0;
    }
    if e >= len {
      e = len - 1;
    }
    if s > e || s >= len {
      return 0;
    }
    simd_bit_count(&bytes[s as usize..=e as usize])
  } else {
    // 位模式 (BIT)
    let bit_len = (bytes.len() as isize).saturating_mul(8);
    if bit_len == 0 {
      return 0;
    }
    let mut s = if start_offset < 0 {
      start_offset.saturating_add(bit_len)
    } else {
      start_offset
    };
    let mut e = if end_offset < 0 {
      end_offset.saturating_add(bit_len)
    } else {
      end_offset
    };
    if s < 0 {
      s = 0;
    }
    if e >= bit_len {
      e = bit_len - 1;
    }
    if s > e || s >= bit_len {
      return 0;
    }

    let start_bit = s as usize;
    let end_bit = e as usize;

    let start_byte = start_bit / 8;
    let end_byte = end_bit / 8;

    if start_byte == end_byte {
      bit_index_count_byte(bytes[start_byte], start_bit & 7, (end_bit & 7) + 1)
    } else {
      let mut count = bit_index_count_byte(bytes[start_byte], start_bit & 7, 8)
        + bit_index_count_byte(bytes[end_byte], 0, (end_bit & 7) + 1);
      let inner_start = start_byte + 1;
      let inner_end = end_byte;
      if inner_start < inner_end {
        count += simd_bit_count(&bytes[inner_start..inner_end]);
      }
      count
    }
  }
}

// =========================================================================
// 位图查找 (BITPOS) 驱动与硬件向量加速算法（对标 BitmapManagerBitPos.cs）
// =========================================================================

/// 预计算的位掩码表，索引 k 对应 (0xFF >> k)
pub const BIT_MASK_TABLE: [u8; 9] = [0xFF, 0x7F, 0x3F, 0x1F, 0x0F, 0x07, 0x03, 0x01, 0x00];

/// 预计算的位区间掩码查找表（2D LUT），索引为 `[left_bit_offset][right_bit_offset]`
///
/// 其中 `left_bit_offset` 取值范围为 `0..=8`，`right_bit_offset` 取值范围为 `0..=8`。
/// 对应掩码公式：`(0xFF >> left) ^ (0xFF >> right)`。
/// 该表仅占用 81 字节（天然落入单个 L1D Cache Line），在运行时通过单次内存直接读取替代位移与异或运算。
pub const BIT_RANGE_MASK: [[u8; 9]; 9] = {
  let mut table = [[0u8; 9]; 9];
  let mut left = 0;
  while left <= 8 {
    let mut right = 0;
    while right <= 8 {
      let m_left = (0xFFu16 >> left) as u8;
      let m_right = (0xFFu16 >> right) as u8;
      table[left][right] = m_left ^ m_right;
      right += 1;
    }
    left += 1;
  }
  table
};

/// 128 字节 SIMD 4x 展开批处理大小
const BATCH_128: usize = 128;

/// BITPOS 偏移类型（BYTE 字节偏移 或 BIT 位偏移）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum BitPosOffsetType {
  #[default]
  Byte = 0x0,
  Bit = 0x1,
}

/// 偏移类型：BYTE 字节偏移
pub const OFFSET_TYPE_BYTE: u8 = BitPosOffsetType::Byte as u8;

/// 偏移类型：BIT 位偏移
pub const OFFSET_TYPE_BIT: u8 = BitPosOffsetType::Bit as u8;

/// 处理负偏移量（按模长循环回绕，对标 Garnet `ProcessNegativeOffset`）
#[inline(always)]
pub const fn process_negative_offset(offset: i64, val_len: i64) -> i64 {
  if val_len <= 0 {
    0
  } else {
    (offset % val_len) + val_len
  }
}

/// 在已知包含目标位的 32 字节块内，通过 4x 64 位大端转换与硬件 `leading_zeros()` 零分支精准定位比特
#[inline(always)]
unsafe fn scan_u8x32_for_bit<const SEARCH_BIT: bool>(ptr: *const u8, base_offset: usize) -> i64 {
  for sub in 0..4 {
    let sub_offset = base_offset + sub * SCALAR_8;
    // SAFETY: 由调用方保证 base_offset..base_offset + 32 在 input 缓冲区有效范围内
    let val = u64::from_be(unsafe { (ptr.add(sub_offset) as *const u64).read_unaligned() });
    let transformed = if SEARCH_BIT { val } else { !val };
    if transformed != 0 {
      return ((sub_offset << 3) as i64) + (transformed.leading_zeros() as i64);
    }
  }
  // SAFETY: 调用方通过 SIMD 向量全等性测试已判定该 32 字节内必存在目标位，此处在数学上不可达
  unsafe { unreachable_unchecked() }
}

/// 标量递进位图字节搜索类型化实现（64 位 u64、32 位 u32、16 位 u16、8 位 u8 直线型梯级匹配与硬件 lzcnt）
#[inline(always)]
pub fn bitpos_byte_search_scalar_typed<const SEARCH_BIT: bool>(
  input: &[u8],
  start_offset: usize,
  end_offset: usize,
) -> i64 {
  let len = input.len();
  if start_offset > end_offset || start_offset >= len {
    return -1;
  }
  let end_offset = end_offset.min(len - 1);
  let mut curr = start_offset;
  let ptr = input.as_ptr();

  // 1. 8 字节 u64 高速循环展开（单条 64 位加载 + 大端转换 + lzcnt 硬件指令）
  while curr + SCALAR_8 <= end_offset + 1 {
    // SAFETY: curr + 8 <= end_offset + 1 <= input.len()
    let val = u64::from_be(unsafe { (ptr.add(curr) as *const u64).read_unaligned() });
    let transformed = if SEARCH_BIT { val } else { !val };
    if transformed != 0 {
      return ((curr << 3) as i64) + (transformed.leading_zeros() as i64);
    }
    curr += SCALAR_8;
  }

  // 2. 4 字节 u32 梯级（最多执行 1 次，零循环）
  if curr + 4 <= end_offset + 1 {
    // SAFETY: curr + 4 <= end_offset + 1 <= input.len()
    let val = u32::from_be(unsafe { (ptr.add(curr) as *const u32).read_unaligned() });
    let transformed = if SEARCH_BIT { val } else { !val };
    if transformed != 0 {
      return ((curr << 3) as i64) + (transformed.leading_zeros() as i64);
    }
    curr += 4;
  }

  // 3. 2 字节 u16 梯级（最多执行 1 次，零循环）
  if curr + 2 <= end_offset + 1 {
    // SAFETY: curr + 2 <= end_offset + 1 <= input.len()
    let val = u16::from_be(unsafe { (ptr.add(curr) as *const u16).read_unaligned() });
    let transformed = if SEARCH_BIT { val } else { !val };
    if transformed != 0 {
      return ((curr << 3) as i64) + (transformed.leading_zeros() as i64);
    }
    curr += 2;
  }

  // 4. 尾部最后 1 个字节 u8（最多执行 1 次，零循环）
  if curr <= end_offset {
    // SAFETY: curr <= end_offset < input.len()
    let val = unsafe { *ptr.add(curr) };
    let transformed = if SEARCH_BIT { val } else { !val };
    if transformed != 0 {
      return ((curr << 3) as i64) + (transformed.leading_zeros() as i64);
    }
  }

  -1
}

/// 标量递进位图字节区间搜索（通用动态分发封装）
#[inline(always)]
pub fn bitpos_byte_search_scalar(
  input: &[u8],
  start_offset: usize,
  end_offset: usize,
  search_for: u8,
) -> i64 {
  if search_for == 1 {
    bitpos_byte_search_scalar_typed::<true>(input, start_offset, end_offset)
  } else {
    bitpos_byte_search_scalar_typed::<false>(input, start_offset, end_offset)
  }
}

/// 基于 fearless_simd 的 256 位向量加速字节搜索内核（支持 4x 128 字节流水线展开与编译期单态化特化）
#[inline(always)]
fn bitpos_byte_search_simd<S: Simd, const SEARCH_BIT: bool>(
  simd: S,
  input: &[u8],
  start_offset: usize,
  end_offset: usize,
) -> i64 {
  let mut curr = start_offset;
  let ptr = input.as_ptr();

  let target_vec = if SEARCH_BIT {
    u8x32::splat(simd, 0x00)
  } else {
    u8x32::splat(simd, 0xFF)
  };

  // 1. 128 字节（4x 32 字节向量）并行展开快速跳过
  while curr + BATCH_128 <= end_offset + 1 {
    // SAFETY: curr + 128 <= end_offset + 1 <= input.len()
    unsafe {
      let v0 = load_u8x32(simd, ptr.add(curr));
      let v1 = load_u8x32(simd, ptr.add(curr + 32));
      let v2 = load_u8x32(simd, ptr.add(curr + 64));
      let v3 = load_u8x32(simd, ptr.add(curr + 96));

      let combined = if SEARCH_BIT {
        (v0 | v1) | (v2 | v3)
      } else {
        (v0 & v1) & (v2 & v3)
      };

      if combined.simd_eq(target_vec).all_true() {
        curr += BATCH_128;
        continue;
      }

      // 命中候选 128 字节块，判定具体落入哪个 32 字节向量
      let base_offset = if !v0.simd_eq(target_vec).all_true() {
        curr
      } else if !v1.simd_eq(target_vec).all_true() {
        curr + 32
      } else if !v2.simd_eq(target_vec).all_true() {
        curr + 64
      } else {
        curr + 96
      };

      return scan_u8x32_for_bit::<SEARCH_BIT>(ptr, base_offset);
    }
  }

  // 2. 剩余 32 字节向量单块快速扫描
  while curr + VEC_32 <= end_offset + 1 {
    // SAFETY: curr + 32 <= end_offset + 1 <= input.len()
    unsafe {
      let v = load_u8x32(simd, ptr.add(curr));
      if v.simd_eq(target_vec).all_true() {
        curr += VEC_32;
        continue;
      }
      return scan_u8x32_for_bit::<SEARCH_BIT>(ptr, curr);
    }
  }

  // 3. 尾部小于 32 字节梯级匹配
  bitpos_byte_search_scalar_typed::<SEARCH_BIT>(input, curr, end_offset)
}

/// 字节范围位图搜索编译期特化实现
#[inline]
pub fn bitpos_byte_search_typed<const SEARCH_BIT: bool>(
  input: &[u8],
  start_offset: usize,
  end_offset: usize,
) -> i64 {
  let len = input.len();
  if start_offset > end_offset || start_offset >= len {
    return -1;
  }
  let end_offset = end_offset.min(len - 1);
  let range_len = end_offset - start_offset + 1;

  if range_len >= VEC_32 {
    let level = Level::new();
    dispatch!(level, simd => {
      bitpos_byte_search_simd::<_, SEARCH_BIT>(simd, input, start_offset, end_offset)
    })
  } else {
    bitpos_byte_search_scalar_typed::<SEARCH_BIT>(input, start_offset, end_offset)
  }
}

/// 字节范围位图搜索（对标 Garnet `BitPosByteSearch`）
///
/// 当检索区间大于等于 32 字节时，自动调度 fearless_simd 向量加速；小于 32 字节走标量梯级匹配。
#[inline]
pub fn bitpos_byte_search(
  input: &[u8],
  start_offset: usize,
  end_offset: usize,
  search_for: u8,
) -> i64 {
  if search_for == 1 {
    bitpos_byte_search_typed::<true>(input, start_offset, end_offset)
  } else {
    bitpos_byte_search_typed::<false>(input, start_offset, end_offset)
  }
}

/// 位范围位级搜索编译期特化实现
#[inline]
pub fn bitpos_bit_search_typed<const SEARCH_BIT: bool>(
  input: &[u8],
  start_bit_offset: i64,
  end_bit_offset: i64,
) -> i64 {
  let bit_len = (input.len() as i64) * 8;
  if start_bit_offset > end_bit_offset || start_bit_offset >= bit_len || start_bit_offset < 0 {
    return -1;
  }
  let end_bit_offset = end_bit_offset.min(bit_len - 1);

  let invalid_payload: u8 = if SEARCH_BIT { 0x00 } else { 0xFF };
  let mut current_bit_offset = start_bit_offset;
  let ptr = input.as_ptr();

  while current_bit_offset <= end_bit_offset {
    let byte_index = (current_bit_offset >> 3) as usize;
    let left_bit_offset = (current_bit_offset & 7) as usize;

    // 若当前位于字节边界 (left_bit_offset == 0) 且有完整字节，直接调用 bitpos_byte_search_typed 批量检索
    if left_bit_offset == 0 {
      let full_bytes = ((end_bit_offset - current_bit_offset + 1) >> 3) as usize;
      if full_bytes > 0 {
        let simd_res =
          bitpos_byte_search_typed::<SEARCH_BIT>(input, byte_index, byte_index + full_bytes - 1);
        if simd_res >= 0 {
          return simd_res;
        }
        current_bit_offset += (full_bytes as i64) << 3;
        continue;
      }
    }

    let boundary = 8 - left_bit_offset;
    let right_bit_offset = if current_bit_offset + (boundary as i64) <= end_bit_offset {
      left_bit_offset + boundary
    } else {
      ((end_bit_offset & 7) as usize) + 1
    };

    // 裁剪当前字节起始和结束位的掩码 (通过 precomputed 2D LUT，单次内存直接读取，零分支、零异或计算)
    // SAFETY: left_bit_offset <= 7 < 9, right_bit_offset <= 8 < 9
    let mask = unsafe {
      *BIT_RANGE_MASK
        .get_unchecked(left_bit_offset)
        .get_unchecked(right_bit_offset)
    };
    // SAFETY: byte_index <= end_bit_offset >> 3 <= (bit_len - 1) >> 3 < input.len()
    let byte_val = unsafe { *ptr.add(byte_index) };
    let payload = byte_val & mask;
    let invalid_mask = invalid_payload & mask;

    if payload != invalid_mask {
      let shifted = (payload as u64) << (56 + left_bit_offset);
      let transformed = if SEARCH_BIT { shifted } else { !shifted };
      let lzcnt = transformed.leading_zeros() as i64;
      return current_bit_offset + lzcnt;
    }

    current_bit_offset += boundary as i64;
  }

  -1
}

/// 位范围位级搜索（1:1 对标 Garnet `BitPosBitSearch`）
///
/// 精准处理首字节左侧位偏移（`left_bit_offset`）与尾字节右侧位偏移（`right_bit_offset`）的掩码计算与 `leading_zeros()`。
/// 在中间字节对齐时自动融合 SIMD 向量/标量展开批量扫描。
#[inline]
pub fn bitpos_bit_search(
  input: &[u8],
  start_bit_offset: i64,
  end_bit_offset: i64,
  search_for: u8,
) -> i64 {
  if search_for == 1 {
    bitpos_bit_search_typed::<true>(input, start_bit_offset, end_bit_offset)
  } else {
    bitpos_bit_search_typed::<false>(input, start_bit_offset, end_bit_offset)
  }
}

/// BITPOS 命令核心驱动函数（1:1 对标 Microsoft Garnet `BitmapManager.BitPosDriver`）
///
/// 参数：
/// - `input`: 位图字节切片
/// - `start_offset`: 起始偏移（BYTE 模式下为字节偏移，BIT 模式下为位偏移）
/// - `end_offset`: 结束偏移（BYTE 模式下为字节偏移，BIT 模式下为位偏移）
/// - `search_for`: 查找目标位（0 或 1）
/// - `offset_type`: 偏移类型（0x0 为 BYTE 字节模式，0x1 为 BIT 位模式）
///
/// 返回值：
/// - 找到的目标 bit 绝对偏移；若未找到则返回 -1。
/// - 对齐 Redis / Garnet 规范：当 `search_for == 0` 时，若在位图内未找到 0，
///   且未指定 end_offset（以 -1 传入）或结束范围包含了位图末尾之外的首位，
///   则返回末尾补充的虚拟 0 位位置（即位图总 bit 数 `len * 8`）。
pub fn bitpos_driver(
  input: &[u8],
  start_offset: i64,
  end_offset: i64,
  search_for: u8,
  offset_type: u8,
) -> i64 {
  let input_len = input.len() as i64;

  // 1. 空切片边界处理（100% 对齐 Redis 与 Garnet 规范）
  if input_len == 0 {
    return if search_for == 0 && start_offset <= 0 && (end_offset == -1 || end_offset >= 0) {
      0
    } else {
      -1
    };
  }

  // 非法查找位容错校验（目标 bit 仅能为 0 或 1）
  if search_for > 1 {
    return -1;
  }

  let search_bit = search_for == 1;

  match offset_type {
    OFFSET_TYPE_BYTE => {
      // =========================================================================
      // BYTE 模式
      // =========================================================================
      let raw_end_offset = end_offset;
      let s = if start_offset < 0 {
        process_negative_offset(start_offset, input_len)
      } else {
        start_offset
      };
      let e = if end_offset < 0 {
        process_negative_offset(end_offset, input_len)
      } else {
        end_offset
      };

      if s >= input_len || s > e {
        return -1;
      }

      let clamped_end = if e >= input_len { input_len - 1 } else { e };

      let pos = if search_bit {
        bitpos_byte_search_typed::<true>(input, s as usize, clamped_end as usize)
      } else {
        bitpos_byte_search_typed::<false>(input, s as usize, clamped_end as usize)
      };

      if pos >= 0 {
        return pos;
      }

      // 当找 0 且未在有效字节范围内找到 0 时（数据全为 1）：
      // 若未指定 end_offset（以 -1 传入）或 end_offset 覆盖到了位图末尾之外的补充字节 (raw_end_offset >= input_len):
      if !search_bit && (raw_end_offset == -1 || raw_end_offset >= input_len) {
        return input_len * 8;
      }

      -1
    }
    OFFSET_TYPE_BIT => {
      // =========================================================================
      // BIT 模式
      // =========================================================================
      let bit_len = input_len * 8;
      let raw_end_offset = end_offset;
      let s = if start_offset < 0 {
        process_negative_offset(start_offset, bit_len)
      } else {
        start_offset
      };
      let e = if end_offset < 0 {
        process_negative_offset(end_offset, bit_len)
      } else {
        end_offset
      };

      let start_byte_index = s >> 3;
      let end_byte_index = e >> 3;

      if start_byte_index >= input_len || start_byte_index > end_byte_index {
        return -1;
      }

      let clamped_end = if end_byte_index >= input_len {
        bit_len - 1
      } else {
        e
      };

      let pos = if search_bit {
        bitpos_bit_search_typed::<true>(input, s, clamped_end)
      } else {
        bitpos_bit_search_typed::<false>(input, s, clamped_end)
      };

      if pos >= 0 {
        return pos;
      }

      // 当找 0 且未在有效位范围内找到 0：
      // 若未指定 end_offset（以 -1 传入）或显式 end 范围覆盖到了末尾补充的 0 位 (raw_end_offset >= bit_len)
      if !search_bit && (raw_end_offset == -1 || raw_end_offset >= bit_len) {
        return bit_len;
      }

      -1
    }
    _ => -1,
  }
}
