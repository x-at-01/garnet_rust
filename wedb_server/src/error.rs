use std::{io, net::AddrParseError, result};

use thiserror::Error;

/// WeDB 顶层服务端统一错误枚举
#[derive(Error, Debug)]
pub enum Error {
  /// 输入输出系统底层错误
  #[error(transparent)]
  Io(#[from] io::Error),

  /// 混合日志存储引擎错误
  #[error(transparent)]
  Store(#[from] wkv::Error),

  #[error(transparent)]
  Redis(#[from] wedb_redis::Error),

  /// 检查点快照与崩溃恢复错误
  #[error(transparent)]
  Checkpoint(#[from] wcpr::Error),

  /// 块存储底层设备错误
  #[error(transparent)]
  Device(#[from] wdev::Error),

  /// 访问控制列表错误
  #[error(transparent)]
  Acl(#[from] wedb_acl::Error),

  /// 协议编解码错误
  #[error(transparent)]
  Resp(#[from] wedb_resp::Error),

  /// 主从复制错误
  #[error(transparent)]
  Repl(#[from] wedb_repl::Error),

  /// AOF 追加日志应用层错误
  #[error(transparent)]
  Aof(#[from] wedb_aof::Error),

  /// AOF 效果帧格式错误
  #[error("AOF 效果帧格式错误: {0}")]
  AofFrame(String),

  /// 分布式集群错误
  #[error(transparent)]
  Cluster(#[from] wedb_cluster::Error),

  /// 地址解析错误
  #[error(transparent)]
  AddrParse(#[from] AddrParseError),

  /// 服务已关闭
  #[error("服务已关闭")]
  ServerClosed,

  /// 自定义错误
  #[error("{0}")]
  Custom(String),
}

impl Error {
  /// 判断是否为数据类型不匹配（WRONGTYPE）错误
  #[inline]
  pub fn is_wrong_type(&self) -> bool {
    match self {
      Self::Store(s) => s.is_wrong_type(),
      Self::Redis(s) => s.is_wrong_type(),
      _ => false,
    }
  }
}

/// 服务端统一结果类型
pub type Result<T> = result::Result<T, Error>;
