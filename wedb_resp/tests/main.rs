use std::time::Instant;

use aok::{OK, Void};
use log::info;
use wedb_resp::{
  CRLF, EMPTY_ARRAY, EMPTY_MAP, EMPTY_SET, Error, ExistOpt, ExpirationOpt, INTEGER_ONE,
  INTEGER_ZERO, OK as RESP_OK, PONG, ParseUtils, RESP2_NULL_ARRAY, RESP2_NULL_BULK, RESP3_FALSE,
  RESP3_NULL, RESP3_TRUE, RespCommand, RespCommandOpt, RespDataType, RespLengthEncodingUtils,
  RespReadUtils, RespWriteUtils, SessionParseState, mask_for, parse_session_command,
  simd_fast_parse,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_resp_strings() -> Void {
  assert_eq!(EMPTY_ARRAY, b"*0\r\n");
  assert_eq!(EMPTY_SET, b"~0\r\n");
  assert_eq!(EMPTY_MAP, b"%0\r\n");
  assert_eq!(INTEGER_ZERO, b":0\r\n");
  assert_eq!(INTEGER_ONE, b":1\r\n");
  assert_eq!(RESP2_NULL_ARRAY, b"*-1\r\n");
  assert_eq!(RESP2_NULL_BULK, b"$-1\r\n");
  assert_eq!(RESP3_NULL, b"_\r\n");
  assert_eq!(RESP3_FALSE, b"#f\r\n");
  assert_eq!(RESP3_TRUE, b"#t\r\n");
  assert_eq!(CRLF, b"\r\n");
  assert_eq!(RESP_OK, b"+OK\r\n");
  assert_eq!(PONG, b"+PONG\r\n");
  info!("RESP 常量验证成功");
  OK
}

#[test]
fn test_resp_enums() -> Void {
  // ExpirationOpt
  assert_eq!(ExpirationOpt::from_bytes(b"ex"), ExpirationOpt::EX);
  assert_eq!(ExpirationOpt::from_bytes(b"PX"), ExpirationOpt::PX);
  assert_eq!(ExpirationOpt::from_bytes(b"ExAt"), ExpirationOpt::EXAT);
  assert_eq!(ExpirationOpt::from_bytes(b"pxat"), ExpirationOpt::PXAT);
  assert_eq!(
    ExpirationOpt::from_bytes(b"keepttl"),
    ExpirationOpt::KEEPTTL
  );
  assert_eq!(ExpirationOpt::from_bytes(b"other"), ExpirationOpt::None);
  assert_eq!(ExpirationOpt::EX.as_str(), "EX");

  // ExistOpt
  assert_eq!(ExistOpt::from_bytes(b"nx"), ExistOpt::NX);
  assert_eq!(ExistOpt::from_bytes(b"XX"), ExistOpt::XX);
  assert_eq!(ExistOpt::from_bytes(b"other"), ExistOpt::None);
  assert_eq!(ExistOpt::NX.as_str(), "NX");

  // RespCommandOpt
  assert_eq!(RespCommandOpt::from_bytes(b"EX"), Some(RespCommandOpt::EX));
  assert_eq!(RespCommandOpt::from_bytes(b"gt"), Some(RespCommandOpt::GT));
  assert_eq!(RespCommandOpt::from_bytes(b"invalid"), None);

  // RespDataType
  assert_eq!(
    RespDataType::from_byte(b'+'),
    Some(RespDataType::SimpleString)
  );
  assert_eq!(
    RespDataType::from_byte(b'$'),
    Some(RespDataType::BulkString)
  );
  assert_eq!(RespDataType::from_byte(b'%'), Some(RespDataType::Map));
  assert_eq!(RespDataType::from_byte(b'?'), None);
  assert_eq!(RespDataType::Array.as_byte(), b'*');
  assert_eq!(RespDataType::try_from(b'*'), Ok(RespDataType::Array));
  assert_eq!(
    RespDataType::try_from(b'?'),
    Err(Error::UnexpectedToken(b'?'))
  );

  info!("RESP 枚举解析验证成功");
  OK
}

#[test]
fn test_length_encoding() -> Void {
  let mut buf = [0u8; 10];

  // 6 位编码 (0..=63)
  for len in [0, 1, 42, 63] {
    let written = RespLengthEncodingUtils::try_write_length(len, &mut buf).unwrap();
    assert_eq!(written, 1);
    let (decoded, read_bytes) = RespLengthEncodingUtils::try_read_length(&buf[..written]).unwrap();
    assert_eq!(decoded, len);
    assert_eq!(read_bytes, 1);
  }

  // 14 位编码 (64..=16383)
  for len in [64, 255, 1024, 16383] {
    let written = RespLengthEncodingUtils::try_write_length(len, &mut buf).unwrap();
    assert_eq!(written, 2);
    let (decoded, read_bytes) = RespLengthEncodingUtils::try_read_length(&buf[..written]).unwrap();
    assert_eq!(decoded, len);
    assert_eq!(read_bytes, 2);
  }

  // 32 位编码 (16384..=MAX_LENGTH)
  for len in [16384, 65536, 1_000_000, RespLengthEncodingUtils::MAX_LENGTH] {
    let written = RespLengthEncodingUtils::try_write_length(len, &mut buf).unwrap();
    assert_eq!(written, 5);
    let (decoded, read_bytes) = RespLengthEncodingUtils::try_read_length(&buf[..written]).unwrap();
    assert_eq!(decoded, len);
    assert_eq!(read_bytes, 5);
  }

  // 边界溢出
  assert!(
    RespLengthEncodingUtils::try_write_length(RespLengthEncodingUtils::MAX_LENGTH + 1, &mut buf)
      .is_none()
  );
  assert!(RespLengthEncodingUtils::try_read_length(&[]).is_none());

  // 游标推进读取
  let mut slice: &[u8] = &[0x40, 0x50, 0x01]; // 14-bit
  let val = RespLengthEncodingUtils::read_length(&mut slice)?;
  assert_eq!(val, 80);
  assert_eq!(slice, &[0x01]);

  info!("RESP 紧凑长度编码测试成功");
  OK
}

#[test]
fn test_command_attributes_and_lookup() -> Void {
  // 写入属性
  assert!(RespCommand::APPEND.is_write());
  assert!(RespCommand::SET.is_write());
  assert!(RespCommand::DEL.is_write());
  assert!(RespCommand::BITOP_DIFF.is_write());
  assert!(!RespCommand::GET.is_write());
  assert!(!RespCommand::PING.is_write());

  // 只读属性
  assert!(RespCommand::GET.is_readonly());
  assert!(RespCommand::MGET.is_readonly());
  assert!(RespCommand::HGET.is_readonly());
  assert!(RespCommand::RISCAN.is_readonly());
  assert!(!RespCommand::SET.is_readonly());
  assert!(!RespCommand::PING.is_readonly());

  // 数据命令属性
  assert!(RespCommand::SET.is_data());
  assert!(RespCommand::GET.is_data());
  assert!(RespCommand::EVALSHA.is_data());
  assert!(!RespCommand::DBSIZE.is_data());
  assert!(!RespCommand::PING.is_data());

  // 免认证属性
  assert!(RespCommand::AUTH.is_no_auth());
  assert!(RespCommand::HELLO.is_no_auth());
  assert!(RespCommand::QUIT.is_no_auth());
  assert!(!RespCommand::GET.is_no_auth());

  // ACL 规范化
  assert_eq!(RespCommand::SETEXNX.normalize_for_acls(), RespCommand::SET);
  assert_eq!(
    RespCommand::BITOP_XOR.normalize_for_acls(),
    RespCommand::BITOP
  );
  assert_eq!(RespCommand::SET.expand_for_acls().len(), 4);

  // 命令快速查找（不区分大小写）
  assert_eq!(RespCommand::lookup(b"get"), Some((RespCommand::GET, false)));
  assert_eq!(RespCommand::lookup(b"SET"), Some((RespCommand::SET, false)));
  assert_eq!(
    RespCommand::lookup(b"Hset"),
    Some((RespCommand::HSET, false))
  );
  assert_eq!(
    RespCommand::lookup(b"client"),
    Some((RespCommand::CLIENT, true))
  );
  assert_eq!(
    RespCommand::lookup(b"cluster"),
    Some((RespCommand::CLUSTER, true))
  );
  assert_eq!(RespCommand::lookup(b"unknown_cmd"), None);

  // 子命令查找
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::CLIENT, b"list"),
    Some(RespCommand::CLIENT_LIST)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::CLUSTER, b"SLOTS"),
    Some(RespCommand::CLUSTER_SLOTS)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::CONFIG, b"get"),
    Some(RespCommand::CONFIG_GET)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::BITOP, b"and"),
    Some(RespCommand::BITOP_AND)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::CLIENT, b"not_exist"),
    None
  );

  info!("RESP 命令属性与查找测试成功");
  OK
}

