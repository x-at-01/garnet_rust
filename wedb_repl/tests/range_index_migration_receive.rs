//! RangeIndex 快照读取器与迁移接收状态机测试套件
//!
//! 1:1微软 Garnet:
//! - RangeIndexSnapshotReader.cs (快照读取、地址范围过滤、分块迭代)
//! - RangeIndexMigrationReceiveSession.cs (接收状态机 IDLE -> RECEIVING -> IDLE)
//! - RangeIndexMigrationReceiveStateTests.cs (并发安全 Dispose、延迟清理保证、异常抛出)
use std::{
  fs::{self, read_dir, write},
  io::{Cursor, ErrorKind},
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  thread,
  time::{Duration, Instant},
};

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_cluster::{ClusterConfig, SlotState, hash_slot};
use wedb_repl::{
  CheckpointFileType, CooperativeDisposeGuard, DEFAULT_CHUNK_SIZE, DisposeResult, Error,
  FLUSH_METADATA_LEN, KEY_HASH_LEN, RangeIndexFileDataSink, RangeIndexFileDataSource,
  RangeIndexMigrationReceiveState, RangeIndexSnapshotReader, ReplicationHistory,
};
use wkv::{
  RangeIndexChunkedSerializer, RangeIndexManager, RangeIndexMigrationReader, StoreConfig, WedbStore,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 构造不完整的分块数据，驱动反序列化器进入 ReceivingFileData 状态并创建临时文件 (BuildPartialChunk) /// 格式: [4B keyLen][key][8B fileSize=1000][10B partial data]
fn build_partial_chunk() -> Vec<u8> {
  let key = b"rikey";
  let mut chunk = Vec::with_capacity(4 + key.len() + 8 + 10);
  chunk.extend_from_slice(&(key.len() as i32).to_le_bytes());
  chunk.extend_from_slice(key);
  chunk.extend_from_slice(&1000i64.to_le_bytes());
  chunk.extend_from_slice(&[0u8; 10]);
  chunk
}

/// 统计指定目录下的迁移临时 .bftree 文件数 (CountTempFiles)
fn count_temp_files(ri_dir: &Path) -> usize {
  let dir = ri_dir.join("migration-tmp");
  if !dir.exists() {
    return 0;
  }
  read_dir(&dir)
    .map(|entries| {
      entries
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|ext| ext == "bftree"))
        .count()
    })
    .unwrap_or(0)
}

