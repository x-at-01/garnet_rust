use core::str::from_utf8;

use crate::{
  consts::resp::{INFINITY, NEG_INFINITY, POS_INFINITY},
  error::{Error, Result},
};

#[inline]
pub(crate) fn find_crlf(input: &[u8]) -> Result<Option<usize>> {
  if let Some(pos) = memchr::memchr(b'\r', input) {
    if pos + 1 >= input.len() {
      Ok(None)
    } else if input[pos + 1] == b'\n' {
      Ok(Some(pos))
    } else {
      Err(Error::UnexpectedToken(input[pos + 1]))
    }
  } else {
    Ok(None)
  }
}

/// 长度头 / 整数帧数字部分的最大位数上限（含符号），防御畸形超长输入
const MAX_NUM_DIGITS: usize = 32;

/// 数字行过长（刻意比 C# 更严格：C# 允许任意前导零长度头，此处封顶防 DoS）
const ERR_NUM_TOO_LONG: &str = "数字行过长";

/// 扫描 `[+/-]<digits>\r\n` 数字行（对齐 C# `TryReadSignedLengthHeader` 与 `TryReadInt64` 的公共扫描逻辑）
///
/// 返回 `(数值, 数字结束偏移)`，偏移相对 `rest` 起始（含符号位）；
/// 输入不完整时返回 `Error::Incomplete`，由上层按流式语义等待更多数据
fn scan_int_line(rest: &[u8]) -> Result<(i64, usize)> {
  let sign_len = match rest.first() {
    Some(b'-' | b'+') => 1,
    _ => 0,
  };
  if rest.len() == sign_len {
    return Err(Error::Incomplete);
  }

  let mut i = sign_len;
  while i < rest.len() && rest[i].is_ascii_digit() {
    i += 1;
  }

  if i == sign_len {
    if rest[i..].starts_with(b"\r\n") {
      return Err(Error::NotANumber);
    }
    if rest[i] == b'\r' {
      return Err(Error::Incomplete);
    }
    return Err(Error::UnexpectedToken(rest[i]));
  }

  if i > MAX_NUM_DIGITS {
    return Err(Error::Protocol(ERR_NUM_TOO_LONG));
  }
  if i == rest.len() {
    return Err(Error::Incomplete);
  }
  if rest[i] != b'\r' {
    return Err(Error::UnexpectedToken(rest[i]));
  }
  if i + 1 >= rest.len() {
    return Err(Error::Incomplete);
  }
  if rest[i + 1] != b'\n' {
    return Err(Error::UnexpectedToken(rest[i + 1]));
  }

  let (val, read) = RespReadUtils::try_read_int64(&rest[..i], true)?;
  if read != i {
    return Err(Error::NotANumber);
  }
  Ok((val, i))
}

/// 校验长度头载荷后的 `\r\n` 终止符并零拷贝提取载荷切片
///
/// `input[header_len..header_len + length]` 为载荷，随后必须紧跟 `\r\n`
fn payload_after_header(input: &[u8], header_len: usize, length: usize) -> Result<&[u8]> {
  let total_required = header_len + length + 2;
  if input.len() < total_required {
    return Err(Error::Incomplete);
  }
  // SAFETY: total_required <= input.len() 已验证
  let crlf = unsafe { input.get_unchecked(header_len + length..total_required) };
  if crlf != b"\r\n" {
    return Err(Error::UnexpectedToken(crlf[0]));
  }
  // SAFETY: header_len + length < total_required <= input.len()
  Ok(unsafe { input.get_unchecked(header_len..header_len + length) })
}

/// RESP 协议读取工具集，完全零拷贝读取与切片提取
/// 对齐 Microsoft Garnet `RespReadUtils`
pub struct RespReadUtils;

impl RespReadUtils {
  /// 单个 RESP 参数的最大允许字节数（512MB，匹配 Redis 规范）
  pub const MAX_ARGUMENT_LENGTH_BYTES: usize = 512 * 1024 * 1024;

  /// 在切片中查找 CRLF (`\r\n`)
  #[inline]
  pub fn find_crlf(input: &[u8]) -> Result<Option<usize>> {
    find_crlf(input)
  }

