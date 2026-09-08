//! VectorSet：向量集元素名映射、属性存储与 ANN 索引封装

use std::sync::Arc;

use gxhash::{GxBuildHasher, HashMap};

use crate::{
  error::{Error, Result},
  vamana::{Vamana, VamanaParams, l2_sq},
};

/// 向量集：元素名 <-> 内部 id 映射，属性 KV 与 Vamana 索引
pub struct VectorSet {
  /// 创建时确定的向量维度
  dims: usize,
  index: Vamana,
  /// 元素名 -> 内部 id
  ids: HashMap<Box<[u8]>, u32>,
  /// 内部 id -> 元素名
  names: Vec<Box<[u8]>>,
  /// 元素属性 (VSETATTR 写入，VGETATTR 读取)
  attrs: HashMap<u32, Arc<[u8]>>,
}

/// 余弦相似度：归一化向量下 l2_sq = 2 - 2cos，故 score = 1 - l2_sq/2
fn score(dist: f32) -> f32 {
  1.0 - 0.5 * dist
}

impl VectorSet {
  /// 创建指定维度的向量集，维度即固定（向量校验收敛在 [`Vamana`] 单点）
  pub fn new(dims: usize, params: VamanaParams) -> Result<Self> {
    Ok(Self {
      dims,
      index: Vamana::new(dims, params)?,
      ids: HashMap::with_hasher(GxBuildHasher::default()),
      names: Vec::new(),
      attrs: HashMap::with_hasher(GxBuildHasher::default()),
    })
  }

  /// 向量维度
  pub fn dims(&self) -> usize {
    self.dims
  }

  /// 元素数
  pub fn len(&self) -> usize {
    self.index.len()
  }

  /// 是否为空
  pub fn is_empty(&self) -> bool {
    self.index.is_empty()
  }

  /// 元素是否存在
  pub fn contains(&self, element: &[u8]) -> bool {
    self
      .ids
      .get(element)
      .is_some_and(|&id| self.index.contains(id))
  }

  /// 查元素内部 id
  pub fn id_of(&self, element: &[u8]) -> Option<u32> {
    self
      .ids
      .get(element)
      .copied()
      .filter(|&id| self.index.contains(id))
  }

  /// 插入元素；已存在时原样返回 (Ok(None))，新插入返回 (Ok(Some(id)))
  pub fn add(&mut self, element: &[u8], vector: &[f32]) -> Result<Option<u32>> {
    if self.id_of(element).is_some() {
      return Ok(None);
    }
    let id = self.index.insert(vector)?;
    self.ids.insert(element.into(), id);
    self.names.push(element.into());
    Ok(Some(id))
  }

  /// 删除元素，返回是否确实删除
  pub fn remove(&mut self, element: &[u8]) -> bool {
    let Some(id) = self.id_of(element) else {
      return false;
    };
    self.ids.remove(element);
    self.attrs.remove(&id);
    self.index.remove(id)
  }

  /// 元素属性
  pub fn attr(&self, element: &[u8]) -> Option<Arc<[u8]>> {
    let id = self.id_of(element)?;
    self.attrs.get(&id).cloned()
  }

  /// 写元素属性，返回是否元素存在
  pub fn set_attr(&mut self, element: &[u8], attr: Option<Vec<u8>>) -> bool {
    let Some(id) = self.id_of(element) else {
      return false;
    };
    match attr {
      Some(v) => {
        self.attrs.insert(id, v.into());
      }
      None => {
        self.attrs.remove(&id);
      }
    }
    true
  }

  /// 最近邻检索：返回 (元素名, 余弦相似度) 降序列表
  pub fn search(&self, vector: &[f32], k: usize, ef: usize) -> Result<Vec<(Box<[u8]>, f32)>> {
    Ok(
      self
        .index
        .search(vector, k, ef)?
        .into_iter()
        .map(|(id, d)| (self.names[id as usize].clone(), score(d)))
        .collect(),
    )
  }

  /// 元素归一化向量
  pub fn vector_of(&self, id: u32) -> Option<&[f32]> {
    self.index.vector(id)
  }

  /// 元素内部 id 与其邻接表
  fn link_ids(&self, element: &[u8]) -> Result<(u32, &[u32])> {
    let id = self.id_of(element).ok_or(Error::ElementNotFound)?;
    Ok((id, self.index.links(id)))
  }

  /// 元素邻接的元素名列表
  pub fn links_of(&self, element: &[u8]) -> Result<Vec<Box<[u8]>>> {
    let (_, links) = self.link_ids(element)?;
    Ok(
      links
        .iter()
        .filter(|&&n| self.index.contains(n))
        .map(|&n| self.names[n as usize].clone())
        .collect(),
    )
  }

  /// 元素邻接列表及其与该元素的相似度（按相似度降序）
  pub fn links_with_scores(&self, element: &[u8]) -> Result<Vec<(Box<[u8]>, f32)>> {
    let (id, links) = self.link_ids(element)?;
    let Some(base) = self.index.vector(id) else {
      return Ok(Vec::new());
    };
    let mut out: Vec<(Box<[u8]>, f32)> = links
      .iter()
      // filter_map 兼做存活过滤与向量取用
      .filter_map(|&n| {
        let v = self.index.vector(n)?;
        Some((self.names[n as usize].clone(), score(l2_sq(v, base))))
      })
      .collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(out)
  }

  /// 随机取一个元素名
  pub fn random(&self) -> Option<&[u8]> {
    let id = self.index.random()?;
    Some(&self.names[id as usize])
  }
}
