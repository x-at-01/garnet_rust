//! 40 字节 SHA-1 脚本哈希键
//!
//! 1:1 对标 Microsoft Garnet `ScriptHashKey.cs`:
//! - 固定 40 字节十六进制哈希表示（align 8 保证对齐与 64 位整型快速读取）
//! - 基于 `fearless_simd` 的零分支 256 位 SIMD 重叠比对（对标 Garnet `Vector256.Load(a)` 与 `Vector256.Load(a + 1)`）
//! - 5 次 `u64` 标量读取与纯位运算异或累积零分支回退
//! - 对标 Garnet 取前缀常数级 O(1) `Hash`，避免 40 字节逐字节循环哈希
//! - 编译期优化 `HEX_NORMALIZE_LUT` 与单次末尾位运算校验，循环内消除全部条件分支

use core::{
  cmp::Ordering,
  fmt::{self, Display},
  hash::{Hash, Hasher},
  ops::Deref,
  ptr,
  str::{FromStr, from_utf8_unchecked},
};

use fearless_simd::{Level, Simd, SimdBase, SimdMask, dispatch, u8x32};

use crate::error::{Error, Result};

/// SHA-1 十六进制字符串标准长度（40 字节）
pub const SHA1_HEX_LEN: usize = 40;

/// SHA-1 原始二进制字节数组标准长度（20 字节）
pub const SHA1_RAW_LEN: usize = 20;

const HEX_DIGITS: &[u8; 16] = b"0123456789abcdef";

/// 编译期 256 字节到 2 字节小写十六进制展开查找表
const HEX_EXPAND_LUT: [[u8; 2]; 256] = {
  let mut table = [[0u8; 2]; 256];
  let mut i = 0usize;
  while i < 256 {
    table[i] = [HEX_DIGITS[i >> 4], HEX_DIGITS[i & 0xF]];
    i += 1;
  }
  table
};

/// 编译期十六进制字符到 4 位数值查找表 (0xFF 表示非法)
const HEX_VAL_LUT: [u8; 256] = {
  let mut table = [0xFF; 256];
  let mut i = 0usize;
  while i < 256 {
    let b = i as u8;
    if b >= b'0' && b <= b'9' {
      table[i] = b - b'0';
    } else if b >= b'a' && b <= b'f' {
      table[i] = b - b'a' + 10;
    } else if b >= b'A' && b <= b'F' {
      table[i] = b - b'A' + 10;
    }
    i += 1;
  }
  table
};

/// 256 字节编译期查找表：0xFF 表示非法字符，其余为对应的小写 ASCII 字符
///
/// 利用数学特性：对于任意合法十六进制字符（'0'..='9', 'a'..='f', 'A'..='F'），
/// 执行 `b | 0x20` 均能精确且无分支地映射到其小写 ASCII 表示。
const HEX_NORMALIZE_LUT: [u8; 256] = {
  let mut table = [0xFF; 256];
  let mut i = 0usize;
  while i < 256 {
    let b = i as u8;
    let lower = b | 0x20;
    if b.wrapping_sub(b'0') <= 9 || lower.wrapping_sub(b'a') <= 5 {
      table[i] = lower;
    }
    i += 1;
  }
  table
};

