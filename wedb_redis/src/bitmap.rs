use wdev::Device;
use wkv::StoreSession;

use super::*;
use crate::{
  bitmap_simd::{
    BitmapError, BitmapOp, OFFSET_TYPE_BIT, OFFSET_TYPE_BYTE, bitpos_driver, simd_bit_count,
    simd_bit_count_range, simd_bitop, simd_bitop_binary, simd_bitop_not,
  },
  error::Result,
};

pub trait BitmapCommands<D: Device> {
  /// 设置位图中指定偏移的 bit 值 (SETBIT)
  async fn setbit(&self, key: &[u8], offset: usize, value: u8) -> Result<u8>;

  /// 获取位图中指定偏移的 bit 值 (GETBIT)
  async fn getbit(&self, key: &[u8], offset: usize) -> Result<u8>;

  /// 统计位图中置 1 的位数 (BITCOUNT)
  async fn bitcount(&self, key: &[u8], range: Option<(isize, isize)>) -> Result<usize>;

  /// 统计位图中置 1 的位数，支持指定 BYTE 或 BIT 范围模式 (BITCOUNT [start end [BYTE|BIT]])
  async fn bitcount_range(
    &self,
    key: &[u8],
    start: isize,
    end: isize,
    is_bit_index: bool,
  ) -> Result<usize>;

  /// 查找位图中首个为 0 或 1 的 bit 偏移位置 (BITPOS key bit [start [end [BYTE|BIT]]])
  async fn bitpos(
    &self,
    key: &[u8],
    search_for: u8,
    start: Option<i64>,
    end: Option<i64>,
    is_bit_index: bool,
  ) -> Result<i64>;

  /// 执行位图二进制操作并写入目标键 (BITOP)
  async fn bitop(&self, op: BitmapOp, dest_key: &[u8], src_keys: &[&[u8]]) -> Result<usize>;
}

impl<D: Device> BitmapCommands<D> for StoreSession<D> {
  /// 设置位图中指定偏移的 bit 值 (SETBIT)
  async fn setbit(&self, key: &[u8], offset: usize, value: u8) -> Result<u8> {
    let byte_idx = offset >> 3;
    let bit_idx = 7 - (offset & 7);

    // 1. 惰性过期裁决前移 + 尝试原位翻转 bit（严格对标 C# Garnet SETBIT
    //    InPlaceUpdaterWorker & BitmapManager.UpdateBitmap）：已过期键视同不存在，
    //    降级慢路径按空串重建并回 old_bit=0
    if !(self.has_ttl_tag(key)? && self.check_expired(key).await?) {
      let try_res = self.try_modify_in_place(key, |bytes| {
        if bytes.len() > byte_idx {
          let old_bit = (bytes[byte_idx] >> bit_idx) & 1;
          if value != 0 {
            bytes[byte_idx] |= 1 << bit_idx;
          } else {
            bytes[byte_idx] &= !(1 << bit_idx);
          }
          Some(old_bit)
        } else {
          None
        }
      })?;
      if let Some(old_bit) = try_res {
        return Ok(old_bit);
      }
    }

    // 2. 需要扩容或非可变区，降级走标准路径
    let mut bytes = self.read_string(key).await?.unwrap_or_default();
    if bytes.len() <= byte_idx {
      bytes.resize(byte_idx + 1, 0);
    }
    let old_bit = (bytes[byte_idx] >> bit_idx) & 1;
    if value != 0 {
      bytes[byte_idx] |= 1 << bit_idx;
    } else {
      bytes[byte_idx] &= !(1 << bit_idx);
    }
    self.upsert(key, &bytes).await?;
    Ok(old_bit)
  }

  /// 获取位图中指定偏移的 bit 值 (GETBIT)
  async fn getbit(&self, key: &[u8], offset: usize) -> Result<u8> {
    let res = self
      .read_string_with(key, |bytes| {
        let byte_idx = offset >> 3;
        if byte_idx >= bytes.len() {
          0
        } else {
          let bit_idx = 7 - (offset & 7);
          (bytes[byte_idx] >> bit_idx) & 1
        }
      })
      .await?;
    Ok(res.unwrap_or(0))
  }