/// 测试 1: RangeIndexSnapshotReader 枚举、过滤与分块迭代读取
#[test]
fn test_range_index_snapshot_reader_enumeration_and_chunking() -> Void {
  let dir = tempdir()?;
  let ri_dir = dir.path().join("ri");
  let cpr_dir = dir.path().join("cpr");
  let token = 12345678901234567890u128;
  let token_str = itoa::Buffer::new().format(token).to_string();

  let manager = RangeIndexManager::new(ri_dir.clone(), cpr_dir.clone());

  // 1. 创建位于 ri_log_root 下的 flush 文件
  // - key1_150.flush.bftree (在 [100, 300) 范围内)
  // - key2_250.flush.bftree (在 [100, 300) 范围内) // - key3_500.flush.bftree (超出范围)
  let flush_data1 = b"flush_data_content_1";
  let flush_data2 = b"flush_data_content_2_longer_chunk";
  let flush_data3 = b"flush_data_content_3_out_of_range";

  let key1_hash = "11111111111111111111111111111111";
  let key2_hash = "22222222222222222222222222222222";
  let key3_hash = "33333333333333333333333333333333";

  let path1 = manager.log_flush_path(key1_hash, 150);
  let path2 = manager.log_flush_path(key2_hash, 250);
  let path3 = manager.log_flush_path(key3_hash, 500);

  fs::create_dir_all(path1.parent().unwrap())?;
  write(&path1, flush_data1)?;
  write(&path2, flush_data2)?;
  write(&path3, flush_data3)?;

  // 2. 创建检查点快照文件: cpr/{token}/rangeindex/{key_hash}.bftree
  let key4_hash = "44444444444444444444444444444444";
  let snap_path = manager.checkpoint_snapshot_path(&token_str, key4_hash);
  fs::create_dir_all(snap_path.parent().unwrap())?;
  let snap_data = b"checkpoint_snapshot_tree_bytes";
  write(&snap_path, snap_data)?;

  // 3. 构造 RangeIndexSnapshotReader，指定地址区间 [100, 300)
  let mut reader = RangeIndexSnapshotReader::new(&manager, token, 100, 300)?;

  // 预期枚举出 3 个文件: 2 个 flush 文件 + 1 个快照文件 (key3 超出范围被过滤)
  assert_eq!(reader.len(), 3, "应该正好匹配 3 个待传输文件");
  assert!(!reader.is_empty());
  assert_eq!(reader.shared_buffer().len(), 64 * 1024);

  // 验证数据源类型与元数据
  let sources = reader.data_sources();
  let mut found_key1 = false;
  let mut found_key2 = false;
  let mut found_key4 = false;

  for ds in sources {
    let meta = ds.get_metadata();
    if ds.key_hash == key1_hash {
      found_key1 = true;
      assert_eq!(ds.file_type, CheckpointFileType::StoreRangeIndexFlush);
      assert_eq!(ds.address, 150);
      assert_eq!(meta.len(), FLUSH_METADATA_LEN);
      assert_eq!(&meta[..KEY_HASH_LEN], key1_hash.as_bytes());
      assert_eq!(&meta[KEY_HASH_LEN..], &150i64.to_le_bytes());
    } else if ds.key_hash == key2_hash {
      found_key2 = true;
      assert_eq!(ds.file_type, CheckpointFileType::StoreRangeIndexFlush);
      assert_eq!(ds.address, 250);
      assert_eq!(meta.len(), FLUSH_METADATA_LEN);
    } else if ds.key_hash == key4_hash {
      found_key4 = true;
      assert_eq!(ds.file_type, CheckpointFileType::StoreRangeIndexSnapshot);
      assert_eq!(ds.address, 0);
      assert_eq!(meta.len(), KEY_HASH_LEN);
      assert_eq!(&meta[..KEY_HASH_LEN], key4_hash.as_bytes());
    } else {
      panic!("意外的文件哈希: {}", ds.key_hash);
    }
  }

  assert!(found_key1 && found_key2 && found_key4);

  // 4. 验证逐块迭代读取 (for_each_chunk)
  let mut total_bytes = 0usize;
  reader.for_each_chunk(|ds, chunk| {
    total_bytes += chunk.len();
    if ds.key_hash == key1_hash {
      assert_eq!(chunk, flush_data1);
    } else if ds.key_hash == key2_hash {
      assert_eq!(chunk, flush_data2);
    } else if ds.key_hash == key4_hash {
      assert_eq!(chunk, snap_data);
    }
    Ok(())
  })?;

  assert_eq!(
    total_bytes,
    flush_data1.len() + flush_data2.len() + snap_data.len()
  );

  // 5. 验证清理
  reader.clear();
  assert_eq!(reader.len(), 0);
  assert!(reader.is_empty());

  info!("测试 1 (RangeIndexSnapshotReader 枚举与分块) 验证通过");
  OK
}

/// 测试 1b: 快照源文件传输中途被截断时必须快速失败
/// (1:1 对标 C# RangeIndexFileDataSource.ReadNextChunkAsync 的 unexpected EOF 异常，
/// 严禁把残缺文件静默当作完整数据块发布到从节点造成主从数据不一致)
#[test]
fn test_snapshot_source_truncated_file_reads_fail_fast() -> Void {
  let dir = tempdir()?;
  let file_path = dir.path().join("truncated.bftree");
  write(&file_path, vec![b'A'; 100])?;

  let mut source = RangeIndexFileDataSource::new(
    CheckpointFileType::StoreRangeIndexSnapshot,
    "11111111111111111111111111111111",
    0,
    file_path.clone(),
  )?;
  assert_eq!(source.file_len, 100);

  // 创建数据源后模拟磁盘文件被外部截断（主节点磁盘故障 / 文件被误清等）
  let f = fs::OpenOptions::new().write(true).open(&file_path)?;
  f.set_len(40)?;

  let mut buf = vec![0u8; DEFAULT_CHUNK_SIZE];
  let res = source.read_next_chunk(&mut buf);
  assert!(
    matches!(res, Err(ref e) if e.kind() == ErrorKind::UnexpectedEof),
    "截断文件读取必须报 UnexpectedEof，严禁静默产出残缺数据块"
  );

  info!("测试 1b (截断文件快速失败) 验证通过");
  OK
}

