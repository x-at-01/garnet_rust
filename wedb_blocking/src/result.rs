use bytes::Bytes;

/// 从被观察集合中检索到的元素结果
///
/// 1:1 对齐 Microsoft Garnet `CollectionItemResult`
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CollectionItemResult {
  /// 来源集合的 Key
  pub key: Option<Bytes>,
  /// 检索到的单元素 (BLPOP / BRPOP / BLMOVE / BRPOPLPUSH)
  pub item: Option<Bytes>,
  /// 与单元素关联的分数 (BZPOPMIN / BZPOPMAX)
  pub score: Option<f64>,
  /// 检索到的多元素列表 (BLMPOP / BZMPOP)
  pub items: Option<Vec<Bytes>>,
  /// 与多元素关联的分数列表 (BZMPOP)
  pub scores: Option<Vec<f64>>,
  /// 是否由于 CLIENT UNBLOCK 被强制解除阻塞
  pub is_force_unblocked: bool,
  /// 是否发生数据类型不匹配
  pub is_type_mismatch: bool,
}

impl CollectionItemResult {
  /// 创建空结果 (超时或无数据)
  #[inline]
  pub const fn empty() -> Self {
    Self {
      key: None,
      item: None,
      score: None,
      items: None,
      scores: None,
      is_force_unblocked: false,
      is_type_mismatch: false,
    }
  }

  /// 创建被强制解除阻塞的结果 (CLIENT UNBLOCK)
  #[inline]
  pub const fn force_unblocked() -> Self {
    Self {
      key: None,
      item: None,
      score: None,
      items: None,
      scores: None,
      is_force_unblocked: true,
      is_type_mismatch: false,
    }
  }

  /// 创建类型不匹配结果 (WRONGTYPE)
  #[inline]
  pub const fn type_mismatch() -> Self {
    Self {
      key: None,
      item: None,
      score: None,
      items: None,
      scores: None,
      is_force_unblocked: false,
      is_type_mismatch: true,
    }
  }

  /// 创建 Key 更新唤醒通知结果 (用于信号驱动模式唤醒等待者从实际存储中弹出数据)
  #[inline]
  pub fn key_update(key: impl Into<Bytes>) -> Self {
    Self {
      key: Some(key.into()),
      item: None,
      score: None,
      items: None,
      scores: None,
      is_force_unblocked: false,
      is_type_mismatch: false,
    }
  }

  /// 创建单元素结果 (BLPOP / BRPOP / BLMOVE / BRPOPLPUSH)
  #[inline]
  pub fn single(key: impl Into<Bytes>, item: impl Into<Bytes>) -> Self {
    Self {
      key: Some(key.into()),
      item: Some(item.into()),
      score: None,
      items: None,
      scores: None,
      is_force_unblocked: false,
      is_type_mismatch: false,
    }
  }

  /// 创建多元素结果 (BLMPOP)
  #[inline]
  pub fn multi(key: impl Into<Bytes>, items: Vec<Bytes>) -> Self {
    Self {
      key: Some(key.into()),
      item: None,
      score: None,
      items: Some(items),
      scores: None,
      is_force_unblocked: false,
      is_type_mismatch: false,
    }
  }

  /// 创建带分数的单元素结果 (BZPOPMIN / BZPOPMAX)
  #[inline]
  pub fn single_scored(key: impl Into<Bytes>, score: f64, item: impl Into<Bytes>) -> Self {
    Self {
      key: Some(key.into()),
      item: Some(item.into()),
      score: Some(score),
      items: None,
      scores: None,
      is_force_unblocked: false,
      is_type_mismatch: false,
    }
  }

  /// 创建带分数的多个元素结果 (BZMPOP)
  #[inline]
  pub fn multi_scored(key: impl Into<Bytes>, scores: Vec<f64>, items: Vec<Bytes>) -> Self {
    Self {
      key: Some(key.into()),
      item: None,
      score: None,
      items: Some(items),
      scores: Some(scores),
      is_force_unblocked: false,
      is_type_mismatch: false,
    }
  }

  /// 是否成功检索到元素 (对应 Garnet Found)
  #[inline]
  pub const fn found(&self) -> bool {
    self.key.is_some() && !self.is_force_unblocked && !self.is_type_mismatch
  }

  /// 是否为空结果
  #[inline]
  pub const fn is_empty(&self) -> bool {
    self.key.is_none() && !self.is_force_unblocked && !self.is_type_mismatch
  }

  /// 是否为强制解阻结果
  #[inline]
  pub const fn is_force_unblocked(&self) -> bool {
    self.is_force_unblocked
  }

  /// 是否为类型不匹配
  #[inline]
  pub const fn is_type_mismatch(&self) -> bool {
    self.is_type_mismatch
  }
}
