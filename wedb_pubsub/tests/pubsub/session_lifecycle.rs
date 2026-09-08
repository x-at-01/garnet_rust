use std::{sync::Arc, thread, time::Duration};

use aok::{OK, Void};
use bytes::Bytes;
use crossfire::Rx;
use wedb_pubsub::{PubSubMessage, SubscribeBroker, create_session};

use super::support::setup;

/// 对标 C# SubscribeBroker.RemoveSubscription：会话断开连接时原子级联清理订阅频道与模式
#[test]
fn session_disconnect_cascade_cleanup() -> Void {
  let broker = setup();
  let (session, _) = create_session(201, 16);

  broker.subscribe(b"chanA", &session);
  broker.subscribe(b"chanB", &session);
  broker.pattern_subscribe(b"pat*", &session);

  assert_eq!(broker.num_subscriptions(b"chanA"), 1);
  assert_eq!(broker.num_subscriptions(b"chanB"), 1);
  assert_eq!(broker.num_pattern_subscriptions(), 1);
  assert_eq!(broker.session_subscription_count(session.id), 3);

  // 触发断开连接原子级联清理
  broker.remove_subscription(session.id);

  assert_eq!(broker.num_subscriptions(b"chanA"), 0);
  assert_eq!(broker.num_subscriptions(b"chanB"), 0);
  assert_eq!(broker.num_pattern_subscriptions(), 0);
  assert_eq!(broker.session_subscription_count(session.id), 0);
  assert!(broker.get_channels(None).is_empty());
  assert!(broker.list_all_subscriptions(session.id).is_empty());
  assert!(broker.list_all_pattern_subscriptions(session.id).is_empty());
  OK
}

/// 对标 C# SubscribeBroker 广播健壮性：死亡会话发布期自动侦测并级联清理
#[test]
fn dead_session_cascade_cleanup_during_publish() -> Void {
  let broker = setup();
  let (s1, rx1) = create_session(1001, 128);
  let (s2, rx2) = create_session(1002, 128);

  broker.subscribe(b"alerts", &s1);
  broker.psubscribe(b"alert*", &s1);
  broker.subscribe(b"alerts", &s2);
  broker.psubscribe(b"alert*", &s2);

  assert_eq!(broker.numsub(b"alerts"), 2);
  assert_eq!(broker.numpat(), 1);

  // 模拟 s2 意外断开（释放接收端）
  drop(rx2);

  // 广播发布，发布时自动侦测并清理断开的会话
  let sent = broker.publish(b"alerts", b"warning");
  // s1 收到 1 条精准消息 + 1 条模式消息 = 2 条；s2 已断开不计入
  assert_eq!(sent, 2);

  assert_eq!(broker.numsub(b"alerts"), 1);
  assert_eq!(broker.session_subscription_count(s2.id), 0);
  assert!(broker.list_all_subscriptions(s2.id).is_empty());
  assert!(broker.list_all_pattern_subscriptions(s2.id).is_empty());

  let m1 = rx1.try_recv()?;
  assert_eq!(
    m1,
    PubSubMessage::message(
      Bytes::from_static(b"alerts"),
      Bytes::from_static(b"warning")
    )
  );
  let m2 = rx1.try_recv()?;
  assert_eq!(
    m2,
    PubSubMessage::pmessage(
      Bytes::from_static(b"alert*"),
      Bytes::from_static(b"alerts"),
      Bytes::from_static(b"warning")
    )
  );
  OK
}

/// 对标 C# SubscribeBroker 广播去重：多频道多模式死会话去重级联自动清理
#[test]
fn dead_session_dedup_cascade_cleanup() -> Void {
  let broker = setup();
  let (dead_session, rx_dead) = create_session(8888, 128);
  let (alive_session, rx_alive) = create_session(9999, 128);

  let channels: &[&[u8]] = &[b"news.us", b"news.uk", b"news.cn"];
  let patterns: &[&[u8]] = &[b"news.*", b"*.cn", b"news.u*"];

  for &ch in channels {
    broker.subscribe(ch, &dead_session);
    broker.subscribe(ch, &alive_session);
  }
  for &pat in patterns {
    broker.psubscribe(pat, &dead_session);
    broker.psubscribe(pat, &alive_session);
  }

  assert_eq!(broker.session_subscription_count(dead_session.id), 6);
  assert_eq!(broker.session_subscription_count(alive_session.id), 6);

  // 模拟断开 dead_session
  drop(rx_dead);

  // 发布命中精准频道及多个模式，触发多处 Disconnected 并去重级联清理
  let delivered = broker.publish(b"news.cn", b"important broadcast");
  // alive_session 收到 1 个精准 + 2 个模式 = 3
  assert_eq!(delivered, 3);

  assert_eq!(broker.session_subscription_count(dead_session.id), 0);
  assert!(broker.list_all_subscriptions(dead_session.id).is_empty());
  assert!(
    broker
      .list_all_pattern_subscriptions(dead_session.id)
      .is_empty()
  );

  assert_eq!(broker.session_subscription_count(alive_session.id), 6);
  let mut count = 0;
  while rx_alive.try_recv().is_ok() {
    count += 1;
  }
  assert_eq!(count, 3);

  broker.unsubscribe_all(&alive_session);
  broker.punsubscribe_all(&alive_session);
  assert!(broker.channels(None).is_empty());
  assert_eq!(broker.numpat(), 0);
  OK
}

