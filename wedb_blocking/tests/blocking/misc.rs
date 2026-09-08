//! 杂项语义测试：错误类型、session 销毁、极端超时、观察者清理与死条目回收

use std::{sync::Arc, time::Duration};

use aok::{OK, Void};
use bytes::Bytes;
use compio::{
  runtime::{Runtime, spawn},
  time::{sleep, timeout},
};
use log::info;
use wedb_blocking::{CollectionItemBroker, CollectionItemObserver, MemoryCollectionStore};
use wedb_resp::RespCommand;

use crate::support::exact_broker;

/// 对标 Garnet RespBlockingCollectionTests.BlockingListPopWrongTypeTest:
/// 对 SortedSet key 执行针对 List 的 BLPOP 时直接返回类型不匹配
#[test]
fn test_blpop_wrong_type_rejected() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    // 在 key 上创建 SortedSet 对象
    store.zadd(b"wrong_type_key", 10.0, "member1").unwrap();

    // 尝试执行针对 List 的 BLPOP 操作
    let (obs, rx) = CollectionItemObserver::new(401, RespCommand::Blpop, vec![]);
    let res = broker
      .get_collection_item_async(obs, &[Bytes::from_static(b"wrong_type_key")], 5.0, rx)
      .await;

    // 应当直接返回类型不匹配
    assert!(res.is_type_mismatch());
    assert!(!res.found());
  });

  info!("类型不匹配测试通过");
  OK
}

/// 对齐 Garnet TryAssignItemFromKey 的 failOnSrcTypeMismatch: false:
/// 等待分配路径上类型不匹配不得终结观察者（继续阻塞），仅注册探测路径返回 WRONGTYPE
#[test]
fn test_midwait_mismatch_keeps_waiter_blocked() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();
    let key = Bytes::from_static(b"midwait_key");

    // 观察者先在空 key 上阻塞
    let b = broker.clone();
    let k = key.clone();
    let task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(7001, RespCommand::Blpop, vec![]);
      b.get_collection_item_async(obs, &[k], 5.0, rx).await
    });
    sleep(Duration::from_millis(20)).await;
    assert_eq!(broker.waiting_count(&key), 1);

    // key 期间变为 zset 且有数据：分配路径的类型不匹配不得投递 WRONGTYPE
    store.zadd(&key, 1.0, "zmember").unwrap();
    assert!(
      !broker.handle_collection_update(&key),
      "类型不符不得终结等待者"
    );
    assert_eq!(broker.waiting_count(&key), 1, "观察者应保持阻塞");

    // key 恢复为 List 并写入：观察者被正常唤醒并取得元素
    store.clear();
    store.push_list_right(&key, "late_val").unwrap();
    assert!(broker.handle_collection_update(&key));

    let res = task.await.unwrap();
    assert!(res.found(), "观察者应最终取得元素: {res:?}");
    assert_eq!(res.item.as_deref(), Some(&b"late_val"[..]));
    assert_eq!(broker.waiting_count(&key), 0);
  });

  info!("等待分配路径类型不匹配保持阻塞测试通过");
  OK
}

/// 对标 Garnet CollectionItemBroker.HandleSessionDisposed: 客户端意外断开时清理等待者并返回 Empty
#[test]
fn test_session_disposed_cleans_waiter() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, _store) = exact_broker();

    let broker_clone = broker.clone();
    let blocking_task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(501, RespCommand::Blpop, vec![]);
      broker_clone
        .get_collection_item_async(obs, &[Bytes::from_static(b"disposed_key")], 5.0, rx)
        .await
    });

    sleep(Duration::from_millis(20)).await;
    assert!(broker.try_get_observer(501).is_some());

    // 客户端意外断开连接
    broker.handle_session_disposed(501);

    let res = blocking_task.await.unwrap();
    assert!(res.is_empty());
    assert!(!res.found());
    assert!(broker.try_get_observer(501).is_none());
  });

  info!("会话销毁断开连接测试通过");
  OK
}

