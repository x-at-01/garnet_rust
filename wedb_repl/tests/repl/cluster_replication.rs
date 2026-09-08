use std::{
  sync::{Arc, atomic::Ordering},
  thread,
};

use aok::{OK, Void};
use log::info;
use tempfile::tempdir;
use wedb_repl::{
  AofSyncDriver, DEFAULT_BACKLOG_SIZE, NodeRole, RecoveryStatus, ReplId, ReplicaSessionInfo,
  ReplicationHistory, ReplicationManager, SyncDecision,
};

/// 对应 C# ClusterReplicationBaseTests.ClusterSRTest:
/// 基础主从建立、写入位点对齐与增量同步状态机校验
#[test]
fn cluster_sr() -> Void {
  // 1. 初始化主从复制管理器
  let primary = Arc::new(ReplicationManager::new(NodeRole::Primary, 0));
  let replica = Arc::new(ReplicationManager::new(NodeRole::Replica, 0));

  assert!(primary.is_primary());
  assert!(replica.is_replica());
  assert_eq!(primary.role(), NodeRole::Primary);
  assert_eq!(replica.role(), NodeRole::Replica);

  // 2. 主节点写入数据并推进复制位点
  let tail_offset = 1024u64;
  primary.set_replication_offset(tail_offset);
  assert_eq!(primary.get_replication_offset(), tail_offset);

  // 3. 从节点发起增量握手与回放数据流
  let req_id = primary.get_primary_repl_id();
  let decision = primary.handle_psync(req_id.as_str(), 0, 0, tail_offset)?;
  assert_eq!(
    decision,
    SyncDecision::PartialResync {
      replid: req_id,
      start_offset: 0,
    }
  );

  // 4. 从节点应用主节点增量数据流并对齐自身位点
  replica.set_replication_offset(tail_offset);
  replica.update_last_primary_sync_time();

  assert_eq!(
    replica.get_replication_offset(),
    primary.get_replication_offset()
  );
  assert_eq!(replica.get_last_primary_sync_seconds(), 0);

  info!("对标 ClusterSRTest: 基础主从数据位点完全对齐");
  OK
}

/// 对应 C# ClusterReplicationBaseTests.ClusterSRNoCheckpointRestartSecondary:
/// 无快照检查点下从节点重启，基于增量预写日志重新拉起并追赶
#[test]
fn cluster_sr_no_checkpoint_restart_secondary() -> Void {
  // 1. 主节点累积预写日志 (begin=100, tail=5000)
  let primary = Arc::new(ReplicationManager::new(NodeRole::Primary, 100));
  let primary_replid = primary.get_primary_repl_id();
  primary.set_replication_offset(5000);

  // 2. 从节点重启前记录断点为 2000
  let replica = Arc::new(ReplicationManager::new(NodeRole::Replica, 2000));
  replica.try_update_my_primary_repl_id(primary_replid);

  // 3. 从节点发送增量同步协商请求，请求位点为 2000
  let decision = primary.handle_psync(primary_replid.as_str(), 2000, 100, 5000)?;

  // 4. 主节点判定为增量续订，起始位点对齐从节点断点 2000
  assert_eq!(
    decision,
    SyncDecision::PartialResync {
      replid: primary_replid,
      start_offset: 2000,
    }
  );

  // 5. 挂载 AofSyncDriver 推送 2000..5000 增量日志
  let driver = AofSyncDriver::new("replica_node_1".to_string(), 2000);
  assert_eq!(driver.get_start_address(), 2000);
  assert_eq!(driver.get_previous_address(), 2000);

  driver.advance_watermark(5000);
  assert_eq!(driver.get_previous_address(), 2000);
  assert_eq!(driver.get_shipped_watermark_address(), 5000);

  // 从节点应用追赶后更新位点至 5000
  replica.set_replication_offset(5000);
  assert_eq!(
    replica.get_replication_offset(),
    primary.get_replication_offset()
  );

  info!("对标 ClusterSRNoCheckpointRestartSecondary: 从节点重启增量追赶通过");
  OK
}

