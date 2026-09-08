//! 信号驱动模式语义测试（核验提示、防丢失唤醒、竞争重阻塞、记账语义）
//!
//! 复现 wedb_server 的信号驱动 BLPOP 协议：预检真实存储 → 循环 [注册等待 → 信号唤醒 →
//! 从真实存储弹出 → 弹空则重试直至超时]

use std::{
  slice::from_ref,
  sync::Arc,
  time::{Duration, Instant},
};

use aok::{OK, Void};
use bytes::Bytes;
use compio::{
  runtime::{Runtime, spawn},
  time::sleep,
};
use log::info;
use wedb_blocking::{CollectionItemBroker, CollectionItemResult, MemoryCollectionStore};
use wedb_resp::RespCommand;

/// 模拟 wedb_server 的信号驱动 BLPOP 协议：
/// 预检真实存储 → 循环 [注册等待 → 信号唤醒 → 从真实存储弹出 → 弹空则重试直至超时]
async fn signal_blpop(
  broker: &CollectionItemBroker,
  real: &MemoryCollectionStore,
  session_id: u64,
  keys: &[Bytes],
  deadline: Instant,
) -> CollectionItemResult {
  loop {
    // 1. 预检真实存储
    for key in keys {
      if let Some(item) = real.pop_list_left(key) {
        return CollectionItemResult::single(key.clone(), item);
      }
    }
    let now = Instant::now();
    if now >= deadline {
      return CollectionItemResult::empty();
    }
    let remaining = (deadline - now).as_secs_f64();

    // 2. 注册等待
    let result = broker
      .get_collection_item(session_id, RespCommand::Blpop, keys, remaining, vec![])
      .await;

    if result.is_force_unblocked() || result.is_type_mismatch() {
      return result;
    }
    // 3. 信号唤醒：从真实存储弹出，弹空则继续循环等待
    if let Some(k) = &result.key {
      if let Some(item) = real.pop_list_left(k) {
        return CollectionItemResult::single(k.clone(), item);
      }
      continue;
    }
    return result;
  }
}

/// 对标 Garnet 无: 注册前无见证的推送经核验提示后仍可被取回 —— 预检与注册之间窗口内的推送不丢失
#[test]
fn test_signal_mode_lost_wakeup_recovered() -> Void {
  Runtime::new()?.block_on(async {
    let broker = Arc::new(CollectionItemBroker::new());
    let real = Arc::new(MemoryCollectionStore::new());

    // 写入方推送时无任何注册条目，notify_waiters 无从唤醒（返回 0）
    real.push_list_right(b"sig_key", "lost_item").unwrap();
    assert_eq!(broker.notify_waiters(b"sig_key", 1), 0);

    // 客户端随后发起 BLPOP：预检未弹到数据时，核验提示必须引导其复检真实存储
    let deadline = Instant::now() + Duration::from_secs(2);
    let res = signal_blpop(
      &broker,
      &real,
      1000,
      &[Bytes::from_static(b"sig_key")],
      deadline,
    )
    .await;

    assert!(res.found(), "窗口内的推送不允许丢失: {res:?}");
    assert_eq!(res.item.as_deref(), Some(&b"lost_item"[..]));
    assert_eq!(real.len(b"sig_key"), 0);
  });

  info!("信号模式丢失唤醒恢复测试通过");
  OK
}

/// 对标 Garnet 无: notify_waiters 直接唤醒已注册的信号模式等待者并取回真实存储数据
#[test]
fn test_signal_mode_notify_wakes_waiter() -> Void {
  Runtime::new()?.block_on(async {
    let broker = Arc::new(CollectionItemBroker::new());
    let real = Arc::new(MemoryCollectionStore::new());
    let key = Bytes::from_static(b"notify_key");

    // 先注册（消化核验提示），确保观察者真正进入等待状态
    let deadline = Instant::now() + Duration::from_millis(80);
    let first = signal_blpop(&broker, &real, 1001, from_ref(&key), deadline).await;
    assert!(first.is_empty(), "首次注册应消化核验提示后超时: {first:?}");

    // 客户端阻塞等待，后台写入真实存储并通知
    let broker_clone = broker.clone();
    let real_clone = real.clone();
    let k = key.clone();
    let task = spawn(async move {
      let deadline = Instant::now() + Duration::from_secs(3);
      signal_blpop(&broker_clone, &real_clone, 1002, &[k], deadline).await
    });

    sleep(Duration::from_millis(30)).await;
    real.push_list_right(b"notify_key", "woken_item").unwrap();
    assert_eq!(broker.notify_waiters(b"notify_key", 1), 1);

    let res = task.await.unwrap();
    assert!(res.found());
    assert_eq!(res.item.as_deref(), Some(&b"woken_item"[..]));
  });

  info!("信号模式通知唤醒测试通过");
  OK
}

