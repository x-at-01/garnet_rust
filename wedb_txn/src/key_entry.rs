use std::{
  cmp::Ordering,
  fmt::{Display, Formatter, Result as FmtResult, Write},
  slice::Iter,
};

use bytes::Bytes;
use whasher::fast_hash;

use crate::error::{Error, Result};

/// 键锁类型枚举
#[derive(
  Debug,
  Clone,
  Copy,
  PartialEq,
  Eq,
  PartialOrd,
  Ord,
  Hash,
  Default,
  bitcode::Encode,
  bitcode::Decode,
)]
#[repr(u8)]
pub enum LockType {
  /// 无锁
  #[default]
  None = 0,
  /// 共享读锁
  Shared = 1,
  /// 排他写锁
  Exclusive = 2,
}

impl LockType {
  /// 是否为排他写锁
  #[inline]
  pub const fn is_exclusive(self) -> bool {
    matches!(self, Self::Exclusive)
  }

  /// 是否为共享读锁
  #[inline]
  pub const fn is_shared(self) -> bool {
    matches!(self, Self::Shared)
  }

  /// 升级锁类型（取级别更高者）
  #[inline]
  pub fn upgrade_to(&mut self, other: Self) {
    if other > *self {
      *self = other;
    }
  }
}

/// 事务键锁条目
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TxnKeyEntry {
  /// 键的 64 位哈希值（用于加锁排序与死锁预防）
  pub key_hash: u64,
  /// 锁类型（共享读锁或排他写锁）
  pub lock_type: LockType,
  /// 键的原始字节（可选，用于冲突诊断）
  pub key: Option<Bytes>,
}

#[derive(bitcode::Encode, bitcode::Decode)]
struct TxnKeyEntryWire {
  key_hash: u64,
  lock_type: LockType,
  key: Option<Vec<u8>>,
}

impl TxnKeyEntry {
  /// 创建新的键锁条目
  #[inline]
  pub const fn new(key_hash: u64, lock_type: LockType, key: Option<Bytes>) -> Self {
    Self {
      key_hash,
      lock_type,
      key,
    }
  }

  /// 从字节切片创建键锁条目
  #[inline]
  pub fn from_bytes(key: Bytes, lock_type: LockType) -> Self {
    let key_hash = fast_hash(&key);
    Self {
      key_hash,
      lock_type,
      key: Some(key),
    }
  }

  /// 从只读切片创建键锁条目（零堆分配）
  #[inline]
  pub fn from_slice(key: &[u8], lock_type: LockType) -> Self {
    let key_hash = fast_hash(key);
    Self {
      key_hash,
      lock_type,
      key: None,
    }
  }

  /// 使用 bitcode 编码为二进制字节向量
  pub fn encode_bitcode(&self) -> Vec<u8> {
    let wire = TxnKeyEntryWire {
      key_hash: self.key_hash,
      lock_type: self.lock_type,
      key: self.key.as_ref().map(|b| b.to_vec()),
    };
    bitcode::encode(&wire)
  }

  /// 从 bitcode 二进制切片解码还原
  pub fn decode_bitcode(src: &[u8]) -> Result<Self> {
    let wire: TxnKeyEntryWire = bitcode::decode(src).map_err(|e| Error::Bitcode(e.to_string()))?;
    Ok(Self {
      key_hash: wire.key_hash,
      lock_type: wire.lock_type,
      key: wire.key.map(Bytes::from),
    })
  }
}

impl Display for TxnKeyEntry {
  /// 格式化为文本
  fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
    let type_char = match self.lock_type {
      LockType::None => "-",
      LockType::Shared => "s",
      LockType::Exclusive => "x",
    };
    write!(f, "{}:{}", self.key_hash, type_char)
  }
}

/// 事务键锁条目集合（负责锁收集与全局死锁防御排序）
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TxnKeyEntries {
  /// 锁条目列表
  entries: Vec<TxnKeyEntry>,
}

impl TxnKeyEntries {
  /// 创建空的键锁条目集合
  #[inline]
  pub const fn new() -> Self {
    Self {
      entries: Vec::new(),
    }
  }

  /// 为后续条目预留额外空间
  #[inline]
  pub fn reserve(&mut self, additional: usize) {
    self.entries.reserve(additional);
  }

  /// 注册键哈希与期望的锁类型（零堆分配）
  #[inline]
  pub fn add_key_hash(&mut self, key_hash: u64, lock_type: LockType) {
    self
      .entries
      .push(TxnKeyEntry::new(key_hash, lock_type, None));
  }

  /// 注册键哈希与期望的锁类型及可选键
  #[inline]
  pub fn add_key(&mut self, key_hash: u64, lock_type: LockType, key: Option<Bytes>) {
    self
      .entries
      .push(TxnKeyEntry::new(key_hash, lock_type, key));
  }

  /// 注册原始只读字节切片与期望的锁类型（零额外堆分配）
  #[inline]
  pub fn add_key_slice(&mut self, key: &[u8], lock_type: LockType) {
    self.entries.push(TxnKeyEntry::from_slice(key, lock_type));
  }

  /// 注册原始字节键与期望的锁类型
  #[inline]
  pub fn add_key_bytes(&mut self, key: Bytes, lock_type: LockType) {
    self.entries.push(TxnKeyEntry::from_bytes(key, lock_type));
  }

