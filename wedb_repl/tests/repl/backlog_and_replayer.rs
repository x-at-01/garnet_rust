use std::sync::Arc;

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;
use wedb_redis::prelude::*;
use wedb_repl::{
  Error, NodeRole, ReplicaReplayer, ReplicationBacklog, ReplicationManager, SyncDecision,
};

use super::support::init_test_store;

/// Backlog 环形缓冲区追加写入、覆盖与位点区间计算
#[test]
fn backlog_circular_wrapping_and_offset_calc() -> Void {
  // 1. 创建小容量环形积压缓冲区 (容量 1024 字节)
  let mut backlog = ReplicationBacklog::new(1024);
  assert_eq!(backlog.capacity(), 1024);
  assert_eq!(backlog.master_offset(), 0);
  assert_eq!(backlog.hist_len(), 0);
  assert!(!backlog.is_offset_in_range(0));

  // 2. 写入 500 字节数据
  let data1 = [b'A'; 500];
  backlog.feed(&data1);
  assert_eq!(backlog.master_offset(), 500);
  assert_eq!(backlog.hist_len(), 500);
  assert_eq!(backlog.first_byte_offset(), 0);
  assert!(backlog.is_offset_in_range(0));
  assert!(backlog.is_offset_in_range(250));
  assert!(backlog.is_offset_in_range(500));
  assert!(!backlog.is_offset_in_range(501));

  let read1 = backlog.read_bytes(0, 100);
  assert_eq!(read1, [b'A'; 100]);

  // 3. 再次写入 600 字节数据 (累计 1100 字节，触发回环覆盖)
  let data2 = [b'B'; 600];
  backlog.feed(&data2);
  assert_eq!(backlog.master_offset(), 1100);
  assert_eq!(backlog.hist_len(), 1024);
  // 最早有效位点 = 1100 - 1024 = 76
  assert_eq!(backlog.first_byte_offset(), 76);

  // 位点 0..75 已被覆盖淘汰
  assert!(!backlog.is_offset_in_range(0));
  assert!(!backlog.is_offset_in_range(75));
  assert!(backlog.is_offset_in_range(76));
  assert!(backlog.is_offset_in_range(1100));

  // 4. 跨回环边界读取数据验证
  let cross_read = backlog.read_bytes(400, 200);
  assert_eq!(cross_read.len(), 200);
  assert_eq!(&cross_read[..100], &[b'A'; 100]);
  assert_eq!(&cross_read[100..], &[b'B'; 100]);

  // 5. 写入超大单块 (超过容量)，仅保留末尾数据
  let huge = [b'C'; 2048];
  backlog.feed(&huge);
  assert_eq!(backlog.master_offset(), 3148);
  assert_eq!(backlog.hist_len(), 1024);
  assert_eq!(backlog.first_byte_offset(), 3148 - 1024);
  let read_huge = backlog.read_bytes(backlog.first_byte_offset(), 1024);
  assert_eq!(read_huge, [b'C'; 1024]);

  // 6. 清空验证
  backlog.clear();
  assert_eq!(backlog.hist_len(), 0);
  assert!(!backlog.is_offset_in_range(backlog.master_offset()));

  info!("Backlog 环形缓冲区追加写入、覆盖与位点计算通过");
  OK
}

