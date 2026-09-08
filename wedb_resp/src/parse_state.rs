use core::{
  array::IntoIter as ArrayIntoIter, iter::Take, ops::Deref, ptr::copy_nonoverlapping, slice::Iter,
  str::from_utf8,
};
use std::vec::IntoIter as VecIntoIter;

use hipstr::HipStr;

use crate::{
  cmd::RespCommand,
  error::{Error, Result},
  read::{RespReadUtils, find_crlf},
  simd::{mask_for, simd_fast_parse},
};

/// RESP 解析工具函数集，对齐 Microsoft Garnet `ParseUtils`
pub struct ParseUtils;

impl ParseUtils {
  /// 从参数切片读取 32 位有符号整数（不允许前导零）
  #[inline]
  pub fn read_int(slice: &[u8]) -> Result<i32> {
    Self::try_read_int(slice).ok_or(Error::NotANumber)
  }

  /// 尝试从参数切片读取 32 位有符号整数
  #[inline]
  pub fn try_read_int(slice: &[u8]) -> Option<i32> {
    if slice.is_empty() {
      return None;
    }
    match RespReadUtils::try_read_int32(slice, false) {
      Ok((val, len)) if len == slice.len() => Some(val),
      _ => None,
    }
  }

  /// 从参数切片读取 64 位有符号长整型（不允许前导零）
  #[inline]
  pub fn read_long(slice: &[u8]) -> Result<i64> {
    Self::try_read_long(slice).ok_or(Error::NotANumber)
  }

  /// 尝试从参数切片读取 64 位有符号长整型（默认不允许前导零）
  #[inline]
  pub fn try_read_long(slice: &[u8]) -> Option<i64> {
    Self::try_read_long_ext(slice, false)
  }

  /// 尝试从参数切片读取 64 位有符号长整型，可配置是否允许前导零
  #[inline]
  pub fn try_read_long_ext(slice: &[u8], allow_leading_zeros: bool) -> Option<i64> {
    if slice.is_empty() {
      return None;
    }
    match RespReadUtils::try_read_int64(slice, allow_leading_zeros) {
      Ok((val, len)) if len == slice.len() => Some(val),
      _ => None,
    }
  }

  /// 从参数切片读取 64 位无符号整数
  #[inline]
  pub fn read_ulong(slice: &[u8]) -> Result<u64> {
    Self::try_read_ulong(slice).ok_or(Error::NotANumber)
  }

  /// 尝试从参数切片读取 64 位无符号整数
  #[inline]
  pub fn try_read_ulong(slice: &[u8]) -> Option<u64> {
    if slice.is_empty() {
      return None;
    }
    match RespReadUtils::try_read_uint64(slice) {
      Ok((val, len)) if len == slice.len() => Some(val),
      _ => None,
    }
  }

  /// 从参数切片读取 64 位浮点数
  #[inline]
  pub fn read_double(slice: &[u8], can_be_infinite: bool) -> Result<f64> {
    Self::try_read_double(slice, can_be_infinite).ok_or(Error::NotANumber)
  }

  /// 尝试从参数切片读取 64 位浮点数
  #[inline]
  pub fn try_read_double(slice: &[u8], can_be_infinite: bool) -> Option<f64> {
    if slice.is_empty() {
      return None;
    }
    if let Some(inf) = RespReadUtils::try_read_infinity(slice) {
      return if can_be_infinite { Some(inf) } else { None };
    }
    let s = slice
      .strip_prefix(b"+")
      .or_else(|| slice.strip_prefix(b"-"))
      .unwrap_or(slice);
    if s.eq_ignore_ascii_case(b"NAN") {
      return Some(f64::NAN);
    }
    fast_float::parse(slice).ok()
  }

  /// 从参数切片读取 32 位单精度浮点数
  #[inline]
  pub fn read_float(slice: &[u8], can_be_infinite: bool) -> Result<f32> {
    Self::try_read_float(slice, can_be_infinite).ok_or(Error::NotANumber)
  }

  /// 尝试从参数切片读取 32 位单精度浮点数
  #[inline]
  pub fn try_read_float(slice: &[u8], can_be_infinite: bool) -> Option<f32> {
    Self::try_read_double(slice, can_be_infinite).map(|v| v as f32)
  }

