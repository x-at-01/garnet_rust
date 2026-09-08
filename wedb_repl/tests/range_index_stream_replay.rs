//! RangeIndex 流式回放重组状态机测试套件 (1:1微软 Garnet RangeIndexStreamReplayTests.cs)
//!
//! 验证 AOF 流式回放重组器在各种边缘条件下的状态维护、重置、垃圾回收与无泄漏保证：
//! 1. PartialStreamIsPendingThenCleanedUp: 未完成流处于 pending 态，主动清理后状态彻底归零
//! 2. RetryFirstChunkResetsStalePartialReassembly: 重试流首分块自动重置旧残余状态，后续分块干净重组
//! 3. MalformedChunkIsDropped: 损坏分块立即丢弃，清除状态机并报错
//! 4. FinalFlagOnIncompleteStreamDropsReassembly: 提前带有 is_last 标志的未完成流立即丢弃并报错
//! 5. PublishFailureOnCompleteStreamThrows: 发布入库失败（如键已存在且不可替换）立即报错并彻底清理状态
use std::{fs::read_dir, io::Cursor, sync::Arc};

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_repl::RangeIndexStreamReassembler;
use wkv::{
  RANGE_INDEX_STUB_SIZE, RangeIndexChunkedSerializer, RangeIndexMigrationReader, StorageBackend,
  StoreConfig, TreeTuning, WedbStore,
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

/// 构造定长 35 字节的测试存根数据 (MakeStub)
const fn make_stub() -> [u8; RANGE_INDEX_STUB_SIZE] {
  let mut stub = [0u8; RANGE_INDEX_STUB_SIZE];
  let mut i = 0;
  while i < RANGE_INDEX_STUB_SIZE {
    stub[i] = (0xC0 + i) as u8;
    i += 1;
  }
  stub
}

/// 编译期常量测试存根 (零运行时计算)
const TEST_STUB: [u8; RANGE_INDEX_STUB_SIZE] = make_stub();

/// 确定性生成伪随机字节序列 (RandomBytes)
fn deterministic_bytes(n: usize) -> Vec<u8> {
  (0..n).map(|i| ((i * 37 + 17) % 256) as u8).collect()
}

/// 模拟 Garnet BuildStreamChunks：使用 RangeIndexMigrationReader 生成 (chunk, is_first, is_last) 元组
fn build_stream_chunks(
  key: &[u8],
  stub: &[u8],
  file_data: &[u8],
  chunk_size: usize,
) -> aok::Result<Vec<(Vec<u8>, bool, bool)>> {
  let serializer = RangeIndexChunkedSerializer::new(key, stub, file_data.len() as u64);
  let mut reader =
    RangeIndexMigrationReader::new(serializer, Cursor::new(file_data), None, 1024 * 1024)?;
  let mut result = Vec::new();
  let mut dest = vec![0u8; chunk_size];
  let mut is_first = true;

  while !reader.is_complete() {
    let written = reader.read_next_chunk(&mut dest)?;
    result.push((dest[..written].to_vec(), is_first, reader.is_complete()));
    is_first = false;
  }

  Ok(result)
}

/// 初始化轻量级测试存储引擎
fn init_test_store() -> aok::Result<(Arc<WedbStore<SegmentedDevice>>, tempfile::TempDir)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("store.data");
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
  Ok((store, dir))
}

/// 测试 1: PartialStreamIsPendingThenCleanedUp
/// 仅发送第一个分块，流处于 pending 态，主动调用 clear() 后状态彻底归零
#[test]
fn test_partial_stream_is_pending_then_cleaned_up() -> Void {
  let (store, dir) = init_test_store()?;
  let temp_reassemble_dir = dir.path().join("reassemble");

  Runtime::new()?.block_on(async {
    let session = store.new_session()?;
    let key = b"k1";
    let file_data = deterministic_bytes(8192);
    let chunks = build_stream_chunks(key, &TEST_STUB, &file_data, 512)?;
    assert!(chunks.len() > 2, "测试需要多块分块流");

    let mut reassembler = RangeIndexStreamReassembler::new(&temp_reassemble_dir);

    // 仅输入首个分块：重组状态处于进行中
    let published = reassembler
      .process_chunk(&session, key, &chunks[0].0, chunks[0].1, chunks[0].2)
      .await?;
    assert!(!published, "首分块不能提前发布");
    assert_eq!(reassembler.pending_count(), 1, "重组应处于 pending 状态");

    // 回放结束或超时清理：清除所有未完成状态机
    reassembler.clear();
    assert_eq!(
      reassembler.pending_count(),
      0,
      "清理后 pending 计数必须归零"
    );

    OK
  })?;

  info!("测试 1 (PartialStreamIsPendingThenCleanedUp) 验证通过");
  OK
}

