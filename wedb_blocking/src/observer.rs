use std::sync::Arc;

use bytes::Bytes;
use crossfire::{
  AsyncRx as CfAsyncRx, MAsyncTx,
  mpsc::{Array, bounded_async},
};
use parking_lot::Mutex;
use wedb_resp::RespCommand;

use crate::result::CollectionItemResult;

/// 异步多生产者发送端
pub type ObserverTx = MAsyncTx<Array<CollectionItemResult>>;

/// 异步单/多消费者接收端
pub type ObserverRx = CfAsyncRx<Array<CollectionItemResult>>;

/// 观察者内部状态结构体（合并状态锁与结果锁）
#[derive(Debug)]
struct ObserverInner {
  status: ObserverStatus,
  result: CollectionItemResult,
}

/// 观察者当前状态枚举
///
/// 1:1 对齐 Microsoft Garnet `ObserverStatus`
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum ObserverStatus {
  /// 观察者就绪，等待元素到达
  #[default]
  WaitingForResult = 0,
  /// 结果已被成功设置（已获取元素、超时、或被取消）
  ResultSet = 1,
  /// 发起阻塞调用的会话已被释放关闭
  SessionDisposed = 2,
}

/// 阻塞命令观察者
///
/// 负责维护单次阻塞命令执行时的会话状态、等待通道与通知同步机制。
/// 1:1 对齐 Microsoft Garnet `CollectionItemObserver`
#[derive(Debug)]
pub struct CollectionItemObserver {
  /// 发起调用的会话 ID (对应 Garnet ObjectStoreSessionID)
  pub session_id: u64,
  /// 阻塞操作的 RESP 命令类型 (BLPOP, BRPOP, BLMOVE, BLMPOP, BZPOPMIN, etc.)
  pub command: RespCommand,
  /// 命令附加参数（例如 BLMOVE 的目标 key 与方向，BLMPOP 的弹出数量等）
  pub command_args: Vec<Bytes>,
  /// 观察者内部状态（合并单锁，彻底消除嵌套加锁与锁争抢）
  inner: Mutex<ObserverInner>,
  /// 异步通知通道发送端
  tx: ObserverTx,
}

impl CollectionItemObserver {
  /// 创建新的观察者实例并返回接收端
  #[inline]
  pub fn new(
    session_id: u64,
    command: RespCommand,
    command_args: Vec<Bytes>,
  ) -> (Arc<Self>, ObserverRx) {
    let (tx, rx) = bounded_async(1);
    let observer = Arc::new(Self {
      session_id,
      command,
      command_args,
      inner: Mutex::new(ObserverInner {
        status: ObserverStatus::WaitingForResult,
        result: CollectionItemResult::empty(),
      }),
      tx,
    });
    (observer, rx)
  }

  /// 获取当前观察者状态
  #[inline]
  pub fn status(&self) -> ObserverStatus {
    self.inner.lock().status
  }

  /// 判断观察者是否仍在等待结果
  #[inline]
  pub fn is_waiting(&self) -> bool {
    self.inner.lock().status == ObserverStatus::WaitingForResult
  }

  /// 是否仍可接收投递：处于等待态且接收端未断开
  ///
  /// 接收端断开意味着发起等待的异步任务已被丢弃（如连接异常关闭且未走释放钩子），
  /// 此类观察者必须被视为死条目：既不可作为唤醒目标，也不得吞吃已弹出的元素
  #[inline]
  pub fn is_receptive(&self) -> bool {
    let inner = self.inner.lock();
    inner.status == ObserverStatus::WaitingForResult && !self.tx.is_disconnected()
  }

  /// 获取已设置的结果副本
  #[inline]
  pub fn get_result(&self) -> CollectionItemResult {
    self.inner.lock().result.clone()
  }

  /// 安全设置观察者的最终结果并唤醒等待方
  ///
  /// 若观察者已非 WaitingForResult 状态，则直接忽略。
  pub fn handle_set_result(&self, result: CollectionItemResult) {
    let mut inner = self.inner.lock();
    if inner.status != ObserverStatus::WaitingForResult {
      return;
    }
    inner.status = ObserverStatus::ResultSet;
    inner.result = result.clone();
    drop(inner);
    let _ = self.tx.try_send(result);
  }