  /// 从参数切片读取布尔值（支持 `"1"`/`"0"`）
  #[inline]
  pub fn read_bool(slice: &[u8]) -> Result<bool> {
    Self::try_read_bool(slice).ok_or(Error::NotANumber)
  }

  /// 尝试从参数切片读取布尔值
  #[inline]
  pub fn try_read_bool(slice: &[u8]) -> Option<bool> {
    match slice {
      [b'1'] => Some(true),
      [b'0'] => Some(false),
      s if s.len() == 4 && s.eq_ignore_ascii_case(b"true") => Some(true),
      s if s.len() == 5 && s.eq_ignore_ascii_case(b"false") => Some(false),
      _ => None,
    }
  }

  /// 从参数切片读取 UTF-8 字符串
  #[inline]
  pub fn read_string(slice: &[u8]) -> Result<&str> {
    from_utf8(slice).map_err(Error::from)
  }

  /// 尝试从参数切片读取 UTF-8 字符串
  #[inline]
  pub fn try_read_string(slice: &[u8]) -> Option<&str> {
    from_utf8(slice).ok()
  }
}

/// 最近最少使用（MRU）高频命令缓存槽位（对标 Garnet _cachedPattern / _cachedCmd）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MruSlot {
  /// 掩码匹配后的 128 位模式字（命令名区域已归一化为大写）
  pub pattern: u128,
  /// 匹配使用的 128 位掩码
  pub mask: u128,
  /// 解析出的 RESP 命令枚举
  pub cmd: RespCommand,
  /// 待读取的参数总数
  pub count: usize,
  /// 命令头消耗的字节数（13 ~ 16）
  pub len: usize,
}

/// 栈内联小参数最大容量（8 个参数切片占用 128 字节，覆盖 95%+ 的常见 Redis 指令，实现 0 堆分配）
pub const INLINE_ARGS_CAPACITY: usize = 8;

/// 单条命令数组长度上限（防预认证内存耗尽，对齐 C# `RespServerSession.MaxRespArrayLength = 1 << 20`）
pub const MAX_RESP_ARRAY_LENGTH: usize = 1 << 20;

/// 会话级持久 MRU 命令缓存容器（跨网络请求长久复用，激活 128 位 SIMD 极速模式匹配，模式字大小写归一化）
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SessionMruCache {
  pub mru0: Option<MruSlot>,
  pub mru1: Option<MruSlot>,
}

impl SessionMruCache {
  /// 创建空 MRU 缓存
  #[inline(always)]
  pub const fn new() -> Self {
    Self {
      mru0: None,
      mru1: None,
    }
  }

  /// 清空 MRU 模式匹配缓存
  #[inline(always)]
  pub fn clear(&mut self) {
    self.mru0 = None;
    self.mru1 = None;
  }
}

/// 会话命令解析状态机，在单个会话中复用，零堆分配存储参数切片引用
/// 对齐 Microsoft Garnet `SessionParseState`
#[derive(Debug, Clone)]
pub struct SessionParseState<'a> {
  /// 栈内联小参数切片数组（8 个槽位，0 次堆分配）
  inline_args: [&'a [u8]; INLINE_ARGS_CAPACITY],
  /// 当前参数总数
  len: usize,
  /// 溢出大参数动态数组（仅当参数超过 8 个时使用，且跨命令复用 capacity）
  spill_args: Vec<&'a [u8]>,
  /// 最近最少使用（MRU）高频命令缓存槽位 0
  pub mru0: Option<MruSlot>,
  /// 最近最少使用（MRU）高频命令缓存槽位 1
  pub mru1: Option<MruSlot>,
}

impl<'a> Default for SessionParseState<'a> {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

impl<'a> SessionParseState<'a> {
  /// 创建空解析状态（0 堆分配，小参数内联）
  #[inline]
  pub fn new() -> Self {
    Self {
      inline_args: [b""; INLINE_ARGS_CAPACITY],
      len: 0,
      spill_args: Vec::new(),
      mru0: None,
      mru1: None,
    }
  }

