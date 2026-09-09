//! AOF 效果帧家族与编解码（对标 C# `AofEntryType` 专用帧家族，含
//! `RangeIndexStreamChunk` 思路：不同存储面各有独立帧类型，重放按类型分发）
//!
//! 帧记写效果（最终值）而非命令语义：重放幂等，重复应用收敛于同一状态。
//! 布局：`[op u8][klen u32 LE][vlen u32 LE][key][val]`

use crate::Error;

/// AOF 帧操作类型（对标 C# AofEntryType 的类型化分发）
///
/// 编号分配登记：0-3 已占用；4 起预留给后续帧家族（如 RangeIndex 树实例帧），
/// 未知编号 fail-fast 拒绝，新增变体受 match 穷尽检查保护
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum AofOp {
  /// 混合日志写效果：`val` 为最终值
  Upsert = 0,
  /// 混合日志删除墓碑：`val` 恒为空
  Tombstone = 1,
  /// 共享 BfTree 写效果：`val` 为完整键值（如 Flattened ZSET 的 score）
  BfTreePut = 2,
  /// 共享 BfTree 删除：`val` 恒为空
  BfTreeDelete = 3,
}

impl AofOp {
  /// 从帧首字节解码
  #[inline]
  pub const fn from_byte(b: u8) -> Option<Self> {
    match b {
      0 => Some(Self::Upsert),
      1 => Some(Self::Tombstone),
      2 => Some(Self::BfTreePut),
      3 => Some(Self::BfTreeDelete),
      _ => None,
    }
  }
}

/// AOF 效果帧定长前缀：op(1) + klen(4) + vlen(4)
const FRAME_PREFIX_LEN: usize = 9;

/// 编码物理效果帧
pub fn encode_frame(op: AofOp, key: &[u8], val: &[u8]) -> Vec<u8> {
  let mut frame = Vec::with_capacity(FRAME_PREFIX_LEN + key.len() + val.len());
  frame.push(op as u8);
  frame.extend_from_slice(&(key.len() as u32).to_le_bytes());
  frame.extend_from_slice(&(val.len() as u32).to_le_bytes());
  frame.extend_from_slice(key);
  frame.extend_from_slice(val);
  frame
}

/// 解码物理效果帧，返回 `(op, key, val)`
pub fn decode_frame(frame: &[u8]) -> Result<(AofOp, &[u8], &[u8]), Error> {
  let Some(chunk) = frame.first_chunk::<FRAME_PREFIX_LEN>() else {
    return Err(Error::Frame("帧前缀不足".into()));
  };
  let Some(op) = AofOp::from_byte(chunk[0]) else {
    return Err(Error::Frame(format!("未知帧类型 {}", chunk[0])));
  };
  let klen = u32::from_le_bytes(chunk[1..5].try_into().unwrap()) as usize;
  let vlen = u32::from_le_bytes(chunk[5..9].try_into().unwrap()) as usize;
  if FRAME_PREFIX_LEN + klen + vlen != frame.len() {
    return Err(Error::Frame(format!(
      "长度不符: 声明 {klen}+{vlen}, 实际 {}",
      frame.len() - FRAME_PREFIX_LEN
    )));
  }
  let key = &frame[FRAME_PREFIX_LEN..FRAME_PREFIX_LEN + klen];
  let val = &frame[FRAME_PREFIX_LEN + klen..];
  // 删除类帧恒空值、BfTreePut 恒非空（底层 insert 拒空值），矛盾帧即损坏
  match op {
    AofOp::Tombstone | AofOp::BfTreeDelete if !val.is_empty() => {
      return Err(Error::Frame(format!("{op:?} 帧值应为空")));
    }
    AofOp::BfTreePut if val.is_empty() => {
      return Err(Error::Frame("BfTreePut 帧值不应为空".into()));
    }
    _ => {}
  }
  Ok((op, key, val))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn test_frame_codec_roundtrip_all_ops() {
    let cases: [(AofOp, &[u8], &[u8]); 4] = [
      (AofOp::Upsert, b"ns:db:key", b"value123"),
      (AofOp::Tombstone, b"tomb-key", b""),
      (AofOp::BfTreePut, b"mkey", &8u64.to_be_bytes()),
      (AofOp::BfTreeDelete, b"old-skey", b""),
    ];
    for (op, key, val) in cases {
      let frame = encode_frame(op, key, val);
      assert_eq!(decode_frame(&frame).unwrap(), (op, key, val));
    }
  }

  #[test]
  fn test_decode_frame_rejects_torn_and_unknown_op() {
    assert!(decode_frame(&[0u8; 4]).is_err());
    let mut torn = encode_frame(AofOp::Upsert, b"k", b"v");
    torn.pop();
    assert!(decode_frame(&torn).is_err());
    assert!(decode_frame(&[0xFF, 0, 0, 0, 0, 0, 0, 0, 0]).is_err());
  }
}