/// 测试 2: 释放后调用 ProcessRecord 抛出 ObjectDisposedException
/// (1:1 RangeIndexMigrationReceiveStateTests.ProcessRecordAfterDispose_Throws)
#[test]
fn test_process_record_after_dispose_throws() -> Void {
  let dir = tempdir()?;
  let manager = Arc::new(RangeIndexManager::new(
    dir.path().join("ri"),
    dir.path().join("cpr"),
  ));
  let state = RangeIndexMigrationReceiveState::new(manager);

  // 主动释放状态机
  state.dispose();

  assert!(!state.is_receiving(), "释放后 is_receiving 必须为 false");

  // 调用 process_record_sync
  let chunk = build_partial_chunk();
  let res_sync = state.process_record_sync(&chunk, None, false);
  assert!(
    matches!(res_sync, Err(Error::ObjectDisposed(_))),
    "释放后调用同步 process_record 必须返回 ObjectDisposed 错误"
  );

  // 调用异步 process_record
  Runtime::new()?.block_on(async {
    let res_async = state
      .process_record::<SegmentedDevice>(&chunk, None, None, false)
      .await;
    assert!(
      matches!(res_async, Err(Error::ObjectDisposed(_))),
      "释放后调用异步 process_record 必须返回 ObjectDisposed 错误"
    );
    OK
  })?;

  info!("测试 2 (ProcessRecordAfterDispose_Throws) 验证通过");
  OK
}

/// 测试 3: 处理不完整分块并安全清理临时文件
/// (1:1 RangeIndexMigrationReceiveStateTests.DisposeDuringProcessRecord)
#[test]
fn test_process_partial_chunk_and_cleanup() -> Void {
  let dir = tempdir()?;
  let ri_dir = dir.path().join("ri");
  let manager = Arc::new(RangeIndexManager::new(
    ri_dir.clone(),
    dir.path().join("cpr"),
  ));
  let state = RangeIndexMigrationReceiveState::new(manager);

  let chunk = build_partial_chunk();

  assert!(!state.is_receiving());
  assert_eq!(state.current_chunk_count(), 0);
  assert_eq!(count_temp_files(&ri_dir), 0);

  // 输入未完成的首个分块
  let ok = state.process_record_sync(&chunk, None, false)?;
  assert!(ok);
  assert!(state.is_receiving(), "接收分块后应处于 receiving 态");
  assert_eq!(state.current_chunk_count(), 1, "分块计数应为 1");
  assert_eq!(
    count_temp_files(&ri_dir),
    1,
    "必须创建 1 个临时 .bftree 文件"
  );

  // 调用 dispose，验证临时文件被安全清理，状态回到 IDLE
  state.dispose();
  assert!(!state.is_receiving());
  assert_eq!(
    count_temp_files(&ri_dir),
    0,
    "dispose 必须彻底删除临时 snapshot 文件"
  );

  // 释放后再调用必须报错
  let res = state.process_record_sync(&chunk, None, false);
  assert!(matches!(res, Err(Error::ObjectDisposed(_))));

  info!("测试 3 (test_process_partial_chunk_and_cleanup) 验证通过");
  OK
}

