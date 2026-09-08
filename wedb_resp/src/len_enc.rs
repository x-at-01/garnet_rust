use crate::error::{Error, Result};

/// RESP 紧凑长度编码工具，对齐 Microsoft Garnet `RespLengthEncodingUtils`
/// 支持 6 位、14 位以及 32 位大端紧凑格式
pub struct RespLengthEncodingUtils;

impl RespLengthEncodingUtils {
  /// 可编码的最大长度 (24位无符号上限 0xFFFFFF)
  pub const MAX_LENGTH: usize = 0xFF_FF_FF;

  /// 尝试从输入切片中解码 RESP 紧凑长度
  ///
  /// 返回 `Some((length, bytes_read))`，如果输入不完整或格式非法则返回 `None`
  #[inline]
  pub fn try_read_length(input: &[u8]) -> Option<(usize, usize)> {
    let mut cursor = input;
    let len = Self::read_length(&mut cursor).ok()?;
    let bytes_read = input.len() - cursor.len();
    Some((len, bytes_read))
  }

  /// 尝试从输入切片指针推进读取长度
  #[inline]
  pub fn read_length(input: &mut &[u8]) -> Result<usize> {
    let (&first_byte, rest) = input.split_first().ok_or(Error::Incomplete)?;
    match first_byte >> 6 {
      0 => {
        *input = rest;
        Ok((first_byte & 0x3F) as usize)
      }
      1 => {
        let (&second_byte, rest2) = rest.split_first().ok_or(Error::Incomplete)?;
        *input = rest2;
        Ok((((first_byte & 0x3F) as usize) << 8) | (second_byte as usize))
      }
      2 => {
        if rest.len() < 4 {
          return Err(Error::Incomplete);
        }
        // SAFETY: rest.len() >= 4 已通过检查
        let bytes = unsafe { *(rest.as_ptr() as *const [u8; 4]) };
        *input = unsafe { rest.get_unchecked(4..) };
        // 读取标记字节之后的 4 字节大端值；刻意修复 C# 版 `BinaryPrimitives.TryReadInt32BigEndian(input)`
        // 误读入标记字节导致长度恒为负、16KB 以上长度无法往返的缺陷
        let val = u32::from_be_bytes(bytes) as usize;
        if val > Self::MAX_LENGTH {
          return Err(Error::InvalidLength(val as i64));
        }
        Ok(val)
      }
      // 标记字节高 2 位为 11：非法前缀
      _ => Err(Error::InvalidLength(-1)),
    }
  }

  /// 尝试将长度紧凑写入目标缓冲区
  ///
  /// 返回写入的字节数，若缓冲区空间不足或长度超过上限则返回 `None`
  #[inline]
  pub fn try_write_length(length: usize, output: &mut [u8]) -> Option<usize> {
    if length > Self::MAX_LENGTH {
      return None;
    }

    // 6 位编码 (length <= 63)
    if length < (1 << 6) {
      if output.is_empty() {
        return None;
      }
      output[0] = (length as u8) & 0x3F;
      return Some(1);
    }

    // 14 位编码 (64 <= length <= 16,383)
    if length < (1 << 14) {
      if output.len() < 2 {
        return None;
      }
      output[0] = (((length >> 8) as u8) & 0x3F) | (1 << 6);
      output[1] = length as u8;
      return Some(2);
    }

    // 32 位大端编码 (16,384 <= length <= 16,777,215)
    if output.len() < 5 {
      return None;
    }
    output[0] = 2 << 6;
    output[1..5].copy_from_slice(&(length as u32).to_be_bytes());
    Some(5)
  }

  /// 将长度紧凑编码写入目标切片，空间不足返回 `Error::BufferTooSmall`
  #[inline]
  pub fn write_length(length: usize, output: &mut [u8]) -> Result<usize> {
    if let Some(n) = Self::try_write_length(length, output) {
      Ok(n)
    } else if length > Self::MAX_LENGTH {
      Err(Error::InvalidLength(length as i64))
    } else {
      Err(Error::BufferTooSmall)
    }
  }

  /// 向 `Vec<u8>` 追加紧凑编码长度，预先精确 reserve 消除多次扩容
  #[inline]
  pub fn push_length(buf: &mut Vec<u8>, length: usize) -> Result<usize> {
    if length > Self::MAX_LENGTH {
      return Err(Error::InvalidLength(length as i64));
    }
    if length < (1 << 6) {
      buf.reserve(1);
      buf.push((length as u8) & 0x3F);
      Ok(1)
    } else if length < (1 << 14) {
      buf.reserve(2);
      buf.push((((length >> 8) as u8) & 0x3F) | (1 << 6));
      buf.push(length as u8);
      Ok(2)
    } else {
      buf.reserve(5);
      buf.push(2 << 6);
      buf.extend_from_slice(&(length as u32).to_be_bytes());
      Ok(5)
    }
  }
}
