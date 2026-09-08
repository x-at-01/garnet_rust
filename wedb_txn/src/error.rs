use std::result;

use thiserror::Error;

/// 事务处理引擎错误枚举
#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum Error {
  /// 事务已开启时重复开启事务错误
  #[error("ERR MULTI calls can not be nested")]
  NestedMulti,

  /// 未开启事务即执行提交
  #[error("ERR EXEC without MULTI")]
  ExecWithoutMulti,

  /// 未开启事务即尝试入队命令
  #[error("ERR cannot queue command without MULTI")]
  QueueWithoutMulti,

  /// 未开启事务即执行丢弃
  #[error("ERR DISCARD without MULTI")]
  DiscardWithoutMulti,

  /// 在事务内部执行监视键命令
  #[error("ERR WATCH inside MULTI is not allowed")]
  WatchInsideMulti,

  /// 事务处于中止状态，执行失败
  #[error("EXECABORT Transaction discarded because of previous errors.")]
  TransactionAborted,

  /// 乐观锁版本冲突，受监视键已被其他并发事务修改
  #[error("ERR transaction conflict: watched keys were modified")]
  WatchConflict,

  /// 命令在事务中被禁止执行
  #[error("ERR command '{0}' not allowed in transaction")]
  CommandNotAllowedInTxn(&'static str),

  /// 获取键互斥或共享锁超时
  #[error("ERR key lock acquisition timed out")]
  LockTimeout,

  /// bitcode 编解码失败
  #[error("bitcode 编解码失败: {0}")]
  Bitcode(String),
}

impl From<bitcode::Error> for Error {
  fn from(err: bitcode::Error) -> Self {
    Self::Bitcode(err.to_string())
  }
}

/// 事务引擎通用返回别名
pub type Result<T> = result::Result<T, Error>;