/// Backlog 零拷贝切片借用与边界安全防护测试
#[test]
fn backlog_zero_copy_slices() -> Void {
  let mut backlog = ReplicationBacklog::new(1024);
  backlog.set_master_offset(1000);

  // 1. 写入 400 字节数据（未发生回绕）
  let data1 = [b'A'; 400];
  backlog.feed(&data1);
  assert_eq!(backlog.first_byte_offset(), 1000);
  assert_eq!(backlog.master_offset(), 1400);

  let (s1, s2) = backlog.slices(1000, 400);
  assert_eq!(s1.len(), 400);
  assert_eq!(s1, &data1);
  assert!(s2.is_empty());

  let (s1_part, s2_part) = backlog.slices(1100, 200);
  assert_eq!(s1_part.len(), 200);
  assert_eq!(s1_part, &data1[100..300]);
  assert!(s2_part.is_empty());

  // 2. 写入 800 字节数据（触发回绕覆盖）
  let data2 = [b'B'; 800];
  backlog.feed(&data2);
  assert_eq!(backlog.master_offset(), 2200);
  assert_eq!(backlog.first_byte_offset(), 2200 - 1024); // 1176

  let (s1_wrap, s2_wrap) = backlog.slices(1176, 1024);
  assert_eq!(s1_wrap.len() + s2_wrap.len(), 1024);
  assert!(!s1_wrap.is_empty() && !s2_wrap.is_empty());

  let mut combined = Vec::with_capacity(1024);
  combined.extend_from_slice(s1_wrap);
  combined.extend_from_slice(s2_wrap);
  assert_eq!(combined.len(), 1024);
  assert_eq!(&combined[..224], &data1[176..400]);
  assert_eq!(&combined[224..], &data2);

  // 3. ReplicationManager::with_backlog_slices 闭包借用
  let mgr = ReplicationManager::new(NodeRole::Primary, 0);
  mgr.append_stream(b"HELLO_REPL_ZERO_COPY");
  let read_len = mgr.with_backlog_slices(0, 100, |slice1, slice2| {
    assert_eq!(slice1, b"HELLO_REPL_ZERO_COPY");
    assert!(slice2.is_empty());
    slice1.len() + slice2.len()
  });
  assert_eq!(read_len, 20);

  // 4. 极端边界越界防护
  let (underflow1, underflow2) = backlog.slices(1000, 100);
  assert!(underflow1.is_empty() && underflow2.is_empty());

  let (empty1, empty2) = backlog.slices(9999, 100);
  assert!(empty1.is_empty() && empty2.is_empty());

  let (zero1, zero2) = backlog.slices(1176, 0);
  assert!(zero1.is_empty() && zero2.is_empty());

  info!("Backlog 零拷贝切片借用与边界安全防护通过");
  OK
}

/// Backlog 高频多轮覆写单调性与数据一致性验证
#[test]
fn backlog_high_frequency_multi_round_monotonic_overwrites() -> Void {
  let cap = 2048;
  let mut backlog = ReplicationBacklog::new(cap);
  backlog.set_master_offset(10_000);

  let chunk_size = 100;
  let rounds = 500;
  let mut prev_master = backlog.master_offset();
  let mut prev_first = backlog.first_byte_offset();

  for round in 0..rounds {
    let byte_val = (round % 256) as u8;
    let data = [byte_val; 100];
    backlog.feed(&data);

    let cur_master = backlog.master_offset();
    let cur_first = backlog.first_byte_offset();

    assert!(cur_master > prev_master);
    assert_eq!(cur_master - prev_master, chunk_size as u64);
    assert!(cur_first >= prev_first);

    if cur_master - 10_000 >= cap as u64 {
      assert_eq!(backlog.hist_len(), cap);
      assert_eq!(cur_master - cur_first, cap as u64);
    }

    prev_master = cur_master;
    prev_first = cur_first;
  }

  // 验证多轮覆写后的切片一致性
  let read_start = backlog.master_offset() - 300;
  let (s1, s2) = backlog.slices(read_start, 300);
  assert_eq!(s1.len() + s2.len(), 300);

  let mut combined = Vec::with_capacity(300);
  combined.extend_from_slice(s1);
  combined.extend_from_slice(s2);

  let round_497_byte = ((rounds - 3) % 256) as u8;
  let round_498_byte = ((rounds - 2) % 256) as u8;
  let round_499_byte = ((rounds - 1) % 256) as u8;

  assert_eq!(&combined[..100], &[round_497_byte; 100]);
  assert_eq!(&combined[100..200], &[round_498_byte; 100]);
  assert_eq!(&combined[200..300], &[round_499_byte; 100]);

  let from_read_bytes = backlog.read_bytes(read_start, 300);
  assert_eq!(combined, from_read_bytes);

  info!("Backlog 高频多轮覆写单调性与数据一致性验证通过");
  OK
}

