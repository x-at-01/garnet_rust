//! BLMOVE / BRPOPLPUSH 跨集合转移语义测试（含同 key 旋转与级联唤醒）

use std::{slice::from_ref, time::Duration};

use aok::{OK, Void};
use bytes::Bytes;
use compio::{
  runtime::{Runtime, spawn},
  time::{sleep, timeout},
};
use log::info;
use wedb_blocking::{CollectionItemObserver, Direction};
use wedb_resp::RespCommand;

use crate::support::exact_broker;

/// 对标 Garnet RespBlockingCollectionTests.BasicBlockingListMoveTest: 源有数据时立即转移，
/// 源为空时阻塞被写入唤醒后转移 —— 验证 BLMOVE 跨集合元素的弹出与推入落位
#[test]
fn test_blmove_blocking_transfer() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    let src_key = Bytes::from_static(b"src_list");
    let dst_key = Bytes::from_static(b"dst_list");

    // 1.1 源列表已有元素，BLMOVE 立即转移 (RIGHT -> LEFT)
    store.push_list_right(b"src_list", "elem1").unwrap();
    let res = broker
      .move_collection_item(
        301,
        RespCommand::Blmove,
        &src_key,
        &dst_key,
        (Direction::Right, Direction::Left),
        5.0,
      )
      .await;

    assert!(res.found());
    assert_eq!(res.item.as_deref(), Some(&b"elem1"[..]));
    assert_eq!(store.len(b"src_list"), 0);
    assert_eq!(store.len(b"dst_list"), 1);
    assert_eq!(
      store.pop_list_left(b"dst_list").as_deref(),
      Some(&b"elem1"[..])
    );

    // 1.2 源列表为空时发起 BLMOVE，后台写入后唤醒并转移
    let broker_clone = broker.clone();
    let src = src_key.clone();
    let move_task = spawn(async move {
      broker_clone
        .move_collection_item(
          302,
          RespCommand::Blmove,
          &src,
          b"dst_list",
          (Direction::Left, Direction::Right),
          5.0,
        )
        .await
    });

    let store_clone = store.clone();
    let broker_for_writer = broker.clone();
    let writer = spawn(async move {
      sleep(Duration::from_millis(50)).await;
      store_clone
        .push_list_left(b"src_list", "elem_async_move")
        .unwrap();
      assert!(broker_for_writer.handle_collection_update(b"src_list"));
    });

    let res = move_task.await.unwrap();
    writer.await.unwrap();

    assert!(res.found());
    assert_eq!(res.item.as_deref(), Some(&b"elem_async_move"[..]));
    assert_eq!(store.len(b"src_list"), 0);
    assert_eq!(store.len(b"dst_list"), 1);
    assert_eq!(
      store.pop_list_right(b"dst_list").as_deref(),
      Some(&b"elem_async_move"[..])
    );
  });

  info!("BLMOVE 跨集合转移测试通过");
  OK
}

/// 对标 Garnet 无: BLMOVE 同 key 旋转不得死锁，弹出与推入在同一列表内完成
#[test]
fn test_blmove_same_key_rotation() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();
    let key = Bytes::from_static(b"rotate_key");

    store.push_list_right(b"rotate_key", "a").unwrap();
    store.push_list_right(b"rotate_key", "b").unwrap();

    // 防御性超时：若死锁则测试失败而非挂起
    let move_task = spawn(async move {
      timeout(
        Duration::from_secs(5),
        broker.move_collection_item(
          801,
          RespCommand::Blmove,
          &key,
          &key,
          (Direction::Right, Direction::Left),
          5.0,
        ),
      )
      .await
    });

    let res = move_task.await.unwrap().unwrap();
    // 右端弹出 "b"，左端推回：列表变为 [b, a]
    assert!(res.found());
    assert_eq!(res.item.as_deref(), Some(&b"b"[..]));
    assert_eq!(store.len(b"rotate_key"), 2);
    assert_eq!(
      store.pop_list_left(b"rotate_key").as_deref(),
      Some(&b"b"[..])
    );
    assert_eq!(
      store.pop_list_left(b"rotate_key").as_deref(),
      Some(&b"a"[..])
    );
  });

  info!("BLMOVE 同 key 旋转测试通过");
  OK
}

