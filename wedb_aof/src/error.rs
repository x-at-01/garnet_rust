use std::result;

use thiserror::Error;

/// AOF 应用层错误枚举
#[derive(Error, Debug)]
pub enum Error {
  /// 底层 waof 日志引擎错误（含包装的设备与内存错误）
  #[error(transparent)]
  Waof(#[from] waof::Error),

  /// 块存储底层设备错误
  #[error(transparent)]
  Device(#[from] wdev::Error),

  /// 存储引擎错误（重放会话创建）
  #[error(transparent)]
  Store(#[from] wkv::Error),

  /// AOF 效果帧格式错误
  #[error("AOF 效果帧格式错误: {0}")]
  Frame(String),
}

/// AOF 操作结果类型别名
pub type Result<T> = result::Result<T, Error>;
