//! RespReadUtilsTests：序列化记录跨度（GetSerializedRecordSpan）测试

use aok::{OK, Void};
use log::info;
use wedb_resp::RespReadUtils;

/// 对标 Garnet RespReadUtilsTests.cs: GetSerializedRecordSpanValidTest —— 4 字节小端长度头 + 载荷的正确切分
#[test]
fn test_get_serialized_record_span_valid() -> Void {
  let mut data = vec![0u8; 4 + 5];
  data[..4].copy_from_slice(&5i32.to_le_bytes());
  data[4..].copy_from_slice(&[0xAA, 0xBB, 0xCC, 0xDD, 0xEE]);

  let (record, consumed) = RespReadUtils::get_serialized_record_span(&data)?;
  assert_eq!(record.len(), 5);
  assert_eq!(record[0], 0xAA);
  assert_eq!(record[4], 0xEE);
  assert_eq!(consumed, data.len());

  info!("C# 兼容性测试：GetSerializedRecordSpanValidTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: GetSerializedRecordSpanOverflowLengthTest —— 声明长度超出数据长度应失败
#[test]
fn test_get_serialized_record_span_overflow_length() -> Void {
  let mut data = vec![0u8; 4 + 3];
  data[..4].copy_from_slice(&1000i32.to_le_bytes());
  data[4..].copy_from_slice(&[0x01, 0x02, 0x03]);

  let res = RespReadUtils::get_serialized_record_span(&data);
  assert!(res.is_err(), "声明长度大于数据长度应该失败");

  info!("C# 兼容性测试：GetSerializedRecordSpanOverflowLengthTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: GetSerializedRecordSpanNegativeLengthTest —— 负数长度应失败
#[test]
fn test_get_serialized_record_span_negative_length() -> Void {
  let mut data = vec![0u8; 4 + 10];
  data[..4].copy_from_slice(&(-1i32).to_le_bytes());

  let res = RespReadUtils::get_serialized_record_span(&data);
  assert!(res.is_err(), "负数长度应该失败");

  info!("C# 兼容性测试：GetSerializedRecordSpanNegativeLengthTest 通过");
  OK
}

/// 对标 Garnet RespReadUtilsTests.cs: GetSerializedRecordSpanInsufficientHeaderTest —— 长度头不足 4 字节应失败
#[test]
fn test_get_serialized_record_span_insufficient_header() -> Void {
  let data = [0u8; 2];
  let res = RespReadUtils::get_serialized_record_span(&data);
  assert!(res.is_err(), "长度头不足 4 字节应该失败");

  info!("C# 兼容性测试：GetSerializedRecordSpanInsufficientHeaderTest 通过");
  OK
}
