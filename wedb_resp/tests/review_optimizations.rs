use std::str::from_utf8;

use aok::{OK, Void};
use log::info;
use wedb_resp::{
  Error, MAX_RESP_ARRAY_LENGTH, ParseUtils, RespCommand, RespLengthEncodingUtils, RespReadUtils,
  RespWriteUtils, SHA1_RAW_LEN, ScriptHashKey, SessionParseState, parse_session_command,
  simd_fast_parse,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_read_advancing_apis() -> Void {
  // 1. read_int_with_length_header
  let mut input: &[u8] = b"$4\r\n1234\r\n$3\r\nfoo\r\n";
  let val = RespReadUtils::read_int_with_length_header(&mut input)?;
  assert_eq!(val, 1234);
  assert_eq!(input, b"$3\r\nfoo\r\n");

  // 2. read_long_with_length_header
  let mut input: &[u8] = b"$19\r\n9223372036854775807\r\n";
  let val = RespReadUtils::read_long_with_length_header(&mut input)?;
  assert_eq!(val, i64::MAX);
  assert!(input.is_empty());

  // 3. read_ulong_with_length_header
  let mut input: &[u8] = b"$20\r\n18446744073709551615\r\n";
  let val = RespReadUtils::read_ulong_with_length_header(&mut input)?;
  assert_eq!(val, u64::MAX);
  assert!(input.is_empty());

  // 4. read_bool_with_length_header
  let mut input: &[u8] = b"$1\r\n1\r\n#f\r\n";
  let b1 = RespReadUtils::read_bool_with_length_header(&mut input)?;
  assert!(b1);
  let b2 = RespReadUtils::read_bool_with_length_header(&mut input)?;
  assert!(!b2);
  assert!(input.is_empty());

  // Incomplete bool: starts with # but len < 4
  let mut partial: &[u8] = b"#t";
  assert_eq!(
    RespReadUtils::read_bool_with_length_header(&mut partial),
    Err(Error::Incomplete)
  );

  // 5. read_bulk_string
  let mut input: &[u8] = b"$5\r\nhello\r\n";
  let ptr = RespReadUtils::read_bulk_string(&mut input)?;
  assert_eq!(ptr, b"hello");
  assert!(input.is_empty());

  // 6. read_length_header & read_unsigned_length_header
  let mut input: &[u8] = b"*-1\r\n*3\r\n";
  let len1 = RespReadUtils::read_length_header(&mut input, b'*')?;
  assert_eq!(len1, None);
  let len2 = RespReadUtils::read_unsigned_length_header(&mut input, b'*')?;
  assert_eq!(len2, 3);
  assert!(input.is_empty());

  // 7. read_array_len
  let mut input: &[u8] = b"*5\r\n";
  let arr_len = RespReadUtils::read_array_len(&mut input)?;
  assert_eq!(arr_len, Some(5));
  assert!(input.is_empty());

  // 8. read_double_with_length_header & read_float_with_length_header
  let mut input: &[u8] = b",123.456\r\n$4\r\n2.71\r\n";
  let d = RespReadUtils::read_double_with_length_header(&mut input)?;
  assert!((d - 123.456).abs() < 1e-5);
  let f = RespReadUtils::read_float_with_length_header(&mut input)?;
  assert!((f - 2.71).abs() < 1e-2);
  assert!(input.is_empty());

  // 9. read_big_number
  let mut input: &[u8] = b"(3492890328409238509324850943850943825024385\r\n";
  let big = RespReadUtils::read_big_number(&mut input)?;
  assert_eq!(big, b"3492890328409238509324850943850943825024385");
  assert!(input.is_empty());

  // 10. read_serialized_record_span
  let mut record_buf = Vec::new();
  record_buf.extend_from_slice(&(5i32.to_le_bytes()));
  record_buf.extend_from_slice(b"world");
  let mut input: &[u8] = &record_buf;
  let rec = RespReadUtils::read_serialized_record_span(&mut input)?;
  assert_eq!(rec, b"world");
  assert!(input.is_empty());

  // 11. read_string_with_length_header & read_string
  let mut input: &[u8] = b"$6\r\nfoobar\r\nbazqux\r\n";
  let s1 = RespReadUtils::read_string_with_length_header(&mut input)?;
  assert_eq!(s1, "foobar");
  let s2 = RespReadUtils::read_string(&mut input)?;
  assert_eq!(s2, "bazqux");
  assert!(input.is_empty());

  info!("RespReadUtils 推进指针 API 测试全部通过");
  OK
}

#[test]
fn test_uint64_digit_boundaries() -> Void {
  // 18 digits (fast path)
  let (v, len) = RespReadUtils::try_read_uint64(b"123456789012345678\r\n")?;
  assert_eq!(v, 123456789012345678u64);
  assert_eq!(len, 18);

  // 19 digits (boundary)
  let (v, len) = RespReadUtils::try_read_uint64(b"1000000000000000000\r\n")?;
  assert_eq!(v, 1_000_000_000_000_000_000u64);
  assert_eq!(len, 19);

  // 20 digits u64::MAX
  let (v, len) = RespReadUtils::try_read_uint64(b"18446744073709551615\r\n")?;
  assert_eq!(v, u64::MAX);
  assert_eq!(len, 20);

  // 20 digits u64::MAX + 1 -> Overflow
  assert_eq!(
    RespReadUtils::try_read_uint64(b"18446744073709551616\r\n"),
    Err(Error::IntegerOverflow)
  );

  info!("uint64 数字边界与快速路径测试通过");
  OK
}

#[test]
fn test_write_utils_extensions() -> Void {
  let mut buf = [0u8; 128];

  // write_boolean
  let n1 = RespWriteUtils::write_boolean(&mut buf, true)?;
  assert_eq!(&buf[..n1], b"#t\r\n");
  let n2 = RespWriteUtils::write_boolean(&mut buf, false)?;
  assert_eq!(&buf[..n2], b"#f\r\n");

  // write_big_number
  let n3 = RespWriteUtils::write_big_number(&mut buf, b"12345678901234567890")?;
  assert_eq!(&buf[..n3], b"(12345678901234567890\r\n");

  // push_* helpers
  let mut v = Vec::new();
  RespWriteUtils::push_boolean(&mut v, true);
  RespWriteUtils::push_boolean(&mut v, false);
  RespWriteUtils::push_null(&mut v);
  RespWriteUtils::push_resp3_null(&mut v);
  RespWriteUtils::push_null_array(&mut v);
  RespWriteUtils::push_empty_array(&mut v);
  RespWriteUtils::push_empty_map(&mut v);
  RespWriteUtils::push_empty_set(&mut v);
  RespWriteUtils::push_map_len(&mut v, 2);
  RespWriteUtils::push_set_len(&mut v, 3);
  RespWriteUtils::push_push_len(&mut v, 1);
  RespWriteUtils::push_big_number(&mut v, b"99999");
  RespWriteUtils::push_double_bulk_string(&mut v, 1.25);
  RespWriteUtils::push_double_numeric(&mut v, 2.5);
  RespWriteUtils::push_verbatim_string(&mut v, b"markdown", b"mkd");

  let expected = concat!(
    "#t\r\n#f\r\n$-1\r\n_\r\n*-1\r\n*0\r\n%0\r\n~0\r\n",
    "%2\r\n~3\r\n>1\r\n(99999\r\n$4\r\n1.25\r\n,2.5\r\n",
    "=12\r\nmkd:markdown\r\n"
  );
  assert_eq!(v, expected.as_bytes());

  info!("RespWriteUtils 扩展输出测试通过");
  OK
}

#[test]
fn test_length_encoding_push() -> Void {
  let mut buf = Vec::new();
  // 6-bit (<= 63)
  RespLengthEncodingUtils::push_length(&mut buf, 42)?;
  assert_eq!(buf.len(), 1);
  assert_eq!(
    RespLengthEncodingUtils::try_read_length(&buf),
    Some((42, 1))
  );

  // 14-bit

  buf.clear();
  RespLengthEncodingUtils::push_length(&mut buf, 1000)?;
  assert_eq!(buf.len(), 2);
  assert_eq!(
    RespLengthEncodingUtils::try_read_length(&buf),
    Some((1000, 2))
  );

  // 32-bit

  buf.clear();
  RespLengthEncodingUtils::push_length(&mut buf, 100_000)?;
  assert_eq!(buf.len(), 5);
  assert_eq!(
    RespLengthEncodingUtils::try_read_length(&buf),
    Some((100_000, 5))
  );

  // Overflow
  assert_eq!(
    RespLengthEncodingUtils::push_length(&mut buf, RespLengthEncodingUtils::MAX_LENGTH + 1),
    Err(Error::InvalidLength(
      (RespLengthEncodingUtils::MAX_LENGTH + 1) as i64
    ))
  );

  info!("RespLengthEncodingUtils::push_length 测试通过");
  OK
}

#[test]
fn test_simd_fast_parse_case_insensitivity() -> Void {
  // 测试 lowercase 与 mixed-case 的极速 SIMD 解析
  let cases: &[(&[u8], RespCommand, usize, usize)] = &[
    (b"*2\r\n$3\r\nget\r\n", RespCommand::Get, 1, 13),
    (b"*2\r\n$3\r\nGet\r\n", RespCommand::Get, 1, 13),
    (b"*3\r\n$3\r\nset\r\n", RespCommand::Set, 2, 13),
    (b"*3\r\n$3\r\nSeT\r\n", RespCommand::Set, 2, 13),
    (b"*2\r\n$3\r\ndel\r\n", RespCommand::Del, 1, 13),
    (b"*2\r\n$3\r\nttl\r\n", RespCommand::Ttl, 1, 13),
    (b"*1\r\n$4\r\nping\r\n", RespCommand::Ping, 0, 14),
    (b"*1\r\n$4\r\nPinG\r\n", RespCommand::Ping, 0, 14),
    (b"*2\r\n$4\r\nincr\r\n", RespCommand::Incr, 1, 14),
    (b"*2\r\n$4\r\ndecr\r\n", RespCommand::Decr, 1, 14),
    (b"*1\r\n$4\r\nexec\r\n", RespCommand::Exec, 0, 14),
    (b"*2\r\n$4\r\npttl\r\n", RespCommand::Pttl, 1, 14),
    (b"*1\r\n$5\r\nmulti\r\n", RespCommand::Multi, 0, 15),
    (b"*3\r\n$5\r\nsetnx\r\n", RespCommand::Setnx, 2, 15),
    (b"*4\r\n$5\r\nsetex\r\n", RespCommand::Setex, 3, 15),
    (b"*2\r\n$6\r\nexists\r\n", RespCommand::Exists, 1, 16),
    (b"*2\r\n$6\r\ngetdel\r\n", RespCommand::Getdel, 1, 16),
    (b"*3\r\n$6\r\nappend\r\n", RespCommand::Append, 2, 16),
    (b"*3\r\n$6\r\nincrby\r\n", RespCommand::Incrby, 2, 16),
    (b"*3\r\n$6\r\ndecrby\r\n", RespCommand::Decrby, 2, 16),
    (b"*4\r\n$6\r\npsetex\r\n", RespCommand::Psetex, 3, 16),
  ];

  for &(bytes, expected_cmd, expected_count, expected_len) in cases {
    let res = simd_fast_parse(bytes);
    assert_eq!(
      res,
      Some((expected_cmd, expected_count, expected_len)),
      "大小写不敏感 SIMD 解析失败: {:?}",
      from_utf8(bytes)
    );
  }

  info!("SIMD 大小写不敏感解析全模式验证通过");
  OK
}

#[test]
fn test_script_hash_raw_20() -> Void {
  let hex_bytes = b"0123456789abcdef0123456789abcdef01234567";
  let key = ScriptHashKey::from_bytes(hex_bytes)?;

  // 折叠为 20 字节
  let raw: [u8; SHA1_RAW_LEN] = key.to_raw_20();
  assert_eq!(raw.len(), 20);

  // 从 20 字节原始二进制重建
  let key2 = ScriptHashKey::from_raw_20(raw);
  assert_eq!(key, key2);
  assert_eq!(key.as_str(), key2.as_str());

  // 零分配直接比对
  assert!(key.eq_raw_20(&raw));
  assert_eq!(key, raw);
  assert_eq!(key, &raw);
  assert_eq!(raw, key);
  assert_eq!(&raw, key);

  // 修改任意 1 字节后比对失败
  let mut corrupted_raw = raw;
  corrupted_raw[10] ^= 0xFF;
  assert!(!key.eq_raw_20(&corrupted_raw));
  assert_ne!(key, corrupted_raw);

  // From trait
  let key3: ScriptHashKey = raw.into();
  assert_eq!(key, key3);

  info!("ScriptHashKey 40 字符十六进制与 20 字节原始二进制转换及比较验证通过");
  OK
}

#[test]
fn test_parse_utils_ulong_and_state() -> Void {
  assert_eq!(ParseUtils::read_ulong(b"18446744073709551615")?, u64::MAX);
  assert_eq!(ParseUtils::try_read_ulong(b"42"), Some(42));
  assert_eq!(ParseUtils::try_read_ulong(b"-1"), None);
  assert_eq!(ParseUtils::try_read_ulong(b"abc"), None);

  let mut state = SessionParseState::new();
  state.push(b"100");
  state.push(b"18446744073709551615");
  assert_eq!(state.get_ulong(0)?, 100);
  assert_eq!(state.get_ulong(1)?, u64::MAX);
  assert_eq!(state.try_get_ulong(0), Some(100));
  assert_eq!(state.try_get_ulong(2), None);

  info!("ParseUtils 与 SessionParseState ulong 方法测试通过");
  OK
}

#[test]
fn test_cmd_flags_and_one_if() -> Void {
  // is_write / one_if_write
  assert!(RespCommand::Set.is_write());
  assert_eq!(RespCommand::Set.one_if_write(), 1);
  assert_eq!(RespCommand::Get.one_if_write(), 0);

  // is_readonly / one_if_read
  assert!(RespCommand::Get.is_readonly());
  assert_eq!(RespCommand::Get.one_if_read(), 1);
  assert_eq!(RespCommand::Set.one_if_read(), 0);

  info!("RespCommand 分类与无分支位运算方法验证通过");
  OK
}

#[test]
fn test_round2_review_enhancements() -> Void {
  // 1. push_* 写方法与精确预分配
  let mut v = Vec::new();
  RespWriteUtils::push_int32(&mut v, 42);
  RespWriteUtils::push_int32(&mut v, -100);
  RespWriteUtils::push_int32_as_bulk_string(&mut v, 123);
  RespWriteUtils::push_int64_as_bulk_string(&mut v, -987654321);
  RespWriteUtils::push_int64_as_simple_string(&mut v, 777);
  RespWriteUtils::push_float_bulk_string(&mut v, 1.5);
  RespWriteUtils::push_float_numeric(&mut v, 2.5);
  RespWriteUtils::push_bulk_error(&mut v, b"ERR failure");
  RespWriteUtils::push_bulk_string_chunks(&mut v, &[b"hello", b" ", b"world"]);

  let expected = concat!(
    ":42\r\n:-100\r\n",
    "$3\r\n123\r\n",
    "$10\r\n-987654321\r\n",
    "+777\r\n",
    "$3\r\n1.5\r\n",
    ",2.5\r\n",
    "!11\r\nERR failure\r\n",
    "$11\r\nhello world\r\n",
  );
  assert_eq!(v, expected.as_bytes());

  // 2. 栈缓冲区写入函数
  let mut buf = [0u8; 128];
  let n = RespWriteUtils::write_float_bulk_string(&mut buf, 1.5)?;
  assert_eq!(&buf[..n], b"$3\r\n1.5\r\n");
  let n = RespWriteUtils::write_float_numeric(&mut buf, 2.5)?;
  assert_eq!(&buf[..n], b",2.5\r\n");
  let n = RespWriteUtils::write_bulk_error(&mut buf, b"ERR bad")?;
  assert_eq!(&buf[..n], b"!7\r\nERR bad\r\n");

  // 3. SessionParseState 迭代器
  let mut state = SessionParseState::new();
  state.push(b"GET");
  state.push(b"mykey");
  let ref_iter: Vec<&[u8]> = (&state).into_iter().copied().collect();
  assert_eq!(ref_iter, vec![b"GET".as_slice(), b"mykey".as_slice()]);
  let val_iter: Vec<&[u8]> = state.into_iter().collect();
  assert_eq!(val_iter, vec![b"GET".as_slice(), b"mykey".as_slice()]);

  // 4. SessionParseState 13..=15 字节短报文 MRU 缓存
  let mut state2 = SessionParseState::new();
  let short_pkt = b"*1\r\n$4\r\nPING\r\n"; // 14 bytes
  state2.update_mru(short_pkt, 14, RespCommand::Ping, 0);
  assert_eq!(
    state2.try_match_mru(short_pkt),
    Some((RespCommand::Ping, 0, 14))
  );
  assert_eq!(state2.try_match_mru(b"*1\r\n$4\r\nINFO\r\n"), None);

  // 5. CLUSTER 子命令 SETCONFIGEPOCH 规范兼容性
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::Cluster, b"SETCONFIGEPOCH"),
    Some(RespCommand::ClusterSetconfigepoch)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::Cluster, b"SET-CONFIG-EPOCH"),
    Some(RespCommand::ClusterSetconfigepoch)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::Cluster, b"setconfigepoch"),
    Some(RespCommand::ClusterSetconfigepoch)
  );

  info!("第 2 轮复审增强特性测试通过");
  OK
}

