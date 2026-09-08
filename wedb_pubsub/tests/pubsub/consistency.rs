//! 二轮复审增量回归：投递数公式、订阅计数强一致、反向索引清理、
//! Full/Disconnected 对称路径、会话 id 复用契约与载荷零拷贝共享

use aok::{OK, Void};
use bytes::Bytes;
use wedb_pubsub::{AsyncRx, PubSubMessage, create_session};

use super::support::setup;

/// 排干会话接收队列
fn drain(rx: &AsyncRx<PubSubMessage>) -> Vec<PubSubMessage> {
  let mut msgs = Vec::new();
  while let Ok(m) = rx.try_recv() {
    msgs.push(m);
  }
  msgs
}

/// 多频道多模式交叉投递数公式（对标 Redis 语义）：
/// 会话收到条数 =（精准订阅命中该频道 ? 1 : 0）+ 该会话每个匹配模式各 1 条 pmessage
#[test]
fn multi_channel_multi_pattern_delivery_formula() -> Void {
  let broker = setup();
  let (a, rx_a) = create_session(1, 64);
  let (b, rx_b) = create_session(2, 64);
  let (c, rx_c) = create_session(3, 64);

  // A：精准 c1、c2 + 模式 c*、?1；B：精准 c1 + 模式 c*；C：仅模式 *
  assert!(broker.subscribe(b"c1", &a));
  assert!(broker.subscribe(b"c2", &a));
  assert!(broker.psubscribe(b"c*", &a));
  assert!(broker.psubscribe(b"?1", &a));
  assert!(broker.subscribe(b"c1", &b));
  assert!(broker.psubscribe(b"c*", &b));
  assert!(broker.psubscribe(b"*", &c));

  // 重复订阅幂等：不改变任何计数
  assert!(!broker.subscribe(b"c1", &a));
  assert!(!broker.psubscribe(b"c*", &a));
  assert_eq!(broker.numsub(b"c1"), 2);
  assert_eq!(broker.numpat(), 3);

  // 发布 c1：A = 1 精准 + 2 模式 = 3；B = 1 + 1 = 2；C = 1，总计 6
  assert_eq!(broker.publish(b"c1", b"m1"), 6);
  let msgs_a = drain(&rx_a);
  assert_eq!(msgs_a.len(), 3);
  assert_eq!(
    msgs_a
      .iter()
      .filter(|m| matches!(m, PubSubMessage::Message { .. }))
      .count(),
    1,
    "精准频道恰好 1 条 message"
  );
  let pats_a: Vec<Bytes> = msgs_a
    .iter()
    .filter_map(|m| match m {
      PubSubMessage::PMessage { pattern, .. } => Some(pattern.clone()),
      _ => None,
    })
    .collect();
  assert!(pats_a.contains(&Bytes::from_static(b"c*")));
  assert!(pats_a.contains(&Bytes::from_static(b"?1")));
  assert_eq!(drain(&rx_b).len(), 2);
  assert_eq!(drain(&rx_c).len(), 1);

  // 发布 c2：A = 1 精准 + 1 模式(c*) = 2；B = 1 模式(c*)；C = 1，总计 4
  assert_eq!(broker.publish(b"c2", b"m2"), 4);
  let msgs_a = drain(&rx_a);
  assert_eq!(msgs_a.len(), 2);
  assert!(
    msgs_a
      .iter()
      .any(|m| matches!(m, PubSubMessage::PMessage { pattern, .. } if pattern.as_ref() == b"c*"))
  );
  assert_eq!(
    drain(&rx_b).len(),
    1,
    "B 无 c2 精准订阅，仅经 c* 模式收 1 条 pmessage"
  );
  assert_eq!(drain(&rx_c).len(), 1);

  broker.remove_subscription(a.id);
  broker.remove_subscription(b.id);
  broker.remove_subscription(c.id);
  assert!(broker.channels(None).is_empty());
  assert_eq!(broker.numpat(), 0);
  OK
}