  /// 从 ASCII 字节切片读取无符号 64 位整数，带严格溢出检测
  ///
  /// 返回 `(数值, 消耗字节数)`
  #[inline]
  pub fn try_read_uint64(input: &[u8]) -> Result<(u64, usize)> {
    if input.is_empty() {
      return Err(Error::Incomplete);
    }

    let mut value: u64 = 0;
    let mut bytes_read = 0;

    // 前 18 个十进制数字绝不可能导致 u64 溢出 (10^18 < 2^64 - 1 = 1.844... * 10^19)
    while bytes_read < input.len() && bytes_read < 18 {
      // SAFETY: bytes_read < input.len() 已保证边界有效
      let b = unsafe { *input.get_unchecked(bytes_read) };
      if b.is_ascii_digit() {
        value = value * 10 + (b - b'0') as u64;
        bytes_read += 1;
      } else {
        break;
      }
    }

    if bytes_read == 0 {
      // SAFETY: input 非空已在开头确认
      let b = unsafe { *input.get_unchecked(0) };
      if !b.is_ascii_digit() {
        return Err(Error::NotANumber);
      }
    }

    // 超过 18 位数字时启用严格溢出检测
    while bytes_read < input.len() {
      // SAFETY: bytes_read < input.len() 已保证边界有效
      let b = unsafe { *input.get_unchecked(bytes_read) };
      if b.is_ascii_digit() {
        let digit = (b - b'0') as u64;
        let Some(next_val) = value.checked_mul(10).and_then(|v| v.checked_add(digit)) else {
          return Err(Error::IntegerOverflow);
        };
        value = next_val;
        bytes_read += 1;
      } else {
        break;
      }
    }

    Ok((value, bytes_read))
  }

  /// 读取前导符号（`+` 或 `-`）
  #[inline]
  pub fn try_read_sign(input: &[u8]) -> (bool, bool, usize) {
    if let Some((&b, _)) = input.split_first() {
      match b {
        b'-' => (true, true, 1),
        b'+' => (true, false, 1),
        _ => (false, false, 0),
      }
    } else {
      (false, false, 0)
    }
  }

  /// 从 ASCII 字节切片读取有符号 64 位整数
  ///
  /// 支持可选的正负符号 `+`/`-`，可配置是否允许前导零
  #[inline]
  pub fn try_read_int64(input: &[u8], allow_leading_zeros: bool) -> Result<(i64, usize)> {
    let (val, bytes, _) = Self::try_read_int64_safe(input, allow_leading_zeros)?;
    Ok((val, bytes))
  }

  /// 读取有符号 64 位整数（安全版本，返回 `(数值, 消耗字节数, 是否包含符号)`）
  /// 对齐 C# `TryReadInt64Safe`
  pub fn try_read_int64_safe(
    input: &[u8],
    allow_leading_zeros: bool,
  ) -> Result<(i64, usize, bool)> {
    if input.is_empty() {
      return Err(Error::Incomplete);
    }

    let (sign_read, negative, cursor) = Self::try_read_sign(input);
    let remaining = &input[cursor..];
    if remaining.is_empty() {
      if sign_read {
        return Err(Error::NotANumber);
      }
      return Err(Error::Incomplete);
    }

    // 检查前导零规则（对齐 C#: 剩余长度 > 1 且首字符为 '0' 即拒绝）
    if !allow_leading_zeros && remaining.len() > 1 && remaining[0] == b'0' {
      return Err(Error::NotANumber);
    }

    let (u_val, digits_read) = Self::try_read_uint64(remaining)?;
    let total_bytes = cursor + digits_read;

    let value = if negative {
      const MIN_ABS: u64 = (i64::MAX as u64) + 1;
      if u_val > MIN_ABS {
        return Err(Error::IntegerOverflow);
      }
      u_val.wrapping_neg() as i64
    } else {
      if u_val > i64::MAX as u64 {
        return Err(Error::IntegerOverflow);
      }
      u_val as i64
    };

    Ok((value, total_bytes, sign_read))
  }