  /// 指定初始容量创建解析状态
  #[inline]
  pub fn with_capacity(capacity: usize) -> Self {
    Self {
      inline_args: [b""; INLINE_ARGS_CAPACITY],
      len: 0,
      spill_args: if capacity > INLINE_ARGS_CAPACITY {
        Vec::with_capacity(capacity)
      } else {
        Vec::new()
      },
      mru0: None,
      mru1: None,
    }
  }

  /// 使用已有会话 MRU 缓存初始化解析状态（保留会话热态模式字，0 堆分配）
  #[inline]
  pub fn with_mru(mru: SessionMruCache) -> Self {
    Self {
      inline_args: [b""; INLINE_ARGS_CAPACITY],
      len: 0,
      spill_args: Vec::new(),
      mru0: mru.mru0,
      mru1: mru.mru1,
    }
  }

  /// 提取当前解析状态沉淀的 MRU 缓存（用于在会话级别跨请求持久化）
  #[inline(always)]
  pub fn mru(&self) -> SessionMruCache {
    SessionMruCache {
      mru0: self.mru0,
      mru1: self.mru1,
    }
  }

  /// 清空参数列表以备下一次命令复用（保持 MRU 缓存与溢出数组容量）
  #[inline]
  pub fn clear(&mut self) {
    self.len = 0;
    self.spill_args.clear();
  }

  /// 清空 MRU 缓存
  #[inline]
  pub fn clear_mru(&mut self) {
    self.mru0 = None;
    self.mru1 = None;
  }

  /// 将内联栈参数迁移至溢出缓冲区，并确保溢出缓冲区容量至少为 `min_capacity`
  #[inline]
  fn migrate_to_spill(&mut self, min_capacity: usize) {
    if self.spill_args.capacity() < min_capacity {
      self.spill_args.reserve(min_capacity);
    }
    self
      .spill_args
      .extend_from_slice(&self.inline_args[..self.len]);
  }

  /// 预留参数容量
  #[inline]
  pub fn reserve(&mut self, additional: usize) {
    let needed = self.len + additional;
    if needed > INLINE_ARGS_CAPACITY {
      if self.spill_args.is_empty() && self.len > 0 {
        self.migrate_to_spill(needed);
      } else {
        self.spill_args.reserve(additional);
      }
    }
  }

  /// 追加一个参数切片
  #[inline]
  pub fn push(&mut self, arg: &'a [u8]) {
    if self.spill_args.is_empty() && self.len < INLINE_ARGS_CAPACITY {
      self.inline_args[self.len] = arg;
      self.len += 1;
    } else {
      if self.spill_args.is_empty() {
        self.migrate_to_spill((self.len + 8).max(16));
      }
      self.spill_args.push(arg);
      self.len += 1;
    }
  }

  /// 获取当前已解析的参数总数
  #[inline(always)]
  pub fn len(&self) -> usize {
    self.len
  }

  /// 是否无参数
  #[inline(always)]
  pub fn is_empty(&self) -> bool {
    self.len == 0
  }

  /// 获取指定索引的原始参数切片
  #[inline(always)]
  pub fn get(&self, idx: usize) -> Option<&'a [u8]> {
    if self.spill_args.is_empty() {
      if idx < self.len {
        // SAFETY: 当 spill_args 为空时，idx < len <= INLINE_ARGS_CAPACITY (8)
        Some(unsafe { *self.inline_args.get_unchecked(idx) })
      } else {
        None
      }
    } else {
      self.spill_args.get(idx).copied()
    }
  }