/// 对标 Garnet CollectionItemBroker.CleanKeysToObservers: 观察者全部超时后，
/// 执行清理并回收空 Key，等待者数量归零
#[test]
fn test_clean_keys_to_observers() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, _store) = exact_broker();

    // 注册两个短超时的观察者
    let (obs1, rx1) = CollectionItemObserver::new(701, RespCommand::Blpop, vec![]);
    let (obs2, rx2) = CollectionItemObserver::new(702, RespCommand::Blpop, vec![]);

    let _ = broker
      .get_collection_item_async(obs1, &[Bytes::from_static(b"clean_test_key")], 0.02, rx1)
      .await;
    let _ = broker
      .get_collection_item_async(obs2, &[Bytes::from_static(b"clean_test_key")], 0.02, rx2)
      .await;

    // 此时等待者数量为 0 (已超时)
    assert_eq!(broker.waiting_count(b"clean_test_key"), 0);

    // 执行清理并回收空 Key
    broker.clean_keys_to_observers();
    assert_eq!(broker.waiting_count(b"clean_test_key"), 0);
  });

  info!("观察者队列清理与空 Key 回收测试通过");
  OK
}

/// 对标 Garnet 无: LPUSH / ZADD 类型冲突返回 WRONGTYPE 且不破坏既有数据（对齐 Redis 语义）
#[test]
fn test_push_wrong_type_keeps_data() -> Void {
  let store = MemoryCollectionStore::new();

  // 4.1 对 SortedSet key 执行 LPUSH
  store.zadd(b"wt_key", 1.5, "zmember").unwrap();
  let err = store
    .push_list_left(b"wt_key", "lval")
    .expect_err("应当返回 WRONGTYPE");
  assert!(matches!(err, wedb_blocking::Error::WrongType));
  assert_eq!(store.len(b"wt_key"), 1);
  let (score, member) = store.zpop_min(b"wt_key").expect("zset 数据应保持不变");
  assert_eq!(score, 1.5);
  assert_eq!(member.as_ref(), &b"zmember"[..]);

  // 4.2 对 List key 执行 ZADD
  store.push_list_right(b"wt_list", "litem").unwrap();
  let err = store
    .zadd(b"wt_list", 2.0, "zmember")
    .expect_err("应当返回 WRONGTYPE");
  assert!(matches!(err, wedb_blocking::Error::WrongType));
  assert_eq!(store.len(b"wt_list"), 1);
  assert_eq!(
    store.pop_list_left(b"wt_list").as_deref(),
    Some(&b"litem"[..])
  );

  info!("LPUSH/ZADD 类型冲突防破坏测试通过");
  OK
}

/// 对标 Garnet 无: 极限超时值（NaN / 巨大数值 / 无穷 / 零）不得 panic，且仍可被 CLIENT UNBLOCK 解除
#[test]
fn test_extreme_timeout_no_panic() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, _store) = exact_broker();

    for (i, timeout_sec) in [f64::NAN, 1.0e300, f64::INFINITY, 0.0].iter().enumerate() {
      let session_id = 900 + i as u64;
      let b = broker.clone();
      let key = Bytes::from_static(b"extreme_key");
      let t = *timeout_sec;
      let task = spawn(async move {
        b.get_collection_item(session_id, RespCommand::Blpop, &[key], t, vec![])
          .await
      });

      sleep(Duration::from_millis(20)).await;
      assert!(broker.try_get_observer(session_id).is_some());
      // CLIENT UNBLOCK TIMEOUT 语义解除
      assert!(broker.try_unblock(session_id, false));
      let res = task.await.unwrap();
      assert!(res.is_empty());
      assert!(!res.found());
    }
  });

  info!("极限超时值防护测试通过");
  OK
}

/// 对标 Garnet 无: 等待任务被强制中止（接收端断开）后的生命周期闭环 ——
/// 死条目不得吞吃元素，周期清理须回收残留的会话映射与队列条目
#[test]
fn test_abandoned_wait_purged_not_swallowing() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();
    let key = Bytes::from_static(b"abort_key");

    // 6.1 阻塞等待被超时强制中止（future 连同接收端一并丢弃，
    //      模拟连接异常关闭且未走释放钩子的场景）
    let b = broker.clone();
    let k = key.clone();
    let task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(6001, RespCommand::Blpop, vec![]);
      let _ = timeout(
        Duration::from_millis(30),
        b.get_collection_item_async(obs, &[k], 0.0, rx),
      )
      .await;
    });
    sleep(Duration::from_millis(10)).await;
    assert!(broker.try_get_observer(6001).is_some(), "等待中应在册");
    task.await.unwrap();

    // 6.2 死条目不得吞吃元素：新等待者注册后，推送必须直达新等待者
    let b = broker.clone();
    let k = key.clone();
    let task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(6002, RespCommand::Blpop, vec![]);
      b.get_collection_item_async(obs, &[k], 5.0, rx).await
    });
    sleep(Duration::from_millis(20)).await;
    store.push_list_right(&key, "for_live").unwrap();
    assert!(broker.handle_collection_update(&key));

    let res = task.await.unwrap();
    assert!(res.found(), "元素不得被断开的观察者吞吃: {res:?}");
    assert_eq!(res.item.as_deref(), Some(&b"for_live"[..]));

    // 6.3 周期清理回收死条目与残留的会话映射
    broker.clean_keys_to_observers();
    assert!(
      broker.try_get_observer(6001).is_none(),
      "断开的会话映射应被回收"
    );
    assert_eq!(broker.waiting_count(&key), 0);
    assert_eq!(store.len(&key), 0);
  });

  info!("等待任务中止生命周期闭环测试通过");
  OK
}

