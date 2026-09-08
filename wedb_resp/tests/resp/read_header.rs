//! RespReadUtilsTests：RESP 长度头读取（长度/数组/int/long/ulong/指针/布尔）测试

use aok::{OK, Void};
use log::info;
use wedb_resp::RespReadUtils;

/// 对标 Garnet RespReadUtilsTests.cs: ReadLengthHeaderTest —— 有符号长度头解析与消费字节数
#[test]
fn test_read_length_header() -> Void {
  let cases: &[(&str, i32)] = &[("0", 0), ("-1", -1), ("2147483647", 2147483647)];

  for &(text, expected) in cases {
    let bytes = format!("${text}\r\n");
    let (length, consumed) =
      RespReadUtils::try_read_signed_length_header_i32(bytes.as_bytes(), b'$')?;
    assert_eq!(length, expected);
    assert_eq!(consumed, bytes.len());
  }

  info!("C# 兼容性测试：ReadLengthHeaderTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadLengthHeaderExceptionsTest —— 非法长度头输入全部拒绝
#[test]
fn test_read_length_header_exceptions() -> Void {
  let cases = &[
    "$\r\n\r\n",        // 空长度
    "$-1\r\n",          // 无符号头拒绝 NULL
    "123\r\n",          // 缺失前缀 $
    "$-2147483648\r\n", // 负数溢出/不合法
    "$-2\r\n",          // -2 不合法
    "$2147483648\r\n",  // 整数上溢出
    "$123ab\r\n",       // 非数字
    "$123ab",           // 缺少终止符 \r\n
  ];

  for &text in cases {
    let res = RespReadUtils::try_read_unsigned_length_header(text.as_bytes(), b'$');
    assert!(res.is_err(), "输入应该解析失败: {text}");
  }

  info!("C# 兼容性测试：ReadLengthHeaderExceptionsTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadArrayLengthTest —— 数组长度头解析与消费字节数
#[test]
fn test_read_array_length() -> Void {
  let cases: &[(&str, usize)] = &[("0", 0), ("2147483647", 2147483647)];

  for &(text, expected) in cases {
    let bytes = format!("*{text}\r\n");
    let (length, consumed) = RespReadUtils::try_read_unsigned_array_len(bytes.as_bytes())?;
    assert_eq!(length, expected);
    assert_eq!(consumed, bytes.len());
  }

  info!("C# 兼容性测试：ReadArrayLengthTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadArrayLengthExceptionsTest —— 非法数组长度头输入全部拒绝
#[test]
fn test_read_array_length_exceptions() -> Void {
  let cases = &[
    "*\r\n\r\n",        // 空长度
    "123\r\n",          // 缺失前缀 *
    "*-2147483648\r\n", // 负数不合法
    "*-2\r\n",          // -2 不合法
    "*2147483648\r\n",  // 溢出
    "*123ab\r\n",       // 非数字
    "*123ab",           // 缺少终止符 \r\n
  ];

  for &text in cases {
    let res = RespReadUtils::try_read_unsigned_array_len(text.as_bytes());
    assert!(res.is_err(), "输入应该解析失败: {text}");
  }

  info!("C# 兼容性测试：ReadArrayLengthExceptionsTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadIntWithLengthHeaderTest —— 长度头引导的 i32 载荷解析
#[test]
fn test_read_int_with_length_header() -> Void {
  let cases: &[(&str, i32)] = &[
    ("0", 0),
    ("-2147483648", -2147483648),
    ("2147483647", 2147483647),
  ];

  for &(text, expected) in cases {
    let bytes = format!("${}\r\n{}\r\n", text.len(), text);
    let (val, consumed) = RespReadUtils::try_read_int32_with_length_header(bytes.as_bytes())?;
    assert_eq!(val, expected);
    assert_eq!(consumed, bytes.len());
  }

  info!("C# 兼容性测试：ReadIntWithLengthHeaderTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadIntWithLengthHeaderExceptionsTest —— i32 载荷溢出与含字母输入拒绝
#[test]
fn test_read_int_with_length_header_exceptions() -> Void {
  let cases = &[
    "2147483648",  // 32 位正溢出
    "-2147483649", // 32 位负溢出
    "123abc",      // 尾部含字母
    "abc121cba",   // 头部含字母
  ];

  for &text in cases {
    let bytes = format!("${}\r\n{}\r\n", text.len(), text);
    let res = RespReadUtils::try_read_int32_with_length_header(bytes.as_bytes());
    assert!(res.is_err(), "输入应该解析失败: {text}");
  }

  info!("C# 兼容性测试：ReadIntWithLengthHeaderExceptionsTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadLongWithLengthHeaderTest —— 长度头引导的 i64 载荷解析
#[test]
fn test_read_long_with_length_header() -> Void {
  let cases: &[(&str, i64)] = &[
    ("0", 0),
    ("-9223372036854775808", i64::MIN),
    ("9223372036854775807", i64::MAX),
  ];

  for &(text, expected) in cases {
    let bytes = format!("${}\r\n{}\r\n", text.len(), text);
    let (val, consumed) = RespReadUtils::try_read_int64_with_length_header(bytes.as_bytes())?;
    assert_eq!(val, expected);
    assert_eq!(consumed, bytes.len());
  }

  info!("C# 兼容性测试：ReadLongWithLengthHeaderTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadLongWithLengthHeaderExceptionsTest —— i64 载荷溢出与含字母输入拒绝
#[test]
fn test_read_long_with_length_header_exceptions() -> Void {
  let cases = &[
    "9223372036854775808",  // 64 位正溢出
    "-9223372036854775809", // 64 位负溢出
    "10000000000000000000", // 超长溢出
    "123abc",               // 尾部含字母
    "abc121cba",            // 包含字母
  ];

  for &text in cases {
    let bytes = format!("${}\r\n{}\r\n", text.len(), text);
    let res = RespReadUtils::try_read_int64_with_length_header(bytes.as_bytes());
    assert!(res.is_err(), "输入应该解析失败: {text}");
  }

  info!("C# 兼容性测试：ReadLongWithLengthHeaderExceptionsTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadULongWithLengthHeaderTest —— 长度头引导的 u64 载荷解析
#[test]
fn test_read_ulong_with_length_header() -> Void {
  let cases: &[(&str, u64)] = &[("0", 0), ("18446744073709551615", u64::MAX)];

  for &(text, expected) in cases {
    let bytes = format!("${}\r\n{}\r\n", text.len(), text);
    let (val, consumed) = RespReadUtils::try_read_uint64_with_length_header(bytes.as_bytes())?;
    assert_eq!(val, expected);
    assert_eq!(consumed, bytes.len());
  }

  info!("C# 兼容性测试：ReadULongWithLengthHeaderTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadULongWithLengthHeaderExceptionsTest —— u64 载荷溢出、负数与含字母输入拒绝
#[test]
fn test_read_ulong_with_length_header_exceptions() -> Void {
  let cases = &[
    "18446744073709551616", // 无符号 64 位溢出
    "-1",                   // 负数非法
    "123abc",               // 含非数字
    "abc121cba",            // 含非数字
  ];

  for &text in cases {
    let bytes = format!("${}\r\n{}\r\n", text.len(), text);
    let res = RespReadUtils::try_read_uint64_with_length_header(bytes.as_bytes());
    assert!(res.is_err(), "输入应该解析失败: {text}");
  }

  info!("C# 兼容性测试：ReadULongWithLengthHeaderExceptionsTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadPtrWithLengthHeaderTest —— 长度头引导的原始字节切片读取
#[test]
fn test_read_ptr_with_length_header() -> Void {
  let cases = &["test", ""];

  for &text in cases {
    let bytes = format!("${}\r\n{}\r\n", text.len(), text);
    let (slice, consumed) = RespReadUtils::try_read_ptr_with_length_header(bytes.as_bytes())?;
    assert_eq!(slice, text.as_bytes());
    assert_eq!(slice.len(), text.len());
    assert_eq!(consumed, bytes.len());
  }

  info!("C# 兼容性测试：ReadPtrWithLengthHeaderTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: ReadBoolWithLengthHeaderTest —— "1"/"0" 布尔载荷解析
#[test]
fn test_read_bool_with_length_header() -> Void {
  let cases: &[(&str, bool)] = &[("1", true), ("0", false)];

  for &(text, expected) in cases {
    let bytes = format!("${}\r\n{}\r\n", text.len(), text);
    let (val, consumed) = RespReadUtils::try_read_bool_with_length_header(bytes.as_bytes())?;
    assert_eq!(val, expected);
    assert_eq!(consumed, bytes.len());
  }

  info!("C# 兼容性测试：ReadBoolWithLengthHeaderTest 通过");
  OK
}
