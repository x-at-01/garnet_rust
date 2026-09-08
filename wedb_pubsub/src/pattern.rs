//! 模式订阅管理器
//! 对应 C# `SubscribeBroker` 的 `patternSubscriptions` 集合（元素为 `PatternSubscriptionEntry`）：
//! C# 以链表线性扫描组织模式条目，此处复用 [`SubscriberRegistry`] 键控并发字典 + `Arc` 化会话集合，
//! 保留「先 `Match(key, pattern)` 命中再遍历会话」的语义，并增加空条目自动回收

use bytes::Bytes;

use crate::{
  glob::glob_match, message::PubSubMessage, registry::SubscriberRegistry, session::SessionHandle,
};

/// 模式订阅管理器
pub struct PatternManager {
  /// 模式映射到订阅会话集合的键控注册表
  registry: SubscriberRegistry,
}

impl Default for PatternManager {
  fn default() -> Self {
    Self::new()
  }
}

impl PatternManager {
  /// 创建模式订阅管理器
  pub fn new() -> Self {
    Self {
      registry: SubscriberRegistry::new(),
    }
  }

  /// 订阅指定模式，若该会话首次订阅则返回 true，已存在返回 false
  /// 对应 C# `PatternSubscribe` 的 `TryAddAndGet` + `TryAdd`
  #[inline]
  pub fn subscribe(&self, pattern: &[u8], session: &SessionHandle) -> bool {
    self.registry.subscribe(pattern, session)
  }

  /// 退订指定模式，若成功移除则返回 true，否则返回 false
  /// 对应 C# `PatternUnsubscribe` 的 `TryRemove`
  #[inline]
  pub fn unsubscribe(&self, pattern: &[u8], session_id: u64) -> bool {
    self.registry.unsubscribe(pattern, session_id)
  }

  /// 获取具有活跃订阅者的不同模式总数 (PUBSUB NUMPAT)
  /// 对应 C# `NumPatternSubscriptions`
  #[inline]
  pub fn num_patterns(&self) -> usize {
    self.registry.num_non_empty_keys()
  }

  /// 向所有匹配给定频道的模式订阅者广播消息（零拷贝 Bytes 载荷），返回成功分发的消息数
  /// 遇到断开连接的会话时收集至 dead 列表，并在内部完成初级清理
  pub fn broadcast_bytes(&self, channel: &[u8], payload: Bytes, dead: &mut Vec<u64>) -> usize {
    // 首个命中会话出现时才物化频道副本，无订阅者发布零分配
    let mut ch: Option<Bytes> = None;
    self.registry.broadcast_scattered(
      // 匹配方向与 C# `Match(key, pattern)` 一致：注册键为模式、被匹配者为发布频道
      |pattern| glob_match(pattern, channel, false),
      |pattern| {
        PubSubMessage::pmessage(
          pattern.clone(),
          ch.get_or_insert_with(|| Bytes::copy_from_slice(channel))
            .clone(),
          payload.clone(),
        )
      },
      dead,
    )
  }

  /// 向所有匹配给定频道的模式订阅者广播消息，返回成功分发的消息数
  #[inline]
  pub fn broadcast(&self, channel: &[u8], payload: &[u8]) -> usize {
    let mut dead = Vec::new();
    self.broadcast_bytes(channel, Bytes::copy_from_slice(payload), &mut dead)
  }
}