  /// 获取指定索引的参数切片，不存在则返回 `Error::MissingArgument`
  #[inline]
  pub fn get_arg(&self, idx: usize) -> Result<&'a [u8]> {
    self.get(idx).ok_or(Error::MissingArgument)
  }

  /// 获取指定索引的参数并转换为 UTF-8 字符串切片
  #[inline]
  pub fn get_str(&self, idx: usize) -> Result<&'a str> {
    let slice = self.get_arg(idx)?;
    ParseUtils::read_string(slice)
  }

  /// 尝试获取指定索引的参数并转换为 UTF-8 字符串切片
  #[inline]
  pub fn try_get_str(&self, idx: usize) -> Option<&'a str> {
    self.get(idx).and_then(ParseUtils::try_read_string)
  }

  /// 获取指定索引的 32 位有符号整数
  #[inline]
  pub fn get_int(&self, idx: usize) -> Result<i32> {
    let slice = self.get_arg(idx)?;
    ParseUtils::read_int(slice)
  }

  /// 尝试获取指定索引的 32 位有符号整数
  #[inline]
  pub fn try_get_int(&self, idx: usize) -> Option<i32> {
    self.get(idx).and_then(ParseUtils::try_read_int)
  }

  /// 获取指定索引的 64 位有符号长整型
  #[inline]
  pub fn get_long(&self, idx: usize) -> Result<i64> {
    let slice = self.get_arg(idx)?;
    ParseUtils::read_long(slice)
  }

  /// 尝试获取指定索引的 64 位有符号长整型
  #[inline]
  pub fn try_get_long(&self, idx: usize) -> Option<i64> {
    self.get(idx).and_then(ParseUtils::try_read_long)
  }

  /// 获取指定索引的 64 位无符号长整型
  #[inline]
  pub fn get_ulong(&self, idx: usize) -> Result<u64> {
    let slice = self.get_arg(idx)?;
    ParseUtils::read_ulong(slice)
  }

  /// 尝试获取指定索引的 64 位无符号长整型
  #[inline]
  pub fn try_get_ulong(&self, idx: usize) -> Option<u64> {
    self.get(idx).and_then(ParseUtils::try_read_ulong)
  }

  /// 获取指定索引的 64 位双精度浮点数
  #[inline]
  pub fn get_double(&self, idx: usize, can_be_infinite: bool) -> Result<f64> {
    let slice = self.get_arg(idx)?;
    ParseUtils::read_double(slice, can_be_infinite)
  }

  /// 尝试获取指定索引的 64 位双精度浮点数
  #[inline]
  pub fn try_get_double(&self, idx: usize, can_be_infinite: bool) -> Option<f64> {
    self
      .get(idx)
      .and_then(|s| ParseUtils::try_read_double(s, can_be_infinite))
  }

  /// 获取指定索引的布尔值
  #[inline]
  pub fn get_bool(&self, idx: usize) -> Result<bool> {
    let slice = self.get_arg(idx)?;
    ParseUtils::read_bool(slice)
  }

  /// 尝试获取指定索引的布尔值
  #[inline]
  pub fn try_get_bool(&self, idx: usize) -> Option<bool> {
    self.get(idx).and_then(ParseUtils::try_read_bool)
  }

  /// 用单个参数快速初始化状态
  #[inline]
  pub fn init_with_arg(&mut self, arg: &'a [u8]) {
    self.clear();
    self.inline_args[0] = arg;
    self.len = 1;
  }

  /// 用两个参数快速初始化状态
  #[inline]
  pub fn init_with_args2(&mut self, arg1: &'a [u8], arg2: &'a [u8]) {
    self.clear();
    self.inline_args[0] = arg1;
    self.inline_args[1] = arg2;
    self.len = 2;
  }

  /// 用三个参数快速初始化状态
  #[inline]
  pub fn init_with_args3(&mut self, arg1: &'a [u8], arg2: &'a [u8], arg3: &'a [u8]) {
    self.clear();
    self.inline_args[0] = arg1;
    self.inline_args[1] = arg2;
    self.inline_args[2] = arg3;
    self.len = 3;
  }

  /// 用四个参数快速初始化状态
  #[inline]
  pub fn init_with_args4(
    &mut self,
    arg1: &'a [u8],
    arg2: &'a [u8],
    arg3: &'a [u8],
    arg4: &'a [u8],
  ) {
    self.clear();
    self.inline_args[0] = arg1;
    self.inline_args[1] = arg2;
    self.inline_args[2] = arg3;
    self.inline_args[3] = arg4;
    self.len = 4;
  }

  /// 用五个参数快速初始化状态
  #[inline]
  pub fn init_with_args5(
    &mut self,
    arg1: &'a [u8],
    arg2: &'a [u8],
    arg3: &'a [u8],
    arg4: &'a [u8],
    arg5: &'a [u8],
  ) {
    self.clear();
    self.inline_args[0] = arg1;
    self.inline_args[1] = arg2;
    self.inline_args[2] = arg3;
    self.inline_args[3] = arg4;
    self.inline_args[4] = arg5;
    self.len = 5;
  }

  /// 设置指定索引的参数（对标 Garnet SetArgument）
  #[inline]
  pub fn set_arg(&mut self, idx: usize, arg: &'a [u8]) {
    if self.spill_args.is_empty() && idx < INLINE_ARGS_CAPACITY {
      self.inline_args[idx] = arg;
      if idx >= self.len {
        self.len = idx + 1;
      }
    } else {
      if self.spill_args.is_empty() {
        self.migrate_to_spill((idx + 8).max(16));
      }
      if idx >= self.spill_args.len() {
        self.spill_args.resize(idx + 1, b"");
      }
      self.spill_args[idx] = arg;
      if idx >= self.len {
        self.len = idx + 1;
      }
    }
  }

  /// 获取自指定偏移开始的指定长度切片视图
  #[inline]
  pub fn slice_range(&self, start: usize, len: usize) -> &[&'a [u8]] {
    let s = self.as_slice();
    if start >= s.len() {
      &[]
    } else {
      let end = (start + len).min(s.len());
      &s[start..end]
    }
  }

  /// 获取指定索引的 32 位单精度浮点数
  #[inline]
  pub fn get_float(&self, idx: usize, can_be_infinite: bool) -> Result<f32> {
    let slice = self.get_arg(idx)?;
    ParseUtils::read_float(slice, can_be_infinite)
  }

  /// 尝试获取指定索引的 32 位单精度浮点数
  #[inline]
  pub fn try_get_float(&self, idx: usize, can_be_infinite: bool) -> Option<f32> {
    self
      .get(idx)
      .and_then(|s| ParseUtils::try_read_float(s, can_be_infinite))
  }

  /// 获取自指定偏移开始的参数切片视图
  #[inline]
  pub fn slice(&self, start: usize) -> &[&'a [u8]] {
    let s = self.as_slice();
    if start >= s.len() { &[] } else { &s[start..] }
  }

  /// 获取全部参数切片视图
  #[inline(always)]
  pub fn as_slice(&self) -> &[&'a [u8]] {
    if self.spill_args.is_empty() {
      // SAFETY: 当 spill_args 为空时，len <= INLINE_ARGS_CAPACITY (8) 绝对成立
      unsafe { self.inline_args.get_unchecked(..self.len) }
    } else {
      &self.spill_args
    }
  }

  /// 尝试匹配 MRU 高频命令缓存（2~3 个 CPU 周期内快速识别）
  #[inline(always)]
  pub fn try_match_mru(&mut self, input: &[u8]) -> Option<(RespCommand, usize, usize)> {
    let Some(s0) = self.mru0 else {
      return None; // 1 个 CPU 周期快速逃逸，冷态/未命中无需多余读取 16 字节
    };
    let len = input.len();
    // 缓存模式最短 13 字节，更短的输入不可能命中任何槽位
    if len < 13 {
      return None;
    }
    // SAFETY: input 非空（len >= 13）
    let mut val = unsafe { load_u128_head(input) };
    upper_cmd_bytes(&mut val);

    // 两槽位模式长度可能不同，须各自校验输入长度后再比对
    if len >= s0.len && val & s0.mask == s0.pattern {
      return Some((s0.cmd, s0.count, s0.len));
    }
    if let Some(s1) = self.mru1
      && len >= s1.len
      && val & s1.mask == s1.pattern
    {
      // 命中槽位 1，执行 MRU 提升与槽位交换
      self.mru0 = Some(s1);
      self.mru1 = Some(s0);
      return Some((s1.cmd, s1.count, s1.len));
    }
    None
  }

  /// 动态记录/更新会话 MRU 缓存（对标 Garnet UpdateCommandCache）
  #[inline]
  pub fn update_mru(
    &mut self,
    orig_input: &[u8],
    header_consumed: usize,
    cmd: RespCommand,
    remaining_count: usize,
  ) {
    if orig_input.len() < header_consumed
      || !(13..=16).contains(&header_consumed)
      || remaining_count > 255
    {
      return;
    }
    // SAFETY: orig_input.len() >= header_consumed >= 13，非空
    let mut val = unsafe { load_u128_head(orig_input) };
    upper_cmd_bytes(&mut val);
    let mask = mask_for(header_consumed);
    let pattern = val & mask;

    // 若槽位 0 已是当前模式，无需重复更新
    if let Some(s0) = self.mru0
      && s0.pattern == pattern
    {
      return;
    }
    // 若槽位 1 包含该模式，提升至槽位 0
    if let Some(s1) = self.mru1
      && s1.pattern == pattern
    {
      self.mru1 = self.mru0;
      self.mru0 = Some(s1);
      return;
    }

    // 槽位 0 下沉至槽位 1，新匹配项提升至槽位 0
    self.mru1 = self.mru0;
    self.mru0 = Some(MruSlot {
      pattern,
      mask,
      cmd,
      count: remaining_count,
      len: header_consumed,
    });
  }

  /// 计算序列化后的总字节数：[4字节参数数量] + 每个参数 [4字节长度 + 载荷]
  /// 对齐 Garnet SessionParseState.GetSerializedLength
  #[inline]
  pub fn serialized_length(&self) -> usize {
    4 + self
      .as_slice()
      .iter()
      .map(|arg| 4 + arg.len())
      .sum::<usize>()
  }

  /// 序列化参数状态至目标切片缓冲区
  /// 对齐 Garnet SessionParseState.SerializeTo
  pub fn serialize_to(&self, output: &mut [u8]) -> Result<usize> {
    let needed = self.serialized_length();
    if output.len() < needed {
      return Err(Error::BufferTooSmall);
    }
    let slice = self.as_slice();
    let count = slice.len() as u32;
    let out_ptr = output.as_mut_ptr();
    // SAFETY: output.len() >= needed >= 4，内存足够写入全部参数头部与载荷
    unsafe {
      (out_ptr as *mut u32).write_unaligned(count.to_le());
      let mut offset = 4;
      for arg in slice {
        let len = arg.len() as u32;
        (out_ptr.add(offset) as *mut u32).write_unaligned(len.to_le());
        offset += 4;
        copy_nonoverlapping(arg.as_ptr(), out_ptr.add(offset), arg.len());
        offset += arg.len();
      }
    }
    Ok(needed)
  }

  /// 从序列化二进制切片零拷贝反序列化参数（引用的生命周期与输入切片一致）
  /// 对齐 Garnet SessionParseState.DeserializeFrom
  pub fn deserialize_from<'b>(&mut self, input: &'b [u8]) -> Result<usize>
  where
    'b: 'a,
  {
    if input.len() < 4 {
      return Err(Error::Incomplete);
    }
    // SAFETY: input.len() >= 4
    let count = u32::from_le_bytes(unsafe { *(input.as_ptr() as *const [u8; 4]) }) as usize;
    // 每个参数至少占用 4 字节的长度前缀，防止恶意大数导致巨量 reserve 耗尽内存
    let max_possible = (input.len() - 4) / 4;
    if count > max_possible {
      return Err(Error::Incomplete);
    }
    self.clear();
    self.reserve(count);
    let mut offset = 4;
    for _ in 0..count {
      if input.len().saturating_sub(offset) < 4 {
        return Err(Error::Incomplete);
      }
      // SAFETY: offset + 4 <= input.len()
      let arg_len =
        u32::from_le_bytes(unsafe { *(input.as_ptr().add(offset) as *const [u8; 4]) }) as usize;
      offset += 4;
      let arg_end = offset.checked_add(arg_len).ok_or(Error::Incomplete)?;
      if arg_end > input.len() {
        return Err(Error::Incomplete);
      }
      // SAFETY: offset..arg_end 在 input 范围内
      self.push(unsafe { input.get_unchecked(offset..arg_end) });
      offset = arg_end;
    }
    Ok(offset)
  }
}