/// 对标 Garnet RespBlockingCollectionTests.BlockingListMoveWrongTypeTest:
/// 目标类型不匹配时返回 WRONGTYPE，且源元素绝不丢失
#[test]
fn test_blmove_dst_wrong_type_preserves_src() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    store.push_list_right(b"mv_src", "keep_me").unwrap();
    store.zadd(b"mv_dst", 1.0, "zmember").unwrap();

    let res = broker
      .move_collection_item(
        802,
        RespCommand::Blmove,
        b"mv_src",
        b"mv_dst",
        (Direction::Left, Direction::Left),
        5.0,
      )
      .await;

    assert!(res.is_type_mismatch());
    // 源列表元素必须原样保留（修复前会因先弹出后校验而丢失）
    assert_eq!(store.len(b"mv_src"), 1);
    assert_eq!(
      store.pop_list_left(b"mv_src").as_deref(),
      Some(&b"keep_me"[..])
    );
    assert_eq!(store.len(b"mv_dst"), 1);
  });

  info!("BLMOVE 目标类型不匹配防丢测试通过");
  OK
}

/// 对标 Garnet 无: BRPOPLPUSH 单参数（仅目标 key）跨集合转移，右弹出源推入目标左端
#[test]
fn test_brpoplpush_single_arg_direct_move() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    let src = Bytes::from_static(b"brpoplpush_src");
    let dst = Bytes::from_static(b"brpoplpush_dst");

    // 推入源列表：从右向左为 [elem_first, elem_second]
    store.push_list_right(&src, "elem_first").unwrap();
    store.push_list_right(&src, "elem_second").unwrap();

    // 发起 BRPOPLPUSH，command_args 仅含 dst_key (len == 1)
    let res = broker
      .get_collection_item(
        8901,
        RespCommand::Brpoplpush,
        from_ref(&src),
        5.0,
        vec![dst.clone()],
      )
      .await;

    assert!(res.found());
    // 右端弹出 elem_second，推入 dst 左端
    assert_eq!(res.item.as_deref(), Some(&b"elem_second"[..]));
    assert_eq!(store.len(&src), 1);
    assert_eq!(store.len(&dst), 1);
    assert_eq!(
      store.pop_list_left(&dst).as_deref(),
      Some(&b"elem_second"[..])
    );
  });

  info!("BRPOPLPUSH 单参数跨集合转移测试通过");
  OK
}

/// 对标 Garnet 无: 等待中的 BLMOVE 被唤醒时若目标 key 类型不符，必须以 WRONGTYPE
/// 错误终结（对齐 C#: BLMOVE 的 dst 类型不符总是返回 TypeMismatch），且源元素绝不丢失
#[test]
fn test_blmove_dst_wrong_type_wakeup_error() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    // 客户端阻塞在空 src 上等待 BLMOVE src -> dst
    let b = broker.clone();
    let task = spawn(async move {
      b.move_collection_item(
        9501,
        RespCommand::Blmove,
        b"wt_move_src",
        b"wt_move_dst",
        (Direction::Left, Direction::Right),
        5.0,
      )
      .await
    });
    sleep(Duration::from_millis(20)).await;
    assert_eq!(broker.waiting_count(b"wt_move_src"), 1);

    // 阻塞期间 dst 变为 zset；随后 src 到数据并触发唤醒
    store.zadd(b"wt_move_dst", 1.0, "zmember").unwrap();
    store.push_list_left(b"wt_move_src", "mover").unwrap();
    assert!(broker.handle_collection_update(b"wt_move_src"));

    // 唤醒路径必须返回 WRONGTYPE 错误，而非继续阻塞或吞掉源元素
    let res = task.await.unwrap();
    assert!(res.is_type_mismatch());
    assert!(!res.found());
    // 源元素原样保留（类型校验先于弹出）
    assert_eq!(store.len(b"wt_move_src"), 1);
    assert_eq!(
      store.pop_list_left(b"wt_move_src").as_deref(),
      Some(&b"mover"[..])
    );
    // dst 的 zset 数据不受影响；src 队列上的观察者已终结并摘除
    assert_eq!(store.len(b"wt_move_dst"), 1);
    assert_eq!(broker.waiting_count(b"wt_move_src"), 0);
  });

  info!("BLMOVE 唤醒路径目标类型不匹配错误测试通过");
  OK
}

