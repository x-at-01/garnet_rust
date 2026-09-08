use std::{
  fs::{read, write},
  sync::Arc,
};

use aok::{OK, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpListener,
  runtime::{Runtime, spawn},
};
use log::info;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_repl::{
  CheckpointFileType, FLUSH_METADATA_LEN, KEY_HASH_LEN, NodeRole, RangeIndexFileDataSink,
  RangeIndexFileDataSource, RangeIndexStreamReassembler, ReplId, ReplicaClient, ReplicaReplayer,
  ReplicationManager, SyncDecision,
};
use wkv::{
  RangeIndexChunkedSerializer, StorageBackend, StoreConfig, TreeTuning, WedbStore,
  compute_checksum, encode_ri_create, encode_ri_del, encode_ri_set,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 测试通用 RangeIndex 调优参数
const TUNE: TreeTuning = TreeTuning {
  cache_size: 65536,
  min_record_size: 8,
  max_record_size: 1024,
  max_key_len: 128,
  leaf_page_size: 4096,
};

/// 冒烟测试 1: ReplId 生成、唯一性与十六进制格式属性（对标 Garnet ReplicationId 规范）
#[test]
fn test_repl_id_smoke() -> Void {
  let id1 = ReplId::generate();
  let id2 = ReplId::generate();
  assert_ne!(id1, id2);
  assert_eq!(id1.as_str().len(), 40);
  assert_eq!(id2.as_str().len(), 40);

  for &b in id1.as_bytes() {
    assert!(b.is_ascii_hexdigit() && !b.is_ascii_uppercase());
  }

  let empty_id = ReplId::empty();
  assert!(empty_id.is_empty());
  assert!(!id1.is_empty());

  let q_id = ReplId::question_mark();
  assert!(q_id.is_question_mark());

  let parsed = ReplId::from_str_val(id1.as_str())?;
  assert_eq!(parsed, id1);

  info!("冒烟测试 1 (ReplId 核心属性) 验证通过");
  OK
}

/// 冒烟测试 2: 端到端 TCP ReplicaClient 握手与连接建立冒烟（对标 Garnet ReplicaClient 握手流程）
#[test]
fn test_e2e_replica_client_smoke() -> Void {
  let runtime = Runtime::new()?;
  runtime.block_on(async {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;

    let server_task = spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let mut buf = vec![0u8; 1024];

      // 阶段 1: PING -> PONG
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      assert!(res.unwrap() > 0);
      let BufResult(res, _) = stream.write_all(b"+PONG\r\n").await;
      res.unwrap();

      // 阶段 2: REPLCONF listening-port -> OK
      buf.clear();
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      assert!(res.unwrap() > 0);
      let BufResult(res, _) = stream.write_all(b"+OK\r\n").await;
      res.unwrap();

      // 阶段 3: REPLCONF ip-address -> OK
      buf.clear();
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      assert!(res.unwrap() > 0);
      let BufResult(res, _) = stream.write_all(b"+OK\r\n").await;
      res.unwrap();

      // 阶段 4: REPLCONF capa -> OK
      buf.clear();
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      assert!(res.unwrap() > 0);
      let BufResult(res, _) = stream.write_all(b"+OK\r\n").await;
      res.unwrap();

      // 阶段 5: PSYNC -> CONTINUE
      buf.clear();
      let BufResult(res, _) = stream.read(buf).await;
      assert!(res.unwrap() > 0);
      let resp = b"+CONTINUE 0123456789abcdef0123456789abcdef01234567\r\n";
      let BufResult(res, _) = stream.write_all(resp).await;
      res.unwrap();
    });

    let dir = tempdir()?;
    let db_path = dir.path().join("client_smoke.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let config = StoreConfig::new(1024, 64 * 1024, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let manager = Arc::new(ReplicationManager::new(NodeRole::Replica, 0));

    let client = ReplicaClient::new(
      local_addr.to_string(),
      6381,
      "127.0.0.1".to_string(),
      None,
      manager,
      store,
      None,
    );

    let (_stream, decision) = client.connect_and_handshake().await?;
    assert!(matches!(decision, SyncDecision::PartialResync { .. }));

    let _ = server_task.await;
    info!("冒烟测试 2 (TCP 客户端握手) 验证通过");
    OK
  })?;

  OK
}

