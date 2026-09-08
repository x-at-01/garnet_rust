//! 频道精准订阅管理器
//! 对应 C# `SubscribeBroker` 的 `subscriptions` 字典与 `Broadcast`/`GetChannels` 的精准频道分支

use bytes::Bytes;

use crate::{
  glob::glob_match, message::PubSubMessage, registry::SubscriberRegistry, session::SessionHandle,
};

/// 频道精准订阅管理器
pub struct ChannelManager {
  /// 复用的键控订阅者注册表内核
  registry: SubscriberRegistry,
}

impl Default for ChannelManager {
  fn default() -> Self {
    Self::new()
  }
}

impl ChannelManager {
  /// 创建频道管理器
  pub fn new() -> Self {
    Self {
      registry: SubscriberRegistry::new(),
    }
  }

  /// 订阅指定频道，若该会话首次订阅此频道则返回 true，已存在返回 false
  /// 对应 C# `Subscribe`
  #[inline]
  pub fn subscribe(&self, channel: &[u8], session: &SessionHandle) -> bool {
    self.registry.subscribe(channel, session)
  }

  /// 退订指定频道，若成功移除则返回 true，不存在返回 false
  /// 对应 C# `Unsubscribe`
  #[inline]
  pub fn unsubscribe(&self, channel: &[u8], session_id: u64) -> bool {
    self.registry.unsubscribe(channel, session_id)
  }

  /// 查询指定频道的活跃订阅者总数
  /// 对应 C# `NumSubscriptions`
  #[inline]
  pub fn num_subscribers(&self, channel: &[u8]) -> usize {
    self.registry.num_subscribers(channel)
  }

  /// 获取所有具有活跃订阅者的频道列表（支持通配符模式单次遍历过滤，避免冗余堆分配）
  /// 对应 C# `GetChannels` / `GetChannels(pattern)`
  pub fn get_active_channels_filter(&self, pattern: Option<&[u8]>) -> Vec<Vec<u8>> {
    let pin = self.registry.entries.pin();
    pin
      .iter()
      .filter(|(channel, inner)| {
        !inner.pin().is_empty() && pattern.is_none_or(|pat| glob_match(pat, channel, false))
      })
      .map(|(channel, _)| channel.to_vec())
      .collect()
  }

  /// 向指定频道的所有订阅者同步广播消息（零拷贝 Bytes 载荷），返回成功分发的消息数
  /// 遇到断开连接的会话时收集至 dead 列表，并在内部完成初级清理
  pub fn broadcast_bytes(&self, channel: &[u8], payload: Bytes, dead: &mut Vec<u64>) -> usize {
    self.registry.broadcast_exact(
      channel,
      |ch| PubSubMessage::message(ch.clone(), payload.clone()),
      dead,
    )
  }
}
