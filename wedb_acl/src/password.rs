use core::hash::{Hash, Hasher};
use std::{fmt, hint::black_box, str};

use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{Error, Result};

/// ACL 密码结构体，内部存储 32 字节 SHA-256 哈希值
///
/// 认证比较时强制使用恒定时间比对，彻底防御时序侧信道攻击
#[derive(Clone, Copy, Eq, bitcode::Encode, bitcode::Decode)]
pub struct AclPassword {
  /// SHA-256 哈希字节数组
  pub hash: [u8; 32],
}

impl PartialEq for AclPassword {
  #[inline]
  fn eq(&self, other: &Self) -> bool {
    self.ct_eq(other)
  }
}

impl Hash for AclPassword {
  #[inline]
  fn hash<H: Hasher>(&self, state: &mut H) {
    self.hash.hash(state);
  }
}

impl AclPassword {
  /// 哈希字节长度（32 字节 / 256 位）
  pub const HASH_LEN: usize = 32;
  /// 十六进制字符长度（64 字符）
  pub const HEX_LEN: usize = 64;

  /// 直接从 32 字节数组构造密码对象
  #[inline]
  pub const fn new(hash: [u8; 32]) -> Self {
    Self { hash }
  }

  /// 获取 32 字节哈希数组引用
  #[inline]
  pub const fn hash(&self) -> &[u8; 32] {
    &self.hash
  }

  /// 从明文密码构造 AclPassword（计算其 SHA-256 哈希）
  #[inline]
  pub fn from_cleartext(password: &str) -> Self {
    Self {
      hash: Sha256::digest(password.as_bytes()).into(),
    }
  }

  /// 伪哈希构造（用于用户不存在时消耗恒定 SHA-256 时间，消除用户名枚举侧信道）
  ///
  /// 使用 `black_box` 强制阻止编译器在 Release 模式下将其当作死代码优化消除
  #[inline]
  pub fn dummy(password: &str) {
    let digest = Sha256::digest(password.as_bytes());
    black_box(digest);
  }

  /// 从 64 字符十六进制哈希字符串解析构造 AclPassword
  #[inline]
  pub fn from_hash_hex(hex_str: &str) -> Result<Self> {
    if hex_str.len() != Self::HEX_LEN {
      return Err(Error::InvalidPasswordHashLength(hex_str.len()));
    }
    let mut hash = [0u8; Self::HASH_LEN];
    hex::decode_to_slice(hex_str, &mut hash)
      .map_err(|e| Error::InvalidPasswordHash(e.to_string()))?;
    Ok(Self { hash })
  }

  /// 转换为 64 字符十六进制小写字符串
  #[inline]
  pub fn to_hex(&self) -> String {
    hex::encode(self.hash)
  }

  /// 恒定时间无侧信道比对
  #[inline]
  pub fn ct_eq(&self, other: &Self) -> bool {
    self.hash.ct_eq(&other.hash).into()
  }

  /// 随机密码生成器（对标 Redis / Garnet ACL GENPASS [bits]）
  ///
  /// `bits` 指定生成密码的随机位数（1..=4096，默认 256 位），
  /// 输出对应的十六进制字符串（字符长度为 `(bits + 3) / 4`）。
  pub fn genpass(bits: Option<usize>) -> Result<String> {
    let bits = bits.unwrap_or(256);
    if bits == 0 || bits > 4096 {
      return Err(Error::InvalidGenpassBits(bits));
    }
    let hex_len = bits.div_ceil(4);
    let byte_len = hex_len.div_ceil(2);
    let mut bytes = [0u8; 512];
    let slice = &mut bytes[..byte_len];
    fastrand::fill(slice);
    let mut s = hex::encode(slice);
    s.truncate(hex_len);
    Ok(s)
  }
}

impl fmt::Display for AclPassword {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let mut buf = [0u8; Self::HEX_LEN];
    hex::encode_to_slice(self.hash, &mut buf).map_err(|_| fmt::Error)?;
    // SAFETY: hex::encode_to_slice 输出必然是合法 ASCII 十六进制小写字符
    let s = unsafe { str::from_utf8_unchecked(&buf) };
    f.write_str(s)
  }
}

impl fmt::Debug for AclPassword {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str("AclPassword(")?;
    fmt::Display::fmt(self, f)?;
    f.write_str(")")
  }
}
