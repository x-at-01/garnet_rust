//! RespReadUtilsTests：超大长度限制（RejectOversizedLengths）测试

use aok::{OK, Void};
use log::info;
use wedb_resp::RespReadUtils;

/// 对标 Garnet RespReadUtilsTests.cs: RejectOversizedLengths —— 各读取接口统一拒绝超过 512MB 限制的长度头
#[test]
fn test_reject_oversized_lengths() -> Void {
  let small_payload = format!("$1234\r\n{}\r\n", "a".repeat(1234));
  let small = small_payload.as_bytes();

  // 超过 512MB 限制的头部 (536870913 = 512*1024*1024 + 1)
  let big = b"$536870913\r\n";

  // 1. TryReadPtrWithSignedLengthHeader
  {
    let (opt_slice, consumed) = RespReadUtils::try_read_ptr_with_signed_length_header(small)?;
    assert_eq!(opt_slice.unwrap().len(), 1234);
    assert_eq!(consumed, small.len());

    assert!(RespReadUtils::try_read_ptr_with_signed_length_header(big).is_err());
  }

  // 2. TrySkipByteArrayWithLengthHeader
  {
    let consumed = RespReadUtils::try_skip_byte_array_with_length_header(small)?;
    assert_eq!(consumed, small.len());

    assert!(RespReadUtils::try_skip_byte_array_with_length_header(big).is_err());
  }

  // 3. TrySliceWithLengthHeader
  {
    let (slice, consumed) = RespReadUtils::try_slice_with_length_header(small)?;
    assert_eq!(slice.len(), 1234);
    assert_eq!(consumed, small.len());

    assert!(RespReadUtils::try_slice_with_length_header(big).is_err());
  }

  // 4. TryReadSpanWithLengthHeader
  {
    let (slice, consumed) = RespReadUtils::try_read_span_with_length_header(small)?;
    assert_eq!(slice.len(), 1234);
    assert_eq!(consumed, small.len());

    assert!(RespReadUtils::try_read_span_with_length_header(big).is_err());
  }

  // 5. TryReadPtrWithLengthHeader
  {
    let (slice, consumed) = RespReadUtils::try_read_ptr_with_length_header(small)?;
    assert_eq!(slice.len(), 1234);
    assert_eq!(consumed, small.len());

    assert!(RespReadUtils::try_read_ptr_with_length_header(big).is_err());
  }

  info!("C# 兼容性测试：RejectOversizedLengths 通过");
  OK
}