/// 测试 4: 临界区并发 Dispose 延迟清理至工作线程退出 (1:1 DisposeDuringProcessRecord_DefersCleanupToWorker)
#[test]
fn test_dispose_during_process_record_defers_cleanup_to_worker() -> Void {
  let dir = tempdir()?;
  let ri_dir = dir.path().join("ri");
  let manager = Arc::new(RangeIndexManager::new(
    ri_dir.clone(),
    dir.path().join("cpr"),
  ));
  let state = Arc::new(RangeIndexMigrationReceiveState::new(manager));
  let chunk = build_partial_chunk();

  let worker_paused = Arc::new(AtomicBool::new(false));
  let can_proceed = Arc::new(AtomicBool::new(false));

  let wp = Arc::clone(&worker_paused);
  let cp = Arc::clone(&can_proceed);

  // 设置暂停钩子：工作线程在写入临时文件后、退出临界区前暂停
  state.set_test_pause_hook(Some(Arc::new(move || {
    wp.store(true, Ordering::SeqCst);
    while !cp.load(Ordering::SeqCst) {
      thread::yield_now();
    }
  })));

  // 启动后台工作线程处理分块
  let state_clone = Arc::clone(&state);
  let chunk_clone = chunk.clone();
  let worker = thread::spawn(move || state_clone.process_record_sync(&chunk_clone, None, false));

  // 主线程等待工作线程到达暂停点且临时文件已创建
  let start = Instant::now();
  while count_temp_files(&ri_dir) == 0 && start.elapsed() < Duration::from_secs(5) {
    thread::yield_now();
  }

  while !worker_paused.load(Ordering::SeqCst) && start.elapsed() < Duration::from_secs(5) {
    thread::yield_now();
  }

  assert_eq!(state.current_chunk_count(), 1, "工作线程应已处理 1 个分块");
  assert!(state.is_receiving(), "反序列化器正在运行");
  assert_eq!(count_temp_files(&ri_dir), 1, "临时文件必须存在");

  // 并发调用 dispose：守卫判定有活跃工作线程，延迟清理
  state.dispose();

  // 此时临时文件仍然存在，未被主线程强行删除
  assert_eq!(count_temp_files(&ri_dir), 1, "延迟释放期间临时文件暂不删除");

  // 唤醒工作线程，让其退出临界区
  can_proceed.store(true, Ordering::SeqCst);

  let worker_res = worker.join().expect("工作线程异常退出");
  assert!(worker_res.is_ok(), "延迟清理路径下 process_record 不能抛错");

  // 工作线程退出后，Deferred 延迟清理必须已完全触发
  assert!(!state.is_receiving(), "状态机必须重置为 IDLE");
  assert_eq!(
    count_temp_files(&ri_dir),
    0,
    "工作线程退出时必须已清理临时文件"
  );

  // 再次调用必须抛出 ObjectDisposed
  let res = state.process_record_sync(&chunk, None, false);
  assert!(matches!(res, Err(Error::ObjectDisposed(_))));

  info!("测试 4 (DisposeDuringProcessRecord_DefersCleanupToWorker) 验证通过");
  OK
}

/// 测试 5: 流完成时目标槽位 Importing 状态校验与自动发布注册
#[test]
fn test_process_record_complete_stream_slot_importing_and_publish() -> Void {
  let dir = tempdir()?;
  let db_path = dir.path().join("store.wal");
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
  let session = store.new_session()?;

  let ri_dir = dir.path().join("ri");
  let cpr_dir = dir.path().join("cpr");
  let manager = Arc::new(RangeIndexManager::new(ri_dir.clone(), cpr_dir));

  let key = b"complete_ri_key";
  let slot = hash_slot(key);

  // 构造集群配置
  let mut cluster_conf = ClusterConfig::default();

  // 生成真实的 BfTree CPR 快照数据与存根
  let tree_path = dir.path().join("source_tree.bftree");
  let snap_path = dir.path().join("source_snap.bftree");
  let tree = wkv::BfTreeService::open_disk(&tree_path, 4)?;
  tree.insert(b"sub_key_1", b"sub_val_1");
  tree.cpr_snapshot(&snap_path)?;
  let file_data = fs::read(&snap_path)?;

  let stub = wkv::RangeIndexStub::new(0, 65536, 4, 1024, 128, 4096, wkv::StorageBackendType::Disk);
  let stub_bytes = stub.encode();

  let serializer_a = RangeIndexChunkedSerializer::new(key, &stub_bytes, file_data.len() as u64);
  let mut reader_a =
    RangeIndexMigrationReader::new(serializer_a, Cursor::new(&file_data), None, 1024 * 1024)?;

  // 情况 A: 槽位处于 Stable 态 (非 Importing)，完成时应被拒绝
  let state_a = RangeIndexMigrationReceiveState::new(manager.clone());
  Runtime::new()?.block_on(async {
    let mut chunk_buf = vec![0u8; 4096];
    let mut final_res = true;
    while !reader_a.is_complete() {
      let n = reader_a.read_next_chunk(&mut chunk_buf)?;
      if n == 0 {
        break;
      }
      final_res = state_a
        .process_record(&chunk_buf[..n], Some(&cluster_conf), Some(&session), false)
        .await?;
    }
    assert!(
      !final_res,
      "槽位非 Importing 状态时必须拒绝发布并返回 false"
    );
    assert!(!state_a.is_receiving(), "失败后状态必须重置为 IDLE");
    assert_eq!(count_temp_files(&ri_dir), 0, "失败后临时文件必须被删除");
    OK
  })?;

  // 情况 B: 槽位处于 Importing 态，完成时自动调用 publish_migrated_range_index
  cluster_conf.update_slot_state(slot, 1, SlotState::Importing);
  assert!(cluster_conf.is_importing_slot(slot));

  let serializer_b = RangeIndexChunkedSerializer::new(key, &stub_bytes, file_data.len() as u64);
  let mut reader_b =
    RangeIndexMigrationReader::new(serializer_b, Cursor::new(&file_data), None, 1024 * 1024)?;

  let state_b = RangeIndexMigrationReceiveState::new(manager.clone());
  Runtime::new()?.block_on(async {
    let mut chunk_buf = vec![0u8; 4096];
    let mut final_res = true;
    while !reader_b.is_complete() {
      let n = reader_b.read_next_chunk(&mut chunk_buf)?;
      if n == 0 {
        break;
      }
      final_res = state_b
        .process_record(&chunk_buf[..n], Some(&cluster_conf), Some(&session), false)
        .await?;
    }
    assert!(final_res, "槽位 Importing 态下流完成必须成功发布");
    assert!(!state_b.is_receiving(), "发布成功后状态重置为 IDLE");
    assert_eq!(count_temp_files(&ri_dir), 0, "临时文件已移至正式库或已清理");
    OK
  })?;

  info!("测试 5 (Slot Importing 校验与自动发布) 验证通过");
  OK
}