#[test]
fn test_read_utils_integers() -> Void {
  // uint64
  let (val, len) = RespReadUtils::try_read_uint64(b"1234567890abc")?;
  assert_eq!(val, 1234567890);
  assert_eq!(len, 10);

  // u64 溢出
  assert_eq!(
    RespReadUtils::try_read_uint64(b"18446744073709551616"),
    Err(Error::IntegerOverflow)
  );

  // int64 正常与边界
  let (val, len) = RespReadUtils::try_read_int64(b"-9223372036854775808", false)?;
  assert_eq!(val, i64::MIN);
  assert_eq!(len, 20);

  let (val, len) = RespReadUtils::try_read_int64(b"+9223372036854775807", false)?;
  assert_eq!(val, i64::MAX);
  assert_eq!(len, 20);

  // 前导零被拒绝
  assert_eq!(
    RespReadUtils::try_read_int64(b"0123", false),
    Err(Error::NotANumber)
  );
  // 前导零允许
  assert!(RespReadUtils::try_read_int64(b"0123", true).is_ok());

  // int32
  let (val, _) = RespReadUtils::try_read_int32(b"2147483647", false)?;
  assert_eq!(val, i32::MAX);
  assert_eq!(
    RespReadUtils::try_read_int32(b"2147483648", false),
    Err(Error::IntegerOverflow)
  );

  // int64 协议帧 (:1234\r\n)
  let (val, len) = RespReadUtils::try_read_int64_frame(b":-42\r\n")?;
  assert_eq!(val, -42);
  assert_eq!(len, 6);

  info!("RESP 整数读取测试成功");
  OK
}