/// 对标 C# SubscribeBroker 并发安全性：高并发订阅与注销竞态下的 ABA 安全与内存物理回收
#[test]
fn concurrent_subscribe_unsubscribe_race() -> Void {
  let broker = Arc::new(SubscribeBroker::new());
  let thread_count = 6;
  let iterations = 200;

  let handles: Vec<_> = (0..thread_count)
    .map(|t| {
      let b = broker.clone();
      thread::spawn(move || {
        let (session, rx) = create_session(5000 + t as u64, 64);
        let channel = b"race_channel";
        for _ in 0..iterations {
          b.subscribe(channel, &session);
          b.publish_now(channel, b"ping");
          while rx.try_recv().is_ok() {}
          b.unsubscribe(channel, &session);
        }
      })
    })
    .collect();

  for h in handles {
    h.join().unwrap();
  }

  assert!(broker.channels(None).is_empty());
  assert_eq!(broker.numsub(b"race_channel"), 0);
  OK
}

/// 对标 C# 消费端背压处理：慢消费者背压平滑降级与丢弃机制
#[test]
fn backpressure_smooth_degradation() -> Void {
  let broker = setup();
  // 队列容量仅为 2
  let (session, rx) = create_session(2001, 2);
  broker.subscribe(b"stream", &session);

  // 容量为 2，后 2 条触发背压平滑丢弃，发布端不阻塞、不 panic
  let s1 = broker.publish(b"stream", b"msg1");
  let s2 = broker.publish(b"stream", b"msg2");
  let s3 = broker.publish(b"stream", b"msg3");
  let s4 = broker.publish(b"stream", b"msg4");

  assert_eq!(s1, 1);
  assert_eq!(s2, 1);
  assert_eq!(s3, 0);
  assert_eq!(s4, 0);

  let r1 = rx.try_recv()?;
  assert_eq!(
    r1,
    PubSubMessage::message(Bytes::from_static(b"stream"), Bytes::from_static(b"msg1"))
  );
  let r2 = rx.try_recv()?;
  assert_eq!(
    r2,
    PubSubMessage::message(Bytes::from_static(b"stream"), Bytes::from_static(b"msg2"))
  );

  // 队列空出后，新消息恢复正常写入
  let s5 = broker.publish(b"stream", b"msg5");
  assert_eq!(s5, 1);
  let r5 = rx.try_recv()?;
  assert_eq!(
    r5,
    PubSubMessage::message(Bytes::from_static(b"stream"), Bytes::from_static(b"msg5"))
  );
  OK
}

/// 对标 C# RespPubSubTests 多客户端隔离：多会话消息广播与频道隔离
#[test]
fn multi_session_broadcast_isolation() -> Void {
  let broker = setup();
  let (session1, rx1) = create_session(101, 16);
  let rx1 = Rx::from(rx1);
  let (session2, rx2) = create_session(102, 16);
  let rx2 = Rx::from(rx2);
  let (session3, rx3) = create_session(103, 16);
  let rx3 = Rx::from(rx3);

  broker.subscribe(b"chan1", &session1);
  broker.subscribe(b"chan2", &session2);
  broker.subscribe(b"chan1", &session3);
  broker.subscribe(b"chan2", &session3);

  let c1 = broker.publish_now(b"chan1", b"hello chan1");
  assert_eq!(c1, 2, "chan1 应分发给 2 个订阅者");

  let c2 = broker.publish_now(b"chan2", b"hello chan2");
  assert_eq!(c2, 2, "chan2 应分发给 2 个订阅者");

  let m1 = rx1.recv_timeout(Duration::from_millis(50))?;
  assert!(matches!(m1, PubSubMessage::Message { channel, .. } if &channel[..] == b"chan1"));
  assert!(rx1.recv_timeout(Duration::from_millis(20)).is_err());

  let m2 = rx2.recv_timeout(Duration::from_millis(50))?;
  assert!(matches!(m2, PubSubMessage::Message { channel, .. } if &channel[..] == b"chan2"));
  assert!(rx2.recv_timeout(Duration::from_millis(20)).is_err());

  let m3_1 = rx3.recv_timeout(Duration::from_millis(50))?;
  let m3_2 = rx3.recv_timeout(Duration::from_millis(50))?;
  let m3_channels: whasher::HashSet<_> = [m3_1, m3_2]
    .into_iter()
    .filter_map(|m| match m {
      PubSubMessage::Message { channel, .. } => Some(channel),
      _ => None,
    })
    .collect();
  assert!(m3_channels.contains(b"chan1".as_slice()));
  assert!(m3_channels.contains(b"chan2".as_slice()));
  assert!(rx3.recv_timeout(Duration::from_millis(20)).is_err());
  OK
}