/// 测试 6: CooperativeDisposeGuard 状态机单元测试 (CleanupNow, DeferredToWorker, AlreadyDisposed)
#[test]
fn test_cooperative_dispose_guard_transitions() -> Void {
  // 场景 A: 无工作线程时调用 dispose -> CleanupNow
  let guard_a = CooperativeDisposeGuard::new();
  assert!(!guard_a.is_disposed());
  assert_eq!(guard_a.try_dispose(), DisposeResult::CleanupNow);
  assert!(guard_a.is_disposed());
  assert_eq!(guard_a.try_dispose(), DisposeResult::AlreadyDisposed);
  assert!(!guard_a.try_enter(), "已释放守卫拒绝进入");

  // 场景 B: 工作线程在临界区内调用 dispose -> DeferredToWorker
  let guard_b = CooperativeDisposeGuard::new();
  assert!(guard_b.try_enter(), "正常进入临界区");
  assert!(!guard_b.is_disposed());

  assert_eq!(guard_b.try_dispose(), DisposeResult::DeferredToWorker);
  assert!(guard_b.is_disposed());

  // 工作线程退出临界区，应检测到需要清理
  let should_clean = guard_b.exit_and_check_should_cleanup();
  assert!(should_clean, "必须指示工作线程执行延迟清理");

  // 退出后再尝试进入应拒绝
  assert!(!guard_b.try_enter());

  // 场景 C: 正常进出临界区，未发生 dispose
  let guard_c = CooperativeDisposeGuard::new();
  assert!(guard_c.try_enter());
  let should_clean = guard_c.exit_and_check_should_cleanup();
  assert!(!should_clean, "无 dispose 发生，退出不需要清理");
  assert_eq!(guard_c.try_dispose(), DisposeResult::CleanupNow);

  info!("测试 6 (CooperativeDisposeGuard 状态转移) 验证通过");
  OK
}

/// 测试 7: CooperativeDisposeGuard 多工作线程并发重入与最后一个 Worker 退出执行延迟清理
#[test]
fn test_cooperative_dispose_guard_multi_worker_deferred_cleanup() -> Void {
  let guard = Arc::new(CooperativeDisposeGuard::new());

  // 两个工作线程同时进入临界区
  assert!(guard.try_enter(), "Worker 1 应该成功进入");
  assert!(guard.try_enter(), "Worker 2 应该成功进入");

  // 并发调用 dispose
  assert_eq!(
    guard.try_dispose(),
    DisposeResult::DeferredToWorker,
    "存在活跃 worker 时必须返回 DeferredToWorker"
  );
  assert!(guard.is_disposed());

  // 新 worker 无法再次进入
  assert!(!guard.try_enter(), "已释放态下禁止新 worker 进入");

  // Worker 1 退出临界区：还有 Worker 2 在运行，此时绝不能触发清理！
  let should_clean_1 = guard.exit_and_check_should_cleanup();
  assert!(
    !should_clean_1,
    "尚有其他活跃 worker 在运行，Worker 1 退出时不得清理"
  );

  // Worker 2 退出临界区：最后一个活跃 worker，必须指示触发清理！
  let should_clean_2 = guard.exit_and_check_should_cleanup();
  assert!(should_clean_2, "最后一个活跃 worker 退出时必须触发延迟清理");

  info!("测试 7 (多 Worker 并发安全与延迟清理保证) 验证通过");
  OK
}