#[test]
fn test_read_utils_bulk_and_structures() -> Void {
  // 零拷贝 Bulk String 切片提取
  let payload = b"$5\r\nhello\r\n";
  let (slice, bytes) = RespReadUtils::try_slice_with_length_header(payload)?;
  assert_eq!(slice, b"hello");
  assert_eq!(bytes, 11);

  // 数据不完整测试
  assert_eq!(
    RespReadUtils::try_slice_with_length_header(b"$10\r\nhello"),
    Err(Error::Incomplete)
  );

  // 格式异常测试
  assert_eq!(
    RespReadUtils::try_slice_with_length_header(b"$5\r\nhello\r\x00"),
    Err(Error::UnexpectedToken(b'\r'))
  );

  // 空 Bulk String
  let (slice, bytes) = RespReadUtils::try_slice_with_length_header(b"$0\r\n\r\n")?;
  assert_eq!(slice, b"");
  assert_eq!(bytes, 6);

  // 简单字符串
  let (s, bytes) = RespReadUtils::try_read_simple_string(b"+PONG\r\n")?;
  assert_eq!(s, b"PONG");
  assert_eq!(bytes, 7);

  // 错误字符串
  let (err, bytes) = RespReadUtils::try_read_error(b"-ERR syntax error\r\n")?;
  assert_eq!(err, b"ERR syntax error");
  assert_eq!(bytes, 19);

  // 布尔值
  let (b, _) = RespReadUtils::try_read_bool_with_length_header(b"#t\r\n")?;
  assert!(b);
  let (b, _) = RespReadUtils::try_read_bool_with_length_header(b"$1\r\n0\r\n")?;
  assert!(!b);

  // 浮点数与无穷大
  let (f, _) = RespReadUtils::try_read_double_with_length_header(b"$5\r\n12.75\r\n")?;
  assert!((f - 12.75).abs() < 1e-6);

  let (f, _) = RespReadUtils::try_read_double_with_length_header(b"$3\r\ninf\r\n")?;
  assert_eq!(f, f64::INFINITY);

  let (f, _) = RespReadUtils::try_read_double_with_length_header(b",-inf\r\n")?;
  assert_eq!(f, f64::NEG_INFINITY);

  info!("RESP 协议结构读取测试成功");
  OK
}

