#![cfg_attr(docsrs, feature(doc_cfg))]

//! WeDB AOF 追加日志应用层（对标 C# Garnet 的 `GarnetLog` 门面 + `AofProcessor`）
//!
//! 三层职责拆分：
//! - [`frame`]：效果帧家族编解码（帧类型化，对标 `AofEntryType`）
//! - [`AofReplayer`]：帧 → 存储面应用，恢复与复制重放共用单一真源
//! - [`AofLog`]：日志生命周期门面（写监听适配、提交策略、checkpoint 截断）

mod error;
mod facade;
mod frame;
mod replayer;

pub use error::{Error, Result};
pub use facade::{AOF_FILE_NAME, AofLog, DEFAULT_AOF_BUFFER_SIZE};
pub use frame::{AofOp, decode_frame, encode_frame};
pub use replayer::AofReplayer;