/// 从切片头部加载零填充的 128 位字（不足 16 字节时尾部补零）
///
/// # Safety
/// 调用方必须保证 `input` 非空
#[inline(always)]
unsafe fn load_u128_head(input: &[u8]) -> u128 {
  if input.len() >= 16 {
    // SAFETY: input.len() >= 16，内存有效且可未对齐读取 128 位
    unsafe { (input.as_ptr() as *const u128).read_unaligned() }
  } else {
    let mut buf = [0u8; 16];
    // SAFETY: input.len() < 16，目标缓冲区大小为 16
    unsafe {
      copy_nonoverlapping(input.as_ptr(), buf.as_mut_ptr(), input.len());
    }
    u128::from_ne_bytes(buf)
  }
}

/// 将 128 位字中命令名区域（字节 8..14）就地归一化为大写
///
/// 命令帧头 `*N\r\n$L\r\nCMD\r\n` 的命令名位于字节 8 起，静态表模式最长覆盖至字节 14；
/// 字节 14 起为 `\r\n` 或下一参数（归一化为无操作/被掩码剔除），归一化不影响其余语义。
/// 对齐 C# `MakeUpperCase` 就地大写后再查 MRU 缓存的行为，使任意大小写变体均可命中。
#[inline(always)]
fn upper_cmd_bytes(val: &mut u128) {
  let mut bytes = val.to_ne_bytes();
  let mut i = 8;
  while i < 14 {
    bytes[i] = bytes[i].to_ascii_uppercase();
    i += 1;
  }
  *val = u128::from_ne_bytes(bytes);
}

