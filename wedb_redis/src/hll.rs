use wdev::Device;
use wedb_hll::HyperLogLog;
use wkv::StoreSession;

use super::*;
use crate::error::Result;

pub trait HllCommands<D: Device> {
  /// 向 HyperLogLog 键添加元素 (PFADD key element [element ...])
  ///
  /// 对齐 Redis 规范：键不存在且未携带任何元素时不创建键，直接返回 false
  async fn pfadd(&self, key: &[u8], elements: &[&[u8]]) -> Result<bool>;

  /// 估算一个或多个 HyperLogLog 键的联合基数 (PFCOUNT key [key ...])
  async fn pfcount(&self, keys: &[&[u8]]) -> Result<u64>;

  /// 合并多个源 HyperLogLog 键至目标键 (PFMERGE destkey sourcekey [sourcekey ...])
  async fn pfmerge(&self, dest_key: &[u8], src_keys: &[&[u8]]) -> Result<()>;
}

impl<D: Device> HllCommands<D> for StoreSession<D> {
  /// 向 HyperLogLog 键添加元素 (PFADD key element [element ...])
  ///
  /// 对齐 Redis 规范：键不存在且未携带任何元素时不创建键，直接返回 false
  async fn pfadd(&self, key: &[u8], elements: &[&[u8]]) -> Result<bool> {
    let mut hll = match self.read_string(key).await? {
      None => {
        if elements.is_empty() {
          return Ok(false);
        }
        HyperLogLog::new()
      }
      Some(bytes) => HyperLogLog::from_bytes(&bytes)?,
    };

    let mut updated = false;
    for elem in elements {
      if hll.add(elem) {
        updated = true;
      }
    }

    if updated {
      self.upsert(key, hll.as_bytes()).await?;
    }

    Ok(updated)
  }

  /// 估算一个或多个 HyperLogLog 键的联合基数 (PFCOUNT key [key ...])
  async fn pfcount(&self, keys: &[&[u8]]) -> Result<u64> {
    if keys.is_empty() {
      return Ok(0);
    }
    if keys.len() == 1 {
      return match self.read_string(keys[0]).await? {
        None => Ok(0),
        Some(bytes) => {
          let mut hll = HyperLogLog::from_bytes(&bytes)?;
          // 计数会回填头部基数缓存：仅当字节确有变化（缓存缺失/陈旧）才回写，缓存命中的常态读零写放大
          let count = hll.count();
          if hll.as_bytes() != bytes.as_slice() {
            self.upsert(keys[0], hll.as_bytes()).await?;
          }
          Ok(count)
        }
      };
    }

    let mut hlls = Vec::with_capacity(keys.len());
    for key in keys {
      if let Some(bytes) = self.read_string(key).await? {
        hlls.push(HyperLogLog::from_bytes(&bytes)?);
      }
    }

    if hlls.is_empty() {
      return Ok(0);
    }

    let refs: Vec<&HyperLogLog> = hlls.iter().collect();
    Ok(HyperLogLog::count_multiple(&refs))
  }

  /// 合并多个源 HyperLogLog 键至目标键 (PFMERGE destkey sourcekey [sourcekey ...])
  async fn pfmerge(&self, dest_key: &[u8], src_keys: &[&[u8]]) -> Result<()> {
    let mut dest_hll = match self.read_string(dest_key).await? {
      None => HyperLogLog::new(),
      Some(bytes) => HyperLogLog::from_bytes(&bytes)?,
    };

    for src_key in src_keys {
      if let Some(bytes) = self.read_string(src_key).await? {
        let src_hll = HyperLogLog::from_bytes(&bytes)?;
        dest_hll.merge(&src_hll);
      }
    }

    self.upsert(dest_key, dest_hll.as_bytes()).await?;
    Ok(())
  }
}