  /// 从 ASCII 字节切片读取有符号 32 位整数
  #[inline]
  pub fn try_read_int32(input: &[u8], allow_leading_zeros: bool) -> Result<(i32, usize)> {
    let (val, bytes, _) = Self::try_read_int32_safe(input, allow_leading_zeros)?;
    Ok((val, bytes))
  }

  /// 读取有符号 32 位整数（安全版本，返回 `(数值, 消耗字节数, 是否包含符号)`）
  /// 对齐 C# `TryReadInt32Safe`
  #[inline]
  pub fn try_read_int32_safe(
    input: &[u8],
    allow_leading_zeros: bool,
  ) -> Result<(i32, usize, bool)> {
    let (val64, bytes_read, sign_read) = Self::try_read_int64_safe(input, allow_leading_zeros)?;
    let val32 = i32::try_from(val64).map_err(|_| Error::IntegerOverflow)?;
    Ok((val32, bytes_read, sign_read))
  }

  /// 读取有符号 RESP 32 位长度头（对齐 C# `TryReadSignedLengthHeader`）
  ///
  /// 返回 `(长度值, 消耗字节数)`，若为 NULL 则长度值为 -1
  pub fn try_read_signed_length_header_i32(
    input: &[u8],
    expected_sigil: u8,
  ) -> Result<(i32, usize)> {
    if input.len() < 3 {
      return Err(Error::Incomplete);
    }

    // RESP3 null: "_\r\n"
    if input.starts_with(b"_\r\n") {
      return Ok((-1, 3));
    }

    // 长度头前缀必须匹配（如 '$' / '*'）
    if input[0] != expected_sigil {
      return Err(Error::UnexpectedToken(input[0]));
    }

    // 快速匹配 "*-1\r\n" 或 "$-1\r\n"
    if input[1..].starts_with(b"-1\r\n") {
      return Ok((-1, 5));
    }

    let (val64, i) = scan_int_line(&input[1..])?;
    if val64 > i32::MAX as i64 || val64 < i32::MIN as i64 {
      return Err(Error::IntegerOverflow);
    }
    Ok((val64 as i32, 1 + i + 2))
  }

  /// 读取有符号 RESP 长度头，允许表示 NULL 的 `-1`（如 `$-1\r\n` 或 `*-1\r\n`）
  ///
  /// 返回 `(Option<长度>, 消耗字节数)`，若为 NULL 则长度为 `None`
  pub fn try_read_signed_length_header(
    input: &[u8],
    expected_sigil: u8,
  ) -> Result<(Option<usize>, usize)> {
    let (val32, total_bytes) = Self::try_read_signed_length_header_i32(input, expected_sigil)?;
    if val32 < 0 {
      if val32 == -1 {
        Ok((None, total_bytes))
      } else {
        Err(Error::InvalidLength(val32 as i64))
      }
    } else {
      Ok((Some(val32 as usize), total_bytes))
    }
  }

  /// 读取无符号 RESP 长度头（严格不允许负数或 NULL）
  pub fn try_read_unsigned_length_header(
    input: &[u8],
    expected_sigil: u8,
  ) -> Result<(usize, usize)> {
    let (val32, total_bytes) = Self::try_read_signed_length_header_i32(input, expected_sigil)?;
    if val32 < 0 {
      return Err(Error::InvalidLength(val32 as i64));
    }
    Ok((val32 as usize, total_bytes))
  }

  /// 读取 RESP 数组长度头 `*<len>\r\n`，支持 `*-1\r\n` NULL 数组
  #[inline]
  pub fn try_read_signed_array_len(input: &[u8]) -> Result<(Option<usize>, usize)> {
    Self::try_read_signed_length_header(input, b'*')
  }

  /// 读取严格无符号的 RESP 数组长度头 `*<len>\r\n`
  #[inline]
  pub fn try_read_unsigned_array_len(input: &[u8]) -> Result<(usize, usize)> {
    Self::try_read_unsigned_length_header(input, b'*')
  }

  /// 读取 RESP3 字典长度头 `%<len>\r\n`
  #[inline]
  pub fn try_read_signed_map_len(input: &[u8]) -> Result<(Option<usize>, usize)> {
    Self::try_read_signed_length_header(input, b'%')
  }

