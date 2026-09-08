use std::sync::Arc;

use bytes::Bytes;
use whasher::{GxPapayaMap, GxPapayaSet, new_papaya_map, new_papaya_set};

use crate::{channel::ChannelManager, pattern::PatternManager, session::SessionHandle};

/// 订阅键类别（精准频道 / 通配模式），统一驱动四组订阅语义的公共实现
#[derive(Clone, Copy)]
enum KeyKind {
  /// 精准频道（对应 C# `subscriptions`）
  Channel,
  /// 通配模式（对应 C# `patternSubscriptions`）
  Pattern,
}

/// 客户端会话的订阅状态记录
struct SessionState {
  channels: GxPapayaSet<Bytes>,
  patterns: GxPapayaSet<Bytes>,
}

impl SessionState {
  fn new() -> Self {
    Self {
      channels: new_papaya_set(),
      patterns: new_papaya_set(),
    }
  }

  /// 会话是否已无任何订阅
  fn is_empty(&self) -> bool {
    self.channels.pin().is_empty() && self.patterns.pin().is_empty()
  }
}

/// 高并发零拷贝发布订阅 Broker
/// 对标 Microsoft Garnet `SubscribeBroker`：
/// C# 的 AOF 异步发布（TsavoriteLog 落盘重放）由本工作区其他模块承担，
/// 此处统一走 `PublishNow` 同步广播语义，并以会话维度状态表替代 C# 的全表扫描
pub struct SubscribeBroker {
  channels: ChannelManager,
  patterns: PatternManager,
  sessions: GxPapayaMap<u64, Arc<SessionState>>,
}

impl Default for SubscribeBroker {
  fn default() -> Self {
    Self::new()
  }
}

impl SubscribeBroker {
  /// 创建新的发布订阅中继器
  pub fn new() -> Self {
    Self {
      channels: ChannelManager::new(),
      patterns: PatternManager::new(),
      sessions: new_papaya_map(),
    }
  }

  /// 获取或新建会话状态
  #[inline]
  fn state_of(&self, session_id: u64) -> Arc<SessionState> {
    Arc::clone(
      self
        .sessions
        .pin()
        .get_or_insert_with(session_id, || Arc::new(SessionState::new())),
    )
  }

  /// 会话登记订阅键：先记会话状态，再注册至对应管理器
  fn grant_key(&self, kind: KeyKind, key: &[u8], session: &SessionHandle) -> bool {
    let state = self.state_of(session.id);
    let is_new = match kind {
      KeyKind::Channel => {
        let pin = state.channels.pin();
        if pin.contains(key) {
          false
        } else {
          pin.insert(Bytes::copy_from_slice(key))
        }
      }
      KeyKind::Pattern => {
        let pin = state.patterns.pin();
        if pin.contains(key) {
          false
        } else {
          pin.insert(Bytes::copy_from_slice(key))
        }
      }
    };
    if !is_new {
      return false;
    }
    match kind {
      KeyKind::Channel => self.channels.subscribe(key, session),
      KeyKind::Pattern => self.patterns.subscribe(key, session),
    }
  }

  /// 会话摘除订阅键：摘除成功且会话状态归零时原子回收会话条目
  fn revoke_key(&self, kind: KeyKind, key: &[u8], session: &SessionHandle) -> bool {
    let pin = self.sessions.pin();
    let removed = match pin.get(&session.id) {
      Some(state) => {
        let removed = match kind {
          KeyKind::Channel => state.channels.pin().remove(key),
          KeyKind::Pattern => state.patterns.pin().remove(key),
        };
        if removed && state.is_empty() {
          let _ = pin.remove_if(&session.id, |_, current| {
            Arc::ptr_eq(current, state) && current.is_empty()
          });
        }
        removed
      }
      None => false,
    };
    if removed {
      match kind {
        KeyKind::Channel => self.channels.unsubscribe(key, session.id),
        KeyKind::Pattern => self.patterns.unsubscribe(key, session.id),
      };
      true
    } else {
      false
    }
  }

