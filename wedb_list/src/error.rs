use std::result;

use thiserror::Error;

/// 列表对象错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 索引超出列表范围
  #[error("索引超出列表范围")]
  IndexOutOfRange,

  /// 缓冲区长度不足
  #[error("缓冲区长度不足")]
  BufferTooShort,

  /// 数据损坏
  #[error("数据格式损坏")]
  CorruptedData,

  /// Bitcode 编解码失败
  #[error(transparent)]
  Bitcode(#[from] bitcode::Error),
}

impl PartialEq for Error {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (Self::IndexOutOfRange, Self::IndexOutOfRange) => true,
      (Self::BufferTooShort, Self::BufferTooShort) => true,
      (Self::CorruptedData, Self::CorruptedData) => true,
      (Self::Bitcode(a), Self::Bitcode(b)) => a.to_string() == b.to_string(),
      _ => false,
    }
  }
}

impl Eq for Error {}

pub type Result<T> = result::Result<T, Error>;
