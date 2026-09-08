//! 单/多键阻塞弹出语义测试（BLPOP / BRPOP / BZPOPMIN / BZPOPMAX / BLMPOP / BZMPOP）

use std::{
  slice::from_ref,
  time::{Duration, Instant},
};

use aok::{OK, Void};
use bytes::Bytes;
use compio::{
  runtime::{Runtime, spawn},
  time::sleep,
};
use log::info;
use wedb_blocking::CollectionItemObserver;
use wedb_resp::RespCommand;

use crate::support::exact_broker;

/// 对标 Garnet RespBlockingCollectionTests.BasicListBlockingPopTest: 预检命中立即返回、
/// 空键阻塞后被写入唤醒、BRPOP 从右端弹出 —— 验证单键 BLPOP/BRPOP 的阻塞弹出与唤醒语义
#[test]
fn test_blpop_single_key_wakes_waiter() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    // 1.1 集合预先有数据时，BLPOP 立即返回，不进行异步阻塞
    store.push_list_left(b"mykey", "val_pre").unwrap();
    let (obs, rx) = CollectionItemObserver::new(1, RespCommand::Blpop, vec![]);
    let res = broker
      .get_collection_item_async(obs, &[Bytes::from_static(b"mykey")], 5.0, rx)
      .await;
    assert!(res.found());
    assert_eq!(res.key.as_deref(), Some(&b"mykey"[..]));
    assert_eq!(res.item.as_deref(), Some(&b"val_pre"[..]));
    assert_eq!(store.len(b"mykey"), 0);

    // 1.2 集合为空时发起阻塞，后台任务延迟推入后唤醒
    let broker_clone = broker.clone();

    // 启动异步等待任务
    let blocking_task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(2, RespCommand::Blpop, vec![]);
      broker_clone
        .get_collection_item_async(obs, &[Bytes::from_static(b"wait_key")], 5.0, rx)
        .await
    });

    // 启动延迟写入任务
    let broker_for_writer = broker.clone();
    let store_clone = store.clone();
    let writer = spawn(async move {
      sleep(Duration::from_millis(50)).await;
      store_clone.push_list_left(b"wait_key", "val_wake").unwrap();
      assert!(broker_for_writer.handle_collection_update(b"wait_key"));
    });

    let res = blocking_task.await.unwrap();
    writer.await.unwrap();

    assert!(res.found());
    assert_eq!(res.key.as_deref(), Some(&b"wait_key"[..]));
    assert_eq!(res.item.as_deref(), Some(&b"val_wake"[..]));
    assert_eq!(store.len(b"wait_key"), 0);

    // 1.3 验证 BRPOP (右端弹出) 语义
    let broker_clone = broker.clone();

    let blocking_rpop = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(3, RespCommand::Brpop, vec![]);
      broker_clone
        .get_collection_item_async(obs, &[Bytes::from_static(b"rpop_key")], 5.0, rx)
        .await
    });

    let broker_for_writer = broker.clone();
    let store_clone = store.clone();
    let writer = spawn(async move {
      sleep(Duration::from_millis(50)).await;
      // 推入两个元素：head -> [first, second] -> tail
      store_clone.push_list_right(b"rpop_key", "first").unwrap();
      store_clone.push_list_right(b"rpop_key", "second").unwrap();
      assert!(broker_for_writer.handle_collection_update(b"rpop_key"));
    });

    let res = blocking_rpop.await.unwrap();
    writer.await.unwrap();

    assert!(res.found());
    assert_eq!(res.key.as_deref(), Some(&b"rpop_key"[..]));
    // BRPOP 应当从右侧弹出最新的 "second"
    assert_eq!(res.item.as_deref(), Some(&b"second"[..]));
    // 列表中剩余 "first"
    assert_eq!(store.len(b"rpop_key"), 1);
    assert_eq!(
      store.pop_list_left(b"rpop_key").as_deref(),
      Some(&b"first"[..])
    );
  });

  info!("单键阻塞与唤醒测试通过");
  OK
}