#[test]
fn test_write_utils() -> Void {
  let mut buf = [0u8; 128];

  // 数组头
  let n = RespWriteUtils::write_array_len(&mut buf, 3)?;
  assert_eq!(&buf[..n], b"*3\r\n");

  // BulkString
  let n = RespWriteUtils::write_bulk_string(&mut buf, b"foobar")?;
  assert_eq!(&buf[..n], b"$6\r\nfoobar\r\n");

  // 分块 BulkString
  let n = RespWriteUtils::write_bulk_string_chunks(&mut buf, &[b"foo", b"bar", b"123"])?;
  assert_eq!(&buf[..n], b"$9\r\nfoobar123\r\n");

  // 简单字符串
  let n = RespWriteUtils::write_simple_string(&mut buf, b"OK")?;
  assert_eq!(&buf[..n], b"+OK\r\n");

  // 错误
  let n = RespWriteUtils::write_error(&mut buf, b"ERR bad argument")?;
  assert_eq!(&buf[..n], b"-ERR bad argument\r\n");

  // 整数
  let n = RespWriteUtils::write_int64(&mut buf, -12345)?;
  assert_eq!(&buf[..n], b":-12345\r\n");

  // 整数作为 BulkString
  let n = RespWriteUtils::write_int64_as_bulk_string(&mut buf, 100)?;
  assert_eq!(&buf[..n], b"$3\r\n100\r\n");

  // 浮点数
  let n = RespWriteUtils::write_double_bulk_string(&mut buf, f64::INFINITY)?;
  assert_eq!(&buf[..n], b"$3\r\ninf\r\n");

  let n = RespWriteUtils::write_double_numeric(&mut buf, 42.5)?;
  assert_eq!(&buf[..n], b",42.5\r\n");

  // NULL 与常量
  let n = RespWriteUtils::write_null(&mut buf)?;
  assert_eq!(&buf[..n], b"$-1\r\n");

  let n = RespWriteUtils::write_resp3_null(&mut buf)?;
  assert_eq!(&buf[..n], b"_\r\n");

  let n = RespWriteUtils::write_true(&mut buf)?;
  assert_eq!(&buf[..n], b"#t\r\n");

  // 缓冲区空间不足保护
  let mut tiny_buf = [0u8; 3];
  assert_eq!(
    RespWriteUtils::write_bulk_string(&mut tiny_buf, b"too large"),
    Err(Error::BufferTooSmall)
  );

  // 动态 Vec 写入
  let mut vec = Vec::new();
  RespWriteUtils::push_array_len(&mut vec, 2);
  RespWriteUtils::push_bulk_string(&mut vec, b"ping");
  RespWriteUtils::push_int64(&mut vec, 99);
  assert_eq!(vec, b"*2\r\n$4\r\nping\r\n:99\r\n");

  info!("RESP 写工具测试成功");
  OK
}

