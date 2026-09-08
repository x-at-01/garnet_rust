//! 键控订阅者注册表内核：`ChannelManager` 与 `PatternManager` 复用的公共底座
//! 对应 C# `SubscribeBroker` 中
//! `ConcurrentDictionary<ByteArrayWrapper, ReadOptimizedConcurrentSet<ServerSessionBase>>`
//! 与 `ReadOptimizedConcurrentSet<PatternSubscriptionEntry>` 两类容器（Rust 化统一为 键 → 会话集合）；
//! C# 的键条目不回收（退订后残留空集合），此处订阅者归零即原子摘除外层条目，杜绝内存泄漏

use std::sync::Arc;

use bytes::Bytes;
use whasher::{GxPapayaMap, new_papaya_map};

use crate::{
  message::PubSubMessage,
  session::{SendStatus, SessionHandle},
};

/// 内层会话集合：会话 id → 会话句柄
type SessionMap = GxPapayaMap<u64, SessionHandle>;

/// 创建空的会话集合
#[inline]
fn new_session_map() -> Arc<SessionMap> {
  Arc::new(new_papaya_map())
}

/// 键（频道名 / 模式）→ 订阅会话集合 的订阅者注册表
pub struct SubscriberRegistry {
  /// 键到会话集合的并发字典（对应 C# `subscriptions` / `patternSubscriptions`）
  pub(crate) entries: GxPapayaMap<Bytes, Arc<SessionMap>>,
}

impl Default for SubscriberRegistry {
  fn default() -> Self {
    Self::new()
  }
}

impl SubscriberRegistry {
  /// 创建空注册表
  pub fn new() -> Self {
    Self {
      entries: new_papaya_map(),
    }
  }

  /// 订阅指定键，若该会话首次订阅返回 true，已订阅返回 false
  /// 对应 C# `Subscribe`/`PatternSubscribe` 中 `TryAdd(session)` 语义
  pub fn subscribe(&self, key: &[u8], session: &SessionHandle) -> bool {
    let pin = self.entries.pin();
    loop {
      let inner = if let Some(map) = pin.get(key) {
        Arc::clone(map)
      } else {
        Arc::clone(pin.get_or_insert_with(Bytes::copy_from_slice(key), new_session_map))
      };
      let inserted = inner.pin().insert(session.id, session.clone()).is_none();
      // 乐观验证外层条目未被并发回收重建（避免 ABA 脱落），失败则重试
      if pin
        .get(key)
        .is_some_and(|current| Arc::ptr_eq(current, &inner))
      {
        return inserted;
      }
    }
  }

  /// 退订指定键，成功移除返回 true，不存在返回 false
  /// 当该键所有订阅者离开时原子回收外层条目，防止内存泄漏
  pub fn unsubscribe(&self, key: &[u8], session_id: u64) -> bool {
    let pin = self.entries.pin();
    let Some(inner) = pin.get(key) else {
      return false;
    };
    let inner_pin = inner.pin();
    let removed = inner_pin.remove(&session_id).is_some();
    if removed && inner_pin.is_empty() {
      self.reap_if_empty(key, inner);
    }
    removed
  }

  /// 查询指定键的活跃订阅者数量（对应 C# `NumSubscriptions`）
  pub fn num_subscribers(&self, key: &[u8]) -> usize {
    self
      .entries
      .pin()
      .get(key)
      .map_or(0, |inner| inner.pin().len())
  }

  /// 统计具有活跃订阅者的不同键数量（对应 C# `NumPatternSubscriptions` 计数语义）
  pub(crate) fn num_non_empty_keys(&self) -> usize {
    self
      .entries
      .pin()
      .iter()
      .filter(|(_, inner)| !inner.pin().is_empty())
      .count()
  }

