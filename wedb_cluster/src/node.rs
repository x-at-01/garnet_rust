use std::{
  fmt,
  str::{self, FromStr},
};

use crate::error::{Error, Result};

/// 集群节点 40 位十六进制唯一标识符 (对齐 Redis / Garnet)
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, bitcode::Encode, bitcode::Decode)]
pub struct NodeId(pub [u8; 40]);

impl Default for NodeId {
  #[inline]
  fn default() -> Self {
    Self([b'0'; 40])
  }
}

impl fmt::Debug for NodeId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("NodeId(\"")?;
    f.write_str(self.as_str())?;
    f.write_str("\")")
  }
}

impl fmt::Display for NodeId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

impl NodeId {
  /// 生成一个全新的随机 40 位十六进制节点 ID (8+8+4 字节随机数恰好铺满 160 位)
  pub fn generate() -> Self {
    let mut bytes = [0u8; 40];
    let mut r1 = fastrand::u64(..);
    let mut r2 = fastrand::u64(..);
    let mut r3 = fastrand::u32(..);
    for b in &mut bytes[..16] {
      *b = HEX_CHARS[(r1 & 0xF) as usize];
      r1 >>= 4;
    }
    for b in &mut bytes[16..32] {
      *b = HEX_CHARS[(r2 & 0xF) as usize];
      r2 >>= 4;
    }
    for b in &mut bytes[32..40] {
      *b = HEX_CHARS[(r3 & 0xF) as usize];
      r3 >>= 4;
    }
    Self(bytes)
  }

  /// 转换为字符串切片（零拷贝）
  #[inline]
  pub fn as_str(&self) -> &str {
    // 经校验始终为有效的 ASCII 小写十六进制字符
    unsafe { str::from_utf8_unchecked(&self.0) }
  }

  /// 检查是否为全 0 默认保留 ID
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.0.iter().all(|&b| b == b'0')
  }
}

impl FromStr for NodeId {
  type Err = Error;

  fn from_str(s: &str) -> Result<Self> {
    let bytes = s.as_bytes();
    if bytes.len() != 40 {
      return Err(Error::InvalidNodeId(format!(
        "长度必须为 40 字节，实际为 {}",
        bytes.len()
      )));
    }
    let mut arr = [0u8; 40];
    for (dst, &b) in arr.iter_mut().zip(bytes) {
      if !b.is_ascii_hexdigit() {
        return Err(Error::InvalidNodeId(format!(
          "字符 '{}' 不是合法的十六进制字符",
          b as char
        )));
      }
      *dst = b.to_ascii_lowercase();
    }
    Ok(Self(arr))
  }
}

/// 节点角色标识（对标 Garnet `NodeRole`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, bitcode::Encode, bitcode::Decode)]
#[repr(u8)]
pub enum NodeRole {
  /// 主节点
  #[default]
  Primary = 0x0,
  /// 从节点（副本）
  Replica = 0x1,
  /// 未分配角色
  Unassigned = 0x2,
}

impl NodeRole {
  /// 从原始字节值转换
  #[inline]
  pub const fn from_u8(val: u8) -> Self {
    match val {
      0x0 => Self::Primary,
      0x1 => Self::Replica,
      _ => Self::Unassigned,
    }
  }

  /// 转换为对应字节值
  #[inline]
  pub const fn to_u8(self) -> u8 {
    self as u8
  }

  /// 转换为规范小写字符串 (用于 CLUSTER NODES 与 NestedText 配置)
  #[inline]
  pub const fn as_str(&self) -> &'static str {
    match self {
      Self::Primary => "master",
      Self::Replica => "slave",
      Self::Unassigned => "unassigned",
    }
  }

  /// 从字符串解析角色
  pub fn from_role_str(s: &str) -> Result<Self> {
    if s.eq_ignore_ascii_case("master") || s.eq_ignore_ascii_case("primary") {
      Ok(Self::Primary)
    } else if s.eq_ignore_ascii_case("slave") || s.eq_ignore_ascii_case("replica") {
      Ok(Self::Replica)
    } else if s.eq_ignore_ascii_case("unassigned") {
      Ok(Self::Unassigned)
    } else {
      Err(Error::InvalidNodeRole(s.to_string()))
    }
  }
}

impl fmt::Display for NodeRole {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::Primary => f.write_str("PRIMARY"),
      Self::Replica => f.write_str("REPLICA"),
      Self::Unassigned => f.write_str("UNASSIGNED"),
    }
  }
}

/// 连接状态标识
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, bitcode::Encode, bitcode::Decode)]
pub enum LinkState {
  /// 处于已连接状态
  #[default]
  Connected,
  /// 处于未连接状态
  Disconnected,
}

impl LinkState {
  /// 转换为规范字符串
  #[inline]
  pub const fn as_str(&self) -> &'static str {
    match self {
      Self::Connected => "connected",
      Self::Disconnected => "disconnected",
    }
  }
}