/// advance_watermark 双水位线单调性回归：乱序推进下绝不回退
#[test]
fn aof_sync_driver_watermark_monotonic_under_out_of_order_advance() -> Void {
  // 1. 乱序（较旧位点晚于较新位点）到达时，shipped_watermark 绝不允许回退
  let driver = AofSyncDriver::new("replica_node_ooo".to_string(), 100);
  driver.advance_watermark(1000);
  driver.advance_watermark(500);
  assert_eq!(driver.get_shipped_watermark_address(), 1000);
  driver.advance_watermark(2000);
  assert_eq!(driver.get_shipped_watermark_address(), 2000);
  // 上一位点始终不超前于已发送水位线
  assert!(driver.get_previous_address() <= driver.get_shipped_watermark_address());

  // 2. 多线程并发乱序推进，最终水位线必须精确等于推送过的最大地址
  let driver = Arc::new(AofSyncDriver::new("replica_node_wc".to_string(), 100));
  let handles: Vec<_> = (0..8)
    .map(|t| {
      let driver = Arc::clone(&driver);
      thread::spawn(move || {
        // 偶数线程升序推进、奇数线程降序推进，制造最恶劣的水位线竞态
        for i in 0..512u64 {
          let addr = if t % 2 == 0 { 101 + i } else { 4096 - i };
          driver.advance_watermark(addr);
        }
      })
    })
    .collect();
  for h in handles {
    h.join().expect("水位线推进线程不应 panic");
  }

  assert_eq!(driver.get_shipped_watermark_address(), 4096);
  assert!(driver.get_previous_address() <= driver.get_shipped_watermark_address());

  info!("AofSyncDriver 双水位线单调性回归验证通过");
  OK
}

/// 对应 C# ClusterReplicationBaseTests.ClusterSRPrimaryCheckpointAsync:
/// 主节点触发快照检查点期间锁定状态，完成后平滑衔接后续增量复制
#[test]
fn cluster_sr_primary_checkpoint_async() -> Void {
  let primary = Arc::new(ReplicationManager::new(NodeRole::Primary, 0));
  let replica = Arc::new(ReplicationManager::new(NodeRole::Replica, 0));

  // 1. 主节点初次写入至 2048
  primary.set_replication_offset(2048);

  // 2. 主节点触发快照开始（锁定角色为只读态，防止快照写入冲突）
  primary.checkpoint_version_shift_start();
  assert_eq!(*primary.recovery_status.read(), RecoveryStatus::ReadRole);
  // 对标 C# ReplicationManager.CannotStreamAOF：ReadRole 仅锁定角色突变（IsRecovering=false），
  // 检查点版本轮转期间日志流推送继续，绝不阻断从节点追赶
  assert!(primary.can_stream_aof());

  // 从节点角色调用版本轮转必须被旁路（对标 C# CheckpointVersionShiftStart/End 的 replica 早退）
  replica.checkpoint_version_shift_start();
  assert_eq!(*replica.recovery_status.read(), RecoveryStatus::NoRecovery);

  // 3. 快照持久化完成，更新安全恢复位点并结束版本轮转
  primary.set_safe_aof_address(2048);
  primary.checkpoint_version_shift_end();
  replica.checkpoint_version_shift_end();
  assert_eq!(*primary.recovery_status.read(), RecoveryStatus::NoRecovery);
  assert!(primary.can_stream_aof());
  assert_eq!(primary.get_current_safe_aof_address(), 2048);

  // 4. 快照后继续产生新写入至 4096
  primary.set_replication_offset(4096);

  // 5. 从节点自位点 2048 发起增量追赶，平滑衔接无丢失
  let decision = primary.handle_psync(primary.get_primary_repl_id().as_str(), 2048, 2048, 4096)?;
  assert_eq!(
    decision,
    SyncDecision::PartialResync {
      replid: primary.get_primary_repl_id(),
      start_offset: 2048,
    }
  );

  replica.set_replication_offset(4096);
  assert_eq!(replica.get_replication_offset(), 4096);

  info!("对标 ClusterSRPrimaryCheckpointAsync: 快照版本轮转与连续增量验证通过");
  OK
}

/// 对应 C# ClusterReplicationBaseTests.ClusterSRAddReplicaAfterPrimaryCheckpoint:
/// 主节点已打快照后新挂从节点，判定触发全量快照导入与后续预写日志追赶
#[test]
fn cluster_sr_add_replica_after_primary_checkpoint() -> Void {
  // 1. 主节点已有历史，当前位点 8192，且旧日志已截断至 4096
  let primary = Arc::new(ReplicationManager::new(NodeRole::Primary, 0));
  let primary_replid = primary.get_primary_repl_id();
  let wal_begin = 4096u64;
  let wal_tail = 8192u64;
  primary.set_replication_offset(wal_tail);

  // 2. 新接入全新从节点，测试标准 Redis '?' 与 40 字节 '?' 简写兼容
  let decision_new = primary.handle_psync("?", -1, wal_begin, wal_tail)?;
  assert_eq!(
    decision_new,
    SyncDecision::FullResync {
      replid: primary_replid,
      snapshot_offset: wal_tail,
    }
  );

  let decision_40q = primary.handle_psync(
    "????????????????????????????????????????",
    -1,
    wal_begin,
    wal_tail,
  )?;
  assert_eq!(decision_40q, decision_new);

  // 3. 落后从节点请求已被截断的位点 (1000 < wal_begin 4096)，必须触发全量同步
  let decision_lagging =
    primary.handle_psync(primary_replid.as_str(), 1000, wal_begin, wal_tail)?;
  assert!(matches!(decision_lagging, SyncDecision::FullResync { .. }));

  info!("对标 ClusterSRAddReplicaAfterPrimaryCheckpoint: 快照后新从节点接入验证通过");
  OK
}

