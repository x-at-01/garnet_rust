use std::result;

use thiserror::Error;

/// 发布订阅模块统一错误定义
#[derive(Error, Debug)]
pub enum Error {
  /// RESP 协议编解码错误
  #[error(transparent)]
  Resp(#[from] wedb_resp::Error),

  /// Bitcode 编解码错误
  #[error("Bitcode 编解码失败: {0}")]
  Bitcode(String),
}

/// 发布订阅模块标准 Result 类型
pub type Result<T> = result::Result<T, Error>;