/// 测试 2: RetryFirstChunkResetsStalePartialReassembly /// 首 chunk 失败后残留，重试时发送 is_first: true 的新首 chunk 自动重置旧的残余状态，后续分块干净重组
#[test]
fn test_retry_first_chunk_resets_stale_partial_reassembly() -> Void {
  let (store, dir) = init_test_store()?;
  let temp_reassemble_dir = dir.path().join("reassemble");

  Runtime::new()?.block_on(async {
    let session = store.new_session()?;
    let key = b"k1";
    let file_data = deterministic_bytes(8192);
    let chunks = build_stream_chunks(key, &TEST_STUB, &file_data, 512)?;
    assert!(chunks.len() > 3, "测试需要至少 4 个分块");

    let mut reassembler = RangeIndexStreamReassembler::new(&temp_reassemble_dir);

    // 第 1 次尝试：发送首个分块后中止，残留局部状态
    let published = reassembler
      .process_chunk(&session, key, &chunks[0].0, true, false)
      .await?;
    assert!(!published);
    assert_eq!(reassembler.pending_count(), 1);

    // 第 2 次尝试（重试）：重放同一条流。首分块 (is_first: true) 必须重置旧的局部状态。
    // 喂入除最后一个分块之外的所有分块，确保保持未完成态
    for (i, chunk) in chunks.iter().take(chunks.len() - 1).enumerate() {
      let published = reassembler
        .process_chunk(&session, key, &chunk.0, i == 0, false)
        .await?;
      assert!(!published);
    }

    assert_eq!(
      reassembler.pending_count(),
      1,
      "重试流在首块重置后应干净完成局部重组"
    );

    reassembler.clear();
    assert_eq!(reassembler.pending_count(), 0);

    OK
  })?;

  info!("测试 2 (RetryFirstChunkResetsStalePartialReassembly) 验证通过");
  OK
}

/// 测试 3: MalformedChunkIsDropped /// 非法的首 chunk（如非正数键长）被反序列化器拒绝，立即清除 pending 状态并返回错误
#[test]
fn test_malformed_chunk_is_dropped() -> Void {
  let (store, dir) = init_test_store()?;
  let temp_reassemble_dir = dir.path().join("reassemble");

  Runtime::new()?.block_on(async {
    let session = store.new_session()?;
    let key = b"k1";
    let mut reassembler = RangeIndexStreamReassembler::new(&temp_reassemble_dir);

    // 前 4 字节为 0，代表键长度为 0（非法），反序列化器应拒绝
    let malformed = [0u8; 16];
    let res = reassembler
      .process_chunk(&session, key, &malformed, true, false)
      .await;

    assert!(res.is_err(), "非法首分块必须返回错误");
    assert_eq!(
      reassembler.pending_count(),
      0,
      "失败重组状态必须被立即清理，严禁残留"
    );

    OK
  })?;

  info!("测试 3 (MalformedChunkIsDropped) 验证通过");
  OK
}

/// 测试 4: FinalFlagOnIncompleteStreamDropsReassembly /// 在流尚未完整时带有 is_last: true 标志，立即报错并丢弃该状态
#[test]
fn test_final_flag_on_incomplete_stream_drops_reassembly() -> Void {
  let (store, dir) = init_test_store()?;
  let temp_reassemble_dir = dir.path().join("reassemble");

  Runtime::new()?.block_on(async {
    let session = store.new_session()?;
    let key = b"k1";
    let file_data = deterministic_bytes(8192);
    let chunks = build_stream_chunks(key, &TEST_STUB, &file_data, 512)?;
    assert!(chunks.len() > 2, "测试需要多块分块流");

    let mut reassembler = RangeIndexStreamReassembler::new(&temp_reassemble_dir);

    // 将首分块伪造为最后一个分块（is_last: true），但实际数据流并未结束
    let res = reassembler
      .process_chunk(&session, key, &chunks[0].0, true, true)
      .await;

    assert!(res.is_err(), "未完成流带有 is_last 标志必须报错");
    assert_eq!(
      reassembler.pending_count(),
      0,
      "截断/损坏流的状态必须立即被清理丢弃"
    );

    OK
  })?;

  info!("测试 4 (FinalFlagOnIncompleteStreamDropsReassembly) 验证通过");
  OK
}

/// 测试 5: PublishFailureOnCompleteStreamThrows /// 当目标已存在且未开启 replace 时，完整流发布触发失败，报错并彻底清除 pending 状态与临时文件
#[test]
fn test_publish_failure_on_complete_stream_throws() -> Void {
  let (store, dir) = init_test_store()?;
  let temp_reassemble_dir = dir.path().join("reassemble");

  Runtime::new()?.block_on(async {
    let session = store.new_session()?;
    let key = b"k1";

    // 预先在本地存储引擎创建目标索引，使得后续无 replace 选项的发布必定失败 (AlreadyExists)
    session
      .range_index_create(key, StorageBackend::Std, TUNE)
      .await?;
    assert!(session.range_index_exists(key).await?);

    let file_data = deterministic_bytes(8192);
    let chunks = build_stream_chunks(key, &TEST_STUB, &file_data, 512)?;
    assert!(chunks.len() > 2, "测试需要多块分块流");

    let mut reassembler = RangeIndexStreamReassembler::new(&temp_reassemble_dir);

    // 喂入除最后一个分块之外的所有数据
    for chunk in chunks.iter().take(chunks.len() - 1) {
      let published = reassembler
        .process_chunk(&session, key, &chunk.0, chunk.1, chunk.2)
        .await?;
      assert!(!published);
    }
    assert_eq!(
      reassembler.pending_count(),
      1,
      "流在最后分块到达前应保持 pending"
    );

    // 发送最后一个分块：反序列化器完成并触发 publish，因键已存在而发布失败
    let last = chunks.last().expect("chunks not empty");
    let res = reassembler
      .process_chunk(&session, key, &last.0, last.1, last.2)
      .await;

    assert!(res.is_err(), "完整流发布失败时必须返回错误");
    assert_eq!(
      reassembler.pending_count(),
      0,
      "发布失败后必须彻底清除 pending 状态，杜绝内存泄漏"
    );

    // 验证磁盘临时文件也已被显式清理 (dispose)，无文件泄漏 (流式检查避免 Vec 分配)
    assert!(
      read_dir(&temp_reassemble_dir)?.next().is_none(),
      "发布失败后重组临时文件必须已被清理丢弃"
    );

    OK
  })?;

  info!("测试 5 (PublishFailureOnCompleteStreamThrows) 验证通过");
  OK
}
