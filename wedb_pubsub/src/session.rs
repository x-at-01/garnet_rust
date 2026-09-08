use std::sync::Arc;

use crossfire::{
  AsyncRx as CfAsyncRx, MAsyncTx, TrySendError,
  mpsc::{Array, bounded_async},
};

use crate::message::PubSubMessage;

/// 会话消息发送状态枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SendStatus {
  /// 发送成功
  Success,
  /// 会话队列已满（背压平滑降级，丢弃单条消息）
  Full,
  /// 会话连接已断开（接收端已释放，需级联清理）
  Disconnected,
}

/// 异步多生产者发送端
pub type AsyncTx<T> = MAsyncTx<Array<T>>;

/// 异步接收端
pub type AsyncRx<T> = CfAsyncRx<Array<T>>;

/// 订阅客户端会话连接结构体
#[derive(Debug)]
pub struct SubscriberSession {
  /// 会话唯一标识符
  pub id: u64,
  /// 异步非阻塞发送通道
  pub tx: AsyncTx<PubSubMessage>,
}

impl SubscriberSession {
  /// 创建新的订阅者会话
  #[inline]
  pub fn new(id: u64, tx: AsyncTx<PubSubMessage>) -> Self {
    Self { id, tx }
  }

  /// 发送消息并返回详细状态
  #[inline]
  pub fn send_msg(&self, msg: PubSubMessage) -> SendStatus {
    match self.tx.try_send(msg) {
      Ok(()) => SendStatus::Success,
      Err(TrySendError::Full(_)) => SendStatus::Full,
      Err(TrySendError::Disconnected(_)) => SendStatus::Disconnected,
    }
  }

  /// 非阻塞向客户端发送一条消息
  /// 若成功返回 true，若队列满或连接已断开则返回 false
  #[inline]
  pub fn try_send(&self, msg: PubSubMessage) -> bool {
    self.send_msg(msg) == SendStatus::Success
  }

  /// 检查当前会话接收端是否仍然连通
  #[inline]
  pub fn is_connected(&self) -> bool {
    !self.tx.is_disconnected()
  }

  /// 触发会话关闭停机信号，通知接收转发协程立即终止
  #[inline]
  pub fn close(&self) {
    let _ = self.tx.try_send(PubSubMessage::Close);
  }
}

/// 订阅者会话共享句柄
pub type SessionHandle = Arc<SubscriberSession>;

/// 便捷创建订阅会话及其接收通道
pub fn create_session(id: u64, capacity: usize) -> (SessionHandle, AsyncRx<PubSubMessage>) {
  let (tx, rx) = bounded_async(capacity);
  (Arc::new(SubscriberSession::new(id, tx)), rx)
}
