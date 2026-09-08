use aok::{OK, Void};
use wedb_resp::consts::{cmd, err, resp};

#[test]
fn test_namespaced_consts() -> Void {
  // 1. 命令常量测试
  assert_eq!(cmd::GET, b"GET");
  assert_eq!(cmd::SET, b"SET");
  assert_eq!(cmd::BLPOP, b"BLPOP");
  assert_eq!(cmd::BRPOP, b"BRPOP");
  assert_eq!(cmd::NX, b"NX");
  assert_eq!(cmd::XX, b"XX");
  assert_eq!(cmd::LEFT, b"LEFT");
  assert_eq!(cmd::RIGHT, b"RIGHT");

  // 2. 错误响应常量测试
  assert_eq!(
    err::WRONG_TYPE,
    b"WRONGTYPE Operation against a key holding the wrong kind of value"
  );
  assert_eq!(err::SYNTAX, b"ERR syntax error");
  assert_eq!(
    err::INT_OUT_OF_RANGE,
    b"ERR value is not an integer or out of range"
  );
  assert_eq!(err::NOAUTH, b"NOAUTH Authentication required.");

  // 3. 协议帧常量测试
  assert_eq!(resp::OK, b"+OK\r\n");
  assert_eq!(resp::PONG, b"+PONG\r\n");
  assert_eq!(resp::QUEUED, b"+QUEUED\r\n");
  assert_eq!(resp::CRLF, b"\r\n");
  assert_eq!(resp::NULL_BULK, b"$-1\r\n");
  assert_eq!(resp::NULL_ARRAY, b"*-1\r\n");

  OK
}