  /// 原子认领投递权：在状态锁临界区内完成「仍在等待校验 → 弹出 → 结果设置」
  ///
  /// 对齐 C# 在 ObserverStatusLock 写锁内执行 TryGetResult + HandleSetResult 的原子语义，
  /// 闭合「元素已从存储弹出，结果却被并发超时 / CLIENT UNBLOCK / 会话销毁路径抢占，
  /// 元素凭空丢失」的竞态窗口。
  ///
  /// 返回 None 表示观察者已不可投递（已终结或接收端断开），闭包未执行；
  /// 返回 Some(out) 表示闭包已执行（弹出已发生），命中时结果已同步投递。
  #[inline]
  pub(crate) fn assign_atomic(
    &self,
    pop: impl FnOnce() -> crate::Result<Option<CollectionItemResult>>,
  ) -> Option<crate::Result<Option<CollectionItemResult>>> {
    let mut inner = self.inner.lock();
    if inner.status != ObserverStatus::WaitingForResult || self.tx.is_disconnected() {
      return None;
    }
    let out = pop();
    if let Ok(Some(res)) = &out {
      inner.status = ObserverStatus::ResultSet;
      inner.result = res.clone();
      drop(inner);
      // 容量 1 通道且每个观察者至多发送一次，缓冲区必为空，
      // 仅接收端恰在此刻断开时才会失败（元素已被消费，无法回滚）
      let _ = self.tx.try_send(res.clone());
    }
    Some(out)
  }

  /// 尝试强制解除阻塞 (CLIENT UNBLOCK)
  ///
  /// throw_error: 若为 true，返回 ForceUnblocked 结果；若为 false，返回 Empty 结果。
  pub fn try_force_unblock(&self, throw_error: bool) -> bool {
    let mut inner = self.inner.lock();
    if inner.status != ObserverStatus::WaitingForResult {
      return false;
    }
    inner.status = ObserverStatus::ResultSet;
    let result = if throw_error {
      CollectionItemResult::force_unblocked()
    } else {
      CollectionItemResult::empty()
    };
    inner.result = result.clone();
    drop(inner);
    let _ = self.tx.try_send(result);
    true
  }

  /// 当客户端会话关闭销毁时安全变更状态
  ///
  /// 仅在等待中被销毁时才转移状态并唤醒等待方，避免覆盖已设置的最终结果
  pub fn handle_session_disposed(&self) {
    let mut inner = self.inner.lock();
    if inner.status != ObserverStatus::WaitingForResult {
      return;
    }
    inner.status = ObserverStatus::SessionDisposed;
    drop(inner);
    let _ = self.tx.try_send(CollectionItemResult::empty());
  }
}

#[cfg(test)]
mod tests {
  use wedb_resp::RespCommand;

  use super::{CollectionItemObserver, CollectionItemResult, ObserverStatus};
  use crate::provider::{CollectionProvider, MemoryCollectionStore};

  /// assign_atomic 认领原子性：已终结的观察者认领失败，弹出闭包不得执行，
  /// 闭合「元素已弹出却无人接收」的丢失竞态
  #[test]
  fn assign_atomic_claim_semantics() {
    let store = MemoryCollectionStore::new();
    store.push_list_left(b"claim_key", "val").unwrap();

    // 1. 已终结的观察者：认领失败，弹出闭包不执行
    let (obs1, _rx1) = CollectionItemObserver::new(1, RespCommand::Blpop, vec![]);
    obs1.handle_set_result(CollectionItemResult::empty());
    let mut popped = false;
    let out = obs1.assign_atomic(|| {
      popped = true;
      store.try_pop_item(b"claim_key", RespCommand::Blpop, &[], true)
    });
    assert!(out.is_none());
    assert!(!popped, "已终结观察者的认领不得触发弹出");
    assert_eq!(store.len(b"claim_key"), 1);

    // 2. 等待中的观察者：认领成功，弹出与投递同步完成
    let (obs2, _rx2) = CollectionItemObserver::new(2, RespCommand::Blpop, vec![]);
    let out =
      obs2.assign_atomic(|| store.try_pop_item(b"claim_key", RespCommand::Blpop, &[], true));
    assert!(matches!(out, Some(Ok(Some(_)))));
    assert_eq!(obs2.status(), ObserverStatus::ResultSet);
    assert_eq!(obs2.get_result().item.as_deref(), Some(&b"val"[..]));
    assert_eq!(store.len(b"claim_key"), 0);

    // 3. 认领成功即终结：再次认领失败且不再弹出
    let mut popped = false;
    let out = obs2.assign_atomic(|| {
      popped = true;
      Ok(None)
    });
    assert!(out.is_none());
    assert!(!popped);
  }

  /// 接收端断开（等待任务被丢弃）后，观察者不可再认领投递
  #[test]
  fn assign_atomic_skips_disconnected() {
    let (obs, rx) = CollectionItemObserver::new(3, RespCommand::Blpop, vec![]);
    drop(rx);
    assert!(!obs.is_receptive());
    let mut popped = false;
    let out = obs.assign_atomic(|| {
      popped = true;
      Ok(None)
    });
    assert!(out.is_none());
    assert!(!popped);
  }
}