/// 零分支 SIMD 40 字节比对核心内核
///
/// 对标 Microsoft Garnet `ScriptHashKey.Equals`:
/// ```csharp
/// var aVec1 = Vector256.Load(a);
/// var aVec2 = Vector256.Load(a + 1); // a+1 偏移 8 字节
/// var bVec1 = Vector256.Load(b);
/// var bVec2 = Vector256.Load(b + 1);
/// return Vector256.EqualsAll(aVec1, bVec1) & Vector256.EqualsAll(aVec2, bVec2);
/// ```
/// 两次加载 32 字节（256-bit）向量，分别覆盖 [0..32] 与 [8..40]，
/// 零分支在向量寄存器内部执行 `(a1.simd_eq(b1) & a2.simd_eq(b2)).all_true()`，
/// 仅需一次向量向标量寄存器的规约指令即可完成判定。
#[inline(always)]
pub fn simd_eq_40_kernel<S: Simd>(simd: S, a: &[u8; SHA1_HEX_LEN], b: &[u8; SHA1_HEX_LEN]) -> bool {
  let a_ptr = a.as_ptr();
  let b_ptr = b.as_ptr();
  // SAFETY:
  // 1. a 与 b 均通过引用传入类型为 &[u8; 40] 的定长数组，保证指向至少 40 字节的有效已初始化内存。
  // 2. 偏移 0 和 偏移 8 读取 32 字节向量，覆盖区间分别为 [0..32] 和 [8..40]，完全落在 [0..40] 有效范围内。
  // 3. ScriptHashKey 结构体标记有 #[repr(C, align(8))]，确保 a_ptr 和 a_ptr.add(8) 均满足 8 字节对齐。
  // 4. u8x32::load_array_ref 执行对齐/非对齐向量加载，对内存无别名或写入副作用。
  let (a1, a2, b1, b2) = unsafe {
    (
      u8x32::load_array_ref(simd, &*(a_ptr as *const [u8; 32])),
      u8x32::load_array_ref(simd, &*(a_ptr.add(8) as *const [u8; 32])),
      u8x32::load_array_ref(simd, &*(b_ptr as *const [u8; 32])),
      u8x32::load_array_ref(simd, &*(b_ptr.add(8) as *const [u8; 32])),
    )
  };
  SimdMask::<S>::all_true(a1.simd_eq(b1) & a2.simd_eq(b2))
}

/// 标量/短指令回退：5 次 64 位整型读取与位异或无分支累积比对（覆盖 0..8, 8..16, 16..24, 24..32, 32..40）
///
/// 相比于多次布尔比较和跳转，采用 5 次 64 位字 XOR 并进行位或累积：
/// 单发射多执行端口并行计算差异，完全消除分支预测失败开销与 sete 指令。
#[inline(always)]
pub fn scalar_eq_40(a: &[u8; SHA1_HEX_LEN], b: &[u8; SHA1_HEX_LEN]) -> bool {
  let a_ptr = a.as_ptr().cast::<u64>();
  let b_ptr = b.as_ptr().cast::<u64>();
  // SAFETY:
  // 1. a 与 b 均为 40 字节定长数组，总大小恰好为 5 * 8 字节。
  // 2. a_ptr.add(i) 对于 i in 0..5 读取 8 字节（u64），分别访问区间 [0..8], [8..16], [16..24], [24..32], [32..40]，完全在有效边界内。
  // 3. 使用 read_unaligned 确保即使在任意非对齐切片入参下也绝对安全。
  unsafe {
    let d0 = a_ptr.read_unaligned() ^ b_ptr.read_unaligned();
    let d1 = a_ptr.add(1).read_unaligned() ^ b_ptr.add(1).read_unaligned();
    let d2 = a_ptr.add(2).read_unaligned() ^ b_ptr.add(2).read_unaligned();
    let d3 = a_ptr.add(3).read_unaligned() ^ b_ptr.add(3).read_unaligned();
    let d4 = a_ptr.add(4).read_unaligned() ^ b_ptr.add(4).read_unaligned();
    (d0 | d1 | d2 | d3 | d4) == 0
  }
}

/// 40 字节 SHA-1 脚本哈希键结构体
///
/// 8 字节对齐，内含规范化后的小写十六进制字符数组。
#[derive(Clone, Copy)]
#[repr(C, align(8))]
pub struct ScriptHashKey {
  bytes: [u8; SHA1_HEX_LEN],
}

impl ScriptHashKey {
  /// 从字节切片构造 ScriptHashKey
  ///
  /// 要求长度恰好为 40 字节且全为合法十六进制字符（`0-9`, `a-f`, `A-F`）。
  /// 大写字母自动规范化为小写字母。
  ///
  /// 采用展开查表与位或累积算法：
  /// - 循环内部无任何条件跳转分支
  /// - 合法字符最高位（bit 7）必定为 0，非法字符在表中为 0xFF（bit 7 为 1）
  /// - 循环结束后仅需单次位运算判定是否存在非法字符
  pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
    if bytes.len() != SHA1_HEX_LEN {
      return Err(Error::InvalidScriptHash);
    }
    // SAFETY: 已校验 bytes.len() == SHA1_HEX_LEN (40)，转为定长数组引用完全安全
    let src: &[u8; SHA1_HEX_LEN] = unsafe { &*(bytes.as_ptr() as *const [u8; SHA1_HEX_LEN]) };
    let mut arr = [0u8; SHA1_HEX_LEN];
    let mut invalid_mask = 0u8;