  /// 检查本次事务是否为纯只读事务（无任何排他写锁）
  #[inline]
  pub fn is_read_only(&self) -> bool {
    !self.entries.iter().any(|e| e.lock_type.is_exclusive())
  }

  /// 键条目总数（对标 Garnet `TxnKeyEntries.Count`）
  #[inline]
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// 是否为空
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// 转换为切片引用
  #[inline]
  pub fn as_slice(&self) -> &[TxnKeyEntry] {
    &self.entries
  }

  /// 获取条目迭代器
  #[inline]
  pub fn iter(&self) -> Iter<'_, TxnKeyEntry> {
    self.entries.iter()
  }

  /// 清空所有条目
  #[inline]
  pub fn clear(&mut self) {
    self.entries.clear();
  }

  /// 死锁防御排序核心算法：
  ///
  /// 1. 按照键哈希升序排序；若哈希相同则排他锁优先；存在键数据时按键全序排序
  /// 2. 原址单次遍历合并相同哈希条目，锁级别自动升级为最高者
  /// 3. 去重保证每个键哈希在有序列表中唯一出现
  pub fn sort_by_key_hash(&mut self) {
    if self.entries.len() <= 1 {
      return;
    }

    self.entries.sort_unstable_by(TxnKeyComparison::compare);

    let mut write_idx = 0;
    for read_idx in 1..self.entries.len() {
      let is_same = self.entries[write_idx].key_hash == self.entries[read_idx].key_hash
        && match (&self.entries[write_idx].key, &self.entries[read_idx].key) {
          (Some(k1), Some(k2)) => k1 == k2,
          _ => true,
        };

      if is_same {
        let incoming_lock = self.entries[read_idx].lock_type;
        self.entries[write_idx].lock_type.upgrade_to(incoming_lock);
        if self.entries[write_idx].key.is_none() && self.entries[read_idx].key.is_some() {
          self.entries[write_idx].key = self.entries[read_idx].key.take();
        }
      } else {
        write_idx += 1;
        if write_idx != read_idx {
          self.entries.swap(write_idx, read_idx);
        }
      }
    }

    self.entries.truncate(write_idx + 1);
  }

  /// 释放并清空所有键锁（对标 Garnet UnlockAllKeys 先释放物理锁、再清空条目的顺序；
  /// 物理哈希锁由调用方（如 wedb_net 会话）在持锁句柄上释放，本方法仅清理条目集合）
  #[inline]
  pub fn unlock_all_keys(&mut self) {
    self.clear();
  }

  /// 获取锁集合的调试文本
  pub fn get_lockset(&self, phase: u8) -> String {
    if self.entries.is_empty() {
      return String::new();
    }
    let mut sb = String::with_capacity(self.entries.len() * 20 + 16);
    for entry in &self.entries {
      let _ = write!(sb, "{entry}");
    }
    let phase_str = match phase {
      1 => "lock",
      2 => "unlock",
      _ => "none",
    };
    let _ = write!(sb, " (phase: {phase_str})");
    sb
  }

  /// 使用 bitcode 编码为二进制字节向量
  pub fn encode_bitcode(&self) -> Vec<u8> {
    let wire: Vec<TxnKeyEntryWire> = self
      .entries
      .iter()
      .map(|e| TxnKeyEntryWire {
        key_hash: e.key_hash,
        lock_type: e.lock_type,
        key: e.key.as_ref().map(|b| b.to_vec()),
      })
      .collect();
    bitcode::encode(&wire)
  }

  /// 从 bitcode 二进制切片解码还原
  pub fn decode_bitcode(src: &[u8]) -> Result<Self> {
    let wires: Vec<TxnKeyEntryWire> =
      bitcode::decode(src).map_err(|e| Error::Bitcode(e.to_string()))?;
    let entries = wires
      .into_iter()
      .map(|w| TxnKeyEntry {
        key_hash: w.key_hash,
        lock_type: w.lock_type,
        key: w.key.map(Bytes::from),
      })
      .collect();
    Ok(Self { entries })
  }
}

impl<'a> IntoIterator for &'a TxnKeyEntries {
  type Item = &'a TxnKeyEntry;
  type IntoIter = Iter<'a, TxnKeyEntry>;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    self.iter()
  }
}

/// 键锁条目比较器（对标 Garnet `TxnKeyEntryComparison.cs` → Tsavorite `CompareKeyHashes`）
#[derive(Debug, Default, Clone, Copy)]
pub struct TxnKeyComparison;

impl TxnKeyComparison {
  /// 比较两个键锁条目：按键哈希升序，哈希相同（64 位哈希碰撞）时按键字节全序排序。
  ///
  /// 与 C# 的差异：Tsavorite 按"哈希表桶索引 + 锁类型"排序，因为其物理锁以桶为粒度；
  /// 本实现键字节即全序，且同哈希条目在 [`Self::sort_by_key_hash`] 中按最高锁级别合并，
  /// 故无需锁类型决胜项——剩余条目两两严格可分，保证加锁全序无死锁。
  #[inline]
  pub fn compare(key1: &TxnKeyEntry, key2: &TxnKeyEntry) -> Ordering {
    key1
      .key_hash
      .cmp(&key2.key_hash)
      .then_with(|| key1.key.cmp(&key2.key))
  }
}
