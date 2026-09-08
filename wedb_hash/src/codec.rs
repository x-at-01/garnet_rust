use core::{borrow::Borrow, ops::Deref};

use crate::error::{Error, Result};

/// 字段值无过期时间编码标识 (0x00)
pub const TAG_NO_EXPIRE: u8 = 0x00;

/// 字段值含绝对过期时间戳编码标识 (0x01)
pub const TAG_WITH_EXPIRE: u8 = 0x01;

/// 无过期时间头部长度 (1 字节)
pub const NO_EXPIRE_HEADER_LEN: usize = 1;

/// 含过期时间头部长度 (1 字节标识 + 8 字节大端时间戳)
pub const EXPIRE_HEADER_LEN: usize = 9;

/// 字段值优先栈分配容量上限 (73 字节 = 9 字节头 + 64 字节载荷)
pub const FIELD_VALUE_STACK_CAP: usize = 73;

/// 定长 73 字节字段值编码缓冲区（优先栈分配消除堆分配，超长自动回退至堆）
///
/// 73 字节支持：9 字节定长前缀头（1 字节标志 + 8 字节过期时间）+ 64 字节载荷，
/// 完美覆盖 >90% 的 Redis 哈希常规字段值，避免每次写入都触发堆内存分配。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValueBuf {
  /// 栈上分配（编码总字节数 <= 73）
  Stack([u8; FIELD_VALUE_STACK_CAP], u8),
  /// 堆分配（长字段值回退）
  Heap(Vec<u8>),
}

impl Deref for FieldValueBuf {
  type Target = [u8];

  #[inline(always)]
  fn deref(&self) -> &Self::Target {
    match self {
      Self::Stack(buf, len) => &buf[..*len as usize],
      Self::Heap(vec) => vec.as_slice(),
    }
  }
}

impl AsRef<[u8]> for FieldValueBuf {
  #[inline(always)]
  fn as_ref(&self) -> &[u8] {
    self.deref()
  }
}

impl Borrow<[u8]> for FieldValueBuf {
  #[inline(always)]
  fn borrow(&self) -> &[u8] {
    self.deref()
  }
}

impl Default for FieldValueBuf {
  #[inline(always)]
  fn default() -> Self {
    Self::Stack([0u8; FIELD_VALUE_STACK_CAP], 0)
  }
}

impl FieldValueBuf {
  /// 获取只读字节切片借用（零拷贝）
  #[inline(always)]
  pub fn as_slice(&self) -> &[u8] {
    self.deref()
  }

  /// 转换为拥有所有权的字节向量（堆分配分支零拷贝转移）
  #[inline]
  pub fn into_vec(self) -> Vec<u8> {
    match self {
      Self::Stack(buf, len) => buf[..len as usize].to_vec(),
      Self::Heap(vec) => vec,
    }
  }
}

/// 哈希字段值编解码器（支持绝对时间戳过期机制与零拷贝借用）
pub struct FieldValueCodec;

impl FieldValueCodec {
  /// 编码总长 = 头部 (无过期 1 字节 / 含过期 9 字节) + 载荷长度
  #[inline]
  const fn encoded_len(val_len: usize, with_expire: bool) -> usize {
    if with_expire {
      EXPIRE_HEADER_LEN + val_len
    } else {
      NO_EXPIRE_HEADER_LEN + val_len
    }
  }

  /// 优先栈分配编码字段值与可选绝对过期时间戳
  #[inline]
  pub fn encode_buf(val: &[u8], expire_at_ms: Option<u64>) -> FieldValueBuf {
    let total_len = Self::encoded_len(val.len(), expire_at_ms.is_some());
    if total_len <= FIELD_VALUE_STACK_CAP {
      let mut buf = [0u8; FIELD_VALUE_STACK_CAP];
      match expire_at_ms {
        None => {
          buf[0] = TAG_NO_EXPIRE;
          buf[NO_EXPIRE_HEADER_LEN..total_len].copy_from_slice(val);
        }
        Some(exp) => {
          buf[0] = TAG_WITH_EXPIRE;
          buf[1..EXPIRE_HEADER_LEN].copy_from_slice(&exp.to_be_bytes());
          buf[EXPIRE_HEADER_LEN..total_len].copy_from_slice(val);
        }
      }
      FieldValueBuf::Stack(buf, total_len as u8)
    } else {
      FieldValueBuf::Heap(Self::encode(val, expire_at_ms))
    }
  }

  /// 编码字段值与可选绝对过期时间戳 (毫秒)
  ///
  /// 编码格式：
  /// - 无过期时间：`[0x00 | val]`
  /// - 有过期时间：`[0x01 | expire_at_ms: 8 字节大端 | val]`
  #[inline]
  pub fn encode(val: &[u8], expire_at_ms: Option<u64>) -> Vec<u8> {
    let mut buf = Vec::with_capacity(Self::encoded_len(val.len(), expire_at_ms.is_some()));
    Self::encode_to_buf(val, expire_at_ms, &mut buf);
    buf
  }

  /// 编码字段值至现有缓冲区（重用内存避免多余分配）
  #[inline]
  pub fn encode_to_buf(val: &[u8], expire_at_ms: Option<u64>, buf: &mut Vec<u8>) {
    buf.clear();
    let needed = Self::encoded_len(val.len(), expire_at_ms.is_some());
    if buf.capacity() < needed {
      buf.reserve(needed - buf.capacity());
    }
    match expire_at_ms {
      None => {
        buf.push(TAG_NO_EXPIRE);
        buf.extend_from_slice(val);
      }
      Some(expire_at) => {
        buf.push(TAG_WITH_EXPIRE);
        buf.extend_from_slice(&expire_at.to_be_bytes());
        buf.extend_from_slice(val);
      }
    }
  }

  /// 零拷贝借用解码字段值与绝对过期时间戳 (毫秒)（const fn）
  ///
  /// 若切片长度不足或标识位非法，返回相应错误。
  #[inline]
  pub const fn decode(slice: &[u8]) -> Result<(Option<u64>, &[u8])> {
    if slice.is_empty() {
      return Err(Error::BufferTooShort);
    }

    match slice[0] {
      TAG_NO_EXPIRE => {
        let (_, val) = slice.split_at(NO_EXPIRE_HEADER_LEN);
        Ok((None, val))
      }
      TAG_WITH_EXPIRE => {
        if slice.len() < EXPIRE_HEADER_LEN {
          return Err(Error::BufferTooShort);
        }
        let expire_at = u64::from_be_bytes([
          slice[1], slice[2], slice[3], slice[4], slice[5], slice[6], slice[7], slice[8],
        ]);
        let (_, val) = slice.split_at(EXPIRE_HEADER_LEN);
        Ok((Some(expire_at), val))
      }
      _ => Err(Error::CorruptedData),
    }
  }
}

/// 字段索引分块编解码器（用于 KeyTag::HashChunk = 0x07）
pub type FieldChunkCodec = wrecord::ChunkCodec;

/// 字段分块只读流式零拷贝迭代器
pub type FieldChunkIter<'a> = wrecord::ChunkIter<'a>;
