use core::result;

use thiserror::Error;

/// 有序集合对象错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 分数不是有效的数值 (NaN)
  #[error("分数不是有效数值")]
  InvalidScore,

  /// 坐标超出有效范围 (-180..=180, -90..=90)
  #[error("经纬度坐标超出有效范围")]
  InvalidCoordinates,

  /// 指定的成员不存在
  #[error("指定的成员不存在")]
  MemberNotFound,

  /// 距离或形状尺寸必须大于等于 0
  #[error("距离必须为非负数")]
  InvalidDistance,

  /// 选项互斥或无效
  #[error("参数或选项冲突")]
  InvalidOpt,

  /// 缓冲区长度不足
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

  // Record 编解码错误
  #[error(transparent)]
  Record(#[from] wrecord::Error),

  // 值层编解码错误
  #[error(transparent)]
  Value(#[from] wval::Error),
}

impl PartialEq for Error {
  fn eq(&self, other: &Self) -> bool {
    match (self, other) {
      (Self::InvalidScore, Self::InvalidScore)
      | (Self::InvalidCoordinates, Self::InvalidCoordinates)
      | (Self::MemberNotFound, Self::MemberNotFound)
      | (Self::InvalidDistance, Self::InvalidDistance)
      | (Self::InvalidOpt, Self::InvalidOpt)
      | (Self::BufferTooShort, Self::BufferTooShort)
      | (Self::CorruptedData, Self::CorruptedData) => true,
      (Self::UnsupportedVersion(a), Self::UnsupportedVersion(b)) => a == b,
      (Self::Bitcode(a), Self::Bitcode(b)) => a.to_string() == b.to_string(),
      (Self::Record(a), Self::Record(b)) => a.to_string() == b.to_string(),
      (Self::Value(a), Self::Value(b)) => a.to_string() == b.to_string(),
      _ => false,
    }
  }
}

impl Eq for Error {}

pub type Result<T> = result::Result<T, Error>;