#[test]
fn test_extreme_boundaries_and_error_handling() -> Void {
  // 1. i64::MIN 与 i64::MAX 边界解析
  assert_eq!(
    RespReadUtils::try_read_int64(b"-9223372036854775808\r\n", false)?,
    (i64::MIN, 20)
  );
  assert_eq!(
    RespReadUtils::try_read_int64(b"9223372036854775807\r\n", false)?,
    (i64::MAX, 19)
  );
  assert_eq!(
    RespReadUtils::try_read_int64(b"+9223372036854775807\r\n", false)?,
    (i64::MAX, 20)
  );

  // 2. i64 溢出检测
  assert_eq!(
    RespReadUtils::try_read_int64(b"-9223372036854775809\r\n", false),
    Err(Error::IntegerOverflow)
  );
  assert_eq!(
    RespReadUtils::try_read_int64(b"9223372036854775808\r\n", false),
    Err(Error::IntegerOverflow)
  );
  assert_eq!(
    RespReadUtils::try_read_int64(b"+9223372036854775808\r\n", false),
    Err(Error::IntegerOverflow)
  );

  // 3. 整数协议帧极值与畸形报文测试
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":-9223372036854775808\r\n")?,
    (i64::MIN, 23)
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":9223372036854775807\r\n")?,
    (i64::MAX, 22)
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":-9223372036854775809\r\n"),
    Err(Error::IntegerOverflow)
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":\r\n"),
    Err(Error::NotANumber)
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":+\r\n"),
    Err(Error::NotANumber)
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":-\r\n"),
    Err(Error::NotANumber)
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":++1\r\n"),
    Err(Error::UnexpectedToken(b'+'))
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":+-1\r\n"),
    Err(Error::UnexpectedToken(b'-'))
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":--1\r\n"),
    Err(Error::UnexpectedToken(b'-'))
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":+abc\r\n"),
    Err(Error::UnexpectedToken(b'a'))
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":-xyz\r\n"),
    Err(Error::UnexpectedToken(b'x'))
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":123"),
    Err(Error::Incomplete)
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":+123"),
    Err(Error::Incomplete)
  );
  assert_eq!(
    RespReadUtils::try_read_int64_frame(b":-123"),
    Err(Error::Incomplete)
  );

  // 4. 长度头符号与非法字符
  assert_eq!(
    RespReadUtils::try_read_signed_length_header_i32(b"$+\r\n", b'$'),
    Err(Error::NotANumber)
  );
  assert_eq!(
    RespReadUtils::try_read_signed_length_header_i32(b"$+abc\r\n", b'$'),
    Err(Error::UnexpectedToken(b'a'))
  );
  assert_eq!(
    RespReadUtils::try_read_signed_length_header_i32(b"$-\r\n", b'$'),
    Err(Error::NotANumber)
  );

  // 5. 浮点无穷大与 NaN 解析
  let (f1, _) = RespReadUtils::try_read_double_with_length_header(b",inf\r\n")?;
  assert_eq!(f1, f64::INFINITY);
  let (f2, _) = RespReadUtils::try_read_double_with_length_header(b",+inf\r\n")?;
  assert_eq!(f2, f64::INFINITY);
  let (f3, _) = RespReadUtils::try_read_double_with_length_header(b",-inf\r\n")?;
  assert_eq!(f3, f64::NEG_INFINITY);
  let (f4, _) = RespReadUtils::try_read_double_with_length_header(b",infinity\r\n")?;
  assert_eq!(f4, f64::INFINITY);
  let (f5, _) = RespReadUtils::try_read_double_with_length_header(b",+infinity\r\n")?;
  assert_eq!(f5, f64::INFINITY);
  let (f6, _) = RespReadUtils::try_read_double_with_length_header(b",-infinity\r\n")?;
  assert_eq!(f6, f64::NEG_INFINITY);
  let (f7, _) = RespReadUtils::try_read_double_with_length_header(b",nan\r\n")?;
  assert!(f7.is_nan());

  // 6. RespLengthEncodingUtils 32-bit 溢出边界防御
  let mut buf = vec![0x80];
  buf.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
  let mut slice = &buf[..];
  assert_eq!(
    RespLengthEncodingUtils::read_length(&mut slice),
    Err(Error::InvalidLength(0xFFFF_FFFF))
  );

  info!("极端边界与异常输入鲁棒性测试通过");
  OK
}

