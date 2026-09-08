use crate::error::{Error, Result};

/// 过期时间选项枚举，与 Garnet ExpirationOpt 保持一致
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum ExpirationOpt {
  /// 未设置过期选项
  #[default]
  None = 0,
  /// 秒级相对过期时间
  EX = 1,
  /// 毫秒级相对过期时间
  PX = 2,
  /// 秒级绝对时间戳过期时间
  EXAT = 3,
  /// 毫秒级绝对时间戳过期时间
  PXAT = 4,
  /// 保持原有生存时间
  KEEPTTL = 5,
}

impl ExpirationOpt {
  /// 从字节切片解析（忽略大小写）
  #[inline]
  pub fn from_bytes(slice: &[u8]) -> Self {
    match slice.len() {
      2 => {
        if slice.eq_ignore_ascii_case(b"EX") {
          Self::EX
        } else if slice.eq_ignore_ascii_case(b"PX") {
          Self::PX
        } else {
          Self::None
        }
      }
      4 => {
        if slice.eq_ignore_ascii_case(b"EXAT") {
          Self::EXAT
        } else if slice.eq_ignore_ascii_case(b"PXAT") {
          Self::PXAT
        } else {
          Self::None
        }
      }
      7 => {
        if slice.eq_ignore_ascii_case(b"KEEPTTL") {
          Self::KEEPTTL
        } else {
          Self::None
        }
      }
      _ => Self::None,
    }
  }

  /// 转换为对应字符串表示
  #[inline]
  pub const fn as_str(&self) -> &'static str {
    match self {
      Self::None => "",
      Self::EX => "EX",
      Self::PX => "PX",
      Self::EXAT => "EXAT",
      Self::PXAT => "PXAT",
      Self::KEEPTTL => "KEEPTTL",
    }
  }
}

/// 键存在性选项枚举，与 Garnet ExistOpt 保持一致
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[repr(u8)]
pub enum ExistOpt {
  /// 未设置存在性选项
  #[default]
  None = 0,
  /// 仅当键不存在时设置 (Not eXists)
  NX = 1,
  /// 仅当键已存在时设置 (eXists)
  XX = 2,
}

impl ExistOpt {
  /// 从字节切片解析（忽略大小写）
  #[inline]
  pub fn from_bytes(slice: &[u8]) -> Self {
    if slice.len() == 2 {
      if slice.eq_ignore_ascii_case(b"NX") {
        Self::NX
      } else if slice.eq_ignore_ascii_case(b"XX") {
        Self::XX
      } else {
        Self::None
      }
    } else {
      Self::None
    }
  }

  /// 转换为对应字符串表示
  #[inline]
  pub const fn as_str(&self) -> &'static str {
    match self {
      Self::None => "",
      Self::NX => "NX",
      Self::XX => "XX",
    }
  }
}

/// RESP 命令修饰选项枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RespCommandOpt {
  /// 秒级过期
  EX = 0,
  /// 仅不存在时写入
  NX = 1,
  /// 仅存在时写入
  XX = 2,
  /// 获取旧值
  GET = 3,
  /// 毫秒级过期
  PX = 4,
  /// 秒级绝对时间戳过期
  EXAT = 5,
  /// 毫秒级绝对时间戳过期
  PXAT = 6,
  /// 移除过期时间
  PERSIST = 7,
  /// 大于当前值时设置 (Greater Than)
  GT = 8,
  /// 小于当前值时设置 (Less Than)
  LT = 9,
}

impl RespCommandOpt {
  /// 从字节切片解析（忽略大小写）
  #[inline]
  pub fn from_bytes(slice: &[u8]) -> Option<Self> {
    match slice.len() {
      2 => {
        if slice.eq_ignore_ascii_case(b"EX") {
          Some(Self::EX)
        } else if slice.eq_ignore_ascii_case(b"NX") {
          Some(Self::NX)
        } else if slice.eq_ignore_ascii_case(b"XX") {
          Some(Self::XX)
        } else if slice.eq_ignore_ascii_case(b"PX") {
          Some(Self::PX)
        } else if slice.eq_ignore_ascii_case(b"GT") {
          Some(Self::GT)
        } else if slice.eq_ignore_ascii_case(b"LT") {
          Some(Self::LT)
        } else {
          None
        }
      }
      3 => {
        if slice.eq_ignore_ascii_case(b"GET") {
          Some(Self::GET)
        } else {
          None
        }
      }
      4 => {
        if slice.eq_ignore_ascii_case(b"EXAT") {
          Some(Self::EXAT)
        } else if slice.eq_ignore_ascii_case(b"PXAT") {
          Some(Self::PXAT)
        } else {
          None
        }
      }
      7 => {
        if slice.eq_ignore_ascii_case(b"PERSIST") {
          Some(Self::PERSIST)
        } else {
          None
        }
      }
      _ => None,
    }
  }
}

/// RESP 协议数据类型前缀标识符
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RespDataType {
  /// 简单字符串 `+`
  SimpleString = b'+',
  /// 错误 `-`
  Error = b'-',
  /// 整数 `:`
  Integer = b':',
  /// 定长字符串 `$`
  BulkString = b'$',
  /// 数组 `*`
  Array = b'*',
  /// 空值 `_` (RESP3)
  Null = b'_',
  /// 布尔值 `#` (RESP3)
  Boolean = b'#',
  /// 浮点数 `,` (RESP3)
  Double = b',',
  /// 大整数 `(` (RESP3)
  BigNumber = b'(',
  /// 定长错误 `!` (RESP3)
  BulkError = b'!',
  /// 原样字符串 `=` (RESP3)
  VerbatimString = b'=',
  /// 字典 `%` (RESP3)
  Map = b'%',
  /// 集合 `~` (RESP3)
  Set = b'~',
  /// 推送类型 `>` (RESP3)
  Push = b'>',
}

impl RespDataType {
  /// 从前缀字节转换
  pub const fn from_byte(byte: u8) -> Option<Self> {
    match byte {
      b'+' => Some(Self::SimpleString),
      b'-' => Some(Self::Error),
      b':' => Some(Self::Integer),
      b'$' => Some(Self::BulkString),
      b'*' => Some(Self::Array),
      b'_' => Some(Self::Null),
      b'#' => Some(Self::Boolean),
      b',' => Some(Self::Double),
      b'(' => Some(Self::BigNumber),
      b'!' => Some(Self::BulkError),
      b'=' => Some(Self::VerbatimString),
      b'%' => Some(Self::Map),
      b'~' => Some(Self::Set),
      b'>' => Some(Self::Push),
      _ => None,
    }
  }

  /// 获取对应的前缀字符字节
  #[inline]
  pub const fn as_byte(&self) -> u8 {
    *self as u8
  }
}

impl TryFrom<u8> for RespDataType {
  type Error = Error;

  #[inline]
  fn try_from(byte: u8) -> Result<Self> {
    Self::from_byte(byte).ok_or(Error::UnexpectedToken(byte))
  }
}