/// 测试 8: RangeIndexFileDataSink 写入 .tmp 临时文件、异常崩溃/未 complete Drop 自动清理、以及 complete 原子重命名
#[test]
fn test_range_index_file_data_sink_tmp_cleanup_and_atomic_complete() -> Void {
  let dir = tempdir()?;
  let dest_file = dir.path().join("sub_dir").join("dest.bftree");

  // 场景 A: 写入部分数据后未调用 complete() 即发生 Drop（模拟网络中断或 panic）
  {
    let mut sink = RangeIndexFileDataSink::from_path(
      CheckpointFileType::StoreRangeIndexFlush,
      1001,
      dest_file.clone(),
    )?;

    assert_eq!(sink.tmp_path, dest_file.with_extension("bftree.tmp"));
    assert!(sink.tmp_path.exists(), ".tmp 临时文件在创建时必须存在");
    assert!(
      !dest_file.exists(),
      "正式目标文件在未 complete 前绝不能出现"
    );

    let chunk = [0x5au8; 1024];
    sink.write_chunk(0, &chunk)?;
    assert_eq!(sink.current_position, 1024);
    assert!(sink.tmp_path.exists());

    // 显式 drop，不调用 complete()
    drop(sink);
  }

  // 验证: Drop 机制必须已彻底删除残留的 .tmp 临时文件，且正式文件未创建
  assert!(
    !dest_file.exists(),
    "异常退出的写入器不得生成残缺的正式文件"
  );
  let tmp_file = dest_file.with_extension("bftree.tmp");
  assert!(
    !tmp_file.exists(),
    "异常退出的写入器必须彻底清理 .tmp 临时文件"
  );

  // 场景 B: 正常完整分块写入并 complete()
  {
    let mut sink = RangeIndexFileDataSink::from_path(
      CheckpointFileType::StoreRangeIndexSnapshot,
      1002,
      dest_file.clone(),
    )?;

    // 模拟对标 Garnet 64KB 大块与跨块多片写入
    let chunk1 = [0xaau8; DEFAULT_CHUNK_SIZE];
    let chunk2 = [0xbbu8; 32 * 1024];

    sink.write_chunk(0, &chunk1)?;
    sink.write_chunk(DEFAULT_CHUNK_SIZE as u64, &chunk2)?;
    assert_eq!(
      sink.current_position,
      (DEFAULT_CHUNK_SIZE + 32 * 1024) as u64
    );

    sink.complete()?;
    assert!(sink.completed);
  }

  // 验证: 正式文件已原子生成且内容完全一致，临时文件已不存在
  assert!(dest_file.exists(), "complete 后正式文件必须存在");
  assert!(
    !tmp_file.exists(),
    "complete 后 .tmp 临时文件已被原子重命名"
  );

  let content = fs::read(&dest_file)?;
  assert_eq!(content.len(), DEFAULT_CHUNK_SIZE + 32 * 1024);
  assert_eq!(
    &content[..DEFAULT_CHUNK_SIZE],
    &[0xaau8; DEFAULT_CHUNK_SIZE]
  );
  assert_eq!(&content[DEFAULT_CHUNK_SIZE..], &[0xbbu8; 32 * 1024]);

  info!("测试 8 (RangeIndexFileDataSink .tmp 清理与原子 complete) 验证通过");
  OK
}