#[test]
fn test_float_numeric_all_branches() -> Void {
  // 1. push_float_numeric 全分支验证
  let mut buf = Vec::new();
  RespWriteUtils::push_float_numeric(&mut buf, f32::NAN);
  assert_eq!(buf, b",nan\r\n");

  buf.clear();
  RespWriteUtils::push_float_numeric(&mut buf, f32::INFINITY);
  assert_eq!(buf, b",inf\r\n");

  buf.clear();
  RespWriteUtils::push_float_numeric(&mut buf, f32::NEG_INFINITY);
  assert_eq!(buf, b",-inf\r\n");

  buf.clear();
  RespWriteUtils::push_float_numeric(&mut buf, 123.45f32);
  let mut slice: &[u8] = &buf;
  let parsed = RespReadUtils::read_float_with_length_header(&mut slice)?;
  assert!((parsed - 123.45).abs() < 1e-3);
  assert!(slice.is_empty());

  // 2. write_float_numeric 全分支验证
  let mut target = [0u8; 32];
  let n = RespWriteUtils::write_float_numeric(&mut target, f32::NAN)?;
  assert_eq!(&target[..n], b",nan\r\n");

  let n = RespWriteUtils::write_float_numeric(&mut target, f32::INFINITY)?;
  assert_eq!(&target[..n], b",inf\r\n");

  let n = RespWriteUtils::write_float_numeric(&mut target, f32::NEG_INFINITY)?;
  assert_eq!(&target[..n], b",-inf\r\n");

  let n = RespWriteUtils::write_float_numeric(&mut target, 67.89f32)?;
  let mut slice: &[u8] = &target[..n];
  let parsed = RespReadUtils::read_float_with_length_header(&mut slice)?;
  assert!((parsed - 67.89).abs() < 1e-3);
  assert!(slice.is_empty());

  info!("float numeric 全部特殊值与普通值分支测试通过");
  OK
}