/// 对标 Garnet 无: notify_waiters 不得在接收端断开的观察者上虚报唤醒数，正常观察者仍被准确唤醒
#[test]
fn test_notify_skips_disconnected_observer() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, _store) = exact_broker();
    let key = Bytes::from_static(b"dead_notify_key");

    // 造出滞留队列的死条目：等待任务被外层超时强制中止，
    // 接收端随 future 一并丢弃，队列摘除逻辑未及执行
    let b = broker.clone();
    let k = key.clone();
    let dead = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(9001, RespCommand::Blpop, vec![]);
      // 超时 0.0 视为永久等待，由外层 30ms 超时强制中止
      let _ = timeout(
        Duration::from_millis(30),
        b.get_collection_item_async(obs, &[k], 0.0, rx),
      )
      .await;
    });
    dead.await.unwrap();
    assert_eq!(
      broker.notify_waiters(&key, 1),
      0,
      "断开的观察者不得计入唤醒数"
    );

    // 正常观察者仍可被准确唤醒
    let b = broker.clone();
    let k = key.clone();
    let live = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(9002, RespCommand::Blpop, vec![]);
      b.get_collection_item_async(obs, &[k], 5.0, rx).await
    });
    sleep(Duration::from_millis(20)).await;
    assert_eq!(broker.notify_waiters(&key, 1), 1, "正常观察者应被准确唤醒");
    let got = live.await.unwrap();
    assert_eq!(got.key.as_deref(), Some(&key[..]));
  });

  info!("notify 跳过断开观察者测试通过");
  OK
}

/// 对标 Garnet CollectionItemBroker.CleanKeysToObservers: 多 key 注册与超时后，
/// 清理彻底回收全部 key，零孤儿条目残留
#[test]
fn test_multi_key_cleanup_no_orphans() -> Void {
  Runtime::new()?.block_on(async {
    let broker = Arc::new(CollectionItemBroker::new());

    let k1 = Bytes::from_static(b"orphan_k1");
    let k2 = Bytes::from_static(b"orphan_k2");
    let k3 = Bytes::from_static(b"orphan_k3");
    let keys = vec![k1.clone(), k2.clone(), k3.clone()];

    // 第一次调用：k1 提示
    let r1 = broker
      .get_collection_item(8801, RespCommand::Blpop, &keys, 0.05, vec![])
      .await;
    assert_eq!(r1.key.as_deref(), Some(&k1[..]));

    // 第二次调用：k2 提示
    let r2 = broker
      .get_collection_item(8801, RespCommand::Blpop, &keys, 0.05, vec![])
      .await;
    assert_eq!(r2.key.as_deref(), Some(&k2[..]));

    // 第三次调用：k3 提示
    let r3 = broker
      .get_collection_item(8801, RespCommand::Blpop, &keys, 0.05, vec![])
      .await;
    assert_eq!(r3.key.as_deref(), Some(&k3[..]));

    // 第四次调用：所有 key 均已探测过，真正进入阻塞并超时
    let r4 = broker
      .get_collection_item(8801, RespCommand::Blpop, &keys, 0.03, vec![])
      .await;
    assert!(r4.is_empty());

    // 执行清理
    broker.clean_keys_to_observers();

    // 验证所有 key 的等待者为 0
    assert_eq!(broker.waiting_count(&k1), 0);
    assert_eq!(broker.waiting_count(&k2), 0);
    assert_eq!(broker.waiting_count(&k3), 0);
  });

  info!("多键清理零孤儿条目测试通过");
  OK
}
