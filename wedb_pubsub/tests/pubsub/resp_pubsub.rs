use std::time::Duration;

use aok::{OK, Void};
use crossfire::Rx;
use wedb_pubsub::{PubSubMessage, create_session};

use super::support::{MockSession, read_available, send_command, setup, subscribe_and_publish};

/// 对标 C# RespPubSubTests.BasicSUBSCRIBE
/// 基础单频道订阅与消息发布投递
#[test]
fn basic_subscribe() -> Void {
  let broker = setup();
  let (session, rx) = create_session(1, 16);
  let rx = Rx::from(rx);
  let channel = b"messages";
  let value = b"published message";

  let mut received_channel = Vec::new();
  let mut received_message = Vec::new();
  let mut called = false;

  subscribe_and_publish(
    &broker,
    &session,
    channel,
    false,
    channel,
    value,
    |ch, msg| {
      received_channel.extend_from_slice(ch);
      received_message.extend_from_slice(msg);
      called = true;
    },
  );

  assert!(called, "应当成功执行订阅接收");
  assert_eq!(received_channel.as_slice(), channel);
  assert_eq!(received_message.as_slice(), value);

  let msg = rx.recv_timeout(Duration::from_secs(1))?;
  match msg {
    PubSubMessage::Message {
      channel: ch,
      payload,
    } => {
      assert_eq!(&ch[..], channel);
      assert_eq!(&payload[..], value);
    }
    _ => panic!("收到非预期的消息类型"),
  }

  assert!(broker.unsubscribe(channel, &session), "退订频道应当成功");
  OK
}

/// 对标 C# RespPubSubTests.LargeSUBSCRIBE
/// 140KB 大报文消息订阅与广播投递（零额外克隆，原地借用验证）
#[test]
fn large_subscribe() -> Void {
  let broker = setup();
  let (session, rx) = create_session(2, 16);
  let rx = Rx::from(rx);
  let channel = b"messages";

  let large_size = 140 * 1024;
  let mut large_value = vec![0u8; large_size];
  for b in &mut large_value {
    *b = fastrand::u8(..);
  }

  let mut received_len = 0usize;
  let mut called = false;

  subscribe_and_publish(
    &broker,
    &session,
    channel,
    false,
    channel,
    &large_value,
    |ch, msg| {
      assert_eq!(ch, b"messages");
      assert_eq!(msg, large_value.as_slice());
      received_len = msg.len();
      called = true;
    },
  );

  assert!(called, "大报文应当成功接收");
  assert_eq!(received_len, large_size);

  let msg = rx.recv_timeout(Duration::from_secs(1))?;
  match msg {
    PubSubMessage::Message {
      channel: ch,
      payload,
    } => {
      assert_eq!(&ch[..], channel);
      assert_eq!(payload.len(), large_size);
      assert_eq!(&payload[..], &large_value[..]);
    }
    _ => panic!("收到非预期的消息类型"),
  }

  broker.unsubscribe(channel, &session);
  OK
}

/// 对标 C# RespPubSubTests.BasicPSUBSCRIBE
/// 模式订阅匹配与投递
#[test]
fn basic_psubscribe() -> Void {
  let broker = setup();
  let (session, rx) = create_session(3, 16);
  let rx = Rx::from(rx);
  let glob = b"messagesA*";
  let actual = b"messagesAtest";
  let value = b"published message";

  let mut received_channel = Vec::new();
  let mut called = false;

  subscribe_and_publish(&broker, &session, glob, true, actual, value, |ch, _| {
    received_channel.extend_from_slice(ch);
    called = true;
  });

  assert!(called, "应当成功执行模式订阅接收");
  assert_eq!(received_channel.as_slice(), actual);

  let msg = rx.recv_timeout(Duration::from_secs(1))?;
  match msg {
    PubSubMessage::PMessage {
      pattern,
      channel,
      payload,
    } => {
      assert_eq!(&pattern[..], glob);
      assert_eq!(&channel[..], actual);
      assert_eq!(&payload[..], value);
    }
    _ => panic!("应当收到模式匹配消息"),
  }

  broker.pattern_unsubscribe(glob, &session);
  OK
}