  /// 向键恰好等于 `key` 的条目下全部会话广播（消息由键派生），返回成功投递数
  /// 对应 C# `Broadcast` 的精准频道分支；断连会话级联清理，空条目原子回收
  pub(crate) fn broadcast_exact(
    &self,
    key: &[u8],
    make_msg: impl FnMut(&Bytes) -> PubSubMessage,
    dead: &mut Vec<u64>,
  ) -> usize {
    let pin = self.entries.pin();
    let Some((entry, inner)) = pin.get_key_value(key) else {
      return 0;
    };
    let inner_pin = inner.pin();
    if inner_pin.is_empty() {
      drop(inner_pin);
      self.reap_if_empty(entry, inner);
      return 0;
    }

    let mut make_msg = make_msg;
    let mut dead_in_key = Vec::new();
    let sent = fanout(|| make_msg(entry), inner_pin.values(), &mut dead_in_key);
    self.prune_dead(entry, inner, &dead_in_key, dead);
    sent
  }

  /// 向所有键满足 `matched` 的条目下会话广播（消息由键派生），返回成功投递数
  /// 对应 C# `Broadcast` 的模式分支（`Match(key, pattern)` 全表扫描 + 逐会话投递）；
  /// 断连会话级联清理，空条目顺带原子回收
  pub(crate) fn broadcast_scattered(
    &self,
    matched: impl Fn(&Bytes) -> bool,
    make_msg: impl FnMut(&Bytes) -> PubSubMessage,
    dead: &mut Vec<u64>,
  ) -> usize {
    let pin = self.entries.pin();
    let mut make_msg = make_msg;
    let mut sent = 0;
    let mut dead_in_key = Vec::new();
    for (key, inner) in pin.iter() {
      if !matched(key) {
        continue;
      }
      let inner_pin = inner.pin();
      if inner_pin.is_empty() {
        drop(inner_pin);
        self.reap_if_empty(key, inner);
        continue;
      }
      dead_in_key.clear();
      sent += fanout(|| make_msg(key), inner_pin.values(), &mut dead_in_key);
      self.prune_dead(key, inner, &dead_in_key, dead);
    }
    sent
  }

  /// 将条目内断连会话摘除并汇入调用方 `dead` 列表；条目订阅者归零时原子回收外层条目
  fn prune_dead(
    &self,
    key: &[u8],
    inner: &Arc<SessionMap>,
    dead_in_key: &[u64],
    dead: &mut Vec<u64>,
  ) {
    if dead_in_key.is_empty() {
      return;
    }
    let inner_pin = inner.pin();
    for &id in dead_in_key {
      inner_pin.remove(&id);
    }
    dead.extend_from_slice(dead_in_key);
    if inner_pin.is_empty() {
      drop(inner_pin);
      self.reap_if_empty(key, inner);
    }
  }

  /// 若键对应的会话集合为空则原子摘除外层条目（Arc 比对防 ABA 误删重建条目）
  pub(crate) fn reap_if_empty(&self, key: &[u8], inner: &Arc<SessionMap>) {
    let pin = self.entries.pin();
    let _ = pin.remove_if(key, |_, current| {
      Arc::ptr_eq(current, inner) && current.pin().is_empty()
    });
  }
}

/// 向会话集合逐一投递消息（每个会话独立克隆消息载荷），返回成功投递数
/// 断连会话 id 收集进 `dead` 供调用方级联清理；队列已满的慢订阅者平滑丢弃当条消息，不阻塞广播
fn fanout<'a>(
  mut make_msg: impl FnMut() -> PubSubMessage,
  sessions: impl Iterator<Item = &'a SessionHandle>,
  dead: &mut Vec<u64>,
) -> usize {
  let mut sent = 0;
  for session in sessions {
    match session.send_msg(make_msg()) {
      SendStatus::Success => sent += 1,
      SendStatus::Full => {}
      SendStatus::Disconnected => dead.push(session.id),
    }
  }
  sent
}

#[cfg(test)]
mod tests {
  use super::*;
  use crate::session::create_session;

  /// 外层条目物理数量
  fn entry_count(registry: &SubscriberRegistry) -> usize {
    registry.entries.pin().len()
  }

  /// 向键 `chan` 广播静态载荷
  fn broadcast_chan(registry: &SubscriberRegistry, dead: &mut Vec<u64>) -> usize {
    registry.broadcast_exact(
      b"chan",
      |ch| PubSubMessage::message(ch.clone(), Bytes::from_static(b"x")),
      dead,
    )
  }

