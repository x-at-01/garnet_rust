use core::result;

use thiserror::Error;

/// 集合对象错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 缓冲区长度不足
  #[error("缓冲区长度不足")]
  BufferTooShort,

  /// 数据损坏
  #[error("数据格式损坏")]
  CorruptedData,

  /// 不受支持的格式版本
  #[error("不支持的版本: {0}")]
  UnsupportedVersion(u8),

  /// Bitcode 编解码失败
  #[error(transparent)]
  Bitcode(#[from] bitcode::Error),
}

impl PartialEq for Error {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (Self::BufferTooShort, Self::BufferTooShort) | (Self::CorruptedData, Self::CorruptedData) => {
        true
      }
      (Self::UnsupportedVersion(a), Self::UnsupportedVersion(b)) => a == b,
      (Self::Bitcode(a), Self::Bitcode(b)) => a.to_string() == b.to_string(),
      _ => false,
    }
  }
}

impl Eq for Error {}

pub type Result<T> = result::Result<T, Error>;