#[test]
fn test_session_parse_state() -> Void {
  let mut state = SessionParseState::new();

  // 1. 测试标准 RESP 数组命令解析
  let mut input: &[u8] = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
  let cmd = parse_session_command(&mut input, &mut state)?;
  assert_eq!(cmd, RespCommand::SET);
  assert_eq!(state.len(), 2);
  assert_eq!(state.get_str(0)?, "key");
  assert_eq!(state.get_str(1)?, "value");
  assert!(input.is_empty());

  // 2. 测试带子命令的命令解析 (CLIENT LIST)
  let mut input: &[u8] = b"*2\r\n$6\r\nCLIENT\r\n$4\r\nLIST\r\n";
  let cmd = parse_session_command(&mut input, &mut state)?;
  assert_eq!(cmd, RespCommand::CLIENT_LIST);
  assert_eq!(state.len(), 0);
  assert!(input.is_empty());

  // 3. 测试带子命令和附加参数的命令 (CONFIG GET maxmemory)
  let mut input: &[u8] = b"*3\r\n$6\r\nCONFIG\r\n$3\r\nGET\r\n$9\r\nmaxmemory\r\n";
  let cmd = parse_session_command(&mut input, &mut state)?;
  assert_eq!(cmd, RespCommand::CONFIG_GET);
  assert_eq!(state.len(), 1);
  assert_eq!(state.get_str(0)?, "maxmemory");
  assert!(input.is_empty());

  // 4. 测试内联命令 (PING)
  let mut input: &[u8] = b"PING\r\n";
  let cmd = parse_session_command(&mut input, &mut state)?;
  assert_eq!(cmd, RespCommand::PING);
  assert_eq!(state.len(), 0);
  assert!(input.is_empty());

  // 5. 测试内联命令带参数 (SET foo 123)
  let mut input: &[u8] = b"SET foo 123\r\n";
  let cmd = parse_session_command(&mut input, &mut state)?;
  assert_eq!(cmd, RespCommand::SET);
  assert_eq!(state.len(), 2);
  assert_eq!(state.get_str(0)?, "foo");
  assert_eq!(state.get_int(1)?, 123);
  assert!(input.is_empty());

  // 6. 测试 ParseUtils 与 SessionParseState 参数提取
  assert_eq!(ParseUtils::read_int(b"42")?, 42);
  assert_eq!(ParseUtils::read_long(b"-999")?, -999);
  assert!(ParseUtils::read_bool(b"1")?);
  assert!(!ParseUtils::read_bool(b"false")?);
  assert_eq!(ParseUtils::read_double(b"12.75", false)?, 12.75);
  assert_eq!(ParseUtils::read_double(b"+inf", true)?, f64::INFINITY);
  assert!(ParseUtils::try_read_double(b"inf", false).is_none());
  assert!(ParseUtils::try_read_double(b"-inf", false).is_none());
  assert!(
    ParseUtils::try_read_double(b"+nan", false)
      .unwrap()
      .is_nan()
  );

  // 7. 测试事务原子性：输入不完整时，不推进 input 指针
  let incomplete_data: &[u8] = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n";
  let mut incomplete_input = incomplete_data;
  let res = parse_session_command(&mut incomplete_input, &mut state);
  assert_eq!(res, Err(Error::Incomplete));
  assert_eq!(incomplete_input, incomplete_data);

  // 8. 测试 Deref 特性与参数直接访问
  let mut input: &[u8] = b"*3\r\n$3\r\nSET\r\n$3\r\nkey\r\n$5\r\nvalue\r\n";
  parse_session_command(&mut input, &mut state)?;
  assert_eq!(state[0], b"key");
  assert_eq!(state[1], b"value");
  assert_eq!(&state[..], &[b"key".as_slice(), b"value".as_slice()]);

  // 9. 测试未知命令错误（HipStr 存储）
  let mut input: &[u8] = b"*1\r\n$7\r\nUNKNOWN\r\n";
  let err = parse_session_command(&mut incomplete_input, &mut state);
  assert_eq!(err, Err(Error::Incomplete));
  let err = parse_session_command(&mut input, &mut state);
  assert_eq!(err, Err(Error::UnknownCommand("UNKNOWN".into())));

  // 10. 测试 SessionParseState 辅助方法与序列化/反序列化往返
  state.init_with_args2(b"field1", b"val1");
  assert_eq!(state.len(), 2);
  assert_eq!(state.slice_range(0, 1), &[b"field1".as_slice()]);
  assert_eq!(state.slice_range(1, 10), &[b"val1".as_slice()]);

  let mut ser_buf = [0u8; 64];
  let ser_len = state.serialize_to(&mut ser_buf)?;
  assert_eq!(ser_len, state.serialized_length());

  let mut de_state = SessionParseState::new();
  let de_consumed = de_state.deserialize_from(&ser_buf[..ser_len])?;
  assert_eq!(de_consumed, ser_len);
  assert_eq!(de_state.len(), 2);
  assert_eq!(de_state[0], b"field1");
  assert_eq!(de_state[1], b"val1");

  info!("会话解析状态机测试成功");
  OK
}

#[test]
fn test_edge_cases_and_robustness() -> Void {
  // 1. 紧凑长度编码分包/不完整包测试
  let mut partial_14bit: &[u8] = &[0x40]; // 14位编码需要2字节，目前仅有1字节
  assert_eq!(
    RespLengthEncodingUtils::read_length(&mut partial_14bit),
    Err(Error::Incomplete)
  );

  let mut partial_32bit: &[u8] = &[0x80, 0x01, 0x02]; // 32位编码需要5字节，目前仅有3字节
  assert_eq!(
    RespLengthEncodingUtils::read_length(&mut partial_32bit),
    Err(Error::Incomplete)
  );

  let mut invalid_prefix: &[u8] = &[0xC0]; // 11开头是非法前缀
  assert_eq!(
    RespLengthEncodingUtils::read_length(&mut invalid_prefix),
    Err(Error::InvalidLength(-1))
  );

  // 2. 整数协议帧空内容与仅符号
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
    RespReadUtils::try_read_int64(b"+", false),
    Err(Error::NotANumber)
  );
  assert_eq!(
    RespReadUtils::try_read_int64(b"-", false),
    Err(Error::NotANumber)
  );

  // 3. parse_double 处理 +/-nan 以及正号浮点数
  assert!(RespReadUtils::parse_double(b"+nan")?.is_nan());
  assert!(RespReadUtils::parse_double(b"-nan")?.is_nan());
  assert!(RespReadUtils::parse_double(b"NAN")?.is_nan());
  assert!((RespReadUtils::parse_double(b"+12.34")? - 12.34).abs() < 1e-6);

  // 4. AOF 独立命令属性验证
  assert!(RespCommand::PING.is_aof_independent());
  assert!(RespCommand::SELECT.is_aof_independent());
  assert!(RespCommand::INFO.is_aof_independent());
  assert!(RespCommand::CLIENT_LIST.is_aof_independent());
  assert!(RespCommand::MULTI.is_aof_independent());
  assert!(!RespCommand::SET.is_aof_independent());
  assert!(!RespCommand::GET.is_aof_independent());

  // 5. 写工具扩展验证
  let mut w_buf = [0u8; 64];
  let item_len = RespWriteUtils::write_array_item(&mut w_buf, 42)?;
  assert_eq!(&w_buf[..item_len], b"$2\r\n42\r\n");

  let hdr_len = RespWriteUtils::write_verbatim_string_header(&mut w_buf, 5, b"txt")?;
  assert_eq!(&w_buf[..hdr_len], b"=9\r\ntxt:");

  info!("边界异常与鲁棒性增强测试成功");
  OK
}

