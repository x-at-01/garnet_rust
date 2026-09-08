use crate::{
  consts::resp::{
    CRLF, EMPTY_ARRAY, EMPTY_MAP, EMPTY_SET, INTEGER_ONE, INTEGER_ZERO, RESP2_NULL_ARRAY,
    RESP2_NULL_BULK, RESP3_FALSE, RESP3_NULL, RESP3_TRUE,
  },
  error::{Error, Result},
};

/// 浮点数统一文本表示：`nan` → `"nan"`，±inf → `"inf"`/`"-inf"`，有限值 → 最短往返十进制文本
///
/// 宏同时覆盖 f64/f32（对齐 Garnet `TryWriteDoubleBulkString`/`TryWriteInfinity`/`TryWriteNaN` 系列）
macro_rules! with_float_repr {
  ($value:expr, $f:expr) => {{
    let value = $value;
    if value.is_nan() {
      $f(b"nan")
    } else if value.is_infinite() {
      $f(if value.is_sign_positive() {
        b"inf"
      } else {
        b"-inf"
      })
    } else {
      let mut buf = zmij::Buffer::new();
      $f(buf.format_finite(value).as_bytes())
    }
  }};
}

/// RESP 协议写工具集，支持高性能缓冲区切片写入
/// 对齐 Microsoft Garnet `RespWriteUtils`
pub struct RespWriteUtils;

impl RespWriteUtils {
  /// 写入 CRLF 换行符 `\r\n`
  #[inline]
  pub fn write_crlf(output: &mut [u8]) -> Result<usize> {
    if output.len() < 2 {
      return Err(Error::BufferTooSmall);
    }
    output[..2].copy_from_slice(CRLF);
    Ok(2)
  }

  /// 直接写入原始字节切片
  #[inline]
  pub fn write_direct(output: &mut [u8], bytes: &[u8]) -> Result<usize> {
    if output.len() < bytes.len() {
      return Err(Error::BufferTooSmall);
    }
    output[..bytes.len()].copy_from_slice(bytes);
    Ok(bytes.len())
  }

  #[inline]
  fn write_sigil_and_len(output: &mut [u8], sigil: u8, len: usize) -> Result<usize> {
    let mut num_buf = itoa::Buffer::new();
    let num_bytes = num_buf.format(len).as_bytes();
    let total_len = 1 + num_bytes.len() + 2;

    if output.len() < total_len {
      return Err(Error::BufferTooSmall);
    }

    output[0] = sigil;
    output[1..1 + num_bytes.len()].copy_from_slice(num_bytes);
    output[1 + num_bytes.len()..total_len].copy_from_slice(CRLF);
    Ok(total_len)
  }

  #[inline]
  fn write_sigil_and_payload(output: &mut [u8], sigil: u8, payload: &[u8]) -> Result<usize> {
    let total_len = 1 + payload.len() + 2;
    if output.len() < total_len {
      return Err(Error::BufferTooSmall);
    }

    output[0] = sigil;
    output[1..1 + payload.len()].copy_from_slice(payload);
    output[1 + payload.len()..total_len].copy_from_slice(CRLF);
    Ok(total_len)
  }

  /// 写入数组长度头 `*<len>\r\n`
  #[inline]
  pub fn write_array_len(output: &mut [u8], len: usize) -> Result<usize> {
    Self::write_sigil_and_len(output, b'*', len)
  }

  /// 写入字典长度头 `%<len>\r\n`
  #[inline]
  pub fn write_map_len(output: &mut [u8], len: usize) -> Result<usize> {
    Self::write_sigil_and_len(output, b'%', len)
  }

  /// 写入集合长度头 `~<len>\r\n`
  #[inline]
  pub fn write_set_len(output: &mut [u8], len: usize) -> Result<usize> {
    Self::write_sigil_and_len(output, b'~', len)
  }

  /// 写入推送消息长度头 `><len>\r\n`
  #[inline]
  pub fn write_push_len(output: &mut [u8], len: usize) -> Result<usize> {
    Self::write_sigil_and_len(output, b'>', len)
  }

