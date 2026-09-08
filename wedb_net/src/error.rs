use std::{io, result};

use thiserror::Error;
use wedb_acl::Error as AclError;
use wedb_txn::Error as TxnError;
use wkv::Error as StoreError;

/// 网络层统一错误定义
#[derive(Error, Debug)]
pub enum Error {
  /// 底层网络输入输出错误
  #[error("网络 I/O 错误: {0}")]
  Io(#[from] io::Error),

  /// 存储引擎层返回错误
  #[error("存储引擎错误: {0}")]
  Store(#[from] StoreError),

  /// Redis 命令层错误
  #[error("Redis 命令错误: {0}")]
  Redis(#[from] wedb_redis::Error),

  /// 访问控制列表权限错误
  #[error("ACL 权限错误: {0}")]
  Acl(#[from] AclError),

  /// 事务管理器错误
  #[error("事务错误: {0}")]
  Txn(#[from] TxnError),

  /// 服务端已被关闭或正在终止
  #[error("服务端已关闭")]
  ServerClosed,

  /// 接收缓冲区溢出
  #[error("接收缓冲区溢出: 当前大小 {0} 超过上限 {1}")]
  BufferOverflow(usize, usize),
}

/// 网络层统一结果类型
pub type Result<T> = result::Result<T, Error>;
