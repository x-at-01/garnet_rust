use std::slice::Iter;

use bytes::Bytes;

use super::{
  error::{Error, Result},
  key_entry::{LockType, TxnKeyEntries},
  version_map::WatchVersionMap,
};

/// 单个受监视键记录
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchedKeyEntry {
  /// 键的原始字节
  pub key: Bytes,
  /// 键的 64 位哈希值
  pub hash: u64,
  /// 开启监视时记录的全局版本号
  pub version: u64,
  /// 当前是否处于受监视状态
  pub is_watched: bool,
}

#[derive(bitcode::Encode, bitcode::Decode)]
struct WatchedKeyEntryWire {
  key: Vec<u8>,
  hash: u64,
  version: u64,
  is_watched: bool,
}

impl WatchedKeyEntry {
  /// 使用 bitcode 编码为二进制字节向量
  pub fn encode_bitcode(&self) -> Vec<u8> {
    let wire = WatchedKeyEntryWire {
      key: self.key.to_vec(),
      hash: self.hash,
      version: self.version,
      is_watched: self.is_watched,
    };
    bitcode::encode(&wire)
  }

  /// 从 bitcode 二进制切片解码还原
  pub fn decode_bitcode(src: &[u8]) -> Result<Self> {
    let wire: WatchedKeyEntryWire =
      bitcode::decode(src).map_err(|e| Error::Bitcode(e.to_string()))?;
    Ok(Self {
      key: Bytes::from(wire.key),
      hash: wire.hash,
      version: wire.version,
      is_watched: wire.is_watched,
    })
  }
}

/// 会话级受监视键容器
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct WatchedKeysContainer {
  /// 监视键列表
  entries: Vec<WatchedKeyEntry>,
}

impl WatchedKeysContainer {
  /// 创建新的监视键容器
  #[inline]
  pub const fn new() -> Self {
    Self {
      entries: Vec::new(),
    }
  }

  /// 监视一个键，记录当前的哈希值与全局版本号
  pub fn watch(&mut self, key: Bytes, version_map: &WatchVersionMap) {
    let hash = WatchVersionMap::hash_key(&key);
    let version = version_map.read_version(hash);

    if let Some(entry) = self
      .entries
      .iter_mut()
      .find(|e| e.hash == hash && e.key == key)
    {
      entry.version = version;
      entry.is_watched = true;
      return;
    }

    self.entries.push(WatchedKeyEntry {
      key,
      hash,
      version,
      is_watched: true,
    });
  }

  /// 取消对特定键的监视
  pub fn remove_watch(&mut self, key: &[u8]) -> bool {
    let hash = WatchVersionMap::hash_key(key);
    if let Some(entry) = self
      .entries
      .iter_mut()
      .find(|e| e.is_watched && e.hash == hash && e.key.as_ref() == key)
    {
      entry.is_watched = false;
      true
    } else {
      false
    }
  }

  /// 取消所有监视并重置容器（对标 Garnet `WatchedKeysContainer.Reset`，UNWATCH /
  /// DISCARD / 断连清理统一走此入口）
  #[inline]
  pub fn reset(&mut self) {
    self.entries.clear();
  }

  /// 验证所有受监视键的版本号是否与全局版本表一致
  /// 任一键版本改变即判定发生并发修改冲突
  #[inline]
  pub fn validate_versions(&self, version_map: &WatchVersionMap) -> bool {
    self
      .entries
      .iter()
      .filter(|e| e.is_watched)
      .all(|e| version_map.read_version(e.hash) == e.version)
  }

  /// 将所有受监视键以共享读锁注册到事务锁集合中
  ///
  /// 锁条目仅按哈希参与加锁排序，原始键字节保留在监视容器内即可，无需克隆。
  pub fn save_keys_to_lock(&self, key_entries: &mut TxnKeyEntries) {
    key_entries.reserve(self.watched_count());
    for entry in self.entries.iter().filter(|e| e.is_watched) {
      key_entries.add_key(entry.hash, LockType::Shared, None);
    }
  }

  /// 获取条目切片引用
  #[inline]
  pub fn as_slice(&self) -> &[WatchedKeyEntry] {
    &self.entries
  }

  /// 容器中所有条目数量（含已取消监视项）
  #[inline]
  pub fn len(&self) -> usize {
    self.entries.len()
  }

  /// 容器是否为空
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.entries.is_empty()
  }

  /// 当前处于有效监视状态的键数量
  #[inline]
  pub fn watched_count(&self) -> usize {
    self.entries.iter().filter(|e| e.is_watched).count()
  }

  /// 使用 bitcode 编码为二进制字节向量
  pub fn encode_bitcode(&self) -> Vec<u8> {
    let wire: Vec<WatchedKeyEntryWire> = self
      .entries
      .iter()
      .map(|e| WatchedKeyEntryWire {
        key: e.key.to_vec(),
        hash: e.hash,
        version: e.version,
        is_watched: e.is_watched,
      })
      .collect();
    bitcode::encode(&wire)
  }

  /// 从 bitcode 二进制切片解码还原
  pub fn decode_bitcode(src: &[u8]) -> Result<Self> {
    let wires: Vec<WatchedKeyEntryWire> =
      bitcode::decode(src).map_err(|e| Error::Bitcode(e.to_string()))?;
    let entries = wires
      .into_iter()
      .map(|w| WatchedKeyEntry {
        key: Bytes::from(w.key),
        hash: w.hash,
        version: w.version,
        is_watched: w.is_watched,
      })
      .collect();
    Ok(Self { entries })
  }
}

impl<'a> IntoIterator for &'a WatchedKeysContainer {
  type Item = &'a WatchedKeyEntry;
  type IntoIter = Iter<'a, WatchedKeyEntry>;

  #[inline]
  fn into_iter(self) -> Self::IntoIter {
    self.entries.iter()
  }
}
