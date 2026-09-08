//! VectorManager：向量集键空间管理 (对标 Garnet VectorManager 的索引生命周期部分)

use std::sync::Arc;

use parking_lot::RwLock;
use whasher::{GxPapayaMap, new_papaya_map};

use crate::{error::Result, set::VectorSet, vamana::VamanaParams};

/// 向量集键空间：key -> 加锁的向量集
pub struct VectorManager {
  sets: GxPapayaMap<String, Arc<RwLock<VectorSet>>>,
}

impl Default for VectorManager {
  fn default() -> Self {
    Self::new()
  }
}

impl VectorManager {
  pub fn new() -> Self {
    Self {
      sets: new_papaya_map(),
    }
  }

  /// 获取向量集（只读快速路径）
  pub fn get(&self, key: &str) -> Option<Arc<RwLock<VectorSet>>> {
    self.sets.pin().get(key).cloned()
  }

  /// 获取或创建向量集（VADD 自动建集语义）；返回 (向量集, 是否新建)。
  /// try_insert 保证并发首插时不顶掉胜出的集合，已存在的集原样返回。
  pub fn get_or_create(
    &self,
    key: &str,
    dims: usize,
    params: VamanaParams,
  ) -> Result<(Arc<RwLock<VectorSet>>, bool)> {
    if let Some(existing) = self.get(key) {
      return Ok((existing, false));
    }
    let set = Arc::new(RwLock::new(VectorSet::new(dims, params)?));
    match self.sets.pin().try_insert(key.to_owned(), set) {
      Ok(set) => Ok((set.clone(), true)),
      Err(occupied) => Ok((occupied.current.clone(), false)),
    }
  }

  /// 删除向量集，返回是否确实删除
  pub fn drop_set(&self, key: &str) -> bool {
    self.sets.pin().remove(key).is_some()
  }

  /// 已加载向量集数
  pub fn len(&self) -> usize {
    self.sets.pin().len()
  }

  /// 是否没有任何向量集
  pub fn is_empty(&self) -> bool {
    self.sets.pin().is_empty()
  }
}
