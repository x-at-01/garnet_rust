use std::{io, result};

use thiserror::Error;

#[derive(Error, Debug)]
pub enum Error {
  /// Redis 命令层错误
  #[error(transparent)]
  Redis(#[from] wedb_redis::Error),

  #[error(transparent)]
  Wedb(#[from] wkv::Error),

  #[error(transparent)]
  Device(#[from] wdev::Error),

  #[error(transparent)]
  Hlog(#[from] whlog::Error),

  #[error(transparent)]
  Fjall(#[from] fjall::Error),

  #[error(transparent)]
  BfTree(#[from] wbftree::Error),

  #[error("BfTree 插入失败: {0:?}")]
  BfTreeInsert(wbftree::BfTreeInsertResult),

  #[error(transparent)]
  RocksDb(#[from] rocksdb::Error),

  #[error("并发评测线程异常退出")]
  ThreadPanic,

  #[error(transparent)]
  Io(#[from] io::Error),
}

pub type Result<T> = result::Result<T, Error>;