  /// 写入定长字符串长度头 `$<len>\r\n`
  #[inline]
  pub fn write_bulk_string_len(output: &mut [u8], len: usize) -> Result<usize> {
    Self::write_sigil_and_len(output, b'$', len)
  }

  /// 写入填充长度的定长字符串长度头，如 `$<00...0><len>\r\n`
  pub fn write_padded_bulk_string_len(
    output: &mut [u8],
    len: usize,
    padded_len: usize,
  ) -> Result<usize> {
    let mut num_buf = itoa::Buffer::new();
    let num_bytes = num_buf.format(len).as_bytes();
    let min_len = 1 + num_bytes.len() + 2;

    if min_len > padded_len || output.len() < padded_len {
      return Err(Error::BufferTooSmall);
    }

    output[0] = b'$';
    let pad_count = padded_len - min_len;
    output[1..1 + pad_count].fill(b'0');
    let offset = 1 + pad_count;
    output[offset..offset + num_bytes.len()].copy_from_slice(num_bytes);
    output[padded_len - 2..padded_len].copy_from_slice(CRLF);
    Ok(padded_len)
  }

  /// 写入定长字符串 `$<len>\r\n<item>\r\n`
  #[inline]
  pub fn write_bulk_string(output: &mut [u8], item: &[u8]) -> Result<usize> {
    let mut num_buf = itoa::Buffer::new();
    let num_bytes = num_buf.format(item.len()).as_bytes();
    let header_len = 1 + num_bytes.len() + 2;
    let total_len = header_len + item.len() + 2;

    if output.len() < total_len {
      return Err(Error::BufferTooSmall);
    }

    output[0] = b'$';
    output[1..1 + num_bytes.len()].copy_from_slice(num_bytes);
    output[1 + num_bytes.len()..header_len].copy_from_slice(CRLF);
    output[header_len..header_len + item.len()].copy_from_slice(item);
    output[header_len + item.len()..total_len].copy_from_slice(CRLF);
    Ok(total_len)
  }

  /// 分块写入定长字符串，避免大内存合并分配
  pub fn write_bulk_string_chunks(output: &mut [u8], chunks: &[&[u8]]) -> Result<usize> {
    let total_payload_len: usize = chunks.iter().map(|c| c.len()).sum();
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(total_payload_len);
    let total_len = 1 + num_str.len() + 2 + total_payload_len + 2;

    if output.len() < total_len {
      return Err(Error::BufferTooSmall);
    }

    output[0] = b'$';
    let mut offset = 1;
    output[offset..offset + num_str.len()].copy_from_slice(num_str.as_bytes());
    offset += num_str.len();
    output[offset..offset + 2].copy_from_slice(CRLF);
    offset += 2;

    for chunk in chunks {
      output[offset..offset + chunk.len()].copy_from_slice(chunk);
      offset += chunk.len();
    }

    output[offset..offset + 2].copy_from_slice(CRLF);
    Ok(total_len)
  }