/// 对应 C# ClusterReplicationBaseTests.ClusterSRPrimaryRestart:
/// 主节点重启后从磁盘加载复制历史，从节点重新连接恢复增量复制
#[test]
fn cluster_sr_primary_restart() -> Void {
  let temp_dir = tempdir()?;
  let conf_path = temp_dir.path().join("replication.conf");

  // 1. 主节点运行并生成复制历史，持久化刷盘
  let primary_before = Arc::new(ReplicationManager::new(NodeRole::Primary, 5000));
  primary_before.save_history(&conf_path)?;
  let saved_replid = primary_before.get_primary_repl_id();

  // 2. 模拟主节点停机并从磁盘加载配置重启
  let recovered_history = ReplicationHistory::load_from_file(&conf_path)?;
  assert_eq!(recovered_history.primary_replid, saved_replid);
  assert_eq!(recovered_history.replication_offset, 5000);

  let primary_after = Arc::new(ReplicationManager::new(NodeRole::Primary, 0));
  *primary_after.history.write() = recovered_history;

  // 3. 从节点带重启前断点 4500 重连，验证主节点 ID 一致并允许增量续订
  let decision = primary_after.handle_psync(saved_replid.as_str(), 4500, 4000, 5000)?;
  assert_eq!(
    decision,
    SyncDecision::PartialResync {
      replid: saved_replid,
      start_offset: 4500,
    }
  );

  info!("对标 ClusterSRPrimaryRestart: 主节点重启后增量恢复验证通过");
  OK
}

/// 对应 C# ClusterReplicationBaseTests.ClusterSRReplicaOfTest:
/// 从节点收到 REPLICAOF NO ONE 自立为主，触发双复制编号轮转
#[test]
fn cluster_sr_replica_of() -> Void {
  // 1. 初始为从节点
  let node = Arc::new(ReplicationManager::new(NodeRole::Replica, 1000));
  assert!(node.is_replica());
  let old_replid = node.get_primary_repl_id();

  // 2. 收到脱离主节点指令自立为主
  node.replica_of_no_one(1500);

  // 3. 校验角色切换为主节点，且完成双复制编号轮转
  assert!(node.is_primary());
  assert_eq!(*node.role.read(), NodeRole::Primary);
  assert_eq!(*node.recovery_status.read(), RecoveryStatus::NoRecovery);

  // 旧主编号降级为上一代编号 replid2，截断位点为 1500
  assert_eq!(node.get_primary_repl_id2(), old_replid);
  assert_eq!(node.get_replication_offset2(), 1500);

  // 生成全新主复制编号
  let new_replid = node.get_primary_repl_id();
  assert_ne!(new_replid, old_replid);
  assert_eq!(node.get_replication_offset(), 1500);

  info!("对标 ClusterSRReplicaOfTest: 动态角色切换与双复制编号轮转验证通过");
  OK
}

/// 对应 C# ClusterReplicationBaseTests:
/// 异步回放缓冲与心跳延迟滞后量统计、安全截断水位线保护
#[test]
fn cluster_replication_async_replay() -> Void {
  let primary = Arc::new(ReplicationManager::new(NodeRole::Primary, 0));
  let sync_mgr = &primary.sync_manager;

  // 1. 注册两个从节点会话
  let replica1 = Arc::new(ReplicaSessionInfo::new(
    "replica_1".to_string(),
    6381,
    "127.0.0.1".to_string(),
  ));
  let replica2 = Arc::new(ReplicaSessionInfo::new(
    "replica_2".to_string(),
    6382,
    "127.0.0.1".to_string(),
  ));
  sync_mgr.add_session(replica1.clone());
  sync_mgr.add_session(replica2.clone());

  // 2. 主节点写入至 10000 字节
  primary.set_replication_offset(10000);

  // 3. 从节点 1 确认位点 9500 (滞后 500)，从节点 2 确认位点 8000 (滞后 2000)
  replica1.update_ack(9500);
  assert_eq!(replica1.lag(10000), 500);

  replica2.update_ack(8000);
  assert_eq!(replica2.lag(10000), 2000);

  // 4. 校验主节点计算的安全截断保留水位线（必须保留至最慢从节点的位点 8000）
  let min_safe_offset = sync_mgr.min_ack_offset(10000);
  assert_eq!(min_safe_offset, 8000);

  // 5. 模拟两节点陆续追赶完毕
  replica2.update_ack(10000);
  assert_eq!(replica2.lag(10000), 0);
  assert_eq!(sync_mgr.min_ack_offset(10000), 9500);

  replica1.update_ack(10000);
  assert_eq!(sync_mgr.min_ack_offset(10000), 10000);

  info!("对标 ClusterReplicationAsyncReplay: 心跳确认与滞后量统计验证通过");
  OK
}