  /// 读取 RESP3 集合长度头 `~<len>\r\n`
  #[inline]
  pub fn try_read_signed_set_len(input: &[u8]) -> Result<(Option<usize>, usize)> {
    Self::try_read_signed_length_header(input, b'~')
  }

  /// 读取原样字符串长度头 `=<len>\r\n`
  #[inline]
  pub fn try_read_verbatim_string_len(input: &[u8]) -> Result<(Option<usize>, usize)> {
    Self::try_read_signed_length_header(input, b'=')
  }

  /// 零拷贝切片提取定长字符串（Bulk String）载荷
  ///
  /// 输入格式为 `$<len>\r\n<payload>\r\n`
  /// 返回 `(&[u8], 消耗的总字节数)`
  pub fn try_slice_with_length_header(input: &[u8]) -> Result<(&[u8], usize)> {
    let (length, header_len) = Self::try_read_unsigned_length_header(input, b'$')?;

    if length > Self::MAX_ARGUMENT_LENGTH_BYTES {
      return Err(Error::InvalidLength(length as i64));
    }

    let payload = payload_after_header(input, header_len, length)?;
    Ok((payload, header_len + length + 2))
  }

  /// 零拷贝读取 UTF-8 字符串
  #[inline]
  pub fn try_read_string_with_length_header(input: &[u8]) -> Result<(&str, usize)> {
    let (slice, bytes) = Self::try_slice_with_length_header(input)?;
    let s = from_utf8(slice)?;
    Ok((s, bytes))
  }

  /// 读取定长字节数组（对齐 C# `TryReadByteArrayWithLengthHeader`）
  pub fn try_read_byte_array_with_length_header(input: &[u8]) -> Result<(Vec<u8>, usize)> {
    let (slice, bytes) = Self::try_slice_with_length_header(input)?;
    Ok((slice.to_vec(), bytes))
  }

  /// 零拷贝切片提取定长字节数组（对齐 C# `TryReadSpanWithLengthHeader`）
  #[inline]
  pub fn try_read_span_with_length_header(input: &[u8]) -> Result<(&[u8], usize)> {
    Self::try_slice_with_length_header(input)
  }

  /// 读取无符号长度头的指针切片（对齐 C# `TryReadPtrWithLengthHeader`）
  #[inline]
  pub fn try_read_ptr_with_length_header(input: &[u8]) -> Result<(&[u8], usize)> {
    Self::try_slice_with_length_header(input)
  }

  /// 读取带符号长度头的指针切片（对齐 C# `TryReadPtrWithSignedLengthHeader`）
  pub fn try_read_ptr_with_signed_length_header(input: &[u8]) -> Result<(Option<&[u8]>, usize)> {
    let (val32, header_len) = Self::try_read_signed_length_header_i32(input, b'$')?;
    if val32 < 0 {
      if val32 == -1 {
        return Ok((None, header_len));
      }
      return Err(Error::InvalidLength(val32 as i64));
    }
    let length = val32 as usize;
    if length > Self::MAX_ARGUMENT_LENGTH_BYTES {
      return Err(Error::InvalidLength(val32 as i64));
    }
    let payload = payload_after_header(input, header_len, length)?;
    Ok((Some(payload), header_len + length + 2))
  }

  /// 读取带符号长度头的字符串响应（支持 null，对齐 C# `TryReadStringResponseWithLengthHeader`）
  pub fn try_read_string_response_with_length_header(
    input: &[u8],
  ) -> Result<(Option<&str>, usize)> {
    let (slice_opt, bytes) = Self::try_read_ptr_with_signed_length_header(input)?;
    match slice_opt {
      Some(slice) => {
        let s = from_utf8(slice)?;
        Ok((Some(s), bytes))
      }
      None => Ok((None, bytes)),
    }
  }