/// Backlog + PSYNC 断线重连增量续传与位点精确对齐
#[test]
fn reconnect_resume_backlog_psync() -> Void {
  let primary = ReplicationManager::new(NodeRole::Primary, 0);
  let replid = primary.get_primary_repl_id();

  // 1. 主节点先后追加两段流
  let chunk_a = [b'A'; 600];
  let tail_a = primary.append_stream(&chunk_a);
  assert_eq!(tail_a, 600);
  assert_eq!(primary.get_replication_offset(), 600);
  assert_eq!(primary.backlog_offsets(), (0, 600));

  let chunk_b = [b'B'; 400];
  let tail_b = primary.append_stream(&chunk_b);
  assert_eq!(tail_b, 1000);
  assert_eq!(primary.get_replication_offset(), 1000);

  // 2. 从节点在位点 600 断线重连发起 PSYNC
  let decision = primary.handle_psync(replid.as_str(), 600, 0, 1000)?;
  assert_eq!(
    decision,
    SyncDecision::PartialResync {
      replid,
      start_offset: 600
    }
  );

  // 3. 从积压缓冲区精准续传 600..1000（恰好为 chunk_b，不丢不重）
  assert!(primary.is_backlog_in_range(600));
  let resumed = primary.read_backlog(600, 400);
  assert_eq!(resumed, chunk_b);

  let total = primary.with_backlog_slices(600, 400, |s1, s2| s1.len() + s2.len());
  assert_eq!(total, 400);

  // 4. 从节点应用续传数据后位点与主节点对齐
  let replica = ReplicationManager::new(NodeRole::Replica, 0);
  replica.append_stream(&chunk_a);
  replica.append_stream(&resumed);
  assert_eq!(
    replica.get_replication_offset(),
    primary.get_replication_offset()
  );

  // 5. 超出积压有效区间的位点请求必须回退全量同步
  let decision_out = primary.handle_psync(replid.as_str(), 5000, 0, 1000)?;
  assert!(matches!(decision_out, SyncDecision::FullResync { .. }));

  info!("断线重连 backlog 增量续传验证通过");
  OK
}

/// 从节点 ReplicaReplayer 命令回放至本地 WedbStore
#[test]
fn replica_command_replay_to_store() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (store, _dir) = init_test_store()?;
    let replayer = ReplicaReplayer::new(Arc::clone(&store), None);

    // 构造主节点增量数据流: SET user:1001 alice, SET user:1002 bob, DEL user:1001
    let mut payload = Vec::new();
    payload.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$9\r\nuser:1001\r\n$5\r\nalice\r\n");
    payload.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$9\r\nuser:1002\r\n$3\r\nbob\r\n");
    payload.extend_from_slice(b"*2\r\n$3\r\nDEL\r\n$9\r\nuser:1001\r\n");

    replayer.replay_payload(&payload).await?;

    let session = store.new_session()?;
    assert_eq!(session.read(b"user:1001").await?, None);
    assert_eq!(session.read(b"user:1002").await?, Some(b"bob".to_vec()));

    info!("从节点 ReplicaReplayer 数据回放测试通过");
    OK
  })?;

  OK
}

/// 从节点命令回放流式半包自动拼接与无丢失回放
#[test]
fn replica_replayer_partial_chunk_reassembly() -> Void {
  let runtime = Runtime::new()?;
  runtime.block_on(async {
    let (store, _dir) = init_test_store()?;
    let replayer = ReplicaReplayer::new(Arc::clone(&store), None);

    let full_cmd = b"*3\r\n$3\r\nSET\r\n$7\r\nstreamK\r\n$7\r\nstreamV\r\n";
    let split_pos = 15;
    let chunk1 = &full_cmd[..split_pos];
    let chunk2 = &full_cmd[split_pos..];

    // 1. 回放前半段（半包），内部暂存
    replayer.replay_payload(chunk1).await?;
    let session = store.new_session()?;
    assert!(session.read(b"streamK").await?.is_none());

    // 2. 回放后半段，无缝拼接并执行入库
    replayer.replay_payload(chunk2).await?;
    assert_eq!(
      session.read(b"streamK").await?.as_deref(),
      Some(&b"streamV"[..])
    );

    // 3. 清理暂存缓冲
    replayer.clear_pending();
    OK
  })?;

  info!("从节点命令回放半包自动拼接测试通过");
  OK
}

