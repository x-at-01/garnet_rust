#![cfg_attr(docsrs, feature(doc_cfg))]

mod cmd;
pub mod consts;
mod enums;
mod error;
mod len_enc;
mod parse_state;
mod read;
pub mod simd;
mod write;

pub mod cmd_hash;
pub mod script_hash;
pub use cmd::{
  FIRST_CLUSTER_SUB, FIRST_DATA_COMMAND, FIRST_NO_AUTH, FIRST_READ_COMMAND, FIRST_WRITE_COMMAND,
  LAST_CLUSTER_SUB, LAST_DATA_COMMAND, LAST_NO_AUTH, LAST_READ_COMMAND, LAST_VALID_COMMAND,
  LAST_WRITE_COMMAND, RespCommand,
};
pub use consts::resp::{
  CRLF, EMPTY_ARRAY, EMPTY_MAP, EMPTY_SET, INFINITY, INTEGER_ONE, INTEGER_ZERO, NEG_INFINITY, OK,
  PONG, POS_INFINITY, QUEUED, RESP2_NULL_ARRAY, RESP2_NULL_BULK, RESP3_FALSE, RESP3_NULL,
  RESP3_TRUE, VERBATIM_MARKDOWN, VERBATIM_TXT,
};
pub use enums::{ExistOpt, ExpirationOpt, RespCommandOpt, RespDataType};
pub use error::{Error, Result};
pub use len_enc::RespLengthEncodingUtils;
pub use parse_state::{
  INLINE_ARGS_CAPACITY, MAX_RESP_ARRAY_LENGTH, MruSlot, ParseUtils, SessionMruCache,
  SessionParseState, SessionParseStateIter, parse_session_command,
};
pub use read::RespReadUtils;
pub use script_hash::{SHA1_HEX_LEN, SHA1_RAW_LEN, ScriptHashKey};
pub use simd::{mask_for, simd_fast_parse};
pub use write::RespWriteUtils;