    for i in 0..SHA1_HEX_LEN {
      let norm = HEX_NORMALIZE_LUT[src[i] as usize];
      arr[i] = norm;
      invalid_mask |= norm;
    }

    if invalid_mask & 0x80 != 0 {
      return Err(Error::InvalidScriptHash);
    }
    Ok(Self { bytes: arr })
  }

  /// 从 40 字节定长数组构造 ScriptHashKey（执行合法性校验与小写规范化）
  #[inline]
  pub fn from_array(bytes: [u8; SHA1_HEX_LEN]) -> Result<Self> {
    Self::from_bytes(&bytes)
  }

  /// 内部信任路径：直接从 40 字节原始数组构造（假定调用者已确保为小写十六进制，零开销）
  #[inline(always)]
  pub const fn from_raw(bytes: [u8; SHA1_HEX_LEN]) -> Self {
    Self { bytes }
  }

  /// 从 20 字节原始二进制 SHA-1 高效展开为 40 字符十六进制哈希键（零堆分配）
  #[inline]
  pub const fn from_raw_20(raw: [u8; SHA1_RAW_LEN]) -> Self {
    let mut bytes = [0u8; SHA1_HEX_LEN];
    let mut i = 0;
    while i < SHA1_RAW_LEN {
      let pair = HEX_EXPAND_LUT[raw[i] as usize];
      bytes[i * 2] = pair[0];
      bytes[i * 2 + 1] = pair[1];
      i += 1;
    }
    Self { bytes }
  }

  /// 将 40 字节十六进制字符折叠还原为 20 字节原始二进制 SHA-1
  #[inline]
  pub fn to_raw_20(&self) -> [u8; SHA1_RAW_LEN] {
    let mut raw = [0u8; SHA1_RAW_LEN];
    let mut i = 0;
    while i < SHA1_RAW_LEN {
      let hi = HEX_VAL_LUT[self.bytes[i * 2] as usize];
      let lo = HEX_VAL_LUT[self.bytes[i * 2 + 1] as usize];
      raw[i] = (hi << 4) | lo;
      i += 1;
    }
    raw
  }

  /// 与 20 字节原始二进制 SHA-1 进行零分配直接比对
  #[inline]
  pub fn eq_raw_20(&self, raw: &[u8; SHA1_RAW_LEN]) -> bool {
    let mut i = 0;
    while i < SHA1_RAW_LEN {
      let pair = HEX_EXPAND_LUT[raw[i] as usize];
      if self.bytes[i * 2] != pair[0] || self.bytes[i * 2 + 1] != pair[1] {
        return false;
      }
      i += 1;
    }
    true
  }

  /// 获取底层 40 字节只读引用
  #[inline(always)]
  pub const fn as_bytes(&self) -> &[u8; SHA1_HEX_LEN] {
    &self.bytes
  }

  /// 获取底层字符串切片（零拷贝，规范化保证为合法 ASCII 字符串）
  #[inline(always)]
  pub const fn as_str(&self) -> &str {
    // SAFETY:
    // self.bytes 内部的所有字节在构造时均通过 HEX 规范化校验，
    // 其值仅可能为 ASCII 字符 '0'..='9' (0x30..=0x39) 与 'a'..='f' (0x61..=0x66)，
    // 均为标准 7-bit ASCII 码，必定为合法的 UTF-8 编码文本。
    unsafe { from_utf8_unchecked(&self.bytes) }
  }

  /// 拷贝 40 字节哈希键到底层目标切片中（对标 Garnet `CopyTo`）
  #[inline(always)]
  pub fn copy_to_slice(&self, dst: &mut [u8]) {
    dst[..SHA1_HEX_LEN].copy_from_slice(&self.bytes);
  }

  /// 使用零分支 SIMD 加速比对两个哈希键
  #[inline]
  pub fn eq_simd(&self, other: &Self) -> bool {
    let level = Level::new();
    dispatch!(level, simd => simd_eq_40_kernel(simd, &self.bytes, &other.bytes))
  }

  /// 使用 5 次 64 位标量读取与位异或比对两个哈希键
  #[inline(always)]
  pub fn eq_scalar(&self, other: &Self) -> bool {
    scalar_eq_40(&self.bytes, &other.bytes)
  }

  /// 对标 Garnet 获取前缀 64 位整数哈希值（零 unsafe，由编译器直接生成单条 64 位加载指令）
  #[inline(always)]
  pub const fn hash_prefix(&self) -> u64 {
    u64::from_ne_bytes([
      self.bytes[0],
      self.bytes[1],
      self.bytes[2],
      self.bytes[3],
      self.bytes[4],
      self.bytes[5],
      self.bytes[6],
      self.bytes[7],
    ])
  }

  /// 获取后缀 64 位整数哈希值（覆盖第 32..40 字节）
  #[inline(always)]
  pub const fn hash_suffix(&self) -> u64 {
    u64::from_ne_bytes([
      self.bytes[32],
      self.bytes[33],
      self.bytes[34],
      self.bytes[35],
      self.bytes[36],
      self.bytes[37],
      self.bytes[38],
      self.bytes[39],
    ])
  }
}

