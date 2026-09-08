use std::result;

use thiserror::Error;

/// VectorSet 错误类型 (对标 Redis Vector Set 命令错误语义)
#[derive(Error, Debug)]
pub enum Error {
  /// 元素不存在
  #[error("ERR element not found")]
  ElementNotFound,

  /// 维度不匹配
  #[error("ERR vector dimension mismatch: expected {expected}, got {got}")]
  DimMismatch { expected: usize, got: usize },

  /// 向量维度非法
  #[error("ERR invalid vector dimension: {0}")]
  InvalidDims(usize),

  /// 向量含 NaN/Inf 等非有限值
  #[error("ERR invalid vector data")]
  InvalidVector,
}

pub type Result<T> = result::Result<T, Error>;