/// numsub/numpat 与实际注册强一致：增删交错、幂等重复操作后计数不漂移
#[test]
fn subscription_counters_strong_consistency_under_interleave() -> Void {
  let broker = setup();
  let (s1, rx1) = create_session(11, 64);
  let (s2, rx2) = create_session(12, 64);
  let ch = b"interop";

  assert_eq!(broker.numsub(ch), 0);
  assert!(broker.subscribe(ch, &s1));
  assert!(broker.subscribe(ch, &s2));
  assert!(!broker.subscribe(ch, &s2), "重复订阅返回 false");
  assert_eq!(broker.numsub(ch), 2, "重复订阅不得使 numsub 漂移");
  assert_eq!(broker.session_subscription_count(s2.id), 1);

  assert!(broker.unsubscribe(ch, &s1));
  assert!(!broker.unsubscribe(ch, &s1), "重复退订返回 false");
  assert_eq!(broker.numsub(ch), 1, "重复退订不得使 numsub 漂移");

  // 频道与模式混合增删交错（s2 对 ch 重复订阅幂等）
  assert!(!broker.subscribe(ch, &s2));
  assert!(broker.psubscribe(b"inter*", &s2));
  assert!(broker.psubscribe(b"*op", &s2));
  assert_eq!(broker.numpat(), 2);
  assert_eq!(broker.session_subscription_count(s2.id), 3);
  assert_eq!(broker.list_all_subscriptions(s2.id).len(), 1);
  assert_eq!(broker.list_all_pattern_subscriptions(s2.id).len(), 2);

  assert!(broker.punsubscribe(b"inter*", &s2));
  assert!(!broker.punsubscribe(b"inter*", &s2));
  assert_eq!(broker.numpat(), 1, "重复退订不得使 numpat 漂移");

  // s1 会话状态已随退订归零回收，全量退订为空操作
  assert!(broker.unsubscribe_all(&s1).is_empty());
  assert!(broker.punsubscribe_all(&s1).is_empty());

  // s2 全量收尾
  assert!(broker.unsubscribe(ch, &s2));
  assert_eq!(broker.numsub(ch), 0);
  assert_eq!(broker.punsubscribe_all(&s2).len(), 1);
  assert_eq!(broker.numpat(), 0);
  assert_eq!(broker.session_subscription_count(s2.id), 0);
  assert!(broker.channels(None).is_empty());
  drop((rx1, rx2));
  OK
}

/// psubscribe 后 punsubscribe 的反向索引清理：退订模式即刻停止投递，
/// 仍被他人订阅的条目保留，无人订阅的条目归零回收（numpat 归零）
#[test]
fn pattern_unsubscribe_reverse_index_cleanup() -> Void {
  let broker = setup();
  let (a, rx_a) = create_session(21, 64);
  let (b, rx_b) = create_session(22, 64);

  assert!(broker.psubscribe(b"news.*", &a));
  assert!(broker.psubscribe(b"*.us", &a));
  assert!(broker.psubscribe(b"news.*", &b));
  assert_eq!(broker.numpat(), 2);

  // 发布 news.us：A 命中 2 模式 + B 命中 1 模式 = 3
  assert_eq!(broker.publish(b"news.us", b"x"), 3);
  assert_eq!(drain(&rx_a).len(), 2);
  assert_eq!(drain(&rx_b).len(), 1);

  // A 退订 news.*：news.* 条目因 B 仍订阅而保留，A 仅剩 *.us 投递
  assert!(broker.punsubscribe(b"news.*", &a));
  assert_eq!(broker.numpat(), 2, "news.* 仍有订阅者 B，计数不变");
  assert_eq!(broker.publish(b"news.us", b"y"), 2);
  for m in drain(&rx_a) {
    assert!(
      matches!(&m, PubSubMessage::PMessage { pattern, .. } if pattern.as_ref() == b"*.us"),
      "退订的模式不得再投递"
    );
  }
  assert_eq!(drain(&rx_b).len(), 1);

  // A 退订 *.us：A 不再收到任何投递，B 不受影响
  assert!(broker.punsubscribe(b"*.us", &a));
  assert_eq!(broker.publish(b"news.us", b"z"), 1);
  assert!(drain(&rx_a).is_empty());
  assert_eq!(drain(&rx_b).len(), 1);

  // B 退订后最后一个模式条目归零回收
  assert!(broker.punsubscribe(b"news.*", &b));
  assert_eq!(broker.numpat(), 0);
  assert_eq!(broker.publish(b"news.us", b"w"), 0);
  OK
}