  /// 订阅者归零后外层条目物理回收（零内存泄漏）
  #[test]
  fn reclaims_entry_when_last_subscriber_leaves() {
    let registry = SubscriberRegistry::new();
    let (s1, rx1) = create_session(1, 8);
    let (s2, rx2) = create_session(2, 8);

    assert!(registry.subscribe(b"chan", &s1));
    assert!(registry.subscribe(b"chan", &s2));
    assert_eq!(entry_count(&registry), 1);

    assert!(registry.unsubscribe(b"chan", 1));
    assert_eq!(entry_count(&registry), 1, "仍有订阅者，条目保留");

    assert!(registry.unsubscribe(b"chan", 2));
    assert_eq!(entry_count(&registry), 0, "订阅者归零，条目物理摘除");
    drop((rx1, rx2));
  }

  /// ABA 防护：迟滞的旧条目句柄回收不得误删并发重建的新条目
  #[test]
  fn stale_reap_never_removes_rebuilt_entry() {
    let registry = SubscriberRegistry::new();
    let (s1, rx1) = create_session(1, 8);
    let (s2, rx2) = create_session(2, 8);

    assert!(registry.subscribe(b"chan", &s1));
    let stale = registry
      .entries
      .pin()
      .get(b"chan".as_slice())
      .map(Arc::clone)
      .unwrap();

    assert!(registry.unsubscribe(b"chan", 1));
    assert_eq!(entry_count(&registry), 0);
    assert!(registry.subscribe(b"chan", &s2), "模拟并发重建条目");

    // 迟滞线程持旧 Arc 执行回收（等价并发 remove_if）
    registry.reap_if_empty(b"chan", &stale);
    assert_eq!(entry_count(&registry), 1, "旧 Arc 回收不得误删重建条目");
    assert_eq!(registry.num_subscribers(b"chan"), 1);
    drop((rx1, rx2));
  }

  /// 广播断连侦测：Disconnected 会话就地摘除、条目随存活者保留、dead 上报供级联清理
  #[test]
  fn broadcast_exact_prunes_disconnected_sessions() {
    let registry = SubscriberRegistry::new();
    let (dead, rx_dead) = create_session(1, 8);
    let (alive, rx_alive) = create_session(2, 8);
    registry.subscribe(b"chan", &dead);
    registry.subscribe(b"chan", &alive);
    drop(rx_dead);

    let mut dead_ids = Vec::new();
    let sent = broadcast_chan(&registry, &mut dead_ids);
    assert_eq!(sent, 1);
    assert_eq!(dead_ids, vec![1]);

    let pin = registry.entries.pin();
    assert_eq!(pin.len(), 1, "仍有活跃订阅者，条目不回收");
    assert!(
      pin
        .get(b"chan".as_slice())
        .is_some_and(|inner| inner.pin().contains_key(&2))
    );
    drop(pin);
    assert!(matches!(
      rx_alive.try_recv(),
      Ok(PubSubMessage::Message { .. })
    ));
  }

  /// 队列已满（Full）平滑丢弃但保留注册，与 Disconnected 摘除路径明确区分
  #[test]
  fn broadcast_exact_full_drops_message_keeps_session() {
    let registry = SubscriberRegistry::new();
    let (s, rx) = create_session(7, 1); // 容量 1
    registry.subscribe(b"chan", &s);

    let mut dead = Vec::new();
    assert_eq!(broadcast_chan(&registry, &mut dead), 1); // 首条入队
    assert_eq!(broadcast_chan(&registry, &mut dead), 0); // 队列满，平滑丢弃
    assert!(dead.is_empty(), "Full 不得上报断连");
    assert_eq!(registry.num_subscribers(b"chan"), 1, "Full 不得摘除会话");

    assert!(rx.try_recv().is_ok()); // 腾出容量后恢复投递
    assert_eq!(broadcast_chan(&registry, &mut dead), 1);
  }
}