type FastCase<'a> = (&'a [u8], RespCommand, usize, usize, &'a [&'a [u8]]);

#[test]
fn test_simd_fast_parse_all_patterns() -> Void {
  let cases: &[FastCase] = &[
    // 13 字节热点命令
    (
      b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n",
      RespCommand::Get,
      1,
      13,
      &[b"foo"],
    ),
    (
      b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
      RespCommand::Set,
      2,
      13,
      &[b"k", b"v"],
    ),
    (
      b"*2\r\n$3\r\nDEL\r\n$3\r\nbar\r\n",
      RespCommand::Del,
      1,
      13,
      &[b"bar"],
    ),
    (
      b"*2\r\n$3\r\nTTL\r\n$3\r\nbaz\r\n",
      RespCommand::Ttl,
      1,
      13,
      &[b"baz"],
    ),
    // 14 字节热点命令 (b"*1\r\n$4\r\nPING\r\n", RespCommand::Ping, 0, 14, &[]),
    (
      b"*2\r\n$4\r\nINCR\r\n$1\r\nx\r\n",
      RespCommand::Incr,
      1,
      14,
      &[b"x"],
    ),
    (
      b"*2\r\n$4\r\nDECR\r\n$1\r\ny\r\n",
      RespCommand::Decr,
      1,
      14,
      &[b"y"],
    ),
    (b"*1\r\n$4\r\nEXEC\r\n", RespCommand::Exec, 0, 14, &[]),
    (
      b"*2\r\n$4\r\nPTTL\r\n$1\r\nz\r\n",
      RespCommand::Pttl,
      1,
      14,
      &[b"z"],
    ),
    // 15 字节热点命令 (b"*1\r\n$5\r\nMULTI\r\n", RespCommand::Multi, 0, 15, &[]),
    (
      b"*3\r\n$5\r\nSETNX\r\n$1\r\na\r\n$1\r\nb\r\n",
      RespCommand::Setnx,
      2,
      15,
      &[b"a", b"b"],
    ),
    (
      b"*4\r\n$5\r\nSETEX\r\n$1\r\na\r\n$2\r\n10\r\n$1\r\nb\r\n",
      RespCommand::Setex,
      3,
      15,
      &[b"a", b"10", b"b"],
    ),
    // 16 字节热点命令
    (
      b"*2\r\n$6\r\nEXISTS\r\n$1\r\nk\r\n",
      RespCommand::Exists,
      1,
      16,
      &[b"k"],
    ),
    (
      b"*2\r\n$6\r\nGETDEL\r\n$1\r\nk\r\n",
      RespCommand::Getdel,
      1,
      16,
      &[b"k"],
    ),
    (
      b"*3\r\n$6\r\nAPPEND\r\n$1\r\nk\r\n$1\r\nv\r\n",
      RespCommand::Append,
      2,
      16,
      &[b"k", b"v"],
    ),
    (
      b"*3\r\n$6\r\nINCRBY\r\n$1\r\nk\r\n$1\r\n1\r\n",
      RespCommand::Incrby,
      2,
      16,
      &[b"k", b"1"],
    ),
    (
      b"*3\r\n$6\r\nDECRBY\r\n$1\r\nk\r\n$1\r\n1\r\n",
      RespCommand::Decrby,
      2,
      16,
      &[b"k", b"1"],
    ),
    (
      b"*4\r\n$6\r\nPSETEX\r\n$1\r\nk\r\n$4\r\n1000\r\n$1\r\nv\r\n",
      RespCommand::Psetex,
      3,
      16,
      &[b"k", b"1000", b"v"],
    ),
  ];

  let mut state = SessionParseState::new();

  for &(raw, expected_cmd, expected_args_count, expected_consumed, expected_args) in cases {
    // 1. 测试直接调用 simd_fast_parse
    let fast_res = simd_fast_parse(raw);
    assert!(
      fast_res.is_some(),
      "simd_fast_parse 应当匹配: {:?}",
      String::from_utf8_lossy(raw)
    );
    let (cmd, args_count, consumed) = fast_res.unwrap();
    assert_eq!(cmd, expected_cmd);
    assert_eq!(args_count, expected_args_count);
    assert_eq!(consumed, expected_consumed);

    // 2. 测试整合后的 parse_session_command
    let mut cursor = raw;
    let parsed_cmd = parse_session_command(&mut cursor, &mut state)?;
    assert_eq!(parsed_cmd, expected_cmd);
    assert_eq!(state.len(), expected_args.len());
    for (i, &expected_arg) in expected_args.iter().enumerate() {
      assert_eq!(state.get(i), Some(expected_arg));
    }
    assert!(cursor.is_empty(), "必须完整消耗全部字节");
  }

  // 3. 测试未命中快速路径时的平滑回退
  let fallback_cases: &[(&[u8], RespCommand)] = &[
    (b"*2\r\n$3\r\nget\r\n$3\r\nfoo\r\n", RespCommand::Get), // 小写 get
    (
      b"*3\r\n$3\r\nDEL\r\n$1\r\na\r\n$1\r\nb\r\n",
      RespCommand::Del,
    ), // 2 个参数的 DEL（超出模板 *2）
    (b"PING\r\n", RespCommand::Ping),                        // 内联命令
  ];

  for &(raw, expected_cmd) in fallback_cases {
    let mut cursor = raw;
    let parsed_cmd = parse_session_command(&mut cursor, &mut state)?;
    assert_eq!(parsed_cmd, expected_cmd);
  }

  info!("128 位 / SIMD 极速解析模式验证通过");
  OK
}