impl PartialEq for ScriptHashKey {
  #[inline]
  fn eq(&self, other: &Self) -> bool {
    if ptr::eq(self, other) {
      return true;
    }
    self.eq_simd(other)
  }
}

impl Eq for ScriptHashKey {}

impl PartialEq<[u8; SHA1_HEX_LEN]> for ScriptHashKey {
  #[inline]
  fn eq(&self, other: &[u8; SHA1_HEX_LEN]) -> bool {
    let level = Level::new();
    if dispatch!(level, simd => simd_eq_40_kernel(simd, &self.bytes, other)) {
      return true;
    }
    self.bytes.eq_ignore_ascii_case(other)
  }
}

impl PartialEq<ScriptHashKey> for [u8; SHA1_HEX_LEN] {
  #[inline]
  fn eq(&self, other: &ScriptHashKey) -> bool {
    other.eq(self)
  }
}

impl PartialEq<&[u8; SHA1_HEX_LEN]> for ScriptHashKey {
  #[inline]
  fn eq(&self, other: &&[u8; SHA1_HEX_LEN]) -> bool {
    self.eq(*other)
  }
}

impl PartialEq<ScriptHashKey> for &[u8; SHA1_HEX_LEN] {
  #[inline]
  fn eq(&self, other: &ScriptHashKey) -> bool {
    other.eq(*self)
  }
}

impl PartialEq<&[u8]> for ScriptHashKey {
  #[inline]
  fn eq(&self, other: &&[u8]) -> bool {
    if other.len() != SHA1_HEX_LEN {
      return false;
    }
    // SAFETY: other.len() == SHA1_HEX_LEN (40)
    let other_arr: &[u8; SHA1_HEX_LEN] = unsafe { &*(other.as_ptr() as *const [u8; SHA1_HEX_LEN]) };
    let level = Level::new();
    if dispatch!(level, simd => simd_eq_40_kernel(simd, &self.bytes, other_arr)) {
      return true;
    }
    self.bytes.eq_ignore_ascii_case(other)
  }
}

impl PartialEq<ScriptHashKey> for &[u8] {
  #[inline]
  fn eq(&self, other: &ScriptHashKey) -> bool {
    other.eq(self)
  }
}

impl PartialEq<str> for ScriptHashKey {
  #[inline]
  fn eq(&self, other: &str) -> bool {
    self.eq(&other.as_bytes())
  }
}

impl PartialEq<ScriptHashKey> for str {
  #[inline]
  fn eq(&self, other: &ScriptHashKey) -> bool {
    other.eq(&self.as_bytes())
  }
}