impl<'a> Deref for SessionParseState<'a> {
  type Target = [&'a [u8]];

  #[inline]
  fn deref(&self) -> &Self::Target {
    self.as_slice()
  }
}

impl<'a, 'b> IntoIterator for &'b SessionParseState<'a> {
  type Item = &'b &'a [u8];
  type IntoIter = Iter<'b, &'a [u8]>;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    self.as_slice().iter()
  }
}

/// `SessionParseState` 值消耗型迭代器（支持栈内联与堆溢出混合状态）
pub enum SessionParseStateIter<'a> {
  Inline(Take<ArrayIntoIter<&'a [u8], INLINE_ARGS_CAPACITY>>),
  Spill(VecIntoIter<&'a [u8]>),
}

impl<'a> Iterator for SessionParseStateIter<'a> {
  type Item = &'a [u8];

  #[inline]
  fn next(&mut self) -> Option<Self::Item> {
    match self {
      Self::Inline(it) => it.next(),
      Self::Spill(it) => it.next(),
    }
  }

  #[inline]
  fn size_hint(&self) -> (usize, Option<usize>) {
    match self {
      Self::Inline(it) => it.size_hint(),
      Self::Spill(it) => it.size_hint(),
    }
  }
}

impl<'a> ExactSizeIterator for SessionParseStateIter<'a> {}

impl<'a> IntoIterator for SessionParseState<'a> {
  type Item = &'a [u8];
  type IntoIter = SessionParseStateIter<'a>;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    if self.spill_args.is_empty() {
      SessionParseStateIter::Inline(self.inline_args.into_iter().take(self.len))
    } else {
      SessionParseStateIter::Spill(self.spill_args.into_iter())
    }
  }
}