#[test]
fn test_round3_review_enhancements() -> Void {
  use wedb_resp::consts::{err, resp};

  // 1. count_digits_u64 与 count_digits_i64
  assert_eq!(RespWriteUtils::count_digits_u64(0), 1);
  assert_eq!(RespWriteUtils::count_digits_u64(9), 1);
  assert_eq!(RespWriteUtils::count_digits_u64(10), 2);
  assert_eq!(RespWriteUtils::count_digits_u64(999), 3);
  assert_eq!(RespWriteUtils::count_digits_u64(1_000_000), 7);
  assert_eq!(RespWriteUtils::count_digits_u64(u64::MAX), 20);

  assert_eq!(RespWriteUtils::count_digits_i64(0), 1);
  assert_eq!(RespWriteUtils::count_digits_i64(-1), 2);
  assert_eq!(RespWriteUtils::count_digits_i64(-99), 3);
  assert_eq!(RespWriteUtils::count_digits_i64(100), 3);
  assert_eq!(RespWriteUtils::count_digits_i64(i64::MIN), 20);
  assert_eq!(RespWriteUtils::count_digits_i64(i64::MAX), 19);

  // 2. get_bulk_string_length 校验
  let mut buf = [0u8; 64];
  let n = RespWriteUtils::write_bulk_string(&mut buf, b"hello")?;
  assert_eq!(n, RespWriteUtils::get_bulk_string_length(5));

  let n = RespWriteUtils::write_bulk_string(&mut buf, b"")?;
  assert_eq!(n, RespWriteUtils::get_bulk_string_length(0));

  let big_data = vec![b'x'; 1000];
  let mut big_buf = vec![0u8; 2000];
  let n = RespWriteUtils::write_bulk_string(&mut big_buf, &big_data)?;
  assert_eq!(n, RespWriteUtils::get_bulk_string_length(1000));

  // 3. get_integer_as_bulk_string_length 校验
  let n = RespWriteUtils::write_int64_as_bulk_string(&mut buf, 42)?;
  assert_eq!(n, RespWriteUtils::get_integer_as_bulk_string_length(42));

  let n = RespWriteUtils::write_int64_as_bulk_string(&mut buf, -100)?;
  assert_eq!(n, RespWriteUtils::get_integer_as_bulk_string_length(-100));

  let n = RespWriteUtils::write_int64_as_bulk_string(&mut buf, 0)?;
  assert_eq!(n, RespWriteUtils::get_integer_as_bulk_string_length(0));

  // 4. write_integer_from_bytes 与 push_integer_from_bytes
  let mut int_buf = [0u8; 16];
  let n = RespWriteUtils::write_integer_from_bytes(&mut int_buf, b"12345")?;
  assert_eq!(&int_buf[..n], b":12345\r\n");

  let mut vec_buf = Vec::new();
  RespWriteUtils::push_integer_from_bytes(&mut vec_buf, b"67890");
  assert_eq!(&vec_buf[..], b":67890\r\n");

  // 5. SessionParseState init_with_args 3-5 与 set_arg
  let mut state = SessionParseState::new();
  state.init_with_args3(b"a", b"b", b"c");
  assert_eq!(state.len(), 3);
  assert_eq!(state.get(0), Some(&b"a"[..]));
  assert_eq!(state.get(1), Some(&b"b"[..]));
  assert_eq!(state.get(2), Some(&b"c"[..]));

  state.init_with_args4(b"1", b"2", b"3", b"4");
  assert_eq!(state.len(), 4);
  assert_eq!(state.get(3), Some(&b"4"[..]));

  state.init_with_args5(b"1", b"2", b"3", b"4", b"5");
  assert_eq!(state.len(), 5);
  assert_eq!(state.get(4), Some(&b"5"[..]));

  // set_arg (栈内联与堆溢出)
  state.set_arg(1, b"updated_b");
  assert_eq!(state.get(1), Some(&b"updated_b"[..]));
  state.set_arg(10, b"spill_arg");
  assert_eq!(state.len(), 11);
  assert_eq!(state.get(10), Some(&b"spill_arg"[..]));

  // try_get_str 与 ParseUtils::try_read_string
  assert_eq!(state.try_get_str(1), Some("updated_b"));
  assert_eq!(
    ParseUtils::try_read_string(b"valid_utf8"),
    Some("valid_utf8")
  );
  assert_eq!(ParseUtils::try_read_string(&[0xFF, 0xFE]), None);

  // 6. LAST_VALID_COMMAND
  assert_eq!(RespCommand::LAST_VALID_COMMAND, RespCommand::Quit);

  // 7. ScriptHashKey 大小写不敏感比对 (&str, &[u8])
  let hash_key = ScriptHashKey::from_bytes(b"a1b2c3d4e5f678901234567890abcdef12345678")?;
  assert_eq!(hash_key, "A1B2C3D4E5F678901234567890ABCDEF12345678");
  assert_eq!(
    hash_key,
    b"A1B2C3D4E5F678901234567890ABCDEF12345678" as &[u8]
  );
  assert_eq!(*b"A1B2C3D4E5F678901234567890ABCDEF12345678", hash_key);

  // 8. HipStr 常量校验
  assert_eq!(resp::OK_HIPSTR, "+OK\r\n");
  assert_eq!(resp::PONG_HIPSTR, "+PONG\r\n");
  assert_eq!(resp::QUEUED_HIPSTR, "+QUEUED\r\n");
  assert_eq!(
    err::WRONG_TYPE_HIPSTR,
    "WRONGTYPE Operation against a key holding the wrong kind of value"
  );
  assert_eq!(err::SYNTAX_HIPSTR, "ERR syntax error");
  assert_eq!(err::NOAUTH_HIPSTR, "NOAUTH Authentication required.");

  info!("第 3 轮审查重构增强特性测试通过");
  OK
}