  /// 写入 RESP2 空字符串 `$-1\r\n`
  #[inline]
  pub fn write_null(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, RESP2_NULL_BULK)
  }

  /// 写入 RESP3 空值 `_\r\n`
  #[inline]
  pub fn write_resp3_null(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, RESP3_NULL)
  }

  /// 写入 RESP2 空数组 `*-1\r\n`
  #[inline]
  pub fn write_null_array(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, RESP2_NULL_ARRAY)
  }

  /// 写入空数组 `*0\r\n`
  #[inline]
  pub fn write_empty_array(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, EMPTY_ARRAY)
  }

  /// 写入空字典 `%0\r\n`
  #[inline]
  pub fn write_empty_map(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, EMPTY_MAP)
  }

  /// 写入空集合 `~0\r\n`
  #[inline]
  pub fn write_empty_set(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, EMPTY_SET)
  }

  /// 写入简单字符串 `+<str>\r\n`
  #[inline]
  pub fn write_simple_string(output: &mut [u8], simple_str: &[u8]) -> Result<usize> {
    Self::write_sigil_and_payload(output, b'+', simple_str)
  }

  /// 写入错误字符串 `-<error>\r\n`
  #[inline]
  pub fn write_error(output: &mut [u8], error_str: &[u8]) -> Result<usize> {
    Self::write_sigil_and_payload(output, b'-', error_str)
  }

  /// 写入 32 位有符号整数 `:<int>\r\n`
  #[inline]
  pub fn write_int32(output: &mut [u8], value: i32) -> Result<usize> {
    Self::write_int64(output, value as i64)
  }

  /// 写入 64 位有符号整数 `:<int>\r\n`
  #[inline]
  pub fn write_int64(output: &mut [u8], value: i64) -> Result<usize> {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(value);
    Self::write_sigil_and_payload(output, b':', num_str.as_bytes())
  }

  /// 将 32 位整数以定长字符串格式写入 `$<len>\r\n<int>\r\n`
  #[inline]
  pub fn write_int32_as_bulk_string(output: &mut [u8], value: i32) -> Result<usize> {
    Self::write_int64_as_bulk_string(output, value as i64)
  }

  /// 将 64 位整数以定长字符串格式写入 `$<len>\r\n<int>\r\n`
  #[inline]
  pub fn write_int64_as_bulk_string(output: &mut [u8], value: i64) -> Result<usize> {
    let mut val_buf = itoa::Buffer::new();
    let val_str = val_buf.format(value);
    Self::write_bulk_string(output, val_str.as_bytes())
  }

  /// 写入数组整型元素（定长字符串格式，对齐 Garnet TryWriteArrayItem）
  #[inline]
  pub fn write_array_item(output: &mut [u8], integer: i64) -> Result<usize> {
    Self::write_int64_as_bulk_string(output, integer)
  }

  /// 将 64 位整数以简单字符串格式写入 `+<int>\r\n`
  #[inline]
  pub fn write_int64_as_simple_string(output: &mut [u8], value: i64) -> Result<usize> {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(value);
    Self::write_sigil_and_payload(output, b'+', num_str.as_bytes())
  }

  /// 写入双精度浮点数为 BulkString 格式
  #[inline]
  pub fn write_double_bulk_string(output: &mut [u8], value: f64) -> Result<usize> {
    with_float_repr!(value, |s| Self::write_bulk_string(output, s))
  }

  /// 写入 RESP3 浮点数 `,val\r\n`
  #[inline]
  pub fn write_double_numeric(output: &mut [u8], value: f64) -> Result<usize> {
    with_float_repr!(value, |s| Self::write_sigil_and_payload(output, b',', s))
  }

  /// 写入单精度浮点数为 BulkString 格式
  #[inline]
  pub fn write_float_bulk_string(output: &mut [u8], value: f32) -> Result<usize> {
    with_float_repr!(value, |s| Self::write_bulk_string(output, s))
  }

  /// 写入单精度 RESP3 浮点数 `,val\r\n`
  #[inline]
  pub fn write_float_numeric(output: &mut [u8], value: f32) -> Result<usize> {
    with_float_repr!(value, |s| Self::write_sigil_and_payload(output, b',', s))
  }

  /// 写入定长错误字符串 `!<len>\r\n<err>\r\n` (RESP3 BulkError)
  pub fn write_bulk_error(output: &mut [u8], error_str: &[u8]) -> Result<usize> {
    let mut num_buf = itoa::Buffer::new();
    let num_bytes = num_buf.format(error_str.len()).as_bytes();
    let header_len = 1 + num_bytes.len() + 2;
    let total_len = header_len + error_str.len() + 2;

    if output.len() < total_len {
      return Err(Error::BufferTooSmall);
    }

    output[0] = b'!';
    output[1..1 + num_bytes.len()].copy_from_slice(num_bytes);
    output[1 + num_bytes.len()..header_len].copy_from_slice(CRLF);
    output[header_len..header_len + error_str.len()].copy_from_slice(error_str);
    output[header_len + error_str.len()..total_len].copy_from_slice(CRLF);
    Ok(total_len)
  }

  /// 写入 RESP3 布尔 True `#t\r\n`
  #[inline]
  pub fn write_true(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, RESP3_TRUE)
  }

  /// 写入 RESP3 布尔 False `#f\r\n`
  #[inline]
  pub fn write_false(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, RESP3_FALSE)
  }

  /// 写入 RESP3 布尔值（`#t\r\n` 或 `#f\r\n`）
  #[inline]
  pub fn write_boolean(output: &mut [u8], value: bool) -> Result<usize> {
    if value {
      Self::write_true(output)
    } else {
      Self::write_false(output)
    }
  }

  /// 写入 RESP3 大数帧 `(<big_number>\r\n`
  #[inline]
  pub fn write_big_number(output: &mut [u8], big_num: &[u8]) -> Result<usize> {
    Self::write_sigil_and_payload(output, b'(', big_num)
  }

  /// 写入整数 0 `:0\r\n`
  #[inline]
  pub fn write_zero(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, INTEGER_ZERO)
  }

  /// 写入整数 1 `:1\r\n`
  #[inline]
  pub fn write_one(output: &mut [u8]) -> Result<usize> {
    Self::write_direct(output, INTEGER_ONE)
  }

  /// 写入原样字符串 `=<actual_len>\r\n<ext>:<s>\r\n`
  pub fn write_verbatim_string(output: &mut [u8], s: &[u8], ext: &[u8; 3]) -> Result<usize> {
    let actual_len = 3 + 1 + s.len();
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(actual_len);

    let total_len = 1 + num_str.len() + 2 + actual_len + 2;
    if output.len() < total_len {
      return Err(Error::BufferTooSmall);
    }

    output[0] = b'=';
    let mut offset = 1;
    output[offset..offset + num_str.len()].copy_from_slice(num_str.as_bytes());
    offset += num_str.len();
    output[offset..offset + 2].copy_from_slice(CRLF);
    offset += 2;
    output[offset..offset + 3].copy_from_slice(ext);
    offset += 3;
    output[offset] = b':';
    offset += 1;
    output[offset..offset + s.len()].copy_from_slice(s);
    offset += s.len();
    output[offset..offset + 2].copy_from_slice(CRLF);
    Ok(total_len)
  }

  /// 写入原样字符串头部 `={actual_len}\r\n{ext}:`，对齐 Garnet TryWriteVerbatimStringHeader
  pub fn write_verbatim_string_header(
    output: &mut [u8],
    str_len: usize,
    ext: &[u8; 3],
  ) -> Result<usize> {
    let actual_len = 3 + 1 + str_len;
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(actual_len);

    let header_len = 1 + num_str.len() + 2 + 3 + 1;
    if output.len() < header_len {
      return Err(Error::BufferTooSmall);
    }

    output[0] = b'=';
    let mut offset = 1;
    output[offset..offset + num_str.len()].copy_from_slice(num_str.as_bytes());
    offset += num_str.len();
    output[offset..offset + 2].copy_from_slice(CRLF);
    offset += 2;
    output[offset..offset + 3].copy_from_slice(ext);
    offset += 3;
    output[offset] = b':';
    offset += 1;

    Ok(offset)
  }

  /// 写入 ETag 与 Value 的复合数组响应 (etag: i64, val: &[u8])
  pub fn write_etag_val_array(
    output: &mut [u8],
    etag: i64,
    val: &[u8],
    write_direct: bool,
  ) -> Result<usize> {
    let mut written = 0;
    let n1 = Self::write_array_len(&mut output[written..], 2)?;
    written += n1;
    let n2 = Self::write_int64(&mut output[written..], etag)?;
    written += n2;

    if write_direct {
      let n3 = Self::write_direct(&mut output[written..], val)?;
      written += n3;
    } else {
      let n3 = Self::write_bulk_string(&mut output[written..], val)?;
      written += n3;
    }

    Ok(written)
  }

  // ====== 动态 Vec 扩展辅助方法 ======

  #[inline]
  fn push_sigil_and_payload(buf: &mut Vec<u8>, sigil: u8, payload: &[u8]) {
    buf.reserve(1 + payload.len() + 2);
    buf.push(sigil);
    buf.extend_from_slice(payload);
    buf.extend_from_slice(CRLF);
  }

  /// 向 `Vec<u8>` 追加定长字符串
  #[inline]
  pub fn push_bulk_string(buf: &mut Vec<u8>, item: &[u8]) {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(item.len());
    buf.reserve(1 + num_str.len() + 2 + item.len() + 2);
    buf.push(b'$');
    buf.extend_from_slice(num_str.as_bytes());
    buf.extend_from_slice(CRLF);
    buf.extend_from_slice(item);
    buf.extend_from_slice(CRLF);
  }

  /// 向 `Vec<u8>` 追加简单字符串
  #[inline]
  pub fn push_simple_string(buf: &mut Vec<u8>, s: &[u8]) {
    Self::push_sigil_and_payload(buf, b'+', s);
  }

  /// 向 `Vec<u8>` 追加错误字符串
  #[inline]
  pub fn push_error(buf: &mut Vec<u8>, err: &[u8]) {
    Self::push_sigil_and_payload(buf, b'-', err);
  }

  /// 向 `Vec<u8>` 追加定长错误字符串 (RESP3 BulkError)
  #[inline]
  pub fn push_bulk_error(buf: &mut Vec<u8>, err: &[u8]) {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(err.len());
    let total_len = 1 + num_str.len() + 2 + err.len() + 2;

    buf.reserve(total_len);
    buf.push(b'!');
    buf.extend_from_slice(num_str.as_bytes());
    buf.extend_from_slice(CRLF);
    buf.extend_from_slice(err);
    buf.extend_from_slice(CRLF);
  }

  /// 向 `Vec<u8>` 追加整数
  #[inline]
  pub fn push_int64(buf: &mut Vec<u8>, val: i64) {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(val);
    Self::push_sigil_and_payload(buf, b':', num_str.as_bytes());
  }

  /// 向 `Vec<u8>` 追加 32 位有符号整数
  #[inline]
  pub fn push_int32(buf: &mut Vec<u8>, val: i32) {
    Self::push_int64(buf, val as i64);
  }

  /// 将 32 位整数以定长字符串格式追加到 `Vec<u8>`
  #[inline]
  pub fn push_int32_as_bulk_string(buf: &mut Vec<u8>, val: i32) {
    Self::push_int64_as_bulk_string(buf, val as i64);
  }

  /// 将 64 位整数以定长字符串格式追加到 `Vec<u8>`
  #[inline]
  pub fn push_int64_as_bulk_string(buf: &mut Vec<u8>, val: i64) {
    let mut val_buf = itoa::Buffer::new();
    let val_str = val_buf.format(val);
    Self::push_bulk_string(buf, val_str.as_bytes());
  }

  /// 将 64 位整数以简单字符串格式追加到 `Vec<u8>`
  #[inline]
  pub fn push_int64_as_simple_string(buf: &mut Vec<u8>, val: i64) {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(val);
    Self::push_sigil_and_payload(buf, b'+', num_str.as_bytes());
  }

  /// 向 `Vec<u8>` 追加数组头 `*<len>\r\n`
  #[inline]
  pub fn push_array_len(buf: &mut Vec<u8>, len: usize) {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(len);
    Self::push_sigil_and_payload(buf, b'*', num_str.as_bytes());
  }

  /// 向 `Vec<u8>` 追加字典头 `%<len>\r\n`
  #[inline]
  pub fn push_map_len(buf: &mut Vec<u8>, len: usize) {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(len);
    Self::push_sigil_and_payload(buf, b'%', num_str.as_bytes());
  }

  /// 向 `Vec<u8>` 追加集合头 `~<len>\r\n`
  #[inline]
  pub fn push_set_len(buf: &mut Vec<u8>, len: usize) {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(len);
    Self::push_sigil_and_payload(buf, b'~', num_str.as_bytes());
  }

  /// 向 `Vec<u8>` 追加推送头 `><len>\r\n`
  #[inline]
  pub fn push_push_len(buf: &mut Vec<u8>, len: usize) {
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(len);
    Self::push_sigil_and_payload(buf, b'>', num_str.as_bytes());
  }

  /// 向 `Vec<u8>` 追加 RESP3 布尔值
  #[inline]
  pub fn push_boolean(buf: &mut Vec<u8>, val: bool) {
    let b = if val { RESP3_TRUE } else { RESP3_FALSE };
    buf.reserve(b.len());
    buf.extend_from_slice(b);
  }

  /// 向 `Vec<u8>` 追加 RESP2 空字符串 `$-1\r\n`
  #[inline]
  pub fn push_null(buf: &mut Vec<u8>) {
    buf.reserve(RESP2_NULL_BULK.len());
    buf.extend_from_slice(RESP2_NULL_BULK);
  }

  /// 向 `Vec<u8>` 追加 RESP3 空值 `_\r\n`
  #[inline]
  pub fn push_resp3_null(buf: &mut Vec<u8>) {
    buf.reserve(RESP3_NULL.len());
    buf.extend_from_slice(RESP3_NULL);
  }

  /// 向 `Vec<u8>` 追加 RESP2 空数组 `*-1\r\n`
  #[inline]
  pub fn push_null_array(buf: &mut Vec<u8>) {
    buf.reserve(RESP2_NULL_ARRAY.len());
    buf.extend_from_slice(RESP2_NULL_ARRAY);
  }

  /// 向 `Vec<u8>` 追加空数组 `*0\r\n`
  #[inline]
  pub fn push_empty_array(buf: &mut Vec<u8>) {
    buf.reserve(EMPTY_ARRAY.len());
    buf.extend_from_slice(EMPTY_ARRAY);
  }

  /// 向 `Vec<u8>` 追加空字典 `%0\r\n`
  #[inline]
  pub fn push_empty_map(buf: &mut Vec<u8>) {
    buf.reserve(EMPTY_MAP.len());
    buf.extend_from_slice(EMPTY_MAP);
  }

  /// 向 `Vec<u8>` 追加空集合 `~0\r\n`
  #[inline]
  pub fn push_empty_set(buf: &mut Vec<u8>) {
    buf.reserve(EMPTY_SET.len());
    buf.extend_from_slice(EMPTY_SET);
  }

  /// 向 `Vec<u8>` 追加 RESP3 大数帧
  #[inline]
  pub fn push_big_number(buf: &mut Vec<u8>, big_num: &[u8]) {
    Self::push_sigil_and_payload(buf, b'(', big_num);
  }

  /// 向 `Vec<u8>` 追加双精度浮点数（BulkString 格式）
  #[inline]
  pub fn push_double_bulk_string(buf: &mut Vec<u8>, value: f64) {
    with_float_repr!(value, |s| Self::push_bulk_string(buf, s));
  }

  /// 向 `Vec<u8>` 追加双精度浮点数（RESP3 `,val\r\n` 格式）
  #[inline]
  pub fn push_double_numeric(buf: &mut Vec<u8>, value: f64) {
    with_float_repr!(value, |s| Self::push_sigil_and_payload(buf, b',', s));
  }

  /// 向 `Vec<u8>` 追加原样字符串
  pub fn push_verbatim_string(buf: &mut Vec<u8>, s: &[u8], ext: &[u8; 3]) {
    let actual_len = 3 + 1 + s.len();
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(actual_len);
    let total_len = 1 + num_str.len() + 2 + actual_len + 2;

    buf.reserve(total_len);
    buf.push(b'=');
    buf.extend_from_slice(num_str.as_bytes());
    buf.extend_from_slice(CRLF);
    buf.extend_from_slice(ext);
    buf.push(b':');
    buf.extend_from_slice(s);
    buf.extend_from_slice(CRLF);
  }

  /// 向 `Vec<u8>` 追加单精度浮点数（BulkString 格式）
  #[inline]
  pub fn push_float_bulk_string(buf: &mut Vec<u8>, value: f32) {
    with_float_repr!(value, |s| Self::push_bulk_string(buf, s));
  }

  /// 向 `Vec<u8>` 追加单精度浮点数（RESP3 `,val\r\n` 格式）
  #[inline]
  pub fn push_float_numeric(buf: &mut Vec<u8>, value: f32) {
    with_float_repr!(value, |s| Self::push_sigil_and_payload(buf, b',', s));
  }

  /// 分块追加定长字符串，预先精确单次 reserve 内存
  pub fn push_bulk_string_chunks(buf: &mut Vec<u8>, chunks: &[&[u8]]) {
    let total_payload_len: usize = chunks.iter().map(|c| c.len()).sum();
    let mut num_buf = itoa::Buffer::new();
    let num_str = num_buf.format(total_payload_len);
    let total_len = 1 + num_str.len() + 2 + total_payload_len + 2;

    buf.reserve(total_len);
    buf.push(b'$');
    buf.extend_from_slice(num_str.as_bytes());
    buf.extend_from_slice(CRLF);
    for chunk in chunks {
      buf.extend_from_slice(chunk);
    }
    buf.extend_from_slice(CRLF);
  }

  /// 计算无符号 64 位整数的十进制位数（对齐 Garnet NumUtils.CountDigits）
  #[inline(always)]
  pub const fn count_digits_u64(mut val: u64) -> usize {
    let mut digits = 1;
    while val >= 10 {
      digits += 1;
      val /= 10;
    }
    digits
  }

  /// 计算有符号 64 位整数的十进制字符长度（含负号，对齐 Garnet NumUtils.CountDigits）
  #[inline(always)]
  pub const fn count_digits_i64(val: i64) -> usize {
    let sign = if val < 0 { 1 } else { 0 };
    let uval = if val == i64::MIN {
      (i64::MAX as u64) + 1
    } else {
      val.unsigned_abs()
    };
    sign + Self::count_digits_u64(uval)
  }

  /// 计算定长字符串编码为 BulkString 后的总字节数（对标 Garnet GetBulkStringLength）
  /// 格式：`$<digits>\r\n<payload>\r\n`，长度为 `1 + digits(len) + 2 + len + 2`
  #[inline(always)]
  pub const fn get_bulk_string_length(payload_len: usize) -> usize {
    1 + Self::count_digits_u64(payload_len as u64) + 2 + payload_len + 2
  }

  /// 计算整数编码为 BulkString 后的总字节数（对标 Garnet GetIntegerAsBulkStringLength）
  /// 格式：`$<int_len>\r\n<int>\r\n`
  #[inline(always)]
  pub const fn get_integer_as_bulk_string_length(val: i64) -> usize {
    let int_len = Self::count_digits_i64(val);
    1 + Self::count_digits_u64(int_len as u64) + 2 + int_len + 2
  }

  /// 直接从字节切片写入整型协议帧 `:<integer_bytes>\r\n`（对齐 Garnet TryWriteIntegerFromBytes）
  #[inline]
  pub fn write_integer_from_bytes(output: &mut [u8], int_bytes: &[u8]) -> Result<usize> {
    Self::write_sigil_and_payload(output, b':', int_bytes)
  }

  /// 向 `Vec<u8>` 追加整型协议帧字节切片
  #[inline]
  pub fn push_integer_from_bytes(buf: &mut Vec<u8>, int_bytes: &[u8]) {
    Self::push_sigil_and_payload(buf, b':', int_bytes);
  }
}