/// 心跳超时僵死从节点剔除与降级清空会话的生命周期闭环
#[test]
fn cluster_replication_stale_replica_eviction_and_demote() -> Void {
  let primary = ReplicationManager::new(NodeRole::Primary, 0);
  let s1 = primary.register_replica("n_active".to_string(), 6381, "127.0.0.1".to_string());
  let s2 = primary.register_replica("n_stale".to_string(), 6382, "127.0.0.1".to_string());
  assert_eq!(primary.get_connected_replicas_count(), 2);

  // 1. 人工将 s2 的心跳时间戳回拨 5000ms，模拟僵死从节点
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  s2.last_ack_timestamp_ms
    .store(now.saturating_sub(5000), Ordering::Release);

  assert!(!s1.is_timed_out(1000), "活跃从节点不得被判定超时");
  assert!(s2.is_timed_out(1000), "回拨 5 秒的从节点必须被判定超时");

  // 2. 仅查询不剔除：发现 s2 超时但会话仍在
  assert_eq!(
    primary.get_timed_out_replicas(1000),
    vec!["n_stale".to_string()]
  );
  assert_eq!(primary.get_connected_replicas_count(), 2);

  // 3. evict_timed_out_replicas 原子剔除僵死会话
  let evicted = primary.evict_timed_out_replicas(1000);
  assert_eq!(evicted, vec!["n_stale".to_string()]);
  assert_eq!(primary.get_connected_replicas_count(), 1);
  assert!(primary.get_replica("n_active").is_some());

  // 4. demote_to_replica 清空剩余全部会话，角色与状态机同步归位
  primary.demote_to_replica();
  assert!(primary.is_replica());
  assert_eq!(primary.recovery_status(), RecoveryStatus::NoRecovery);
  assert_eq!(primary.get_connected_replicas_count(), 0);

  info!("心跳超时剔除与降级清空会话验证通过");
  OK
}

/// 并发 append_stream 下 backlog 与 history 位点必须单调一致（原子对齐回归测试）
#[test]
fn append_stream_concurrent_offset_atomicity() -> Void {
  let mgr = Arc::new(ReplicationManager::new(NodeRole::Primary, 0));
  let chunk_len = 1000u64;
  let rounds = 200usize;
  let writers = 8usize;

  let handles: Vec<_> = (0..writers)
    .map(|t| {
      let mgr = Arc::clone(&mgr);
      thread::spawn(move || {
        let data = vec![t as u8; chunk_len as usize];
        for _ in 0..rounds {
          mgr.append_stream(&data);
        }
      })
    })
    .collect();
  for h in handles {
    h.join().expect("append_stream 写入线程不应 panic");
  }

  // 关键不变量：history.replication_offset 与 backlog 尾部严格相等，绝不回退
  let total = (writers * rounds) as u64 * chunk_len;
  assert_eq!(mgr.backlog_offsets().1, total);
  assert_eq!(mgr.get_replication_offset(), total);

  // 追加完毕后积压区间必须自洽：1MB 环形容量已回绕，最早有效位点 = 尾部 - 容量
  let (first, tail) = mgr.backlog_offsets();
  assert_eq!(tail, total);
  assert_eq!(first, total - DEFAULT_BACKLOG_SIZE as u64);
  assert!(!mgr.is_backlog_in_range(0), "头部数据已被环形覆写淘汰");
  assert!(mgr.is_backlog_in_range(first));
  assert!(!mgr.is_backlog_in_range(total + 1), "超前位点必须非法");

  info!("并发 append_stream 位点原子对齐回归验证通过");
  OK
}