/// 测试 9: 对标 Garnet 64KB DEFAULT_CHUNK_SIZE 的分块读取与 RangeIndexSnapshotReader 零拷贝 next_chunk 连续迭代
#[test]
fn test_range_index_snapshot_reader_zero_copy_iteration() -> Void {
  let dir = tempdir()?;
  let ri_dir = dir.path().join("ri");
  let cpr_dir = dir.path().join("cpr");
  let manager = RangeIndexManager::new(ri_dir.clone(), cpr_dir.clone());
  let token = 88888888_u128;
  let token_str = itoa::Buffer::new().format(token).to_string();

  // 创建一个 150KB 的大文件 (测试跨 64KB 分块) 和一个 20KB 的小文件
  let key1_hash = "11111111111111111111111111111111";
  let path1 = manager.log_flush_path(key1_hash, 100);
  fs::create_dir_all(path1.parent().unwrap())?;
  let data1: Vec<u8> = (0..150 * 1024).map(|i| (i % 255) as u8).collect();
  fs::write(&path1, &data1)?;

  let key2_hash = "22222222222222222222222222222222";
  let path2 = manager.checkpoint_snapshot_path(&token_str, key2_hash);
  fs::create_dir_all(path2.parent().unwrap())?;
  let data2: Vec<u8> = (0..20 * 1024).map(|i| ((i + 13) % 255) as u8).collect();
  fs::write(&path2, &data2)?;

  let mut reader = RangeIndexSnapshotReader::new(&manager, token, 50, 200)?;
  assert_eq!(reader.len(), 2);
  assert_eq!(reader.shared_buffer().len(), DEFAULT_CHUNK_SIZE);

  // 使用 next_chunk 进行单次零拷贝连续遍历
  let mut read_data1 = Vec::with_capacity(data1.len());
  let mut read_data2 = Vec::with_capacity(data2.len());

  while let Some((source, chunk)) = reader.next_chunk()? {
    // 验证分块大小不超过 64KB DEFAULT_CHUNK_SIZE
    assert!(chunk.len() <= DEFAULT_CHUNK_SIZE);

    if source.key_hash == key1_hash {
      read_data1.extend_from_slice(chunk);
    } else if source.key_hash == key2_hash {
      read_data2.extend_from_slice(chunk);
    } else {
      panic!("未知 key_hash: {}", source.key_hash);
    }
  }

  assert_eq!(read_data1, data1);
  assert_eq!(read_data2, data2);

  // 验证迭代完成后再次调用返回 None
  assert!(reader.next_chunk()?.is_none());

  // 验证 reset 重置后可重新遍历
  reader.reset();
  assert_eq!(reader.current_source_index(), 0);

  info!("测试 9 (64KB 分块读取与 RangeIndexSnapshotReader 零拷贝迭代) 验证通过");
  OK
}

/// 测试 10: ReplicationHistory 97 字节定长二进制协议与 save_to_file 异常自愈
#[test]
fn test_replication_history_stack_binary_protocol() -> Void {
  let dir = tempdir()?;
  let hist_path = dir.path().join("repl_history.bin");

  let mut history = ReplicationHistory::new(100);
  let id1 = history.primary_replid;
  assert_eq!(history.to_byte_array().len(), 97);

  // 1. 保存至文件
  history.save_to_file(&hist_path)?;
  assert!(hist_path.exists());

  // 2. 栈分配加载与校验
  let loaded = ReplicationHistory::load_from_file(&hist_path)?;
  assert_eq!(loaded.primary_replid, id1);
  assert_eq!(loaded.replication_offset, 100);

  // 3. 故障转移轮转切换与保存
  history = history.failover_update(500);
  assert_eq!(history.primary_replid2, id1);
  assert_eq!(history.replication_offset, 500);
  assert_eq!(history.replication_offset2, 500);
  assert_ne!(history.primary_replid, id1);

  history.save_to_file(&hist_path)?;
  let loaded2 = ReplicationHistory::load_from_file(&hist_path)?;
  assert_eq!(loaded2, history);

  // 4. 主文件损坏时自动从有效 .tmp 备份自愈（save 原子重命名前崩溃残留的 tmp）
  let tmp_path = PathBuf::from(format!("{}.tmp", hist_path.display()));
  history.save_to_file(&tmp_path)?;
  // 破坏主文件内容
  fs::write(&hist_path, b"corrupted_invalid_data")?;

  let recovered = ReplicationHistory::load_from_file(&hist_path)?;
  assert_eq!(recovered, history);

  info!("测试 10 (ReplicationHistory 97 字节二进制协议与自愈) 验证通过");
  OK
}