/// 冒烟测试 3: RangeIndex 命令复制流回放冒烟 (RI.CREATE, RI.SET, RI.DEL，对标 Garnet ClusterRangeIndexCheckpointSync)
#[test]
fn test_range_index_command_replication_smoke() -> Void {
  let dir = tempdir()?;
  let db_path = dir.path().join("db.wal");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let config = StoreConfig {
    index_size: 1024,
    page_size: 4096,
    num_pages: 16,
    mutable_fraction: 0.5,
    max_sessions: 16,
    ..Default::default()
  };
  let store = Arc::new(WedbStore::open(config, device)?);
  let replayer = ReplicaReplayer::new(Arc::clone(&store), None);

  Runtime::new()?.block_on(async {
    let create_cmd = encode_ri_create(b"idx_replay", StorageBackend::Memory, TUNE);
    replayer.replay_payload(&create_cmd).await?;

    let session = store.new_session()?;
    assert!(session.range_index_exists(b"idx_replay").await?);

    let mut payload = Vec::with_capacity(256);
    payload.extend_from_slice(&encode_ri_set(b"idx_replay", b"field_1", b"value_1"));
    payload.extend_from_slice(&encode_ri_set(b"idx_replay", b"field_2", b"value_2"));
    payload.extend_from_slice(&encode_ri_del(b"idx_replay", b"field_1"));

    replayer.replay_payload(&payload).await?;

    assert_eq!(
      session.range_index_get(b"idx_replay", b"field_1").await?,
      None
    );
    assert_eq!(
      session.range_index_get(b"idx_replay", b"field_2").await?,
      Some(b"value_2".to_vec())
    );

    // 幂等容错：重复 RI.CREATE 不破坏现有数据
    replayer.replay_payload(&create_cmd).await?;
    assert_eq!(
      session.range_index_get(b"idx_replay", b"field_2").await?,
      Some(b"value_2".to_vec())
    );

    OK
  })?;

  info!("冒烟测试 3 (RangeIndex 命令复制回放) 验证通过");
  OK
}

/// 冒烟测试 4: RangeIndex 磁盘文件分块传输协议冒烟 (Source 与 Sink，对标 Garnet ClusterRangeIndexCheckpointSyncWithEviction)
#[test]
fn test_range_index_file_data_source_and_sink_smoke() -> Void {
  let dir = tempdir()?;
  let src_file_path = dir.path().join("source.bftree");
  let dest_file_path = dir.path().join("dest.bftree");

  let dummy_data: Vec<u8> = (0..128 * 1024).map(|i| (i % 251) as u8).collect();
  write(&src_file_path, &dummy_data)?;

  let key_hash = "0123456789abcdef0123456789abcdef";
  let address = 0x1234_5678_9abc_def0i64;

  let mut source = RangeIndexFileDataSource::new(
    CheckpointFileType::StoreRangeIndexFlush,
    key_hash,
    address,
    src_file_path.clone(),
  )?;

  let metadata = source.get_metadata();
  assert_eq!(metadata.len(), FLUSH_METADATA_LEN);
  assert_eq!(&metadata[..KEY_HASH_LEN], key_hash.as_bytes());
  assert_eq!(
    &metadata[KEY_HASH_LEN..FLUSH_METADATA_LEN],
    &address.to_le_bytes()
  );

  let mut sink = RangeIndexFileDataSink::from_path(
    CheckpointFileType::StoreRangeIndexFlush,
    42,
    dest_file_path.clone(),
  )?;

  let mut chunk_buf = vec![0u8; 16 * 1024];
  let mut total_transferred = 0u64;

  while !source.is_complete() {
    let n = source.read_next_chunk(&mut chunk_buf)?;
    if n == 0 {
      break;
    }
    sink.write_chunk(total_transferred, &chunk_buf[..n])?;
    total_transferred += n as u64;
  }
  sink.complete()?;

  assert_eq!(total_transferred, dummy_data.len() as u64);
  let read_back = read(&dest_file_path)?;
  assert_eq!(read_back, dummy_data);

  info!("冒烟测试 4 (RangeIndex 分块传输 Source/Sink) 验证通过");
  OK
}