  /// 会话全量摘除指定类别的订阅键，返回被摘除键列表（对标空参数 `UNSUBSCRIBE`/`PUNSUBSCRIBE`）
  fn revoke_all(&self, kind: KeyKind, session: &SessionHandle) -> Vec<Bytes> {
    let pin = self.sessions.pin();
    let Some(state) = pin.get(&session.id) else {
      return Vec::new();
    };
    let keys: Vec<Bytes> = match kind {
      KeyKind::Channel => {
        let pin = state.channels.pin();
        let keys: Vec<Bytes> = pin.iter().cloned().collect();
        pin.clear();
        keys
      }
      KeyKind::Pattern => {
        let pin = state.patterns.pin();
        let keys: Vec<Bytes> = pin.iter().cloned().collect();
        pin.clear();
        keys
      }
    };
    for key in &keys {
      match kind {
        KeyKind::Channel => self.channels.unsubscribe(key, session.id),
        KeyKind::Pattern => self.patterns.unsubscribe(key, session.id),
      };
    }
    if state.is_empty() {
      let _ = pin.remove_if(&session.id, |_, current| {
        Arc::ptr_eq(current, state) && current.is_empty()
      });
    }
    keys
  }

  /// 读取会话已登记的指定类别订阅键快照
  fn keys_of(&self, session_id: u64, kind: KeyKind) -> Vec<Vec<u8>> {
    let pin = self.sessions.pin();
    match pin.get(&session_id) {
      Some(state) => match kind {
        KeyKind::Channel => state.channels.pin().iter().map(|b| b.to_vec()).collect(),
        KeyKind::Pattern => state.patterns.pin().iter().map(|b| b.to_vec()).collect(),
      },
      None => Vec::new(),
    }
  }

  /// 会话精准订阅指定频道
  /// 若该会话此前未订阅此频道则返回 true，已订阅返回 false
  /// 对应 C# `Subscribe`
  #[inline]
  pub fn subscribe(&self, channel: &[u8], session: &SessionHandle) -> bool {
    self.grant_key(KeyKind::Channel, channel, session)
  }

  /// 会话订阅指定通配模式（对标 Garnet / Redis `PSUBSCRIBE`）
  /// 若该会话此前未订阅此模式则返回 true，已订阅返回 false
  /// 对应 C# `PatternSubscribe`
  #[inline]
  pub fn psubscribe(&self, pattern: &[u8], session: &SessionHandle) -> bool {
    self.grant_key(KeyKind::Pattern, pattern, session)
  }

  /// 会话订阅指定通配模式（别名，保持兼容）
  #[inline]
  pub fn pattern_subscribe(&self, pattern: &[u8], session: &SessionHandle) -> bool {
    self.psubscribe(pattern, session)
  }

  /// 会话退订指定频道
  /// 若退订成功返回 true，此前未订阅返回 false
  /// 对应 C# `Unsubscribe`
  #[inline]
  pub fn unsubscribe(&self, channel: &[u8], session: &SessionHandle) -> bool {
    self.revoke_key(KeyKind::Channel, channel, session)
  }

  /// 会话全量退订所有精准频道（对标空参数 `UNSUBSCRIBE`），返回被退订的频道列表
  #[inline]
  pub fn unsubscribe_all(&self, session: &SessionHandle) -> Vec<Bytes> {
    self.revoke_all(KeyKind::Channel, session)
  }

  /// 会话退订指定通配模式（对标 Garnet / Redis `PUNSUBSCRIBE`）
  /// 若退订成功返回 true，此前未订阅返回 false
  /// 对应 C# `PatternUnsubscribe`
  #[inline]
  pub fn punsubscribe(&self, pattern: &[u8], session: &SessionHandle) -> bool {
    self.revoke_key(KeyKind::Pattern, pattern, session)
  }

  /// 会话退订指定通配模式（别名，保持兼容）
  #[inline]
  pub fn pattern_unsubscribe(&self, pattern: &[u8], session: &SessionHandle) -> bool {
    self.punsubscribe(pattern, session)
  }

  /// 会话全量退订所有通配模式（对标空参数 `PUNSUBSCRIBE`），返回被退订的模式列表
  #[inline]
  pub fn punsubscribe_all(&self, session: &SessionHandle) -> Vec<Bytes> {
    self.revoke_all(KeyKind::Pattern, session)
  }