#[inline]
fn to_unknown_cmd_name(slice: &[u8]) -> HipStr<'static> {
  match from_utf8(slice) {
    Ok(s) => HipStr::from(s),
    Err(_) => HipStr::from(String::from_utf8_lossy(slice).into_owned()),
  }
}

/// 单次扫描会话命令解析状态机
///
/// 从客户端输入流 `input` 中解析单个 RESP 命令及所有参数
/// 支持标准 RESP 数组帧（`*N\r\n...`）以及内联命令（如 `PING\r\n`）
/// 具备事务安全性：若解析失败（如数据不完整）则不会推进 `input` 切片指针，并清理状态
pub fn parse_session_command<'a>(
  input: &mut &'a [u8],
  state: &mut SessionParseState<'a>,
) -> Result<RespCommand> {
  if input.is_empty() {
    return Err(Error::Incomplete);
  }

  let mut cursor = *input;
  state.clear();

  // SAFETY: input 非空，0 号字节安全
  let res = if unsafe { *cursor.get_unchecked(0) } != b'*' {
    parse_inline_command(&mut cursor, state)
  } else if let Some((cmd, remaining_count, consumed)) =
    simd_fast_parse(cursor).or_else(|| state.try_match_mru(cursor))
  {
    // SAFETY: consumed <= cursor.len()
    cursor = unsafe { cursor.get_unchecked(consumed..) };
    state.reserve(remaining_count);
    let mut res = Ok(cmd);
    for _ in 0..remaining_count {
      match RespReadUtils::read_bulk_string(&mut cursor) {
        Ok(arg) => state.push(arg),
        Err(e) => {
          res = Err(e);
          break;
        }
      }
    }
    res
  } else {
    parse_array_command(&mut cursor, state)
  };

  match res {
    Ok(cmd) => {
      *input = cursor;
      Ok(cmd)
    }
    Err(e) => {
      state.clear();
      Err(e)
    }
  }
}