#[test]
fn test_session_parse_state_mru_cache() -> Void {
  let mut state = SessionParseState::new();

  // 1. 初始状态 MRU 为空
  assert!(state.mru0.is_none());
  assert!(state.mru1.is_none());

  // 2. 解析不在静态 SIMD 表的命令 (HGET foo bar, 帧头 *3\r\n$4\r\nHGET\r\n 为 14 字节)
  let raw_hget = b"*3\r\n$4\r\nHGET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
  let mut cursor = &raw_hget[..];
  let cmd = parse_session_command(&mut cursor, &mut state)?;
  assert_eq!(cmd, RespCommand::HGET);
  assert_eq!(state.len(), 2);
  assert_eq!(state[0], b"foo");
  assert_eq!(state[1], b"bar");
  assert!(cursor.is_empty());

  // 验证 MRU 槽位 0 被记录
  assert!(state.mru0.is_some());
  let mru0 = state.mru0.unwrap();
  assert_eq!(mru0.cmd, RespCommand::HGET);
  assert_eq!(mru0.count, 2);
  assert_eq!(mru0.len, 14);
  assert_eq!(mru0.mask, mask_for(14));
  assert!(state.mru1.is_none());

  // 3. 再次执行相同命令，走 MRU SIMD 快速路径
  let mut cursor2 = &raw_hget[..];
  let cmd2 = parse_session_command(&mut cursor2, &mut state)?;
  assert_eq!(cmd2, RespCommand::HGET);
  assert_eq!(state.len(), 2);
  assert_eq!(state[0], b"foo");
  assert_eq!(state[1], b"bar");
  assert!(cursor2.is_empty());

  // 4. 解析第二种命令 (LPUSH list v1, 帧头 *3\r\n$5\r\nLPUSH\r\n 为 15 字节)
  let raw_lpush = b"*3\r\n$5\r\nLPUSH\r\n$4\r\nlist\r\n$2\r\nv1\r\n";
  let mut cursor3 = &raw_lpush[..];
  let cmd3 = parse_session_command(&mut cursor3, &mut state)?;
  assert_eq!(cmd3, RespCommand::LPUSH);
  assert_eq!(state.len(), 2);
  assert_eq!(state[0], b"list");
  assert_eq!(state[1], b"v1");

  // 验证槽位下沉：slot 0 是 LPUSH, slot 1 是 HGET
  assert_eq!(state.mru0.unwrap().cmd, RespCommand::LPUSH);
  assert_eq!(state.mru1.unwrap().cmd, RespCommand::HGET);

  // 5. 再次调用 HGET，应触发 slot 1 命中并提升至 slot 0
  let mut cursor4 = &raw_hget[..];
  let cmd4 = parse_session_command(&mut cursor4, &mut state)?;
  assert_eq!(cmd4, RespCommand::HGET);
  assert_eq!(state.mru0.unwrap().cmd, RespCommand::HGET);
  assert_eq!(state.mru1.unwrap().cmd, RespCommand::LPUSH);

  // 5.1 大小写不敏感命中：小写 hget 应同样命中大写模式（对齐 C# MakeUpperCase 归一化行为）
  let raw_hget_lower = b"*3\r\n$4\r\nhget\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
  let mut cursor5 = &raw_hget_lower[..];
  let cmd5 = parse_session_command(&mut cursor5, &mut state)?;
  assert_eq!(cmd5, RespCommand::HGET);
  assert_eq!(state.len(), 2);
  assert_eq!(state[0], b"foo");
  assert_eq!(state[1], b"bar");
  assert!(cursor5.is_empty());

  // 6. 清空 MRU 缓存
  state.clear_mru();
  assert!(state.mru0.is_none());
  assert!(state.mru1.is_none());

  info!("会话 MRU 命令缓存与 SIMD 加速验证成功");
  OK
}

