use std::result;

use thiserror::Error;

/// 统一堆对象错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 哈希对象错误
  #[cfg(feature = "wedb_hash")]
  #[error(transparent)]
  Hash(#[from] wedb_hash::Error),

  /// 列表对象错误
  #[cfg(feature = "wedb_list")]
  #[error(transparent)]
  List(#[from] wedb_list::Error),

  /// 集合对象错误
  #[cfg(feature = "wedb_set")]
  #[error(transparent)]
  Set(#[from] wedb_set::Error),

  /// 有序集合对象错误
  #[cfg(feature = "wedb_zset")]
  #[error(transparent)]
  ZSet(#[from] wedb_zset::Error),

  /// 空对象错误
  #[error("空对象")]
  NullObject,

  /// 缓冲区提前结束
  #[error("缓冲区提前结束")]
  UnexpectedEof,

  /// 不支持的对象序列化格式标识
  #[error("不支持的对象序列化格式标识 0x{0:02X}")]
  UnsupportedFormatMarker(u8),

  /// 未知或不支持的对象类型 ID
  #[error("未知或不支持的对象类型 ID {0}")]
  UnknownObjectType(u8),

  /// 数据类型不匹配错误 (WRONGTYPE)
  #[error("WRONGTYPE Operation against a key holding the wrong kind of value")]
  WrongType,
}

impl Error {
  /// 判断是否属于对象类型不匹配错误 (WRONGTYPE)
  ///
  /// 与存储层 `load_object` 口径一致：任何反序列化失败（损坏/截断/未知类型）
  /// 在 TYPE 探测与 RESP 错误映射中均按 WRONGTYPE 归类处理。
  #[inline]
  pub fn is_wrong_type(&self) -> bool {
    matches!(
      self,
      Self::WrongType
        | Self::UnknownObjectType(_)
        | Self::UnsupportedFormatMarker(_)
        | Self::NullObject
        | Self::UnexpectedEof
    )
  }

  /// 判断是否因缓冲区截断提早结束
  #[inline]
  pub fn is_unexpected_eof(&self) -> bool {
    matches!(self, Self::UnexpectedEof)
  }

  /// 判断是否为空对象错误
  #[inline]
  pub fn is_null(&self) -> bool {
    matches!(self, Self::NullObject)
  }
}

pub type Result<T> = result::Result<T, Error>;
