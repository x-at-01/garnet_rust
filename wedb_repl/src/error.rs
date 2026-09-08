use std::{io, result};

use thiserror::Error;

use crate::role::{NodeRole, RecoveryStatus};

/// 复制引擎专有错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 输入输出错误
  #[error(transparent)]
  Io(#[from] io::Error),

  /// 下层预写日志引擎错误
  #[error(transparent)]
  Wal(#[from] waof::Error),

  /// 下层存储引擎错误
  #[error(transparent)]
  Store(#[from] wkv::Error),

  /// Redis 命令层错误
  #[error(transparent)]
  Redis(#[from] wedb_redis::Error),

  /// 无效的复制历史版本号
  #[error("无效的复制历史版本: 期望 {expected}, 实际 {actual}")]
  InvalidVersion { expected: u8, actual: u8 },

  /// 复制历史数据长度不合法
  #[error("无效的复制历史数据长度: 期望至少 {expected}, 实际 {actual}")]
  InvalidDataLength { expected: usize, actual: usize },

  /// 无效的复制编号
  #[error("无效的复制编号: {0}")]
  InvalidReplId(String),

  /// 当前节点不是主节点
  #[error("当前节点非主节点: 角色为 {0:?}")]
  NotPrimary(NodeRole),

  /// 复制与恢复状态冲突或非法
  #[error("非法复制状态转换: 当前为 {current:?}, 尝试转为 {target:?}")]
  InvalidStateTransition {
    current: RecoveryStatus,
    target: RecoveryStatus,
  },

  /// 复制协议语法或解析错误
  #[error("复制协议错误: {0}")]
  Protocol(String),

  /// 主从握手失败
  #[error("主从握手失败: {0}")]
  HandshakeFailed(String),

  /// 主从网络连接已关闭
  #[error("主从网络连接已关闭")]
  ConnectionClosed,

  /// 主节点身份认证失败
  #[error("主节点身份认证失败: {0}")]
  AuthFailed(String),

  /// 日志或快照回放错误
  #[error("回放失败: {0}")]
  ReplayFailed(String),

  /// 对象已被释放
  #[error("对象已释放: {0}")]
  ObjectDisposed(String),
}

pub type Result<T> = result::Result<T, Error>;