#[test]
fn test_simd_fast_parse_perf_benchmark() -> Void {
  let sample = b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n";
  let mut state = SessionParseState::new();

  // 预热
  for _ in 0..10_000 {
    let mut cursor = &sample[..];
    let _ = parse_session_command(&mut cursor, &mut state)?;
  }

  let iterations = 500_000;
  let start = Instant::now();
  for _ in 0..iterations {
    let mut cursor = &sample[..];
    let _ = parse_session_command(&mut cursor, &mut state)?;
  }
  let duration = start.elapsed();
  let ns_per_op = duration.as_nanos() as f64 / iterations as f64;
  info!(
    "SIMD 静态快速路径吞吐性能: {} 次迭代，总耗时 {:?}，单次耗时: {:.2} ns (吞吐量: {:.2} M ops/sec)",
    iterations,
    duration,
    ns_per_op,
    1_000.0 / ns_per_op
  );

  // MRU 快速路径（重复调用不在静态表中的非标准/小写命令 get）
  let mru_sample = b"*2\r\n$3\r\nget\r\n$3\r\nfoo\r\n";
  let mut mru_state = SessionParseState::new();
  // 首次触发缓存填充
  let mut cursor = &mru_sample[..];
  let _ = parse_session_command(&mut cursor, &mut mru_state)?;

  let start_mru = Instant::now();
  for _ in 0..iterations {
    let mut cursor = &mru_sample[..];
    let _ = parse_session_command(&mut cursor, &mut mru_state)?;
  }
  let duration_mru = start_mru.elapsed();
  let ns_per_op_mru = duration_mru.as_nanos() as f64 / iterations as f64;
  info!(
    "SIMD MRU 缓存路径吞吐性能: {} 次迭代，总耗时 {:?}，单次耗时: {:.2} ns (吞吐量: {:.2} M ops/sec)",
    iterations,
    duration_mru,
    ns_per_op_mru,
    1_000.0 / ns_per_op_mru
  );

  // 通用路径（每轮清空 MRU 强制走 CRLF + 整数解析 + 哈希表查表通用路径）
  let general_sample = b"*2\r\n$4\r\nhget\r\n$3\r\nfoo\r\n";
  let mut general_state = SessionParseState::new();
  for _ in 0..10_000 {
    let mut cursor = &general_sample[..];
    let _ = parse_session_command(&mut cursor, &mut general_state)?;
    general_state.clear_mru();
  }

  let start_general = Instant::now();
  for _ in 0..iterations {
    let mut cursor = &general_sample[..];
    let _ = parse_session_command(&mut cursor, &mut general_state)?;
    general_state.clear_mru();
  }
  let duration_general = start_general.elapsed();
  let ns_per_op_general = duration_general.as_nanos() as f64 / iterations as f64;
  info!(
    "通用路径吞吐性能: {} 次迭代，总耗时 {:?}，单次耗时: {:.2} ns (吞吐量: {:.2} M ops/sec)",
    iterations,
    duration_general,
    ns_per_op_general,
    1_000.0 / ns_per_op_general
  );
  info!(
    "SIMD 静态加速比: {:.2}x (耗时降低 {:.1}%), MRU 加速比: {:.2}x (耗时降低 {:.1}%)",
    ns_per_op_general / ns_per_op,
    (1.0 - ns_per_op / ns_per_op_general) * 100.0,
    ns_per_op_general / ns_per_op_mru,
    (1.0 - ns_per_op_mru / ns_per_op_general) * 100.0
  );
  OK
}
