use std::{sync::Arc, thread};

use aok::{OK, Void};
use bytes::Bytes;
use wedb_pubsub::{PubSubMessage, SubscribeBroker, create_session};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 核心消息 RESP 协议序列化与零拷贝预分配写入验证
#[test]
fn message_resp_serialization_and_exact_capacity() -> Void {
  let messages = [
    PubSubMessage::message(Bytes::from_static(b"chan"), Bytes::from_static(b"payload")),
    PubSubMessage::pmessage(
      Bytes::from_static(b"p*"),
      Bytes::from_static(b"chan"),
      Bytes::from_static(b"payload"),
    ),
    PubSubMessage::subscribe(Bytes::from_static(b"chan"), 42),
    PubSubMessage::unsubscribe(Bytes::from_static(b"chan"), 41),
    PubSubMessage::unsubscribe_null(0),
    PubSubMessage::psubscribe(Bytes::from_static(b"p*"), 10),
    PubSubMessage::punsubscribe(Bytes::from_static(b"p*"), 9),
    PubSubMessage::punsubscribe_null(0),
  ];

  for msg in &messages {
    for resp3 in [false, true] {
      let expected_len = msg.serialized_len(resp3);

      let bytes = msg.to_resp_bytes(resp3);
      assert_eq!(bytes.len(), expected_len);

      let mut buf = vec![0u8; expected_len];
      let written = msg.write_to(&mut buf, resp3)?;
      assert_eq!(written, expected_len);
      assert_eq!(&buf[..], bytes.as_ref());

      let mut vec_buf = Vec::new();
      msg.encode_to_vec(&mut vec_buf, resp3);
      assert_eq!(vec_buf.len(), expected_len);
      assert_eq!(vec_buf.capacity(), expected_len);
      assert_eq!(&vec_buf[..], bytes.as_ref());
    }
  }

  // 校验特定 RESP 格式字节输出
  let msg = PubSubMessage::message(Bytes::from_static(b"news"), Bytes::from_static(b"breaking"));
  assert_eq!(
    msg.to_resp2_bytes().as_ref(),
    b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$8\r\nbreaking\r\n"
  );
  assert_eq!(
    msg.to_resp3_bytes().as_ref(),
    b">3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$8\r\nbreaking\r\n"
  );

  OK
}

/// 核心消息 Bitcode 二进制序列化与反序列化双向无损测试
#[test]
fn message_bitcode_serialization_roundtrip() -> Void {
  let messages = [
    PubSubMessage::message(Bytes::from_static(b"chan"), Bytes::from_static(b"payload")),
    PubSubMessage::pmessage(
      Bytes::from_static(b"p*"),
      Bytes::from_static(b"chan"),
      Bytes::from_static(b"payload"),
    ),
    PubSubMessage::subscribe(Bytes::from_static(b"chan"), 42),
    PubSubMessage::unsubscribe(Bytes::from_static(b"chan"), 41),
    PubSubMessage::unsubscribe_null(0),
    PubSubMessage::psubscribe(Bytes::from_static(b"p*"), 10),
    PubSubMessage::punsubscribe(Bytes::from_static(b"p*"), 9),
    PubSubMessage::punsubscribe_null(0),
    PubSubMessage::Close,
  ];

  for msg in &messages {
    let encoded = msg.to_bitcode();
    assert!(!encoded.is_empty() || matches!(msg, PubSubMessage::Close));
    let decoded = PubSubMessage::from_bitcode(&encoded)?;
    assert_eq!(&decoded, msg);
  }

  OK
}

/// 核心端到端单频道订阅、发布与退订链路（对标 C# RespPubSubTests.BasicSUBSCRIBE 核心语义）
#[test]
fn core_subscribe_publish_end_to_end() -> Void {
  let broker = SubscribeBroker::new();
  let (session, rx) = create_session(1, 128);

  assert!(broker.subscribe(b"sports", &session));
  assert_eq!(broker.numsub(b"sports"), 1);

  let delivered = broker.publish_now(b"sports", b"goal!");
  assert_eq!(delivered, 1);

  let received = rx.try_recv()?;
  assert_eq!(
    received,
    PubSubMessage::message(Bytes::from_static(b"sports"), Bytes::from_static(b"goal!"))
  );

  assert!(broker.unsubscribe(b"sports", &session));
  assert_eq!(broker.numsub(b"sports"), 0);

  let delivered_none = broker.publish_now(b"sports", b"after unsub");
  assert_eq!(delivered_none, 0);
  assert!(rx.try_recv().is_err());
  OK
}

/// 核心端到端模式订阅匹配与广播链路（对标 C# RespPubSubTests.BasicPSUBSCRIBE 核心语义）
#[test]
fn core_pattern_subscribe_publish_end_to_end() -> Void {
  let broker = SubscribeBroker::new();
  let (session, rx) = create_session(2, 128);

  assert!(broker.pattern_subscribe(b"market.*", &session));
  assert_eq!(broker.numpat(), 1);

  let delivered = broker.publish_now(b"market.us", b"index up");
  assert_eq!(delivered, 1);

  let received = rx.try_recv()?;
  assert_eq!(
    received,
    PubSubMessage::pmessage(
      Bytes::from_static(b"market.*"),
      Bytes::from_static(b"market.us"),
      Bytes::from_static(b"index up")
    )
  );

  assert!(broker.pattern_unsubscribe(b"market.*", &session));
  assert_eq!(broker.numpat(), 0);
  OK
}

/// 核心高并发多线程发布订阅压力测试（对标 C# 并发发布订阅吞吐与隔离性）
#[test]
fn core_concurrent_publish_subscribe_stress() -> Void {
  let broker = Arc::new(SubscribeBroker::new());
  let thread_count = 8;
  let ops_per_thread = 100;

  let handles: Vec<_> = (0..thread_count)
    .map(|t| {
      let b = broker.clone();
      thread::spawn(move || {
        let (session, rx) = create_session(t as u64, 512);
        let channel = format!("chan_{}", t);
        let chan_bytes = channel.as_bytes();
        b.subscribe(chan_bytes, &session);

        for i in 0..ops_per_thread {
          let payload = format!("data_{}_{}", t, i);
          b.publish_now(chan_bytes, payload.as_bytes());
        }

        let mut count = 0;
        while rx.try_recv().is_ok() {
          count += 1;
        }
        assert!(count > 0);

        b.unsubscribe(chan_bytes, &session);
      })
    })
    .collect();

  for h in handles {
    h.join().unwrap();
  }

  assert!(broker.channels(None).is_empty());
  OK
}