impl PartialEq<&str> for ScriptHashKey {
  #[inline]
  fn eq(&self, other: &&str) -> bool {
    self.eq(&other.as_bytes())
  }
}

impl PartialEq<ScriptHashKey> for &str {
  #[inline]
  fn eq(&self, other: &ScriptHashKey) -> bool {
    other.eq(&self.as_bytes())
  }
}

impl Hash for ScriptHashKey {
  #[inline(always)]
  fn hash<H: Hasher>(&self, state: &mut H) {
    // 对标 Garnet 取前缀思想，读取前 8 字节 u64 与后 8 字节 u64 写入 Hasher，实现极速常数级 O(1) 哈希
    state.write_u64(self.hash_prefix());
    state.write_u64(self.hash_suffix());
  }
}

impl Display for ScriptHashKey {
  #[inline]
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl fmt::Debug for ScriptHashKey {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "ScriptHashKey(\"{}\")", self.as_str())
  }
}

impl Ord for ScriptHashKey {
  #[inline]
  fn cmp(&self, other: &Self) -> Ordering {
    self.bytes.cmp(&other.bytes)
  }
}

impl PartialOrd for ScriptHashKey {
  #[inline]
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Deref for ScriptHashKey {
  type Target = [u8];

  #[inline]
  fn deref(&self) -> &Self::Target {
    &self.bytes
  }
}

impl AsRef<[u8]> for ScriptHashKey {
  #[inline]
  fn as_ref(&self) -> &[u8] {
    &self.bytes
  }
}

impl AsRef<str> for ScriptHashKey {
  #[inline]
  fn as_ref(&self) -> &str {
    self.as_str()
  }
}

impl TryFrom<&[u8]> for ScriptHashKey {
  type Error = Error;

  #[inline]
  fn try_from(bytes: &[u8]) -> Result<Self> {
    Self::from_bytes(bytes)
  }
}

impl TryFrom<&str> for ScriptHashKey {
  type Error = Error;

  #[inline]
  fn try_from(s: &str) -> Result<Self> {
    Self::from_bytes(s.as_bytes())
  }
}

impl TryFrom<[u8; SHA1_HEX_LEN]> for ScriptHashKey {
  type Error = Error;

  #[inline]
  fn try_from(bytes: [u8; SHA1_HEX_LEN]) -> Result<Self> {
    Self::from_array(bytes)
  }
}

impl FromStr for ScriptHashKey {
  type Err = Error;

  #[inline]
  fn from_str(s: &str) -> Result<Self> {
    Self::from_bytes(s.as_bytes())
  }
}

impl PartialEq<[u8; SHA1_RAW_LEN]> for ScriptHashKey {
  #[inline]
  fn eq(&self, other: &[u8; SHA1_RAW_LEN]) -> bool {
    self.eq_raw_20(other)
  }
}

impl PartialEq<ScriptHashKey> for [u8; SHA1_RAW_LEN] {
  #[inline]
  fn eq(&self, other: &ScriptHashKey) -> bool {
    other.eq_raw_20(self)
  }
}

impl PartialEq<&[u8; SHA1_RAW_LEN]> for ScriptHashKey {
  #[inline]
  fn eq(&self, other: &&[u8; SHA1_RAW_LEN]) -> bool {
    self.eq_raw_20(other)
  }
}

impl PartialEq<ScriptHashKey> for &[u8; SHA1_RAW_LEN] {
  #[inline]
  fn eq(&self, other: &ScriptHashKey) -> bool {
    other.eq_raw_20(self)
  }
}

impl From<[u8; SHA1_RAW_LEN]> for ScriptHashKey {
  #[inline]
  fn from(raw: [u8; SHA1_RAW_LEN]) -> Self {
    Self::from_raw_20(raw)
  }
}

impl From<&[u8; SHA1_RAW_LEN]> for ScriptHashKey {
  #[inline]
  fn from(raw: &[u8; SHA1_RAW_LEN]) -> Self {
    Self::from_raw_20(*raw)
  }
}