/// 对标 C# RespPubSubTests.BasicPUBSUB_CHANNELS
/// PUBSUB CHANNELS 无参数与通配符过滤
#[test]
fn pubsub_channels() -> Void {
  let broker = setup();
  let (session, _) = create_session(4, 16);

  let channel_a = b"messagesAtest";
  let channel_b = b"messagesB";

  broker.subscribe(channel_a, &session);
  broker.subscribe(channel_b, &session);

  let active = broker.get_channels(None);
  assert!(active.iter().any(|c| c == channel_a));
  assert!(active.iter().any(|c| c == channel_b));

  let matching_star = broker.get_channels(Some(b"messages*"));
  assert_eq!(matching_star.len(), 2);
  assert!(matching_star.iter().any(|c| c == channel_a));
  assert!(matching_star.iter().any(|c| c == channel_b));

  let matching_q = broker.get_channels(Some(b"messages?test"));
  assert_eq!(matching_q.len(), 1);
  assert_eq!(matching_q[0], channel_a);

  let matching_none = broker.get_channels(Some(b"messagesC*"));
  assert_eq!(matching_none.len(), 0);

  broker.unsubscribe(channel_a, &session);
  broker.unsubscribe(channel_b, &session);
  OK
}

/// 对标 C# RespPubSubTests.BasicPUBSUB_NUMPAT
/// PUBSUB NUMPAT 模式订阅计数
#[test]
fn pubsub_numpat() -> Void {
  let broker = setup();
  let (session, _) = create_session(5, 16);

  let glob_a = b"com.messages.*";
  let glob_b = b"com.messagesB.*";

  assert_eq!(broker.num_pattern_subscriptions(), 0);

  broker.pattern_subscribe(glob_a, &session);
  broker.pattern_subscribe(glob_b, &session);
  assert_eq!(broker.num_pattern_subscriptions(), 2);

  broker.pattern_unsubscribe(glob_a, &session);
  assert_eq!(broker.num_pattern_subscriptions(), 1);

  broker.pattern_unsubscribe(glob_b, &session);
  assert_eq!(broker.num_pattern_subscriptions(), 0);
  OK
}

/// 对标 C# RespPubSubTests.BasicPUBSUB_NUMSUB
/// PUBSUB NUMSUB 频道订阅计数与多频道统计
#[test]
fn pubsub_numsub() -> Void {
  let broker = setup();
  let (session_a, _) = create_session(6, 16);
  let (session_b, _) = create_session(7, 16);

  let ch_a = b"messagesA";
  let ch_b = b"messagesB";

  assert_eq!(broker.num_subscriptions(ch_a), 0);
  assert_eq!(broker.num_subscriptions(ch_b), 0);

  broker.subscribe(ch_a, &session_a);
  broker.subscribe(ch_b, &session_a);
  assert_eq!(broker.num_subscriptions(ch_a), 1);
  assert_eq!(broker.num_subscriptions(ch_b), 1);

  broker.subscribe(ch_a, &session_b);
  assert_eq!(broker.num_subscriptions(ch_a), 2);
  assert_eq!(broker.num_subscriptions(ch_b), 1);

  let counts = broker.numsub_multi(&[ch_a, ch_b, b"nonexistent"]);
  assert_eq!(
    counts,
    vec![
      (ch_a.to_vec(), 2),
      (ch_b.to_vec(), 1),
      (b"nonexistent".to_vec(), 0),
    ]
  );

  broker.unsubscribe(ch_a, &session_a);
  assert_eq!(broker.num_subscriptions(ch_a), 1);

  broker.unsubscribe(ch_a, &session_b);
  assert_eq!(broker.num_subscriptions(ch_a), 0);

  broker.unsubscribe(ch_b, &session_a);
  assert_eq!(broker.num_subscriptions(ch_b), 0);
  OK
}

/// 对标 C# RespPubSubTests.PubSubModeRejectsDisallowedCommandsInResp2
/// RESP2 订阅模式下拦截非白名单命令
#[test]
fn pubsub_mode_rejects_disallowed_commands_in_resp2() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 8);

  let subscribe_resp = "*3\r\n$9\r\nsubscribe\r\n$3\r\nfoo\r\n:1\r\n";
  assert_eq!(client.execute("SUBSCRIBE foo"), subscribe_resp);

  let get_err = "-ERR Can't execute 'GET': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT are allowed in this context\r\n";
  assert_eq!(client.execute("GET bar"), get_err);

  let set_err = "-ERR Can't execute 'SET': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT are allowed in this context\r\n";
  assert_eq!(client.execute("SET bar value"), set_err);

  let pub_err = "-ERR Can't execute 'PUBLISH': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT are allowed in this context\r\n";
  assert_eq!(client.execute("PUBLISH foo bar"), pub_err);

  let multi_err = "-ERR Can't execute 'MULTI': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT are allowed in this context\r\n";
  assert_eq!(client.execute("MULTI"), multi_err);
  OK
}

