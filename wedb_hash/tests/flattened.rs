use aok::{OK, Void};
use log::info;
use wedb_hash::{FieldChunkCodec, FieldValueCodec};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_field_value_codec_no_expiration() -> Void {
  info!("测试无过期时间的 FieldValue 编码与零拷贝解码");

  let val = b"hello_world_123456";
  let encoded = FieldValueCodec::encode(val, None);
  assert_eq!(encoded[0], 0x00);
  assert_eq!(&encoded[1..], val);

  let (expire_at, decoded_val) = FieldValueCodec::decode(&encoded)?;
  assert_eq!(expire_at, None);
  assert_eq!(decoded_val, val);

  // 空值测试
  let empty_encoded = FieldValueCodec::encode(b"", None);
  assert_eq!(empty_encoded, vec![0x00]);
  let (empty_exp, empty_val) = FieldValueCodec::decode(&empty_encoded)?;
  assert_eq!(empty_exp, None);
  assert_eq!(empty_val, b"");

  OK
}

#[test]
fn test_field_value_codec_with_expiration() -> Void {
  info!("测试包含绝对过期时间戳的 FieldValue 编码与零拷贝解码");

  let val = b"value_with_ttl";
  let expire_ms = 1_700_000_000_123u64;
  let encoded = FieldValueCodec::encode(val, Some(expire_ms));

  assert_eq!(encoded[0], 0x01);
  assert_eq!(&encoded[1..9], &expire_ms.to_be_bytes());
  assert_eq!(&encoded[9..], val);

  let (expire_at, decoded_val) = FieldValueCodec::decode(&encoded)?;
  assert_eq!(expire_at, Some(expire_ms));
  assert_eq!(decoded_val, val);

  // 校验切片过短防御
  assert!(FieldValueCodec::decode(&[]).is_err());
  assert!(FieldValueCodec::decode(&[0x01, 1, 2, 3]).is_err()); // 包含 0x01 但不足 9 字节

  OK
}

#[test]
fn test_field_chunk_codec_roundtrip() -> Void {
  info!("测试 FieldChunkCodec 打包、解包与流式零拷贝迭代");

  let fields: Vec<&[u8]> = vec![b"field_a", b"user_id", b"email_address", b"status_flag"];

  let mut buf = Vec::new();
  FieldChunkCodec::encode(&fields, &mut buf);

  // 解码遍历
  let decoded: Vec<&[u8]> = FieldChunkCodec::iter(&buf)?.collect();
  assert_eq!(decoded, fields);

  // 单独追加测试
  FieldChunkCodec::append(b"new_field_x", &mut buf)?;
  let mut expected = fields.clone();
  expected.push(b"new_field_x");

  let decoded_after_append: Vec<&[u8]> = FieldChunkCodec::iter(&buf)?.collect();
  assert_eq!(decoded_after_append, expected);

  // 空 chunk 测试
  let mut empty_buf = Vec::new();
  FieldChunkCodec::encode(&[], &mut empty_buf);
  let empty_decoded: Vec<&[u8]> = FieldChunkCodec::iter(&empty_buf)?.collect();
  assert!(empty_decoded.is_empty());

  OK
}