/// 对标 Garnet RespBlockingCollectionTests.MultiListBlockingPopTest: 多键均有数据时按
/// 从左到右优先级返回，空键阻塞后被非首选键写入唤醒 —— 验证多键探测优先级与惰性清理
#[test]
fn test_blpop_multi_key_priority() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    let key1 = Bytes::from_static(b"key1");
    let key2 = Bytes::from_static(b"key2");
    let key3 = Bytes::from_static(b"key3");

    // 2.1 探测多键：若多个键均有数据，严格按从左到右优先级返回首个有数据的键
    store.push_list_left(b"key2", "val_key2").unwrap();
    store.push_list_left(b"key3", "val_key3").unwrap();

    let (obs, rx) = CollectionItemObserver::new(10, RespCommand::Blpop, vec![]);
    let res = broker
      .get_collection_item_async(obs, &[key1.clone(), key2.clone(), key3.clone()], 5.0, rx)
      .await;
    assert!(res.found());
    // key1 无数据，key2 有数据，必须返回 key2 而不是 key3
    assert_eq!(res.key.as_deref(), Some(&b"key2"[..]));
    assert_eq!(res.item.as_deref(), Some(&b"val_key2"[..]));

    // 清理 key3
    store.clear();

    // 2.2 多个键均为空时进入阻塞，当后续非首个键写入时唤醒
    let broker_clone = broker.clone();
    let keys = vec![key1.clone(), key2.clone(), key3.clone()];

    let blocking_task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(11, RespCommand::Blpop, vec![]);
      broker_clone
        .get_collection_item_async(obs, &keys, 5.0, rx)
        .await
    });

    let broker_for_writer = broker.clone();
    let store_clone = store.clone();
    let writer = spawn(async move {
      sleep(Duration::from_millis(50)).await;
      // 写入第三个键 key3
      store_clone
        .push_list_left(b"key3", "val_key3_wake")
        .unwrap();
      assert!(broker_for_writer.handle_collection_update(b"key3"));
    });

    let res = blocking_task.await.unwrap();
    writer.await.unwrap();

    assert!(res.found());
    assert_eq!(res.key.as_deref(), Some(&b"key3"[..]));
    assert_eq!(res.item.as_deref(), Some(&b"val_key3_wake"[..]));

    // 验证 key1 和 key2 队列中的无效观察者在后续更新时被惰性清理
    assert!(!broker.handle_collection_update(b"key1"));
    assert!(!broker.handle_collection_update(b"key2"));
  });

  info!("多键优先级阻塞与唤醒测试通过");
  OK
}

/// 对标 Garnet 无: 客户端设置超时时间，到期未写入时安全返回 Empty 结果并清理会话映射
#[test]
fn test_blpop_timeout_returns_empty() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, _store) = exact_broker();

    let (obs, rx) = CollectionItemObserver::new(20, RespCommand::Blpop, vec![]);
    let start = Instant::now();
    let res = broker
      .get_collection_item_async(obs, &[Bytes::from_static(b"timeout_key")], 0.05, rx)
      .await;
    let elapsed = start.elapsed();

    // 应当超时返回 Empty 结果
    assert!(res.is_empty());
    assert!(!res.found());
    assert!(!res.is_force_unblocked());
    assert!(elapsed >= Duration::from_millis(40));

    // 验证 session 映射已被清理
    assert!(broker.try_get_observer(20).is_none());
  });

  info!("超时返回空测试通过");
  OK
}

/// 对标 Garnet 无: 多个客户端在同一 Key 上等待时，严格遵循 FIFO 先入先出顺序被唤醒消费
#[test]
fn test_blpop_fifo_wake_order() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    let client_count = 10;
    let key = Bytes::from_static(b"fifo_queue_key");

    let mut blocking_handles = Vec::new();

    // 依次注册 10 个观察者 (会话 ID: 100..110)
    for i in 0..client_count {
      let b = broker.clone();
      let k = key.clone();
      let handle = spawn(async move {
        let (obs, rx) = CollectionItemObserver::new(100 + i, RespCommand::Blpop, vec![]);
        b.get_collection_item_async(obs, &[k], 5.0, rx).await
      });
      blocking_handles.push(handle);
      // 微小间隔确保注册顺序确定
      sleep(Duration::from_millis(5)).await;
    }

    assert_eq!(
      broker.waiting_count(b"fifo_queue_key"),
      client_count as usize
    );

    // 后台依次推入 10 个不同的元素
    let broker_clone = broker.clone();
    let store_clone = store.clone();
    let writer = spawn(async move {
      for i in 0..client_count {
        let val = format!("val_{}", i);
        store_clone.push_list_right(b"fifo_queue_key", val).unwrap();
        assert!(broker_clone.handle_collection_update(b"fifo_queue_key"));
      }
    });

    writer.await.unwrap();

    // 收集所有观察者的返回值
    let mut results = Vec::new();
    for handle in blocking_handles {
      results.push(handle.await.unwrap());
    }

    // 验证严格遵循 FIFO 顺序唤醒消费
    for (i, res) in results.iter().enumerate() {
      assert!(res.found());
      let expected_val = format!("val_{}", i);
      assert_eq!(res.item.as_deref(), Some(expected_val.as_bytes()));
    }

    // 集合中所有元素应当已被消费完毕
    assert_eq!(store.len(b"fifo_queue_key"), 0);
    assert_eq!(broker.waiting_count(b"fifo_queue_key"), 0);
  });

  info!("并发写唤醒与 FIFO 顺序测试通过");
  OK
}

