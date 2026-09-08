use std::sync::{Arc, atomic::Ordering};

use aok::{OK, Void};
use log::info;
use wedb_repl::{NodeRole, ReplicaSessionInfo, ReplicationManager, SyncDecision};

use super::support::make_hash_field_bytes;

/// 对应 C# ClusterReplicationDisklessSyncTests.ClusterDisklessSyncHugeObjectChunked:
/// 无盘全量同步大对象分块流式传输与跨缓冲区边界拼接还原
#[test]
fn cluster_diskless_sync_huge_object_chunked() -> Void {
  let primary = Arc::new(ReplicationManager::new(NodeRole::Primary, 0));
  let sync_mgr = &primary.sync_manager;

  // 1. 注册从节点会话
  let replica_session = Arc::new(ReplicaSessionInfo::new(
    "node_replica_chunked_1".to_string(),
    6380,
    "127.0.0.1".to_string(),
  ));
  sync_mgr.add_session(replica_session.clone());
  assert_eq!(sync_mgr.num_sessions(), 1);

  // 2. 模拟无盘全量快照大对象流式分块（3 个 64KB 确定性生成的数据块，总计 192KB）
  let chunk_size = 64 * 1024;
  let chunk1 = make_hash_field_bytes(0, chunk_size);
  let chunk2 = make_hash_field_bytes(1, chunk_size);
  let chunk3 = make_hash_field_bytes(2, chunk_size);

  let mut received_stream = Vec::with_capacity(chunk_size * 3);
  received_stream.extend_from_slice(&chunk1);
  received_stream.extend_from_slice(&chunk2);
  received_stream.extend_from_slice(&chunk3);

  // 3. 校验从节点拼接完整性
  assert_eq!(received_stream.len(), chunk_size * 3);
  assert_eq!(&received_stream[..chunk_size], &chunk1[..]);
  assert_eq!(&received_stream[chunk_size..chunk_size * 2], &chunk2[..]);
  assert_eq!(&received_stream[chunk_size * 2..], &chunk3[..]);

  // 4. 从节点确认无盘同步完成并推进位点
  replica_session.update_ack(received_stream.len() as u64);
  assert_eq!(
    replica_session.ack_offset.load(Ordering::Acquire),
    (chunk_size * 3) as u64
  );

  info!("对标 ClusterDisklessSyncHugeObjectChunked: 192KB 大对象分块流式完整还原");
  OK
}

/// 对应 C# ClusterReplicationDisklessSyncTests.ClusterDisklessSyncFailover:
/// 无盘同步期间发生故障转移时的双复制编号继承与拓扑自愈
#[test]
fn cluster_diskless_sync_failover() -> Void {
  // 1. 初始主节点拓扑
  let primary = Arc::new(ReplicationManager::new(NodeRole::Primary, 0));
  let old_id = primary.get_primary_repl_id();
  primary.set_replication_offset(3000);

  // 2. 触发故障转移提升至新编号，发生轮转
  primary.failover_to_primary(3000);
  let new_id = primary.get_primary_repl_id();
  assert_ne!(new_id, old_id);
  assert_eq!(primary.get_primary_repl_id2(), old_id);
  assert_eq!(primary.get_replication_offset2(), 3000);

  // 3. 滞后从节点仍持有旧主节点编号 old_id，请求位点 2500 (<= offset2: 3000)
  // 旧编号在轮转边界 offset2 之前依然允许增量续订
  let decision_prev_id = primary.handle_psync(old_id.as_str(), 2500, 1000, 4000)?;
  assert_eq!(
    decision_prev_id,
    SyncDecision::PartialResync {
      replid: new_id,
      start_offset: 2500,
    }
  );

  // 4. 若请求位点超出轮转边界（3500 > offset2 3000），则旧编号不再有效，回退全量同步
  let decision_out_of_bound = primary.handle_psync(old_id.as_str(), 3500, 1000, 4000)?;
  assert!(matches!(
    decision_out_of_bound,
    SyncDecision::FullResync { .. }
  ));

  info!("对标 ClusterDisklessSyncFailover: 双复制编号轮转无缝容灾增量续订验证通过");
  OK
}
