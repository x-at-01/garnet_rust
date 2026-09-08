//! 并发注册 / 竞争 / 记账不变式压测

use std::{
  slice::from_ref,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread,
  time::Duration,
};

use aok::{OK, Void};
use bytes::Bytes;
use compio::{
  runtime::{Runtime, spawn},
  time::sleep,
};
use log::info;
use wedb_blocking::{CollectionItemBroker, CollectionItemObserver};
use wedb_resp::RespCommand;
use whasher::HashSet;

use crate::support::exact_broker;

/// 对标 Garnet 无: 并发注册 + 并发写入压测 —— 不丢失、不重复、不 panic，剩余数量精确
#[test]
fn test_concurrent_registration_assignment_stress() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();
    let key = Bytes::from_static(b"stress_key");
    let total = 60;

    // 8 个客户端依次注册阻塞
    let mut handles = Vec::new();
    for i in 0..8 {
      let b = broker.clone();
      let k = key.clone();
      handles.push(spawn(async move {
        let (obs, rx) = CollectionItemObserver::new(2000 + i, RespCommand::Blpop, vec![]);
        b.get_collection_item_async(obs, &[k], 5.0, rx).await
      }));
      sleep(Duration::from_millis(3)).await;
    }

    // 写入任务并发推送 60 个元素并逐一触发唤醒
    let broker_clone = broker.clone();
    let store_clone = store.clone();
    let writer = spawn(async move {
      for i in 0..total {
        let val = format!("sval_{}", i);
        store_clone.push_list_right(b"stress_key", val).unwrap();
        broker_clone.handle_collection_update(b"stress_key");
      }
    });
    writer.await.unwrap();

    let mut received = Vec::new();
    for h in handles {
      let res = h.await.unwrap();
      if res.found() {
        received.push(res.item.unwrap());
      }
    }

    // 8 名观察者最多消费 8 个元素，且绝不重复
    let distinct: HashSet<_> = received.iter().collect();
    assert_eq!(distinct.len(), received.len(), "元素不允许重复投递");
    assert!(received.len() <= 8);
    // 剩余元素数量必须精确：总推入数 - 已消费数
    assert_eq!(store.len(b"stress_key"), total - received.len());
    // 无残留等待者
    assert_eq!(broker.waiting_count(b"stress_key"), 0);
  });

  info!("并发注册与写入压测通过");
  OK
}

/// 对标 Garnet 无: notify_waiters 与并发终结（CLIENT UNBLOCK）竞争下的记账不变式压测
///
/// 每次到达必须要么原子认领并真实投递（唤醒数 == 投递数），
/// 要么落入 pending_signals 记账并经后续注册的核验提示补偿，绝不允许凭空蒸发
#[test]
fn test_notify_vs_unblock_race_accounting() -> Void {
  Runtime::new()?.block_on(async {
    let broker = Arc::new(CollectionItemBroker::new());
    let key = Bytes::from_static(b"cg_key");

    // 消化首次注册的核验提示，预先建立条目
    let warmup = broker
      .get_collection_item(0, RespCommand::Blpop, from_ref(&key), 0.05, vec![])
      .await;
    let _ = warmup;

    const CLIENTS: u64 = 5;

    // 5 名观察者进入等待队列（一次性任务，结果二选一：核验提示 / 强制解除）
    let mut tasks = Vec::new();
    for i in 0..CLIENTS {
      let b = broker.clone();
      let k = key.clone();
      tasks.push(spawn(async move {
        b.get_collection_item(100 + i, RespCommand::Blpop, &[k], 10.0, vec![])
          .await
      }));
    }
    sleep(Duration::from_millis(50)).await;

    const ARRIVALS: usize = 64;
    let done = Arc::new(AtomicBool::new(false));

    // 终结线程：首轮无条件全员 CLIENT UNBLOCK，随后与投递并发竞争
    // （刻意使用 OS 线程制造与异步任务的真实并行竞争，不能转为运行时任务）
    let b_kill = broker.clone();
    let done_kill = done.clone();
    let killer = thread::spawn(move || {
      loop {
        for i in 0..CLIENTS {
          let _ = b_kill.try_unblock(100 + i, true);
        }
        if done_kill.load(Ordering::Acquire) {
          break;
        }
        thread::yield_now();
      }
    });

    // 通知线程：并发投递 64 次到达，累计唤醒数（与终结线程保持同构的 OS 线程竞争）
    let b_notify = broker.clone();
    let k_notify = key.clone();
    let done_notify = done.clone();
    let notifier = thread::spawn(move || {
      let mut woken = 0;
      for _ in 0..ARRIVALS {
        woken += b_notify.notify_waiters(&k_notify, 1);
      }
      done_notify.store(true, Ordering::Release);
      woken
    });

    let woken = notifier.join().unwrap();
    killer.join().unwrap();

    // 回收观察者结果并分类
    let mut delivered = 0;
    let mut forced = 0;
    for t in tasks {
      let res = t.await.unwrap();
      if res.is_force_unblocked() {
        forced += 1;
      } else if res.key.is_some() {
        assert!(res.item.is_none(), "核验提示不应携带元素: {res:?}");
        delivered += 1;
      }
    }

    // 生命周期闭环：每名观察者必须被投递或强制解除之一，不得静默超时
    assert_eq!(
      delivered + forced,
      CLIENTS as usize,
      "观察者不得无故滞留: delivered={delivered}, forced={forced}"
    );

    // 不变式 1：唤醒数必须与真实投递数精确相等（虚报即记账丢失）
    assert_eq!(
      woken, delivered,
      "唤醒数不得虚报: woken={woken}, delivered={delivered}"
    );

    // 不变式 2：未投递的到达不得蒸发——后续注册必须立刻收到核验提示补偿
    if delivered < ARRIVALS {
      let res = broker
        .get_collection_item(9999, RespCommand::Blpop, from_ref(&key), 0.5, vec![])
        .await;
      assert!(
        res.found(),
        "未投递的到达必须经记账补偿: delivered={delivered}/{ARRIVALS}"
      );
      assert!(res.item.is_none(), "补偿仅为核验提示: {res:?}");
    }

    assert_eq!(broker.waiting_count(&key), 0);
  });

  info!("notify 并发终结记账不变式压测通过");
  OK
}