/// 对标 Garnet CollectionItemBroker.TryForceUnblock / ClientCommands CLIENT UNBLOCK:
/// ERROR 语义返回 ForceUnblocked、TIMEOUT 语义返回 Empty，解除不存在会话返回 false
#[test]
fn test_client_unblock_cancels_waiter() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, _store) = exact_broker();

    // 5.1 CLIENT UNBLOCK <id> ERROR -> 返回 ForceUnblocked 结果
    let broker_clone = broker.clone();
    let blocking_error_task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(201, RespCommand::Blpop, vec![]);
      broker_clone
        .get_collection_item_async(obs, &[Bytes::from_static(b"unblock_key_1")], 5.0, rx)
        .await
    });

    // 确保已进入等待状态
    sleep(Duration::from_millis(20)).await;
    assert!(broker.try_get_observer(201).is_some());

    // 执行解除阻塞 (throw_error = true)
    assert!(broker.try_unblock(201, true));

    let res = blocking_error_task.await.unwrap();
    assert!(res.is_force_unblocked());
    assert!(!res.found());
    assert!(!res.is_empty());

    // 5.2 CLIENT UNBLOCK <id> TIMEOUT -> 返回 Empty 结果 (nil)
    let broker_clone = broker.clone();
    let blocking_timeout_task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(202, RespCommand::Blpop, vec![]);
      broker_clone
        .get_collection_item_async(obs, &[Bytes::from_static(b"unblock_key_2")], 5.0, rx)
        .await
    });

    sleep(Duration::from_millis(20)).await;
    assert!(broker.try_get_observer(202).is_some());

    // 执行解除阻塞 (throw_error = false)
    assert!(broker.try_unblock(202, false));

    let res = blocking_timeout_task.await.unwrap();
    assert!(!res.is_force_unblocked());
    assert!(res.is_empty());
    assert!(!res.found());

    // 5.3 解除不存在的会话 ID 返回 false
    assert!(!broker.try_unblock(99999, true));
  });

  info!("CLIENT UNBLOCK 取消阻塞测试通过");
  OK
}

/// 对标 Garnet RespBlockingCollectionTests.BasicBzpopMinMaxTest: 预有数据时 BZPOPMIN
/// 立即返回最低分成员；空键阻塞后 BZPOPMAX 被 ZADD 唤醒并返回最高分成员
#[test]
fn test_bzpopmin_bzpopmax_blocking() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    // 6.1 预先有数据时，BZPOPMIN 立即返回
    store.zadd(b"zset_key", 10.5, "item_low").unwrap();
    store.zadd(b"zset_key", 99.0, "item_high").unwrap();

    let (obs, rx) = CollectionItemObserver::new(601, RespCommand::Bzpopmin, vec![]);
    let res = broker
      .get_collection_item_async(obs, &[Bytes::from_static(b"zset_key")], 5.0, rx)
      .await;

    assert!(res.found());
    assert_eq!(res.key.as_deref(), Some(&b"zset_key"[..]));
    assert_eq!(res.score, Some(10.5));
    assert_eq!(res.item.as_deref(), Some(&b"item_low"[..]));
    assert_eq!(store.len(b"zset_key"), 1);

    // 6.2 BZPOPMAX 异步阻塞与唤醒
    let broker_clone = broker.clone();
    let blocking_task = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(602, RespCommand::Bzpopmax, vec![]);
      broker_clone
        .get_collection_item_async(obs, &[Bytes::from_static(b"zset_async_key")], 5.0, rx)
        .await
    });

    let store_clone = store.clone();
    let broker_for_writer = broker.clone();
    let writer = spawn(async move {
      sleep(Duration::from_millis(50)).await;
      store_clone
        .zadd(b"zset_async_key", 88.8, "zval_wake")
        .unwrap();
      assert!(broker_for_writer.handle_collection_update(b"zset_async_key"));
    });

    let res = blocking_task.await.unwrap();
    writer.await.unwrap();

    assert!(res.found());
    assert_eq!(res.key.as_deref(), Some(&b"zset_async_key"[..]));
    assert_eq!(res.score, Some(88.8));
    assert_eq!(res.item.as_deref(), Some(&b"zval_wake"[..]));
    assert_eq!(store.len(b"zset_async_key"), 0);
  });

  info!("有序集合阻塞弹出测试通过");
  OK
}