/// Full 与 Disconnected 处理路径对称性：Full 平滑丢消息但保留会话注册，
/// Disconnected 就地摘除并级联清理会话状态
#[test]
fn full_preserves_session_while_disconnected_removes() -> Void {
  let broker = setup();
  let (s, rx) = create_session(31, 1); // 容量 1

  assert!(broker.subscribe(b"feed", &s));
  assert_eq!(broker.publish(b"feed", b"m1"), 1); // 入队即满
  assert_eq!(broker.publish(b"feed", b"m2"), 0); // 队列满 → Full 丢弃
  assert_eq!(broker.numsub(b"feed"), 1, "Full 不得摘除会话注册");
  assert_eq!(broker.session_subscription_count(s.id), 1);

  // 腾空队列后恢复投递
  assert!(rx.try_recv().is_ok());
  assert_eq!(broker.publish(b"feed", b"m3"), 1);
  assert!(rx.try_recv().is_ok());
  assert!(rx.try_recv().is_err());

  // 断连 → Disconnected → 级联摘除
  drop(rx);
  assert_eq!(broker.publish(b"feed", b"m4"), 0);
  assert_eq!(broker.numsub(b"feed"), 0, "Disconnected 必须摘除会话");
  assert_eq!(broker.session_subscription_count(s.id), 0);
  OK
}

/// 会话 id 复用契约：断连清理后同 id 新会话获得干净状态，旧订阅不残留、不串投
#[test]
fn session_id_reuse_gets_clean_state() -> Void {
  let broker = setup();
  let (old, rx_old) = create_session(7001, 64);
  broker.subscribe(b"legacy", &old);
  broker.psubscribe(b"leg*", &old);

  // 断连级联清理（对标服务端连接关闭路径）
  broker.remove_subscription(7001);
  drop(rx_old);
  assert_eq!(broker.numsub(b"legacy"), 0);

  // 同 id 复用为新会话
  let (renewed, rx_new) = create_session(7001, 64);
  assert!(broker.subscribe(b"fresh", &renewed));
  assert_eq!(broker.session_subscription_count(7001), 1);
  assert_eq!(broker.list_all_subscriptions(7001), vec![b"fresh".to_vec()]);
  assert!(broker.list_all_pattern_subscriptions(7001).is_empty());

  assert_eq!(
    broker.publish(b"legacy", b"stale"),
    0,
    "旧频道不得串投到复用会话"
  );
  assert_eq!(broker.publish(b"fresh", b"new"), 1);
  assert!(
    matches!(
      rx_new.try_recv()?,
      PubSubMessage::Message { ref channel, ref payload }
        if channel.as_ref() == b"fresh" && payload.as_ref() == b"new"
    ),
    "复用会话应仅收到新订阅消息"
  );
  OK
}

/// 零拷贝载荷共享契约：同一发布在各订阅者队列间共享同一底层分配（引用计数、不可变），
/// 共享期间任何一方都无法独占修改（Bytes 类型系统保证），接收方互不影响
#[test]
fn payload_shared_zero_copy_and_immutable() -> Void {
  let broker = setup();
  let (s1, rx1) = create_session(41, 8);
  let (s2, rx2) = create_session(42, 8);
  broker.subscribe(b"bus", &s1);
  broker.subscribe(b"bus", &s2);

  // 堆分配载荷（from_static 指向静态只读区，不走引用计数语义）
  assert_eq!(
    broker.publish_bytes(b"bus", Bytes::copy_from_slice(b"payload-x")),
    2
  );

  let m1 = rx1.try_recv()?;
  let m2 = rx2.try_recv()?;
  let (p1, p2) = match (m1, m2) {
    (PubSubMessage::Message { payload: p1, .. }, PubSubMessage::Message { payload: p2, .. }) => {
      (p1, p2)
    }
    _ => panic!("应收到精准频道消息"),
  };

  // 引用计数共享：两份句柄指向同一底层分配（无逐订阅者深拷贝）
  assert_eq!(p1.as_ptr(), p2.as_ptr(), "载荷应零拷贝共享同一分配");
  assert_eq!(p1.as_ref(), b"payload-x");
  assert_eq!(p2.as_ref(), b"payload-x");

  // 共享期间（引用计数 > 1）无法独占取走可变所有权
  assert!(p1.try_into_mut().is_err(), "共享期间不得独占修改");
  // p1 已随上一行临时值释放，p2 成为唯一持有者后方可独占，且不影响其他订阅者
  assert!(p2.try_into_mut().is_ok(), "唯一持有者可正常接管");
  OK
}
