use std::sync::Arc;

use bytes::Bytes;

use crate::observer::CollectionItemObserver;

/// CollectionItemBroker 事件类型
///
/// 1:1 对齐 Microsoft Garnet `CollectionItemBrokerEventType`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum CollectionItemBrokerEventType {
  /// 未设置状态
  #[default]
  NotSet = 0,
  /// 注册新观察者事件
  NewObserver = 1,
  /// 集合数据更新事件 (写入或推送元素)
  CollectionUpdated = 2,
}

/// CollectionItemBroker 核心调度事件
///
/// 1:1 对齐 Microsoft Garnet `CollectionItemBrokerEvent`
#[derive(Debug, Clone)]
pub enum CollectionItemBrokerEvent {
  /// 默认未初始化状态
  NotSet,
  /// 集合已更新事件
  CollectionUpdated {
    /// 发生更新的集合 Key
    key: Bytes,
  },
  /// 新观察者等待事件
  NewObserver {
    /// 观察者句柄
    observer: Arc<CollectionItemObserver>,
    /// 观察者关注的 Keys 列表 (按优先级排序)
    keys: Vec<Bytes>,
  },
}

impl Default for CollectionItemBrokerEvent {
  #[inline]
  fn default() -> Self {
    Self::NotSet
  }
}

impl CollectionItemBrokerEvent {
  /// 创建集合更新事件
  #[inline]
  pub fn create_collection_updated_event(key: impl Into<Bytes>) -> Self {
    Self::CollectionUpdated { key: key.into() }
  }

  /// 创建新观察者注册事件
  #[inline]
  pub fn create_new_observer_event(
    observer: Arc<CollectionItemObserver>,
    keys: Vec<Bytes>,
  ) -> Self {
    Self::NewObserver { observer, keys }
  }

  /// 获取当前事件类型标识
  #[inline]
  pub const fn event_type(&self) -> CollectionItemBrokerEventType {
    match self {
      Self::NotSet => CollectionItemBrokerEventType::NotSet,
      Self::NewObserver { .. } => CollectionItemBrokerEventType::NewObserver,
      Self::CollectionUpdated { .. } => CollectionItemBrokerEventType::CollectionUpdated,
    }
  }

  /// 判断当前事件是否为未初始化状态
  #[inline]
  pub const fn is_default(&self) -> bool {
    matches!(self, Self::NotSet)
  }
}

#[cfg(test)]
mod tests {
  use bytes::Bytes;
  use wedb_resp::RespCommand;

  use super::{CollectionItemBrokerEvent, CollectionItemBrokerEventType};
  use crate::observer::CollectionItemObserver;

  #[test]
  fn test_event_types_and_defaults() {
    let def = CollectionItemBrokerEvent::default();
    assert!(def.is_default());
    assert_eq!(def.event_type(), CollectionItemBrokerEventType::NotSet);

    let updated = CollectionItemBrokerEvent::create_collection_updated_event(&b"test_k"[..]);
    assert!(!updated.is_default());
    assert_eq!(
      updated.event_type(),
      CollectionItemBrokerEventType::CollectionUpdated
    );

    let (obs, _rx) = CollectionItemObserver::new(1, RespCommand::Blpop, vec![]);
    let new_obs = CollectionItemBrokerEvent::create_new_observer_event(
      obs,
      vec![Bytes::from_static(b"test_k")],
    );
    assert!(!new_obs.is_default());
    assert_eq!(
      new_obs.event_type(),
      CollectionItemBrokerEventType::NewObserver
    );
  }
}