  /// 读取定长整数协议帧 `:<integer>\r\n`
  pub fn try_read_int64_frame(input: &[u8]) -> Result<(i64, usize)> {
    let Some((&b':', rest)) = input.split_first() else {
      if input.is_empty() {
        return Err(Error::Incomplete);
      }
      return Err(Error::UnexpectedToken(input[0]));
    };

    if rest.is_empty() {
      return Err(Error::Incomplete);
    }

    let (val, i) = scan_int_line(rest)?;
    Ok((val, 1 + i + 2))
  }

  /// 读取以定长字符串存储的有符号 32 位整数（形如 `$<len>\r\n<digits>\r\n`）
  pub fn try_read_int32_with_length_header(input: &[u8]) -> Result<(i32, usize)> {
    let (slice, total_len) = Self::try_slice_with_length_header(input)?;
    let (num, read_len) = Self::try_read_int32(slice, true)?;
    if read_len != slice.len() {
      return Err(Error::NotANumber);
    }
    Ok((num, total_len))
  }

  /// 读取以定长字符串存储的有符号 64 位整数
  pub fn try_read_int64_with_length_header(input: &[u8]) -> Result<(i64, usize)> {
    let (slice, total_len) = Self::try_slice_with_length_header(input)?;
    let (num, read_len) = Self::try_read_int64(slice, true)?;
    if read_len != slice.len() {
      return Err(Error::NotANumber);
    }
    Ok((num, total_len))
  }

  /// 读取以定长字符串存储的无符号 64 位整数
  pub fn try_read_uint64_with_length_header(input: &[u8]) -> Result<(u64, usize)> {
    let (slice, total_len) = Self::try_slice_with_length_header(input)?;
    let (num, read_len) = Self::try_read_uint64(slice)?;
    if read_len != slice.len() {
      return Err(Error::NotANumber);
    }
    Ok((num, total_len))
  }

  /// 读取布尔值（支持 `$1\r\n1\r\n`/`$1\r\n0\r\n` 或 RESP3 `#t\r\n`/`#f\r\n`）
  pub fn try_read_bool_with_length_header(input: &[u8]) -> Result<(bool, usize)> {
    if input.is_empty() {
      return Err(Error::Incomplete);
    }

    // 检查 RESP3 布尔格式
    if input.starts_with(b"#") {
      if input.len() < 4 {
        return Err(Error::Incomplete);
      }
      if input.starts_with(b"#t\r\n") {
        return Ok((true, 4));
      }
      if input.starts_with(b"#f\r\n") {
        return Ok((false, 4));
      }
      return Err(Error::UnexpectedToken(input[1]));
    }

    // BulkString 格式
    let (slice, total_len) = Self::try_slice_with_length_header(input)?;
    if slice.len() != 1 {
      return Err(Error::InvalidLength(slice.len() as i64));
    }

    let val = match slice[0] {
      b'1' => true,
      b'0' => false,
      _ => return Err(Error::NotANumber),
    };

    Ok((val, total_len))
  }

  /// 读取简单字符串 `+<string>\r\n`
  pub fn try_read_simple_string(input: &[u8]) -> Result<(&[u8], usize)> {
    let Some((&b'+', rest)) = input.split_first() else {
      if input.is_empty() {
        return Err(Error::Incomplete);
      }
      return Err(Error::UnexpectedToken(input[0]));
    };

    match find_crlf(rest)? {
      Some(pos) => Ok((&rest[..pos], 1 + pos + 2)),
      None => Err(Error::Incomplete),
    }
  }

  /// 读取错误字符串 `-<error>\r\n`
  pub fn try_read_error(input: &[u8]) -> Result<(&[u8], usize)> {
    let Some((&b'-', rest)) = input.split_first() else {
      if input.is_empty() {
        return Err(Error::Incomplete);
      }
      return Err(Error::UnexpectedToken(input[0]));
    };

    match find_crlf(rest)? {
      Some(pos) => Ok((&rest[..pos], 1 + pos + 2)),
      None => Err(Error::Incomplete),
    }
  }

  /// 读取错误并转为 UTF-8 字符串（对齐 C# `TryReadErrorAsString`）
  pub fn try_read_error_as_string(input: &[u8]) -> Result<(&str, usize)> {
    let (slice, bytes) = Self::try_read_error(input)?;
    let s = from_utf8(slice)?;
    Ok((s, bytes))
  }