#[test]
fn test_round4_hardening() -> Void {
  // 1. 超大 *N 头拒绝且不推进输入（对齐 C# MaxRespArrayLength 防预认证内存耗尽）
  let raw: &[u8] = b"*2147483647\r\n";
  let mut input = raw;
  let mut state = SessionParseState::new();
  assert_eq!(
    parse_session_command(&mut input, &mut state),
    Err(Error::ExcessiveArgs(2147483647, MAX_RESP_ARRAY_LENGTH))
  );
  assert_eq!(input, raw, "失败解析不得推进输入指针");

  // 2. 前导零检查对齐 C#：'0' 后跟任意字符（含非数字）均拒绝，单个 "0" 合法
  assert_eq!(
    RespReadUtils::try_read_int64(b"0x", false),
    Err(Error::NotANumber)
  );
  assert_eq!(RespReadUtils::try_read_int64(b"0", false), Ok((0, 1)));

  // 3. MRU 槽位 1 漏检修复：输入短于槽位 0 模式时仍应命中更短的槽位 1
  let mut state = SessionParseState::new();
  // 先记录 14 字节 HGET 头，再记录 16 字节 APPEND 头（HGET 被下沉至槽位 1）
  state.update_mru(b"*3\r\n$4\r\nHGET\r\n", 14, RespCommand::Hget, 2);
  state.update_mru(b"*3\r\n$6\r\nAPPEND\r\n", 16, RespCommand::Append, 2);
  assert_eq!(
    state.try_match_mru(b"*3\r\n$4\r\nHGET\r\n"),
    Some((RespCommand::Hget, 2, 14))
  );

  // 4. 字符串数组读取的预分配按输入实际容量封顶（超大长度头不再触发巨量分配）
  let big_len = MAX_RESP_ARRAY_LENGTH * 4;
  let bytes = format!("*{big_len}\r\n");
  let res = RespReadUtils::try_read_string_array_with_length_header(bytes.as_bytes());
  assert_eq!(res.unwrap_err(), Error::Incomplete);

  info!("第 4 轮加固修复测试通过");
  OK
}