#[test]
fn test_field_value_buf_stack_and_heap_conversion() -> Void {
  // 1. 栈分配分支 (总字节 <= 73B: 1 字节 TAG + 16 字节 = 17 字节)
  let short_val = b"stack_short_val";
  let stack_buf = FieldValueCodec::encode_buf(short_val, None);
  assert!(matches!(stack_buf, wedb_hash::FieldValueBuf::Stack(..)));
  assert_eq!(stack_buf.as_slice()[0], 0x00);
  assert_eq!(&stack_buf.as_slice()[1..], short_val);
  assert_eq!(stack_buf.clone(), stack_buf);
  let owned_vec = stack_buf.into_vec();
  assert_eq!(&owned_vec[1..], short_val);

  // 2. 带过期时间的栈分配 (9 字节头 + 30 字节载荷 = 39 <= 73)
  let expire_ms = 1_800_000_000_000u64;
  let mid_val = b"value_with_timestamp_30_bytes!";
  let stack_exp_buf = FieldValueCodec::encode_buf(mid_val, Some(expire_ms));
  assert!(matches!(stack_exp_buf, wedb_hash::FieldValueBuf::Stack(..)));
  let (dec_exp, dec_val) = FieldValueCodec::decode(stack_exp_buf.as_slice())?;
  assert_eq!(dec_exp, Some(expire_ms));
  assert_eq!(dec_val, mid_val);

  // 3. 堆回退分支 (载荷 100 字节，总长度 109 > 73)
  let long_val = vec![b'A'; 100];
  let heap_buf = FieldValueCodec::encode_buf(&long_val, Some(expire_ms));
  assert!(matches!(heap_buf, wedb_hash::FieldValueBuf::Heap(..)));
  assert_eq!(heap_buf.as_slice().len(), 109);
  let (dec_heap_exp, dec_heap_val) = FieldValueCodec::decode(heap_buf.as_slice())?;
  assert_eq!(dec_heap_exp, Some(expire_ms));
  assert_eq!(dec_heap_val, long_val.as_slice());
  let heap_owned = heap_buf.into_vec();
  assert_eq!(heap_owned.len(), 109);

  OK
}

#[test]
fn test_field_chunk_corrupted_defense() {
  // 头部长度不足 4 字节
  assert!(FieldChunkCodec::iter(&[1, 2, 3]).is_err());

  // 声明长度超过剩余切片总长
  let corrupted = vec![0, 0, 1, 0]; // 声明 256 字节，但切片剩余 0 字节
  assert!(FieldChunkCodec::iter(&corrupted).is_err());
}

#[test]
fn test_field_value_buf_boundary_and_traits() -> Void {
  use core::borrow::Borrow;

  use wedb_hash::{FIELD_VALUE_STACK_CAP, FieldValueBuf};

  // Default trait
  let def = FieldValueBuf::default();
  assert!(matches!(def, FieldValueBuf::Stack(_, 0)));
  assert!(def.as_slice().is_empty());

  // Borrow trait
  let borrowed_slice: &[u8] = def.borrow();
  assert!(borrowed_slice.is_empty());

  // 临界边界：含过期时间（9 字节头）
  // 64 字节载荷 => 总长 73 字节（恰好等于 FIELD_VALUE_STACK_CAP）=> 栈分配
  let payload_64 = [b'x'; 64];
  let buf_73 = FieldValueCodec::encode_buf(&payload_64, Some(100_000));
  assert!(matches!(buf_73, FieldValueBuf::Stack(..)));
  assert_eq!(buf_73.len(), FIELD_VALUE_STACK_CAP);

  // 65 字节载荷 => 总长 74 字节（超过 73 字节）=> 堆回退
  let payload_65 = [b'x'; 65];
  let buf_74 = FieldValueCodec::encode_buf(&payload_65, Some(100_000));
  assert!(matches!(buf_74, FieldValueBuf::Heap(..)));
  assert_eq!(buf_74.len(), 74);

  // 临界边界：无过期时间（1 字节头）
  // 72 字节载荷 => 总长 73 字节 => 栈分配
  let payload_72 = [b'y'; 72];
  let buf_no_exp_73 = FieldValueCodec::encode_buf(&payload_72, None);
  assert!(matches!(buf_no_exp_73, FieldValueBuf::Stack(..)));
  assert_eq!(buf_no_exp_73.len(), FIELD_VALUE_STACK_CAP);

  // 73 字节载荷 => 总长 74 字节 => 堆回退
  let payload_73 = [b'y'; 73];
  let buf_no_exp_74 = FieldValueCodec::encode_buf(&payload_73, None);
  assert!(matches!(buf_no_exp_74, FieldValueBuf::Heap(..)));
  assert_eq!(buf_no_exp_74.len(), 74);

  OK
}