/// 对标 C# RespPubSubTests.PubSubModeAllowsValidCommandsInResp2
/// RESP2 订阅模式下允许白名单命令且退出后恢复普通命令
#[test]
fn pubsub_mode_allows_valid_commands_in_resp2() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 9);

  assert_eq!(
    client.execute("SUBSCRIBE foo"),
    "*3\r\n$9\r\nsubscribe\r\n$3\r\nfoo\r\n:1\r\n"
  );
  assert_eq!(client.execute("PING"), "*2\r\n$4\r\npong\r\n$0\r\n\r\n");
  assert_eq!(
    client.execute("SUBSCRIBE bar"),
    "*3\r\n$9\r\nsubscribe\r\n$3\r\nbar\r\n:2\r\n"
  );
  assert_eq!(
    client.execute("PSUBSCRIBE baz*"),
    "*3\r\n$10\r\npsubscribe\r\n$4\r\nbaz*\r\n:3\r\n"
  );
  assert_eq!(
    client.execute("UNSUBSCRIBE bar"),
    "*3\r\n$11\r\nunsubscribe\r\n$3\r\nbar\r\n:2\r\n"
  );
  assert_eq!(
    client.execute("PUNSUBSCRIBE baz*"),
    "*3\r\n$12\r\npunsubscribe\r\n$4\r\nbaz*\r\n:1\r\n"
  );

  let get_err = "-ERR Can't execute 'GET': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT are allowed in this context\r\n";
  assert_eq!(client.execute("GET bar"), get_err);

  assert_eq!(
    client.execute("UNSUBSCRIBE foo"),
    "*3\r\n$11\r\nunsubscribe\r\n$3\r\nfoo\r\n:0\r\n"
  );
  assert_eq!(client.execute("GET bar"), "$-1\r\n");
  OK
}

/// 对标 C# RespPubSubTests.PubSubSelfPublishResp3NoLockError
/// RESP3 订阅模式自发布无死锁且接收 Push 帧
#[test]
fn pubsub_self_publish_resp3_no_lock_error() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 10);

  assert!(client.execute("HELLO 3").contains("proto"));
  assert_eq!(
    client.execute("SUBSCRIBE foo"),
    "*3\r\n$9\r\nsubscribe\r\n$3\r\nfoo\r\n:1\r\n"
  );

  let expected_push = ">3\r\n$7\r\nmessage\r\n$3\r\nfoo\r\n$3\r\nbar\r\n";
  let expected_pub = ":1\r\n";
  let total = format!("{}{}", expected_push, expected_pub);
  assert_eq!(client.execute("PUBLISH foo bar"), total);

  assert_eq!(
    client.execute("UNSUBSCRIBE foo"),
    "*3\r\n$11\r\nunsubscribe\r\n$3\r\nfoo\r\n:0\r\n"
  );
  assert_eq!(client.execute("PING"), "+PONG\r\n");
  OK
}

/// 对标 C# RespPubSubTests.PubSubSelfPatternPublishResp3NoLockError
/// RESP3 模式订阅自发布匹配投递无死锁
#[test]
fn pubsub_self_pattern_publish_resp3_no_lock_error() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 11);

  assert!(client.execute("HELLO 3").contains("proto"));
  assert_eq!(
    client.execute("PSUBSCRIBE foo*"),
    "*3\r\n$10\r\npsubscribe\r\n$4\r\nfoo*\r\n:1\r\n"
  );

  let expected_push = ">4\r\n$8\r\npmessage\r\n$4\r\nfoo*\r\n$6\r\nfoobar\r\n$3\r\nbaz\r\n";
  let expected_pub = ":1\r\n";
  let total = format!("{}{}", expected_push, expected_pub);
  assert_eq!(client.execute("PUBLISH foobar baz"), total);

  assert_eq!(
    client.execute("PUNSUBSCRIBE foo*"),
    "*3\r\n$12\r\npunsubscribe\r\n$4\r\nfoo*\r\n:0\r\n"
  );
  assert_eq!(client.execute("PING"), "+PONG\r\n");
  OK
}

/// 对标 C# RespPubSubTests.PubSubModeAllowsRegularCommandsInResp3
/// RESP3 订阅模式下允许普通命令正常执行
#[test]
fn pubsub_mode_allows_regular_commands_in_resp3() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 12);

  assert!(client.execute("HELLO 3").contains("proto"));
  assert_eq!(
    client.execute("SUBSCRIBE foo"),
    "*3\r\n$9\r\nsubscribe\r\n$3\r\nfoo\r\n:1\r\n"
  );

  assert_eq!(client.execute("SET mykey myval"), "+OK\r\n");
  assert_eq!(client.execute("GET mykey"), "$5\r\nmyval\r\n");
  assert_eq!(
    client.execute("UNSUBSCRIBE foo"),
    "*3\r\n$11\r\nunsubscribe\r\n$3\r\nfoo\r\n:0\r\n"
  );
  OK
}

