use std::result;

use thiserror::Error;

/// 哈希对象错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 无法解析为有效整数或浮点数
  #[error("数值格式错误或超出范围")]
  InvalidNumber,

  /// 序列化缓冲区截断
  #[error("缓冲区长度不足")]
  BufferTooShort,

  /// 数据损坏
  #[error("数据格式损坏")]
  CorruptedData,

  /// 不受支持的格式版本
  #[error("不支持的版本: {0}")]
  UnsupportedVersion(u8),

  /// Bitcode 编解码错误
  #[error(transparent)]
  Bitcode(#[from] bitcode::Error),
}

impl PartialEq for Error {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (Self::InvalidNumber, Self::InvalidNumber) => true,
      (Self::BufferTooShort, Self::BufferTooShort) => true,
      (Self::CorruptedData, Self::CorruptedData) => true,
      (Self::UnsupportedVersion(v1), Self::UnsupportedVersion(v2)) => v1 == v2,
      (Self::Bitcode(e1), Self::Bitcode(e2)) => e1.to_string() == e2.to_string(),
      _ => false,
    }
  }
}

impl Eq for Error {}

pub type Result<T> = result::Result<T, Error>;
