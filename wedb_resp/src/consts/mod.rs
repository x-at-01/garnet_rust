//! RESP 协议与 Redis 命令常量定义（模块化命名空间，禁用 glob 泛引入）
//!
//! 推荐用法：
//! ```rust
//! use wedb_resp::consts::{cmd, err, resp};
//!
//! let _ = cmd::BLPOP;
//! let _ = err::WRONG_TYPE;
//! let _ = resp::OK;
//! ```

pub mod cmd;
pub mod err;
pub mod resp;