  /// 统计位图中置 1 的位数 (BITCOUNT)
  async fn bitcount(&self, key: &[u8], range: Option<(isize, isize)>) -> Result<usize> {
    let res = self
      .read_string_with(key, |bytes| {
        if bytes.is_empty() {
          0
        } else {
          match range {
            None => simd_bit_count(bytes),
            Some((s, e)) => simd_bit_count_range(bytes, s, e, false),
          }
        }
      })
      .await?;
    Ok(res.unwrap_or(0))
  }

  /// 统计位图中置 1 的位数，支持指定 BYTE 或 BIT 范围模式 (BITCOUNT [start end [BYTE|BIT]])
  async fn bitcount_range(
    &self,
    key: &[u8],
    start: isize,
    end: isize,
    is_bit_index: bool,
  ) -> Result<usize> {
    let res = self
      .read_string_with(key, |bytes| {
        simd_bit_count_range(bytes, start, end, is_bit_index)
      })
      .await?;
    Ok(res.unwrap_or(0))
  }

  /// 查找位图中首个为 0 或 1 的 bit 偏移位置 (BITPOS key bit [start [end [BYTE|BIT]]])
  async fn bitpos(
    &self,
    key: &[u8],
    search_for: u8,
    start: Option<i64>,
    end: Option<i64>,
    is_bit_index: bool,
  ) -> Result<i64> {
    let s = start.unwrap_or(0);
    let e = end.unwrap_or(-1);
    let offset_type = if is_bit_index {
      OFFSET_TYPE_BIT
    } else {
      OFFSET_TYPE_BYTE
    };
    let res = self
      .read_string_with(key, |bytes| {
        bitpos_driver(bytes, s, e, search_for, offset_type)
      })
      .await?;
    Ok(res.unwrap_or_else(|| bitpos_driver(&[], s, e, search_for, offset_type)))
  }

  /// 执行位图二进制操作并写入目标键 (BITOP)
  async fn bitop(&self, op: BitmapOp, dest_key: &[u8], src_keys: &[&[u8]]) -> Result<usize> {
    match src_keys {
      [] => Ok(0),
      [k0] => {
        if op == BitmapOp::Diff {
          return Err(BitmapError::DiffRequiresMultipleSources.into());
        }
        let s0 = self.read_string(k0).await?.unwrap_or_default();
        if s0.is_empty() {
          self.delete(dest_key).await?;
          return Ok(0);
        }
        let len = s0.len();
        let mut dst = vec![0u8; len];
        if op == BitmapOp::Not {
          simd_bitop_not(&s0, &mut dst)?;
        } else {
          dst.copy_from_slice(&s0);
        }
        self.upsert(dest_key, &dst).await?;
        Ok(len)
      }
      [k0, k1] => {
        if op == BitmapOp::Not {
          return Err(BitmapError::NotRequiresSingleSource.into());
        }
        let s0 = self.read_string(k0).await?.unwrap_or_default();
        let s1 = self.read_string(k1).await?.unwrap_or_default();
        let max_len = s0.len().max(s1.len());
        if max_len == 0 {
          self.delete(dest_key).await?;
          return Ok(0);
        }
        let mut dst = vec![0u8; max_len];
        simd_bitop_binary(op, &s0, &s1, &mut dst)?;
        self.upsert(dest_key, &dst).await?;
        Ok(max_len)
      }
      _ => {
        if op == BitmapOp::Not {
          return Err(BitmapError::NotRequiresSingleSource.into());
        }
        let mut src_buffers = Vec::with_capacity(src_keys.len());
        let mut max_len = 0usize;
        for &k in src_keys {
          let val = self.read_string(k).await?.unwrap_or_default();
          max_len = max_len.max(val.len());
          src_buffers.push(val);
        }

        if max_len == 0 {
          self.delete(dest_key).await?;
          return Ok(0);
        }

        let mut dst = vec![0u8; max_len];
        let src_slices: Vec<&[u8]> = src_buffers.iter().map(|b| b.as_slice()).collect();
        simd_bitop(op, &src_slices, &mut dst)?;

        self.upsert(dest_key, &dst).await?;
        Ok(max_len)
      }
    }
  }
}
