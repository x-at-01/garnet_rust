//! RESP 协议通用响应帧与格式标记常量定义（与 Microsoft Garnet RespStrings 保持一致）

use hipstr::HipStr;

/// 换行符 `\r\n`
pub const CRLF: &[u8] = b"\r\n";

/// 成功应答 `+OK\r\n`
pub const OK_STR: &str = "+OK\r\n";
pub const OK: &[u8] = OK_STR.as_bytes();
pub const OK_HIPSTR: HipStr<'static> = HipStr::from_static(OK_STR);

/// PING 应答 `+PONG\r\n`
pub const PONG_STR: &str = "+PONG\r\n";
pub const PONG: &[u8] = PONG_STR.as_bytes();
pub const PONG_HIPSTR: HipStr<'static> = HipStr::from_static(PONG_STR);

/// 事务入队应答 `+QUEUED\r\n`
pub const QUEUED_STR: &str = "+QUEUED\r\n";
pub const QUEUED: &[u8] = QUEUED_STR.as_bytes();
pub const QUEUED_HIPSTR: HipStr<'static> = HipStr::from_static(QUEUED_STR);

/// RESP2 空字符串 `$-1\r\n`
pub const NULL_BULK: &[u8] = b"$-1\r\n";
pub const RESP2_NULL_BULK: &[u8] = NULL_BULK;

/// RESP2 空数组 `*-1\r\n`
pub const NULL_ARRAY: &[u8] = b"*-1\r\n";
pub const RESP2_NULL_ARRAY: &[u8] = NULL_ARRAY;

/// 空数组 `*0\r\n`
pub const EMPTY_ARRAY: &[u8] = b"*0\r\n";

/// 空集合 `~0\r\n`
pub const EMPTY_SET: &[u8] = b"~0\r\n";

/// 空字典 `%0\r\n`
pub const EMPTY_MAP: &[u8] = b"%0\r\n";

/// 整数 0 常量 `:0\r\n`
pub const INTEGER_ZERO: &[u8] = b":0\r\n";

/// 整数 1 常量 `:1\r\n`
pub const INTEGER_ONE: &[u8] = b":1\r\n";

/// RESP3 空值 `_\r\n`
pub const RESP3_NULL: &[u8] = b"_\r\n";

/// RESP3 布尔 True `#t\r\n`
pub const RESP3_TRUE: &[u8] = b"#t\r\n";

/// RESP3 布尔 False `#f\r\n`
pub const RESP3_FALSE: &[u8] = b"#f\r\n";

/// 无穷大常量字符串 `INF`
pub const INFINITY: &[u8] = b"INF";

/// 正无穷大常量字符串 `+INF`
pub const POS_INFINITY: &[u8] = b"+INF";

/// 负无穷大常量字符串 `-INF`
pub const NEG_INFINITY: &[u8] = b"-INF";

/// 原样字符串 Markdown 格式标记 `mkd`
pub const VERBATIM_MARKDOWN: &[u8] = b"mkd";

/// 原样字符串纯文本格式标记 `txt`
pub const VERBATIM_TXT: &[u8] = b"txt";