/// 对标 Garnet 无: 预检窗口内写入非首选 key 时，核验提示逐 key 引导复检并取回数据
#[test]
fn test_signal_mode_multi_key_window() -> Void {
  Runtime::new()?.block_on(async {
    let broker = Arc::new(CollectionItemBroker::new());
    let real = Arc::new(MemoryCollectionStore::new());

    let key1 = Bytes::from_static(b"m_k1");
    let key2 = Bytes::from_static(b"m_k2");

    // 写入发生在无任何注册条目时（notify 无从唤醒）
    real.push_list_right(b"m_k2", "multi_item").unwrap();
    assert_eq!(broker.notify_waiters(b"m_k2", 1), 0);

    // 客户端监听 [key1, key2]：首个核验提示引导复检 key1（空），
    // 后续注册继续消费 key2 的核验标记并取回数据
    let deadline = Instant::now() + Duration::from_secs(2);
    let res = signal_blpop(&broker, &real, 1003, &[key1, key2], deadline).await;

    assert!(res.found(), "多 key 窗口推送不允许丢失: {res:?}");
    assert_eq!(res.key.as_deref(), Some(&b"m_k2"[..]));
    assert_eq!(res.item.as_deref(), Some(&b"multi_item"[..]));
  });

  info!("信号模式多 key 窗口测试通过");
  OK
}

/// 对标 Garnet 无: 被唤醒后数据已被竞争者抢走时重新阻塞，直至新数据再次到达才取回
#[test]
fn test_signal_mode_competitor_reblock() -> Void {
  Runtime::new()?.block_on(async {
    let broker = Arc::new(CollectionItemBroker::new());
    let real = Arc::new(MemoryCollectionStore::new());
    let key = Bytes::from_static(b"race_key");

    // 客户端阻塞等待；第一次被唤醒时数据已被竞争者抢走（弹空），
    // 必须重新阻塞，直至新数据再次到达
    let broker_clone = broker.clone();
    let real_for_client = real.clone();
    let k = key.clone();
    let task = spawn(async move {
      let deadline = Instant::now() + Duration::from_secs(3);
      signal_blpop(&broker_clone, &real_for_client, 1004, &[k], deadline).await
    });

    // 等客户端消化核验提示并进入阻塞
    sleep(Duration::from_millis(40)).await;

    // 竞争场景：通知一个"已不存在"的到达（数据被抢走），客户端应弹空后重新阻塞
    assert_eq!(broker.notify_waiters(b"race_key", 1), 1);

    // 等客户端弹空并重新进入阻塞
    sleep(Duration::from_millis(40)).await;

    // 新数据到达并通知：客户端最终取回
    real.push_list_right(b"race_key", "second_item").unwrap();
    assert_eq!(broker.notify_waiters(b"race_key", 1), 1);

    let res = task.await.unwrap();
    assert!(res.found());
    assert_eq!(res.item.as_deref(), Some(&b"second_item"[..]));
    assert_eq!(real.len(b"race_key"), 0);
  });

  info!("信号模式竞争重阻塞测试通过");
  OK
}

/// 对标 Garnet 无: 核验提示精确语义 —— 新建条目提示一次，复检为空后不再提示（防空转），
/// 预检后、注册前的推送窗口经记账不丢失
#[test]
fn test_signal_mode_verify_hint_semantics() -> Void {
  Runtime::new()?.block_on(async {
    let broker = Arc::new(CollectionItemBroker::new());
    let real = Arc::new(MemoryCollectionStore::new());
    let key = Bytes::from_static(b"verify_key");

    // 5.1 全新 key 注册：新建条目给出核验提示（仅含 key，不含元素）
    let res = broker
      .get_collection_item(8001, RespCommand::Blpop, from_ref(&key), 5.0, vec![])
      .await;
    assert!(res.found(), "新建条目必须给出核验提示: {res:?}");
    assert_eq!(res.key.as_deref(), Some(&key[..]));
    assert!(res.item.is_none(), "核验提示仅含 key");
    // 复检真实存储为空
    assert!(real.pop_list_left(&key).is_none());

    // 5.2 再次注册：标记已消费，无提示，直接阻塞至超时
    let start = Instant::now();
    let res = broker
      .get_collection_item(8001, RespCommand::Blpop, from_ref(&key), 0.05, vec![])
      .await;
    assert!(res.is_empty(), "已消费标记后不得再次提示: {res:?}");
    assert!(
      start.elapsed() >= Duration::from_millis(40),
      "应真实阻塞而非立即返回"
    );

    // 5.3 预检后、注册前的推送窗口：notify 无见证则记账，注册时经核验提示补偿
    real.push_list_right(&key, "window_item").unwrap();
    assert_eq!(
      broker.notify_waiters(&key, 1),
      0,
      "无等待者时不得虚报唤醒数"
    );
    let res = broker
      .get_collection_item(8001, RespCommand::Blpop, from_ref(&key), 5.0, vec![])
      .await;
    assert!(res.found(), "窗口内推送不得丢失: {res:?}");
    // 按提示复检真实存储取回
    assert_eq!(
      real.pop_list_left(&key).as_deref(),
      Some(&b"window_item"[..])
    );
  });

  info!("信号模式核验提示语义测试通过");
  OK
}