/// 对标 Garnet 无: 级联唤醒 found 守卫 —— BLMOVE 以 WRONGTYPE 终结时未向 dst 落位
/// 任何元素，不得级联唤醒 dst 上的等待者（防惊群），dst 等待者仅在 dst 真实更新时被唤醒
#[test]
fn test_blmove_wrong_type_no_cascading_wakeup() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();

    // 1. 客户端 1：BZPOPMIN 阻塞在尚无数据的 dst 上
    let b1 = broker.clone();
    let task1 = spawn(async move {
      b1.get_collection_item(
        9601,
        RespCommand::Bzpopmin,
        &[Bytes::from_static(b"guard_dst")],
        5.0,
        vec![],
      )
      .await
    });
    sleep(Duration::from_millis(20)).await;
    assert_eq!(broker.waiting_count(b"guard_dst"), 1);

    // 2. dst 未经 broker 通知被写入 zset 数据（等待者按契约保持阻塞）
    store.zadd(b"guard_dst", 1.0, "zm").unwrap();

    // 3. 客户端 2：BLMOVE src -> dst 阻塞在空 src 上
    let b2 = broker.clone();
    let task2 = spawn(async move {
      b2.move_collection_item(
        9602,
        RespCommand::Blmove,
        b"guard_src",
        b"guard_dst",
        (Direction::Left, Direction::Right),
        5.0,
      )
      .await
    });
    sleep(Duration::from_millis(20)).await;
    assert_eq!(broker.waiting_count(b"guard_src"), 1);

    // 4. src 到数据并唤醒：BLMOVE 因 dst 类型不符以 WRONGTYPE 终结
    store.push_list_left(b"guard_src", "mover").unwrap();
    assert!(broker.handle_collection_update(b"guard_src"));
    let res2 = task2.await.unwrap();
    assert!(res2.is_type_mismatch());

    // 5. found 守卫：WRONGTYPE 未向 dst 落位元素，客户端 1 不得被级联唤醒
    //    （修复前：级联分配会把 dst 的 zset 数据投递给 BZPOPMIN 等待者）
    assert_eq!(
      broker.waiting_count(b"guard_dst"),
      1,
      "WRONGTYPE 终结不得级联唤醒 dst 等待者"
    );

    // 6. dst 真实更新时客户端 1 才被正常唤醒
    assert!(broker.handle_collection_update(b"guard_dst"));
    let res1 = task1.await.unwrap();
    assert!(res1.found());
    assert_eq!(res1.score, Some(1.0));
    assert_eq!(res1.item.as_deref(), Some(&b"zm"[..]));
    assert_eq!(broker.waiting_count(b"guard_dst"), 0);
  });

  info!("BLMOVE WRONGTYPE 不级联唤醒测试通过");
  OK
}

/// 对标 Garnet 无: BLMOVE 转移落位后自动级联唤醒目标 key 上的阻塞等待者
#[test]
fn test_blmove_cascading_wakeup() -> Void {
  Runtime::new()?.block_on(async {
    let (broker, store) = exact_broker();
    let k_src = Bytes::from_static(b"cascade_src");
    let k_dst = Bytes::from_static(b"cascade_dst");

    // 1. 客户端 1：阻塞在 k_dst 上等待 BLPOP
    let b1 = broker.clone();
    let dst1 = k_dst.clone();
    let task_client1 = spawn(async move {
      let (obs, rx) = CollectionItemObserver::new(9301, RespCommand::Blpop, vec![]);
      b1.get_collection_item_async(obs, &[dst1], 5.0, rx).await
    });

    // 2. 客户端 2：阻塞在 k_src 上等待 BLMOVE k_src -> k_dst
    let b2 = broker.clone();
    let src2 = k_src.clone();
    let dst2 = k_dst.clone();
    let task_client2 = spawn(async move {
      b2.move_collection_item(
        9302,
        RespCommand::Blmove,
        &src2,
        &dst2,
        (Direction::Left, Direction::Right),
        5.0,
      )
      .await
    });

    // 确保两客户端均已进入阻塞等待
    sleep(Duration::from_millis(30)).await;
    assert_eq!(broker.waiting_count(&k_src), 1);
    assert_eq!(broker.waiting_count(&k_dst), 1);

    // 3. 写入任务：向 k_src 写入元素并触发通知
    let store_clone = store.clone();
    let b_writer = broker.clone();
    let src_writer = k_src.clone();
    let writer = spawn(async move {
      store_clone
        .push_list_left(&src_writer, "cascade_token")
        .unwrap();
      assert!(b_writer.handle_collection_update(&src_writer));
    });

    writer.await.unwrap();

    // 客户端 2 完成转移
    let res2 = task_client2.await.unwrap();
    assert!(res2.found());
    assert_eq!(res2.item.as_deref(), Some(&b"cascade_token"[..]));

    // 客户端 1 被级联唤醒并从 k_dst 弹出
    let res1 = task_client1.await.unwrap();
    assert!(res1.found());
    assert_eq!(res1.item.as_deref(), Some(&b"cascade_token"[..]));

    // 最终两队列均为空
    assert_eq!(store.len(&k_src), 0);
    assert_eq!(store.len(&k_dst), 0);
    assert_eq!(broker.waiting_count(&k_src), 0);
    assert_eq!(broker.waiting_count(&k_dst), 0);
  });

  info!("BLMOVE 级联唤醒测试通过");
  OK
}
