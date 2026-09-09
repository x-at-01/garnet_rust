use std::{io, result};

use thiserror::Error;

/// wedb_redis 统一错误类型：包装存储引擎错误与各集合对象错误，附 Redis 语义错误变体
#[derive(Error, Debug)]
pub enum Error {
  #[error(transparent)]
  Store(#[from] wkv::Error),

  #[error(transparent)]
  Device(#[from] wdev::Error),

  #[error(transparent)]
  Epoch(#[from] wepoch::Error),

  #[error(transparent)]
  HLog(#[from] whlog::Error),

  #[error(transparent)]
  Mem(#[from] wram::Error),

  #[error(transparent)]
  BfTree(#[from] wbftree::Error),

  #[error(transparent)]
  Index(#[from] windex::Error),

  #[error(transparent)]
  Record(#[from] wrecord::Error),

  #[error(transparent)]
  Value(#[from] wval::Error),

  #[error(transparent)]
  Bitmap(#[from] crate::bitmap_simd::BitmapError),

  #[error(transparent)]
  Hash(#[from] wedb_hash::Error),

  #[error(transparent)]
  List(#[from] wedb_list::Error),

  #[error(transparent)]
  Object(#[from] wedb_object::Error),

  #[error(transparent)]
  Set(#[from] wedb_set::Error),

  #[error(transparent)]
  ZSet(#[from] wedb_zset::Error),

  #[error(transparent)]
  Hll(#[from] wedb_hll::Error),

  #[error(transparent)]
  Io(#[from] io::Error),

  #[error("value is not an integer or out of range")]
  NotInteger,

  #[error("value is not a valid float")]
  NotFloat,

  #[error("increment would produce NaN or Infinity")]
  NanOrInfinity,

  #[error("could not decode requested zset member")]
  ZSetMemberNotFound,
}

impl Error {
  /// 判断是否为数据类型不匹配（WRONGTYPE）错误
  #[inline]
  pub fn is_wrong_type(&self) -> bool {
    match self {
      Self::Store(err) => err.is_wrong_type(),
      Self::Object(err) => err.is_wrong_type(),
      Self::Value(wval::Error::InvalidCollectionType(_)) => true,
      _ => false,
    }
  }

  /// 判断是否为指定有序集合成员不存在错误
  #[inline]
  pub fn is_zset_member_not_found(&self) -> bool {
    matches!(self, Self::ZSetMemberNotFound)
  }
}

pub type Result<T> = result::Result<T, Error>;