  /// 读取直到 `\r\n` 的切片（无 sigil 前缀，对齐 C# `TryReadAsSpan`）
  pub fn try_read_as_span(input: &[u8]) -> Result<(&[u8], usize)> {
    match find_crlf(input)? {
      Some(pos) => Ok((&input[..pos], pos + 2)),
      None => Err(Error::Incomplete),
    }
  }

  /// 读取直到 `\r\n` 的 UTF-8 字符串（无 sigil 前缀，对齐 C# `TryReadString`）
  pub fn try_read_string(input: &[u8]) -> Result<(&str, usize)> {
    let (slice, bytes) = Self::try_read_as_span(input)?;
    let s = from_utf8(slice)?;
    Ok((s, bytes))
  }

  /// 读取整型协议帧切片（对齐 C# `TryReadIntegerAsSpan`）
  pub fn try_read_integer_as_span(input: &[u8]) -> Result<(&[u8], usize)> {
    if input.len() < 3 {
      return Err(Error::Incomplete);
    }
    if input[0] != b':' {
      return Err(Error::UnexpectedToken(input[0]));
    }
    let (slice, bytes) = Self::try_read_as_span(&input[1..])?;
    Ok((slice, bytes + 1))
  }

  /// 读取整型协议帧并转为字符串（对齐 C# `TryReadIntegerAsString`）
  pub fn try_read_integer_as_string(input: &[u8]) -> Result<(&str, usize)> {
    let (slice, bytes) = Self::try_read_integer_as_span(input)?;
    let s = from_utf8(slice)?;
    Ok((s, bytes))
  }

  /// 读取包含字符串与整型的字符串数组（对齐 C# `TryReadStringArrayWithLengthHeader`）
  pub fn try_read_string_array_with_length_header(input: &[u8]) -> Result<(Vec<&str>, usize)> {
    let (len, mut offset) = Self::try_read_unsigned_array_len(input)?;
    // 每个元素至少占 4 字节（如 `:0\r\n`），预分配按输入实际容量封顶，防止恶意大长度头巨量分配
    let mut result = Vec::with_capacity(len.min(input.len() / 4));
    for _ in 0..len {
      if offset >= input.len() {
        return Err(Error::Incomplete);
      }
      if input[offset] == b'$' {
        let (s, read_bytes) = Self::try_read_string_with_length_header(&input[offset..])?;
        result.push(s);
        offset += read_bytes;
      } else {
        let (s, read_bytes) = Self::try_read_integer_as_string(&input[offset..])?;
        result.push(s);
        offset += read_bytes;
      }
    }
    Ok((result, offset))
  }

  /// 解析无穷大常量（支持大小写不敏感的 `INF`、`+INF`、`-INF`、`INFINITY`、`+INFINITY`、`-INFINITY`）
  pub fn try_read_infinity(value: &[u8]) -> Option<f64> {
    match value.len() {
      3 => {
        if value.eq_ignore_ascii_case(INFINITY) {
          Some(f64::INFINITY)
        } else {
          None
        }
      }
      4 => {
        if value.eq_ignore_ascii_case(POS_INFINITY) {
          Some(f64::INFINITY)
        } else if value.eq_ignore_ascii_case(NEG_INFINITY) {
          Some(f64::NEG_INFINITY)
        } else {
          None
        }
      }
      8 => {
        if value.eq_ignore_ascii_case(b"INFINITY") {
          Some(f64::INFINITY)
        } else {
          None
        }
      }
      9 => {
        if value.eq_ignore_ascii_case(b"+INFINITY") {
          Some(f64::INFINITY)
        } else if value.eq_ignore_ascii_case(b"-INFINITY") {
          Some(f64::NEG_INFINITY)
        } else {
          None
        }
      }
      _ => None,
    }
  }

  /// 解析 32 位浮点无穷大（对齐 C# `TryReadInfinity(ReadOnlySpan<byte>, out float)`）
  #[inline]
  pub fn try_read_infinity_f32(value: &[u8]) -> Option<f32> {
    Self::try_read_infinity(value).map(|v| v as f32)
  }