  /// 客户端断开连接时，原子清除该会话的所有频道订阅和模式订阅（零孤儿注册项、零内存泄漏）
  /// 对应 C# `RemoveSubscription`：C# 遍历全表逐条目摘除（O(全表)），
  /// 此处凭会话状态直达该会话的订阅键（O(该会话订阅数)），语义等价且更优
  pub fn remove_subscription(&self, session_id: u64) {
    let pin = self.sessions.pin();
    let Some(state) = pin.remove(&session_id) else {
      return;
    };
    for ch in state.channels.pin().iter() {
      self.channels.unsubscribe(ch, session_id);
    }
    for pat in state.patterns.pin().iter() {
      self.patterns.unsubscribe(pat, session_id);
    }
  }

  /// 列出指定会话当前订阅的所有频道列表
  #[inline]
  pub fn list_all_subscriptions(&self, session_id: u64) -> Vec<Vec<u8>> {
    self.keys_of(session_id, KeyKind::Channel)
  }

  /// 列出指定会话当前订阅的所有模式列表
  #[inline]
  pub fn list_all_pattern_subscriptions(&self, session_id: u64) -> Vec<Vec<u8>> {
    self.keys_of(session_id, KeyKind::Pattern)
  }

  /// 获取指定会话当前活跃订阅总数（频道数 + 模式数）
  pub fn session_subscription_count(&self, session_id: u64) -> usize {
    let pin = self.sessions.pin();
    match pin.get(&session_id) {
      Some(state) => state.channels.pin().len() + state.patterns.pin().len(),
      None => 0,
    }
  }

  /// 同步广播消息给所有精准频道订阅者及模式匹配订阅者（零拷贝 Bytes 载荷），返回成功分发的订阅者总数
  /// 自动检测并级联清理断开的会话，平滑降级已满队列
  pub fn publish_bytes(&self, channel: &[u8], payload: Bytes) -> usize {
    let mut dead = Vec::new();
    let ch_sent = self
      .channels
      .broadcast_bytes(channel, payload.clone(), &mut dead);
    let pat_sent = self.patterns.broadcast_bytes(channel, payload, &mut dead);

    if !dead.is_empty() {
      dead.sort_unstable();
      dead.dedup();
      for dead_id in dead {
        self.remove_subscription(dead_id);
      }
    }

    ch_sent + pat_sent
  }

  /// 同步广播消息（对标 Garnet / Redis `PUBLISH`）
  #[inline]
  pub fn publish(&self, channel: &[u8], payload: &[u8]) -> usize {
    self.publish_bytes(channel, Bytes::copy_from_slice(payload))
  }

  /// 同步广播消息（别名，保持兼容）
  #[inline]
  pub fn publish_now(&self, channel: &[u8], payload: &[u8]) -> usize {
    self.publish(channel, payload)
  }

  /// 获取当前所有活跃订阅频道，支持可选通配符模式过滤 (PUBSUB CHANNELS [pattern])
  #[inline]
  pub fn channels(&self, pattern: Option<&[u8]>) -> Vec<Vec<u8>> {
    self.channels.get_active_channels_filter(pattern)
  }

  /// 获取当前所有活跃订阅频道（别名，保持兼容）
  #[inline]
  pub fn get_channels(&self, pattern: Option<&[u8]>) -> Vec<Vec<u8>> {
    self.channels(pattern)
  }

  /// 查询指定频道的活跃订阅者数量 (PUBSUB NUMSUB channel)
  #[inline]
  pub fn numsub(&self, channel: &[u8]) -> usize {
    self.channels.num_subscribers(channel)
  }

  /// 批量查询指定频道的活跃订阅者数量 (PUBSUB NUMSUB channel1 channel2 ...)
  pub fn numsub_multi(&self, channels: &[&[u8]]) -> Vec<(Vec<u8>, usize)> {
    channels
      .iter()
      .map(|&ch| (ch.to_vec(), self.channels.num_subscribers(ch)))
      .collect()
  }

  /// 查询指定频道的活跃订阅者数量（别名，保持兼容）
  #[inline]
  pub fn num_subscriptions(&self, channel: &[u8]) -> usize {
    self.numsub(channel)
  }

  /// 查询具有活跃订阅者的不同模式总数 (PUBSUB NUMPAT)
  #[inline]
  pub fn numpat(&self) -> usize {
    self.patterns.num_patterns()
  }

  /// 查询具有活跃订阅者的不同模式总数（别名，保持兼容）
  #[inline]
  pub fn num_pattern_subscriptions(&self) -> usize {
    self.numpat()
  }
}
