use std::result;

use thiserror::Error;
use wedb_resp::RespCommand;

/// 阻塞调度模块错误枚举
#[derive(Error, Debug)]
pub enum Error {
  /// 数据类型不匹配错误 (WRONGTYPE)
  #[error("集合类型不匹配 (WRONGTYPE Operation against a key holding the wrong kind of value)")]
  WrongType,

  /// 不支持的阻塞命令
  #[error("不支持的阻塞命令: {0:?}")]
  UnsupportedCommand(RespCommand),

  /// 无效的命令参数
  #[error("无效参数: {0}")]
  InvalidArgument(&'static str),
}

/// 阻塞调度模块通用结果类型
pub type Result<T> = result::Result<T, Error>;