  /// 读取双精度浮点数（支持 `$len\r\n<double>\r\n` 或 `,val\r\n`）
  pub fn try_read_double_with_length_header(input: &[u8]) -> Result<(f64, usize)> {
    let Some((&first, rest)) = input.split_first() else {
      return Err(Error::Incomplete);
    };

    // RESP3 数字浮点格式 `,123.45\r\n`
    if first == b',' {
      let pos = match find_crlf(rest)? {
        Some(p) => p,
        None => {
          if rest.len() > 64 {
            return Err(Error::Protocol("浮点数格式过长"));
          }
          return Err(Error::Incomplete);
        }
      };
      let raw = &rest[..pos];
      let val = Self::parse_double(raw)?;
      return Ok((val, 1 + pos + 2));
    }

    // BulkString 格式
    let (slice, total_len) = Self::try_slice_with_length_header(input)?;
    let val = Self::parse_double(slice)?;
    Ok((val, total_len))
  }

  /// 读取单精度浮点数（支持 `$len\r\n<float>\r\n` 或 `,val\r\n`）
  #[inline]
  pub fn try_read_float_with_length_header(input: &[u8]) -> Result<(f32, usize)> {
    let (val, len) = Self::try_read_double_with_length_header(input)?;
    Ok((val as f32, len))
  }

  /// 读取 RESP3 大数帧 `(<digits>\r\n`
  pub fn try_read_big_number(input: &[u8]) -> Result<(&[u8], usize)> {
    let Some((&b'(', rest)) = input.split_first() else {
      if input.is_empty() {
        return Err(Error::Incomplete);
      }
      return Err(Error::UnexpectedToken(input[0]));
    };

    match find_crlf(rest)? {
      Some(pos) => {
        let raw = &rest[..pos];
        if raw.is_empty() {
          return Err(Error::NotANumber);
        }
        Ok((raw, 1 + pos + 2))
      }
      None => {
        if rest.len() > 1024 {
          return Err(Error::Protocol("大数格式过长"));
        }
        Err(Error::Incomplete)
      }
    }
  }

  /// 内部浮点数解析辅助
  #[inline]
  pub fn parse_double(slice: &[u8]) -> Result<f64> {
    if slice.is_empty() {
      return Err(Error::NotANumber);
    }
    if let Some(inf) = Self::try_read_infinity(slice) {
      return Ok(inf);
    }
    let s = if let Some(stripped) = slice
      .strip_prefix(b"+")
      .or_else(|| slice.strip_prefix(b"-"))
    {
      stripped
    } else {
      slice
    };
    if s.eq_ignore_ascii_case(b"NAN") {
      return Ok(f64::NAN);
    }
    fast_float::parse(slice).map_err(|_| Error::NotANumber)
  }

  /// 跳过定长字节数组
  #[inline]
  pub fn try_skip_byte_array_with_length_header(input: &[u8]) -> Result<usize> {
    let (_, total_bytes) = Self::try_slice_with_length_header(input)?;
    Ok(total_bytes)
  }

  /// 读取序列化日志记录切片（迁移与复制使用，对齐 C# `GetSerializedRecordSpan`）
  ///
  /// 布局：`[i32 小端长度][数据载荷]`
  /// 返回 `(&[u8], 消耗的总字节数)`
  pub fn get_serialized_record_span(input: &[u8]) -> Result<(&[u8], usize)> {
    if input.len() < 4 {
      return Err(Error::Incomplete);
    }
    // SAFETY: input.len() >= 4 已验证
    let record_length = i32::from_le_bytes(unsafe { *(input.as_ptr() as *const [u8; 4]) });
    if record_length < 0 {
      return Err(Error::InvalidLength(record_length as i64));
    }
    let ulen = record_length as usize;
    if input.len() - 4 < ulen {
      return Err(Error::Incomplete);
    }
    let total = 4 + ulen;
    // SAFETY: 4 + ulen <= input.len() 已验证
    Ok((unsafe { input.get_unchecked(4..total) }, total))
  }

  // ====== 推进指针的高性能便捷 API ======