/// 从节点回放器非法命令帧清空与隔离恢复
#[test]
fn replica_replayer_corrupted_frame_isolation() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (store, _dir) = init_test_store()?;
    let replayer = ReplicaReplayer::new(Arc::clone(&store), None);

    // 1. 非法命令帧必须立即报错并清空缓冲
    let bad_payload = b"?unknown_protocol_frame\r\n";
    let res = replayer.replay_payload(bad_payload).await;
    assert!(matches!(res, Err(Error::ReplayFailed(_))));

    // 2. 后续合法的命令帧能够继续正常回放
    let valid_payload = b"*3\r\n$3\r\nSET\r\n$7\r\npostkey\r\n$7\r\npostval\r\n";
    replayer.replay_payload(valid_payload).await?;

    let session = store.new_session()?;
    assert_eq!(session.read(b"postkey").await?, Some(b"postval".to_vec()));

    info!("从节点回放器非法帧清空与隔离恢复验证通过");
    OK
  })?;

  OK
}

/// 从节点回放器复合集合命令 (HSET, HDEL, SADD, SREM) 与控制命令 (PING, SELECT) 回放测试
#[test]
fn replica_collection_commands_replay_to_store() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (store, _dir) = init_test_store()?;
    let replayer = ReplicaReplayer::new(Arc::clone(&store), None);

    // 1. 回放 PING 与 SELECT 0（主从心跳与连接初始化）
    let ping_select = b"*1\r\n$4\r\nPING\r\n*2\r\n$6\r\nSELECT\r\n$1\r\n0\r\n";
    replayer.replay_payload(ping_select).await?;

    // 2. 回放 HSET 与 HDEL
    let mut hash_payload = Vec::new();
    // HSET user:profile name alice age 30
    hash_payload.extend_from_slice(
      b"*6\r\n$4\r\nHSET\r\n$12\r\nuser:profile\r\n$4\r\nname\r\n$5\r\nalice\r\n$3\r\nage\r\n$2\r\n30\r\n",
    );
    // HDEL user:profile age
    hash_payload.extend_from_slice(b"*3\r\n$4\r\nHDEL\r\n$12\r\nuser:profile\r\n$3\r\nage\r\n");
    replayer.replay_payload(&hash_payload).await?;

    let session = store.new_session()?;
    assert_eq!(
      session.hget(b"user:profile", b"name").await?,
      Some(b"alice".to_vec())
    );
    assert_eq!(session.hget(b"user:profile", b"age").await?, None);

    // 3. 回放 SADD 与 SREM
    let mut set_payload = Vec::new();
    // SADD tags rust database storage
    set_payload.extend_from_slice(
      b"*5\r\n$4\r\nSADD\r\n$4\r\ntags\r\n$4\r\nrust\r\n$8\r\ndatabase\r\n$7\r\nstorage\r\n",
    );
    // SREM tags storage
    set_payload.extend_from_slice(b"*3\r\n$4\r\nSREM\r\n$4\r\ntags\r\n$7\r\nstorage\r\n");
    replayer.replay_payload(&set_payload).await?;

    assert!(session.sismember(b"tags", b"rust").await?);
    assert!(session.sismember(b"tags", b"database").await?);
    assert!(!session.sismember(b"tags", b"storage").await?);

    info!("从节点回放器哈希、集合与控制命令回放验证通过");
    OK
  })?;

  OK
}
