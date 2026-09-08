//! Redis 标准错误消息常量定义（1:1 对标 Microsoft Garnet CmdStrings 错误返回）

use hipstr::HipStr;

// ====== 类型错误 ======
pub const WRONG_TYPE_STR: &str =
  "WRONGTYPE Operation against a key holding the wrong kind of value";
pub const WRONG_TYPE: &[u8] = WRONG_TYPE_STR.as_bytes();
pub const WRONG_TYPE_HIPSTR: HipStr<'static> = HipStr::from_static(WRONG_TYPE_STR);

pub const WRONG_TYPE_HLL_STR: &str = "WRONGTYPE Key is not a valid HyperLogLog string value.";
pub const WRONG_TYPE_HLL: &[u8] = WRONG_TYPE_HLL_STR.as_bytes();
pub const WRONG_TYPE_HLL_HIPSTR: HipStr<'static> = HipStr::from_static(WRONG_TYPE_HLL_STR);

// ====== 鉴权与权限错误 ======
pub const NOAUTH_STR: &str = "NOAUTH Authentication required.";
pub const NOAUTH: &[u8] = NOAUTH_STR.as_bytes();
pub const NOAUTH_HIPSTR: HipStr<'static> = HipStr::from_static(NOAUTH_STR);

pub const NOPERM_STR: &str = "NOPERM this user has no permissions to run the command";
pub const NOPERM: &[u8] = NOPERM_STR.as_bytes();
pub const NOPERM_HIPSTR: HipStr<'static> = HipStr::from_static(NOPERM_STR);

// ====== 语法与参数错误 ======
pub const SYNTAX_STR: &str = "ERR syntax error";
pub const SYNTAX: &[u8] = SYNTAX_STR.as_bytes();
pub const SYNTAX_HIPSTR: HipStr<'static> = HipStr::from_static(SYNTAX_STR);

pub const WRONG_NUM_ARGS_STR: &str = "ERR wrong number of arguments";
pub const WRONG_NUM_ARGS: &[u8] = WRONG_NUM_ARGS_STR.as_bytes();
pub const WRONG_NUM_ARGS_HIPSTR: HipStr<'static> = HipStr::from_static(WRONG_NUM_ARGS_STR);

pub const UNKNOWN_CMD_STR: &str = "ERR unknown command";
pub const UNKNOWN_CMD: &[u8] = UNKNOWN_CMD_STR.as_bytes();
pub const UNKNOWN_CMD_HIPSTR: HipStr<'static> = HipStr::from_static(UNKNOWN_CMD_STR);

pub const EXCESSIVE_ARGS_STR: &str = "ERR excessive number of arguments";
pub const EXCESSIVE_ARGS: &[u8] = EXCESSIVE_ARGS_STR.as_bytes();
pub const EXCESSIVE_ARGS_HIPSTR: HipStr<'static> = HipStr::from_static(EXCESSIVE_ARGS_STR);

// ====== 数字与边界错误 ======
pub const INT_OUT_OF_RANGE_STR: &str = "ERR value is not an integer or out of range";
pub const INT_OUT_OF_RANGE: &[u8] = INT_OUT_OF_RANGE_STR.as_bytes();
pub const INT_OUT_OF_RANGE_HIPSTR: HipStr<'static> = HipStr::from_static(INT_OUT_OF_RANGE_STR);
pub const BIT_OFFSET_OUT_OF_RANGE: &[u8] = b"ERR bit offset is not an integer or out of range";
pub const BIT_NOT_INT: &[u8] = b"ERR bit is not an integer or out of range";
pub const BIT_MUST_BE_ZERO_OR_ONE: &[u8] = b"ERR The bit argument must be 1 or 0.";
pub const BITOP_DIFF_TWO_SOURCE_KEYS: &[u8] =
  b"ERR BITOP DIFF operation requires at least two source bitmaps";
pub const BITOP_NOT_SINGLE_SOURCE: &[u8] = b"ERR BITOP NOT takes only one source key";
pub const BITOP_KEY_LIMIT: &[u8] =
  b"ERR The number of keys for the BITOP operation must not exceed 64";
pub const TIMEOUT_NOT_INT: &[u8] = b"ERR timeout is not an integer or out of range";
pub const TIMEOUT_NOT_FLOAT: &[u8] = b"ERR timeout is not a float or out of range";
pub const TIMEOUT_NEGATIVE: &[u8] = b"ERR timeout is negative";
pub const CLIENT_UNBLOCK_REASON: &[u8] = b"ERR CLIENT UNBLOCK reason should be TIMEOUT or ERROR";
pub const FLOAT_OUT_OF_RANGE: &[u8] = b"ERR value is not a valid float";
pub const NAN_OR_INFINITY: &[u8] = b"ERR increment would produce NaN or Infinity";
pub const SCORE_NAN: &[u8] = b"ERR resulting score is not a number (NaN)";
pub const INDEX_OUT_OF_RANGE: &[u8] = b"ERR index out of range";
pub const OFFSET_OUT_OF_RANGE: &[u8] = b"ERR offset is out of range";
pub const CURSOR_INVALID: &[u8] = b"ERR invalid cursor";
pub const NO_SUCH_KEY: &[u8] = b"ERR no such key";
// ====== 地理位置错误 (GEO) ======
pub const NOT_VALID_GEO_DISTANCE_UNIT: &[u8] =
  b"ERR unsupported unit provided. please use M, KM, FT, MI";
pub const NOT_VALID_RADIUS: &[u8] = b"ERR need numeric radius";
pub const RADIUS_IS_NEGATIVE: &[u8] = b"ERR radius cannot be negative";
pub const NOT_VALID_WIDTH: &[u8] = b"ERR need numeric width";
pub const NOT_VALID_HEIGHT: &[u8] = b"ERR need numeric height";
pub const HEIGHT_OR_WIDTH_NEGATIVE: &[u8] = b"ERR height or width cannot be negative";
pub const COUNT_IS_NOT_POSITIVE: &[u8] = b"ERR COUNT must be > 0";
pub const ZSET_MEMBER_NOT_FOUND: &[u8] = b"ERR could not decode requested zset member";

// ====== 事务与控制错误 ======
pub const MULTI_NESTED: &[u8] = b"ERR MULTI calls can not be nested";
pub const EXEC_WITHOUT_MULTI: &[u8] = b"ERR EXEC without MULTI";
pub const DISCARD_WITHOUT_MULTI: &[u8] = b"ERR DISCARD without MULTI";
pub const WATCH_IN_MULTI: &[u8] = b"ERR WATCH inside MULTI is not allowed";
pub const EXEC_ABORT: &[u8] = b"EXECABORT Transaction discarded because of previous errors.";

// ====== 集群错误 ======
pub const CLUSTER_DISABLED: &[u8] = b"ERR This instance has cluster support disabled";

// ====== 阻塞与取消 ======
pub const UNBLOCKED_CLIENT: &[u8] = b"UNBLOCKED client unblocked via CLIENT UNBLOCK";