/// 对标 C# RespPubSubTests.PubSubModeViaPsubscribeRejectsCommandsInResp2
/// 仅通过 PSUBSCRIBE 进入订阅模式时拦截普通命令
#[test]
fn pubsub_mode_via_psubscribe_rejects_commands_in_resp2() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 13);

  assert_eq!(
    client.execute("PSUBSCRIBE foo*"),
    "*3\r\n$10\r\npsubscribe\r\n$4\r\nfoo*\r\n:1\r\n"
  );

  let error_resp = "-ERR Can't execute 'GET': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT are allowed in this context\r\n";
  assert_eq!(client.execute("GET bar"), error_resp);

  assert_eq!(
    client.execute("PUNSUBSCRIBE foo*"),
    "*3\r\n$12\r\npunsubscribe\r\n$4\r\nfoo*\r\n:0\r\n"
  );
  assert_eq!(client.execute("GET bar"), "$-1\r\n");
  OK
}

/// 对标 C# RespPubSubTests.SelfPublishOnSubscribedChannelDoesNotCorruptConnection
/// 自发布到订阅频道不损坏连接
#[test]
fn self_publish_on_subscribed_channel_does_not_corrupt_connection() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 14);

  assert!(client.execute("HELLO 3").contains("proto"));

  let channel = "self-publish-channel";
  let message = "self-published-message";

  let sub_resp = send_command(&client, &["SUBSCRIBE", channel]);
  assert!(sub_resp.contains("subscribe"));
  assert!(sub_resp.contains(channel));

  let pub_resp = send_command(&client, &["PUBLISH", channel, message]);
  assert!(pub_resp.contains(message));
  assert!(pub_resp.contains(":1"));

  let ping_resp = send_command(&client, &["PING"]);
  assert!(ping_resp.contains("PONG") || ping_resp.contains("pong"));
  OK
}

/// 对标 C# RespPubSubTests.SelfPublishOnPatternSubscribedChannelDoesNotCorruptConnection
/// 模式自发布不损坏连接
#[test]
fn self_publish_on_pattern_subscribed_channel_does_not_corrupt_connection() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 15);

  assert!(client.execute("HELLO 3").contains("proto"));

  let pattern = "self-publish-pattern-*";
  let channel = "self-publish-pattern-channel";
  let message = "self-published-message";

  let sub_resp = send_command(&client, &["PSUBSCRIBE", pattern]);
  assert!(sub_resp.contains("psubscribe"));
  assert!(sub_resp.contains(pattern));

  let pub_resp = send_command(&client, &["PUBLISH", channel, message]);
  assert!(pub_resp.contains(message));
  assert!(pub_resp.contains(":1"));

  let ping_resp = send_command(&client, &["PING"]);
  assert!(ping_resp.contains("PONG") || ping_resp.contains("pong"));
  OK
}

/// 对标 C# RespPubSubTests 及 RESP 协议规范：无参数退订全部频道
#[test]
fn unsubscribe_all_command_behavior() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 501);

  client.execute("SUBSCRIBE foo bar baz");
  assert_eq!(client.subscription_count(), 3);

  let unsub_resp = client.execute("UNSUBSCRIBE");
  assert_eq!(client.subscription_count(), 0);
  assert!(unsub_resp.contains("unsubscribe"));
  assert!(unsub_resp.contains(":0"));

  let empty_resp = client.execute("UNSUBSCRIBE");
  assert_eq!(empty_resp, "*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n");
  OK
}

/// 对标 C# RespPubSubTests 及 RESP 协议规范：无参数退订全部模式
#[test]
fn punsubscribe_all_command_behavior() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 502);

  client.execute("PSUBSCRIBE news.* sports.*");
  assert_eq!(client.subscription_count(), 2);

  let punsub_resp = client.execute("PUNSUBSCRIBE");
  assert_eq!(client.subscription_count(), 0);
  assert!(punsub_resp.contains("punsubscribe"));
  assert!(punsub_resp.contains(":0"));

  let empty_resp = client.execute("PUNSUBSCRIBE");
  assert_eq!(empty_resp, "*3\r\n$12\r\npunsubscribe\r\n$-1\r\n:0\r\n");
  OK
}

/// 验证命令发送与响应工具函数可用性（对标 C# SendCommand 与 ReadAvailable 辅助逻辑）
#[test]
fn command_utils() -> Void {
  let broker = setup();
  let client = MockSession::new(broker, 301);

  let resp = send_command(&client, &["PING"]);
  assert_eq!(resp, "+PONG\r\n");

  let resp2 = read_available(&client, "PING");
  assert_eq!(resp2, "+PONG\r\n");

  assert!(client.try_recv().is_err(), "新会话初始接收队列应当为空");
  OK
}
