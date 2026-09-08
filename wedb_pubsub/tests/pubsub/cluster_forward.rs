use std::time::Duration;

use aok::{OK, Void};
use crossfire::Rx;
use wedb_pubsub::{PubSubMessage, create_session};

use super::support::setup;

/// 对标 C# ClusterPubSubForwardTests.ClusterPublishSurvivesPeerNodeShutdown
/// 验证集群节点故障隔离：对端节点异常断开后，本地 Broker 发布持续稳健工作，自动剔除断开节点并保障本地订阅者正常收发
#[test]
fn cluster_publish_survives_peer_node_shutdown() -> Void {
  let broker = setup();
  // 创建本地普通订阅者和模拟对端集群转发订阅者
  let (local_sub, rx_local) = create_session(401, 16);
  let (peer_forward_sub, rx_peer) = create_session(402, 16);
  let rx_local = Rx::from(rx_local);

  let channel = b"cluster-forward-channel";
  let message = b"forwarded-message";

  broker.subscribe(channel, &local_sub);
  broker.subscribe(channel, &peer_forward_sub);
  assert_eq!(broker.numsub(channel), 2);

  // 1. 基准测试：两节点正常时，广播给双方
  let delivered = broker.publish(channel, message);
  assert_eq!(delivered, 2);

  let m_loc = rx_local.recv_timeout(Duration::from_millis(50))?;
  assert!(
    matches!(m_loc, PubSubMessage::Message { ref payload, .. } if payload.as_ref() == message)
  );

  // 2. 模拟对端集群节点关机或断开 (Peer node shutdown)
  drop(rx_peer);

  // 3. 对端断开后，本地 Broker 执行 publish 稳健继续，不崩溃、不 panic，自动剔除孤儿 peer 会话
  let delivered2 = broker.publish(channel, b"message-after-peer-down");
  assert_eq!(delivered2, 1);

  // 4. 验证对端订阅已被彻底清理，本地订阅者持续正常接收
  assert_eq!(broker.numsub(channel), 1);
  let m_loc2 = rx_local.recv_timeout(Duration::from_millis(50))?;
  assert!(
    matches!(m_loc2, PubSubMessage::Message { ref payload, .. } if payload.as_ref() == b"message-after-peer-down")
  );
  assert!(
    rx_local.recv_timeout(Duration::from_millis(20)).is_err(),
    "不应有额外残留消息"
  );

  OK
}