/// 解析 RESP 数组格式命令
fn parse_array_command<'a>(
  input: &mut &'a [u8],
  state: &mut SessionParseState<'a>,
) -> Result<RespCommand> {
  let orig_input = *input;
  let array_len = match RespReadUtils::read_array_len(input)? {
    Some(len) => len,
    None => return Ok(RespCommand::NONE), // *-1\r\n
  };

  // 拒绝超大 *N 头，防止按元素数巨量预留内存（对齐 C# MaxRespArrayLength 检查）
  if array_len > MAX_RESP_ARRAY_LENGTH {
    return Err(Error::ExcessiveArgs(array_len, MAX_RESP_ARRAY_LENGTH));
  }

  if array_len == 0 {
    return Ok(RespCommand::NONE);
  }

  // 读取第一个元素作为主命令名称
  let cmd_slice = RespReadUtils::read_bulk_string(input)?;
  let (primary_cmd, has_sub) = match RespCommand::lookup(cmd_slice) {
    Some(res) => res,
    None => {
      return Err(Error::UnknownCommand(to_unknown_cmd_name(cmd_slice)));
    }
  };

  let mut final_cmd = primary_cmd;
  let mut remaining_count = array_len - 1;
  let mut matched_sub = false;

  // 若主命令拥有子命令分支且还有后续参数，尝试匹配子命令
  if has_sub && remaining_count > 0 {
    let next_arg = RespReadUtils::read_bulk_string(input)?;
    if let Some(sub_cmd) = RespCommand::lookup_subcommand(primary_cmd, next_arg) {
      final_cmd = sub_cmd;
      remaining_count -= 1;
      matched_sub = true;
    } else {
      // 未匹配到子命令，保留该参数为第一个普通参数
      state.push(next_arg);
      remaining_count -= 1;
    }
  }

  // 若未混入普通参数，尝试更新 MRU 缓存加速后续相同命令
  if !has_sub || matched_sub {
    let header_consumed = orig_input.len() - input.len();
    state.update_mru(orig_input, header_consumed, final_cmd, remaining_count);
  }

  // 顺序读取剩余所有定长参数切片
  state.reserve(remaining_count);
  for _ in 0..remaining_count {
    let arg = RespReadUtils::read_bulk_string(input)?;
    state.push(arg);
  }

  Ok(final_cmd)
}

/// 解析内联命令格式（以空格分隔、以 `\r\n` 结尾的纯文本行，零堆分配）
fn parse_inline_command<'a>(
  input: &mut &'a [u8],
  state: &mut SessionParseState<'a>,
) -> Result<RespCommand> {
  let cr = match find_crlf(input)? {
    Some(pos) => pos,
    None => return Err(Error::Incomplete),
  };

  let line = &input[..cr];
  *input = &input[cr + 2..];

  let mut tokens = line
    .split(|&b| b == b' ' || b == b'\t')
    .filter(|s| !s.is_empty());

  let Some(cmd_slice) = tokens.next() else {
    return Ok(RespCommand::NONE);
  };

  let (primary_cmd, has_sub) = match RespCommand::lookup(cmd_slice) {
    Some(res) => res,
    None => {
      return Err(Error::UnknownCommand(to_unknown_cmd_name(cmd_slice)));
    }
  };

  let mut final_cmd = primary_cmd;

  if has_sub && let Some(next_tok) = tokens.next() {
    if let Some(sub_cmd) = RespCommand::lookup_subcommand(primary_cmd, next_tok) {
      final_cmd = sub_cmd;
    } else {
      state.push(next_tok);
    }
  }

  for tok in tokens {
    state.push(tok);
  }

  Ok(final_cmd)
}