/// 冒烟测试 5: RangeIndex AOF 分块流式重组与发布冒烟（对标 Garnet RangeIndexStreamReplayTests）
#[test]
fn test_range_index_stream_reassembly_smoke() -> Void {
  let dir = tempdir()?;
  let db_path = dir.path().join("db.wal");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let config = StoreConfig {
    index_size: 1024,
    page_size: 4096,
    num_pages: 16,
    mutable_fraction: 0.5,
    max_sessions: 16,
    ..Default::default()
  };
  let store = Arc::new(WedbStore::open(config, device)?);
  let temp_reassemble_dir = dir.path().join("temp_reassemble");

  Runtime::new()?.block_on(async {
    let session = store.new_session()?;
    let key = b"orig_stream_idx";
    session
      .range_index_create(key, StorageBackend::Std, TUNE)
      .await?;

    for i in 0..20 {
      let f = format!("field_{i:04}");
      let v = format!("value_{i:04}");
      session
        .range_index_set(key, f.as_bytes(), v.as_bytes())
        .await?;
    }

    let snap_path = dir.path().join("migrated_snapshot.bftree");
    let tree = store
      .range_index
      .get_or_open_tree(key, &session.range_index_config(key).await?)?;
    tree.cpr_snapshot(&snap_path)?;

    let snap_bytes = read(&snap_path)?;
    let checksum = compute_checksum(&snap_bytes);
    let stub = session.range_index_config(key).await?.encode();

    let target_key = b"published_target_idx";
    let mut serializer = RangeIndexChunkedSerializer::new_with_checksum(
      target_key,
      &stub,
      snap_bytes.len() as u64,
      checksum,
    );

    let mut reassembler = RangeIndexStreamReassembler::new(&temp_reassemble_dir);
    let mut chunk_buf = [0u8; 512];
    let mut file_offset = 0;
    let mut is_first = true;

    while !serializer.is_complete() {
      if serializer.needs_file_data() {
        let remain = snap_bytes.len() - file_offset;
        let take = remain.min(256);
        serializer.supply_file_data(&snap_bytes[file_offset..file_offset + take]);
        file_offset += take;
      }

      let written = serializer.move_next(&mut chunk_buf)?;
      if written == 0 {
        continue;
      }

      let is_last = serializer.is_complete();
      let published = reassembler
        .process_chunk(
          &session,
          target_key,
          &chunk_buf[..written],
          is_first,
          is_last,
        )
        .await?;

      if is_last {
        assert!(published);
      } else {
        assert!(!published);
      }
      is_first = false;
    }

    assert!(session.range_index_exists(target_key).await?);
    for i in 0..20 {
      let f = format!("field_{i:04}");
      let expected_v = format!("value_{i:04}");
      let val = session.range_index_get(target_key, f.as_bytes()).await?;
      assert_eq!(val.as_deref(), Some(expected_v.as_bytes()));
    }

    OK
  })?;

  info!("冒烟测试 5 (RangeIndex AOF 分块流式重组与发布) 验证通过");
  OK
}

/// 冒烟测试 6: RangeIndex 检查点快照导出与故障恢复对齐冒烟（对标 Garnet ClusterRangeIndexCheckpointSyncMultipleKeys）
#[test]
fn test_range_index_checkpoint_recover_smoke() -> Void {
  let dir = tempdir()?;
  let db_path = dir.path().join("db.wal");
  let ckpt_dir = dir.path().join("checkpoints");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let config = StoreConfig {
    index_size: 1024,
    page_size: 4096,
    num_pages: 16,
    mutable_fraction: 0.5,
    max_sessions: 16,
    ..Default::default()
  };
  let store = Arc::new(WedbStore::open(config, device)?);
  let token = 0x9999_8888_7777_6666_u128;

  Runtime::new()?.block_on(async {
    let session = store.new_session()?;

    let key1 = b"idx_ckpt_1";
    let key2 = b"idx_ckpt_2";
    session
      .range_index_create(key1, StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_create(key2, StorageBackend::Std, TUNE)
      .await?;

    for i in 0..10 {
      let f = format!("field_{i:04}");
      let v = format!("value_{i:04}");
      session
        .range_index_set(key1, f.as_bytes(), v.as_bytes())
        .await?;
      session
        .range_index_set(key2, f.as_bytes(), v.as_bytes())
        .await?;
    }

    let count = store.take_range_index_checkpoints(&ckpt_dir, token)?;
    assert_eq!(count, 2);

    store.range_index.dispose_tree(key1, true);
    store.range_index.dispose_tree(key2, true);

    let recovered_count = store.recover_range_indexes(&ckpt_dir, token).await?;
    assert_eq!(recovered_count, 2);

    assert!(session.range_index_exists(key1).await?);
    assert!(session.range_index_exists(key2).await?);

    for i in 0..10 {
      let f = format!("field_{i:04}");
      let expected = format!("value_{i:04}");
      let v1 = session.range_index_get(key1, f.as_bytes()).await?;
      assert_eq!(v1.as_deref(), Some(expected.as_bytes()));
      let v2 = session.range_index_get(key2, f.as_bytes()).await?;
      assert_eq!(v2.as_deref(), Some(expected.as_bytes()));
    }

    session
      .range_index_set(key1, b"new_field", b"new_val")
      .await?;
    assert_eq!(
      session.range_index_get(key1, b"new_field").await?,
      Some(b"new_val".to_vec())
    );

    OK
  })?;

  info!("冒烟测试 6 (RangeIndex 检查点快照导出与恢复) 验证通过");
  OK
}