  /// 读取定长字符串切片并推进指针
  #[inline]
  pub fn read_bulk_string<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let (slice, bytes) = Self::try_slice_with_length_header(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(slice)
  }

  /// 读取定长 UTF-8 字符串切片并推进指针
  #[inline]
  pub fn read_string_with_length_header<'a>(input: &mut &'a [u8]) -> Result<&'a str> {
    let (slice, bytes) = Self::try_read_string_with_length_header(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(slice)
  }

  /// 读取简单字符串切片并推进指针
  #[inline]
  pub fn read_simple_string<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let (slice, bytes) = Self::try_read_simple_string(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(slice)
  }

  /// 读取错误切片并推进指针
  #[inline]
  pub fn read_error<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let (slice, bytes) = Self::try_read_error(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(slice)
  }

  /// 读取无 sigil 前缀的 UTF-8 字符串切片并推进指针
  #[inline]
  pub fn read_string<'a>(input: &mut &'a [u8]) -> Result<&'a str> {
    let (slice, bytes) = Self::try_read_string(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(slice)
  }

  /// 读取数组长度头并推进指针
  #[inline]
  pub fn read_array_len(input: &mut &[u8]) -> Result<Option<usize>> {
    let (len, bytes) = Self::try_read_signed_array_len(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(len)
  }

  /// 读取长度头并推进指针
  #[inline]
  pub fn read_length_header(input: &mut &[u8], expected_sigil: u8) -> Result<Option<usize>> {
    let (len, bytes) = Self::try_read_signed_length_header(input, expected_sigil)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(len)
  }

  /// 读取严格无符号长度头并推进指针
  #[inline]
  pub fn read_unsigned_length_header(input: &mut &[u8], expected_sigil: u8) -> Result<usize> {
    let (len, bytes) = Self::try_read_unsigned_length_header(input, expected_sigil)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(len)
  }

  /// 读取整型协议帧并推进指针
  #[inline]
  pub fn read_int64_frame(input: &mut &[u8]) -> Result<i64> {
    let (val, bytes) = Self::try_read_int64_frame(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(val)
  }

  /// 读取以定长字符串存储的 32 位有符号整数并推进指针
  #[inline]
  pub fn read_int_with_length_header(input: &mut &[u8]) -> Result<i32> {
    let (val, bytes) = Self::try_read_int32_with_length_header(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(val)
  }

  /// 读取以定长字符串存储的 64 位有符号长整型并推进指针
  #[inline]
  pub fn read_long_with_length_header(input: &mut &[u8]) -> Result<i64> {
    let (val, bytes) = Self::try_read_int64_with_length_header(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(val)
  }

  /// 读取以定长字符串存储的 64 位无符号长整型并推进指针
  #[inline]
  pub fn read_ulong_with_length_header(input: &mut &[u8]) -> Result<u64> {
    let (val, bytes) = Self::try_read_uint64_with_length_header(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(val)
  }

  /// 读取布尔值并推进指针
  #[inline]
  pub fn read_bool_with_length_header(input: &mut &[u8]) -> Result<bool> {
    let (val, bytes) = Self::try_read_bool_with_length_header(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(val)
  }

  /// 读取双精度浮点数并推进指针
  #[inline]
  pub fn read_double_with_length_header(input: &mut &[u8]) -> Result<f64> {
    let (val, bytes) = Self::try_read_double_with_length_header(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(val)
  }

  /// 读取单精度浮点数并推进指针
  #[inline]
  pub fn read_float_with_length_header(input: &mut &[u8]) -> Result<f32> {
    let (val, bytes) = Self::try_read_float_with_length_header(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(val)
  }

  /// 读取 RESP3 大数帧并推进指针
  #[inline]
  pub fn read_big_number<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let (slice, bytes) = Self::try_read_big_number(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(slice)
  }

  /// 读取序列化日志记录切片并推进指针
  #[inline]
  pub fn read_serialized_record_span<'a>(input: &mut &'a [u8]) -> Result<&'a [u8]> {
    let (slice, bytes) = Self::get_serialized_record_span(input)?;
    *input = unsafe { (*input).get_unchecked(bytes..) };
    Ok(slice)
  }
}
