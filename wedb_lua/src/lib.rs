//! wedb_lua: Lua 脚本引擎 (对标 Garnet libs/server/Lua 与 Redis EVAL/EVALSHA/SCRIPT 命令族)
//!
//! 基于 Luau 的沙箱运行时：安全标准库、`redis.*` 脚本 API、SHA-1 脚本缓存与
//! 运行期超时强制；命令执行通过 [`wedb_module::ModuleApi`] 由宿主注入。

#![cfg_attr(docsrs, feature(doc_cfg))]

mod engine;
mod error;
mod value;

pub use engine::{DEFAULT_MEMORY_LIMIT, DEFAULT_TIMEOUT, ScriptEngine};
pub use error::{Error, Result};
pub use value::sha1_hex;