/// 多副本滞后与检查点安全位点综合保护
#[test]
fn cluster_replication_safe_aof_address_multi_replica() -> Void {
  let primary = ReplicationManager::new(NodeRole::Primary, 0);

  // 初始无锁定与从节点，安全位点为 u64::MAX
  assert_eq!(primary.get_current_safe_aof_address(), u64::MAX);

  // 1. 主节点锁定检查点位点 5000
  primary.pin_min_served_offset(5000);
  assert_eq!(primary.get_current_safe_aof_address(), 5000);

  // 2. 注册从节点 1 (ACK 4000) 与从节点 2 (ACK 6000)
  let r1 = primary.register_replica("r1".to_string(), 6381, "127.0.0.1".to_string());
  let _r2 = primary.register_replica("r2".to_string(), 6382, "127.0.0.1".to_string());
  primary.update_replica_ack("r1", 4000);
  primary.update_replica_ack("r2", 6000);

  // 安全截断位点取三者最小：min(5000, 4000, 6000) = 4000
  assert_eq!(primary.get_current_safe_aof_address(), 4000);

  // 3. 从节点 1 追赶至 7000，安全截断位点变为 min(5000, 7000, 6000) = 5000
  r1.update_ack(7000);
  assert_eq!(primary.get_current_safe_aof_address(), 5000);

  // 4. 解除检查点锁定，安全位点变为从节点中最小者 6000
  primary.unpin_min_served_offset();
  assert_eq!(primary.get_current_safe_aof_address(), 6000);

  info!("多副本滞后与检查点安全位点综合保护验证通过");
  OK
}

/// ACK 位点单调性回归：乱序/迟到的旧 ACK 绝不回退确认水位，但心跳活性时间戳总是刷新
#[test]
fn replica_ack_offset_monotonic_under_stale_acks() -> Void {
  let primary = ReplicationManager::new(NodeRole::Primary, 0);
  let r1 = primary.register_replica("r1".to_string(), 6381, "127.0.0.1".to_string());

  r1.update_ack(5000);
  assert_eq!(r1.ack_offset.load(Ordering::Acquire), 5000);

  // 迟到的旧 ACK (4000 < 5000) 不回退位点，但仍刷新心跳活性时间戳
  r1.update_ack(4000);
  assert_eq!(r1.ack_offset.load(Ordering::Acquire), 5000);
  assert!(!r1.is_timed_out(1000), "旧 ACK 也要维持心跳活性判定");

  // 更新的 ACK 正常推进
  r1.update_ack(7000);
  assert_eq!(r1.ack_offset.load(Ordering::Acquire), 7000);

  info!("ACK 位点单调性回归验证通过");
  OK
}

/// 从节点握手采纳主节点复制身份的原子性：编号、基线位点与旧纪元积压历史同临界区对齐
#[test]
fn adopt_primary_identity_atomic_adoption() -> Void {
  let replica = ReplicationManager::new(NodeRole::Replica, 0);
  // 旧纪元残留积压
  replica.append_stream(b"stale-epoch-backlog");
  assert!(replica.backlog_offsets().1 > 0);

  let new_id = ReplId::generate();
  replica.adopt_primary_identity(new_id, 4096);

  // 编号与基线位点原子对齐
  assert_eq!(replica.get_primary_repl_id(), new_id);
  assert_eq!(replica.get_replication_offset(), 4096);
  // 旧纪元积压历史被清空：位点单调但有效历史归零，杜绝跨纪元错位续传
  assert_eq!(replica.backlog_offsets(), (4096, 4096));
  assert!(!replica.is_backlog_in_range(4096));

  info!("握手采纳复制身份原子性验证通过");
  OK
}

/// 恢复状态语义全矩阵对标 C# IsRecovering / CannotStreamAOF
#[test]
fn recovery_status_semantics_match_csharp() -> Void {
  // ReadRole 是轻量角色读锁：不算恢复中，也不阻断日志流推送（C# 检查点轮转期间持续推流）
  assert!(!RecoveryStatus::ReadRole.is_recovering());
  assert!(RecoveryStatus::ReadRole.can_stream_aof());

  // 四个真正恢复中的状态：阻断推送
  for st in [
    RecoveryStatus::InitializeRecover,
    RecoveryStatus::ClusterReplicate,
    RecoveryStatus::ClusterFailover,
    RecoveryStatus::ReplicaOfNoOne,
  ] {
    assert!(st.is_recovering());
    assert!(st.cannot_stream_aof());
  }

  // 无恢复与检查点恢复完成态：允许推送
  assert!(!RecoveryStatus::NoRecovery.is_recovering());
  assert!(RecoveryStatus::NoRecovery.can_stream_aof());
  assert!(RecoveryStatus::CheckpointRecoveredAtReplica.is_recovering());
  assert!(RecoveryStatus::CheckpointRecoveredAtReplica.can_stream_aof());

  info!("恢复状态语义对标 C# 全矩阵验证通过");
  OK
}