/// 对标 Garnet RespBlockingCollectionTests.BasicBlmpopTest / BlmpopBlockingWithCountTest:
/// 预有数据时 BLMPOP LEFT 2 立即弹出；空键阻塞后被写入唤醒按 RIGHT 2 弹出多元素
#[test]
fn test_blmpop_multi_pop() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();
    let k = Bytes::from_static(b"blmpop_key");

    // 7.1 预先有数据，BLMPOP LEFT 2 立即弹出 2 个元素
    store.push_list_right(&k, "item_1").unwrap();
    store.push_list_right(&k, "item_2").unwrap();
    store.push_list_right(&k, "item_3").unwrap();

    let res = broker
      .get_collection_item(
        9101,
        RespCommand::Blmpop,
        from_ref(&k),
        5.0,
        vec![Bytes::from_static(b"LEFT"), Bytes::from_static(b"2")],
      )
      .await;

    assert!(res.found());
    assert_eq!(res.key.as_deref(), Some(&k[..]));
    let items = res.items.expect("应包含多元素列表");
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].as_ref(), b"item_1");
    assert_eq!(items[1].as_ref(), b"item_2");
    assert_eq!(store.len(&k), 1);

    // 清空剩余数据
    store.clear();

    // 7.2 空列表时阻塞，后台写入后唤醒弹出多元素 (RIGHT 2)
    let broker_clone = broker.clone();
    let k_clone = k.clone();
    let task = spawn(async move {
      broker_clone
        .get_collection_item(
          9102,
          RespCommand::Blmpop,
          from_ref(&k_clone),
          5.0,
          vec![Bytes::from_static(b"RIGHT"), Bytes::from_static(b"2")],
        )
        .await
    });

    let store_clone = store.clone();
    let broker_for_writer = broker.clone();
    let k_writer = k.clone();
    let writer = spawn(async move {
      sleep(Duration::from_millis(50)).await;
      store_clone.push_list_right(&k_writer, "elem_a").unwrap();
      store_clone.push_list_right(&k_writer, "elem_b").unwrap();
      store_clone.push_list_right(&k_writer, "elem_c").unwrap();
      assert!(broker_for_writer.handle_collection_update(&k_writer));
    });

    let res = task.await.unwrap();
    writer.await.unwrap();

    assert!(res.found());
    let items = res.items.expect("应包含多元素列表");
    assert_eq!(items.len(), 2);
    // 从右端依次弹出 elem_c, elem_b
    assert_eq!(items[0].as_ref(), b"elem_c");
    assert_eq!(items[1].as_ref(), b"elem_b");
    assert_eq!(store.len(&k), 1);
  });

  info!("BLMPOP 多元素弹出测试通过");
  OK
}

/// 对标 Garnet RespBlockingCollectionTests.BasicBzmpopTest / BzmpopBlockingBehaviorTest:
/// 预有数据时 BZMPOP MIN 2 立即弹出；空键阻塞后被写入唤醒按 MAX 2 弹出最高分成员
#[test]
fn test_bzmpop_min_max_multi_pop() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();
    let k = Bytes::from_static(b"bzmpop_key");

    // 8.1 预先有数据，BZMPOP MIN 2
    store.zadd(&k, 10.0, "z1").unwrap();
    store.zadd(&k, 20.0, "z2").unwrap();
    store.zadd(&k, 30.0, "z3").unwrap();

    let res = broker
      .get_collection_item(
        9201,
        RespCommand::Bzmpop,
        from_ref(&k),
        5.0,
        vec![Bytes::from_static(b"MIN"), Bytes::from_static(b"2")],
      )
      .await;

    assert!(res.found());
    let scores = res.scores.expect("应包含分数列表");
    let items = res.items.expect("应包含成员列表");
    assert_eq!(scores, vec![10.0, 20.0]);
    assert_eq!(
      items,
      vec![Bytes::from_static(b"z1"), Bytes::from_static(b"z2")]
    );
    assert_eq!(store.len(&k), 1);

    // 清空剩余
    store.clear();

    // 8.2 空有序集合时阻塞，后台写入后按 MAX 2 唤醒
    let broker_clone = broker.clone();
    let k_clone = k.clone();
    let task = spawn(async move {
      broker_clone
        .get_collection_item(
          9202,
          RespCommand::Bzmpop,
          from_ref(&k_clone),
          5.0,
          vec![Bytes::from_static(b"MAX"), Bytes::from_static(b"2")],
        )
        .await
    });

    let store_clone = store.clone();
    let broker_for_writer = broker.clone();
    let k_writer = k.clone();
    let writer = spawn(async move {
      sleep(Duration::from_millis(50)).await;
      store_clone.zadd(&k_writer, 40.0, "z4").unwrap();
      store_clone.zadd(&k_writer, 50.0, "z5").unwrap();
      assert!(broker_for_writer.handle_collection_update(&k_writer));
    });

    let res = task.await.unwrap();
    writer.await.unwrap();

    assert!(res.found());
    let scores = res.scores.expect("应包含分数列表");
    let items = res.items.expect("应包含成员列表");
    assert_eq!(scores, vec![50.0, 40.0]);
    assert_eq!(
      items,
      vec![Bytes::from_static(b"z5"), Bytes::from_static(b"z4")]
    );
    assert_eq!(store.len(&k), 0);
  });

  info!("BZMPOP 有序集合多元素按 MIN/MAX 弹出测试通过");
  OK
}
