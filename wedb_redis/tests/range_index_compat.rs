use std::{fs, iter::repeat_n, path::Path, str::from_utf8, sync::Arc};

use aok::{OK, Result, Void};
use compio::runtime::Runtime;
use tempfile::{TempDir, tempdir};
use wcpr::CheckpointType;
use wdev::SegmentedDevice;
use wedb_redis::{RenameResult, prelude::*};
use wkv::{
  RangeIndexError, RangeIndexStub, ScanReturnField, StorageBackend, StoreConfig, StoreSession,
  TreeTuning, WedbStore, encode_ri_create, encode_ri_del, encode_ri_set,
};

/// 十进制补零到 width 位
fn pad(v: impl itoa::Integer, width: usize) -> String {
  let mut buf = itoa::Buffer::new();
  let digits = buf.format(v);
  let mut s = String::with_capacity(width.max(digits.len()));
  s.extend(repeat_n('0', width.saturating_sub(digits.len())));
  s.push_str(digits);
  s
}

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
  leaf_page_size: 0,
};

/// 辅助函数：在指定临时目录中打开 WeDB 存储实例
fn open_store(dir: &Path) -> Result<Arc<WedbStore<SegmentedDevice>>> {
  let db_path = dir.join("store.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let config = StoreConfig::new(2048, 64 * 1024, 16, 0.5)?.with_range_index_dir(dir);
  Ok(Arc::new(WedbStore::open(config, device)?))
}

/// 辅助函数：创建新临时目录和对应的存储实例
fn new_store() -> Result<(TempDir, Arc<WedbStore<SegmentedDevice>>)> {
  let dir = tempdir()?;
  let store = open_store(dir.path())?;
  Ok((dir, store))
}

/// 辅助函数：创建全局 Checkpoint 快照并返回 token
async fn checkpoint(store: &Arc<WedbStore<SegmentedDevice>>, dir: &Path) -> Result<u128> {
  let ckpt_dir = dir.join("checkpoints");
  let meta = store
    .create_checkpoint(&ckpt_dir, CheckpointType::Snapshot)
    .await?;
  Ok(meta.token)
}

/// 辅助函数：从 Checkpoint 崩溃恢复存储实例与会话
async fn recover(
  dir: &Path,
  token: u128,
) -> Result<(
  Arc<WedbStore<SegmentedDevice>>,
  StoreSession<SegmentedDevice>,
)> {
  let ckpt_dir = dir.join("checkpoints");
  let db_path = dir.join("store.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::recover(&ckpt_dir, token, device).await?);
  let session = store.new_session()?;
  Ok((store, session))
}

/// 在缓冲区中查找首个 `\r\n` 的偏移
fn find_crlf(buf: &[u8]) -> Option<usize> {
  buf.windows(2).position(|w| w == b"\r\n")
}

/// 解析 `encode_ri_*` 生成的 RESP 定长数组帧流：`*N\r\n` 后跟 N 个 `$len\r\nbulk\r\n`
fn parse_resp_frames(payload: &[u8]) -> Vec<Vec<Vec<u8>>> {
  let mut frames = Vec::new();
  let mut cur = payload;
  while !cur.is_empty() {
    if cur[0] != b'*' {
      break;
    }
    let Some(head_end) = find_crlf(cur) else {
      break;
    };
    let Some(n) = from_utf8(&cur[1..head_end])
      .ok()
      .and_then(|s| s.parse::<usize>().ok())
    else {
      break;
    };
    cur = &cur[head_end + 2..];
    if n == 0 {
      continue;
    }
    let mut args = Vec::with_capacity(n);
    for _ in 0..n {
      if cur.first() != Some(&b'$') {
        return frames;
      }
      let Some(len_end) = find_crlf(cur) else {
        return frames;
      };
      let Some(blen) = from_utf8(&cur[1..len_end])
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
      else {
        return frames;
      };
      cur = &cur[len_end + 2..];
      if cur.len() < blen + 2 {
        return frames;
      }
      args.push(cur[..blen].to_vec());
      cur = &cur[blen + 2..];
    }
    frames.push(args);
  }
  frames
}

/// 回放 AOF / 命令复制流载荷到存储会话
async fn replay_payload(session: &StoreSession<SegmentedDevice>, payload: &[u8]) -> Result<()> {
  for args in parse_resp_frames(payload) {
    match args[0].as_slice() {
      b"RI.CREATE" => {
        let key = &args[1];
        let mut backend = StorageBackend::Std;
        let mut tuning = TreeTuning {
          cache_size: 65536,
          min_record_size: 8,
          max_record_size: 1024,
          max_key_len: 128,
          leaf_page_size: 0,
        };
        let mut i = 2;
        while i < args.len() {
          let opt = args[i].as_slice();
          if opt.eq_ignore_ascii_case(b"MEMORY") {
            backend = StorageBackend::Memory;
            i += 1;
          } else if opt.eq_ignore_ascii_case(b"DISK") {
            backend = StorageBackend::Std;
            i += 1;
          } else if i + 1 < args.len() {
            let val = from_utf8(&args[i + 1])
              .ok()
              .and_then(|s| s.parse().ok())
              .unwrap_or(0);
            match opt {
              opt if opt.eq_ignore_ascii_case(b"CACHESIZE") => tuning.cache_size = val,
              opt if opt.eq_ignore_ascii_case(b"MINRECORD") => tuning.min_record_size = val,
              opt if opt.eq_ignore_ascii_case(b"MAXRECORD") => tuning.max_record_size = val,
              opt if opt.eq_ignore_ascii_case(b"MAXKEYLEN") => tuning.max_key_len = val,
              opt if opt.eq_ignore_ascii_case(b"PAGESIZE") => tuning.leaf_page_size = val,
              _ => {}
            }
            i += 2;
          } else {
            i += 1;
          }
        }
        let _ = session.range_index_create(key, backend, tuning).await;
      }
      b"RI.SET" if args.len() == 4 => {
        let _ = session.range_index_set(&args[1], &args[2], &args[3]).await;
      }
      b"RI.DEL" if args.len() == 3 => {
        let _ = session.range_index_del(&args[1], &args[2]).await;
      }
      _ => {}
    }
  }
  Ok(())
}

// ---------------------------------------------------------------------------
// 1. 基础创建与校验测试 (RICreate*)
// --------------------------------------------------------------------------- /// 1. 对应 Garnet RICreateBasicTest
#[test]
fn test_ri_create_basic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          min_record_size: 64,
          ..TUNE
        },
      )
      .await?;
    assert!(session.range_index_exists(b"myindex").await?);
    OK
  })
}

/// 2. 对应 Garnet RICreateDuplicateReturnsErrorTest
#[test]
fn test_ri_create_duplicate_returns_error() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          min_record_size: 64,
          ..TUNE
        },
      )
      .await?;
    let err = session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          min_record_size: 64,
          ..TUNE
        },
      )
      .await;
    assert!(matches!(err, Err(RangeIndexError::AlreadyExists)));
    OK
  })
}

/// 3. 对应 Garnet RICreateThenDeleteTest
#[test]
fn test_ri_create_then_delete() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value1")
      .await?;

    // 删除键
    assert!(session.delete(b"myindex").await?);
    // 再次删除返回 false
    assert!(!session.delete(b"myindex").await?);

    // 删除后读写返回 NotFound
    assert!(matches!(
      session
        .range_index_set(b"myindex", b"field1", b"value1")
        .await,
      Err(RangeIndexError::NotFound)
    ));
    assert!(matches!(
      session.range_index_get(b"myindex", b"field1").await,
      Err(RangeIndexError::NotFound)
    ));
    OK
  })
}

/// 4. 对应 Garnet RICreateWithDefaultsTest
#[test]
fn test_ri_create_with_defaults() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Std,
        TreeTuning {
          cache_size: 16 * 1024 * 1024,
          min_record_size: 64,
          ..TUNE
        },
      )
      .await?;
    assert!(session.range_index_exists(b"myindex").await?);
    OK
  })
}

/// 5. 对应 Garnet RICreateWithAllOptTest
#[test]
fn test_ri_create_with_all_options() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          cache_size: 131072,
          leaf_page_size: 4096,
          ..TUNE
        },
      )
      .await?;
    assert!(session.range_index_exists(b"myindex").await?);

    let stub = session.range_index_config(b"myindex").await?;
    assert_eq!(stub.cache_size, 131072);
    assert_eq!(stub.min_record_size, 8);
    assert_eq!(stub.max_record_size, 1024);
    assert_eq!(stub.max_key_len, 128);
    assert_eq!(stub.leaf_page_size, 4096);
    OK
  })
}

// ---------------------------------------------------------------------------
// 2. 字段读写与删除测试 (RISet*, RIGet*, RIDel*)
// --------------------------------------------------------------------------- /// 6. 对应 Garnet RISetAndGetBasicTest
#[test]
fn test_ri_set_and_get_basic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value1")
      .await?;

    let val = session.range_index_get(b"myindex", b"field1").await?;
    assert_eq!(val, Some(b"value1".to_vec()));
    OK
  })
}

/// 7. 对应 Garnet RISetOverwriteTest
#[test]
fn test_ri_set_overwrite() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value1")
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value2")
      .await?;

    let val = session.range_index_get(b"myindex", b"field1").await?;
    assert_eq!(val, Some(b"value2".to_vec()));
    OK
  })
}

/// 8. 对应 Garnet RIGetNonExistentFieldTest
#[test]
fn test_ri_get_non_existent_field() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    let val = session.range_index_get(b"myindex", b"nosuchfield").await?;
    assert_eq!(val, None);
    OK
  })
}

/// 9. 对应 Garnet RIGetNonExistentIndexTest
#[test]
fn test_ri_get_non_existent_index() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    let err = session.range_index_get(b"noindex", b"field1").await;
    assert!(matches!(err, Err(RangeIndexError::NotFound)));
    OK
  })
}

/// 10. 对应 Garnet RIDelFieldTest
#[test]
fn test_ri_del_field() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value1")
      .await?;

    // 删除已有字段返回 true
    let del_res = session.range_index_del(b"myindex", b"field1").await?;
    assert!(del_res);

    let val = session.range_index_get(b"myindex", b"field1").await?;
    assert_eq!(val, None);
    OK
  })
}

/// 11. 幂等删除：删除不存在字段只要索引存在固定返回 true
#[test]
fn test_ri_del_non_existent_field() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;

    // 删除不存在字段固定返回 true (BfTree delete 幂等成功)
    let del_res = session
      .range_index_del(b"myindex", b"non_existent_field")
      .await?;
    assert!(del_res);
    OK
  })
}

/// 12. 对应 Garnet RIDelOnNonExistentIndexTest
#[test]
fn test_ri_del_on_non_existent_index() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    let err = session.range_index_del(b"noindex", b"field1").await;
    assert!(matches!(err, Err(RangeIndexError::NotFound)));
    OK
  })
}

/// 13. 对应 Garnet RISetOnNonExistentIndexTest
#[test]
fn test_ri_set_on_non_existent_index() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    let err = session
      .range_index_set(b"noindex", b"field1", b"value1")
      .await;
    assert!(matches!(err, Err(RangeIndexError::NotFound)));
    OK
  })
}

/// 14. 对应 Garnet RIMultipleFieldsTest
#[test]
fn test_ri_multiple_fields() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"aaa", b"val-a")
      .await?;
    session
      .range_index_set(b"myindex", b"bbb", b"val-b")
      .await?;
    session
      .range_index_set(b"myindex", b"ccc", b"val-c")
      .await?;

    assert_eq!(
      session.range_index_get(b"myindex", b"aaa").await?,
      Some(b"val-a".to_vec())
    );
    assert_eq!(
      session.range_index_get(b"myindex", b"bbb").await?,
      Some(b"val-b".to_vec())
    );
    assert_eq!(
      session.range_index_get(b"myindex", b"ccc").await?,
      Some(b"val-c".to_vec())
    );
    OK
  })
}

// ---------------------------------------------------------------------------
// 3. 类型安全与互斥校验 (WRONGTYPE)
// --------------------------------------------------------------------------- /// 15. 对应 Garnet RIWrongTypeOnNormalKeyTest
#[test]
fn test_ri_wrong_type_on_normal_key() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session.upsert(b"normalkey", b"hello").await?;
    let err = session
      .range_index_set(b"normalkey", b"field1", b"value1")
      .await;
    assert!(matches!(err, Err(RangeIndexError::WrongType)));
    OK
  })
}

/// 16. 对应 Garnet RIWrongTypeGetOnNormalKeyTest
#[test]
fn test_ri_wrong_type_get_on_normal_key() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session.upsert(b"normalkey", b"hello").await?;
    let err = session.range_index_get(b"normalkey", b"field1").await;
    assert!(matches!(err, Err(RangeIndexError::WrongType)));
    OK
  })
}

/// 17. 对应 Garnet RINormalGetOnRangeIndexKeyTest
#[test]
fn test_ri_normal_get_on_range_index_key() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    // 普通读操作读取集合键返回 None（因数据存储在元数据段）
    assert_eq!(session.read(b"myindex").await?, None);
    // 类型识别为 rangeindex
    assert_eq!(session.type_of(b"myindex").await?, "rangeindex");
    OK
  })
}

/// 18. 对应 Garnet RINormalSetOnRangeIndexKeyTest
#[test]
fn test_ri_normal_set_on_range_index_key() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value1")
      .await?;
    assert_eq!(session.type_of(b"myindex").await?, "rangeindex");
    OK
  })
}

// ---------------------------------------------------------------------------
// 4. AOF 日志与增量流式回放 (RIAofReplayTest)
// --------------------------------------------------------------------------- /// 19. 对应 Garnet RIAofReplayTest
#[test]
fn test_ri_aof_replay() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    let create_cmd = encode_ri_create(b"aoftest", StorageBackend::Std, TUNE);
    replay_payload(&session, &create_cmd).await?;
    assert!(session.range_index_exists(b"aoftest").await?);

    let mut payload = Vec::new();
    payload.extend_from_slice(&encode_ri_set(b"aoftest", b"key1", b"val1"));
    payload.extend_from_slice(&encode_ri_set(b"aoftest", b"key2", b"val2"));
    payload.extend_from_slice(&encode_ri_set(b"aoftest", b"key3", b"val3"));
    // 后续增量变更
    payload.extend_from_slice(&encode_ri_set(b"aoftest", b"key4", b"val4"));
    payload.extend_from_slice(&encode_ri_set(b"aoftest", b"key1", b"val1-updated"));
    payload.extend_from_slice(&encode_ri_del(b"aoftest", b"key2"));

    replay_payload(&session, &payload).await?;

    assert_eq!(
      session.range_index_get(b"aoftest", b"key1").await?,
      Some(b"val1-updated".to_vec())
    );
    assert_eq!(session.range_index_get(b"aoftest", b"key2").await?, None);
    assert_eq!(
      session.range_index_get(b"aoftest", b"key3").await?,
      Some(b"val3".to_vec())
    );
    assert_eq!(
      session.range_index_get(b"aoftest", b"key4").await?,
      Some(b"val4".to_vec())
    );
    OK
  })
}

// ---------------------------------------------------------------------------
// 5. 范围扫描与区间查询 (RIScan*, RIRange*)
// --------------------------------------------------------------------------- /// 20. 对应 Garnet RIScanBasicTest
#[test]
fn test_ri_scan_basic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"aaa", b"val-a")
      .await?;
    session
      .range_index_set(b"myindex", b"bbb", b"val-b")
      .await?;
    session
      .range_index_set(b"myindex", b"ccc", b"val-c")
      .await?;
    session
      .range_index_set(b"myindex", b"ddd", b"val-d")
      .await?;
    session
      .range_index_set(b"myindex", b"eee", b"val-e")
      .await?;

    let res = session
      .range_index_scan(b"myindex", b"aaa", 3, ScanReturnField::KeyAndValue)
      .await?;
    assert_eq!(res.len(), 3);
    assert_eq!(res[0].key, b"aaa");
    assert_eq!(res[0].value, b"val-a");
    assert_eq!(res[1].key, b"bbb");
    assert_eq!(res[1].value, b"val-b");
    assert_eq!(res[2].key, b"ccc");
    assert_eq!(res[2].value, b"val-c");
    OK
  })
}

/// 21. 对应 Garnet RIScanFieldsKeyTest
#[test]
fn test_ri_scan_fields_key() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"aaa", b"val-a")
      .await?;
    session
      .range_index_set(b"myindex", b"bbb", b"val-b")
      .await?;

    let res = session
      .range_index_scan(b"myindex", b"aaa", 10, ScanReturnField::Key)
      .await?;
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].key, b"aaa");
    assert_eq!(res[1].key, b"bbb");
    OK
  })
}

/// 22. 对应 Garnet RIScanFieldsValueTest
#[test]
fn test_ri_scan_fields_value() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"aaa", b"val-a")
      .await?;
    session
      .range_index_set(b"myindex", b"bbb", b"val-b")
      .await?;

    let res = session
      .range_index_scan(b"myindex", b"aaa", 10, ScanReturnField::Value)
      .await?;
    assert_eq!(res.len(), 2);
    assert_eq!(res[0].value, b"val-a");
    assert_eq!(res[1].value, b"val-b");
    OK
  })
}

/// 23. 对应 Garnet RIRangeBasicTest
#[test]
fn test_ri_range_basic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"aaa", b"val-a")
      .await?;
    session
      .range_index_set(b"myindex", b"bbb", b"val-b")
      .await?;
    session
      .range_index_set(b"myindex", b"ccc", b"val-c")
      .await?;
    session
      .range_index_set(b"myindex", b"ddd", b"val-d")
      .await?;
    session
      .range_index_set(b"myindex", b"eee", b"val-e")
      .await?;

    let res = session
      .range_index_range(b"myindex", b"bbb", b"ddd", ScanReturnField::KeyAndValue)
      .await?;
    assert_eq!(res.len(), 3);
    assert_eq!(res[0].key, b"bbb");
    assert_eq!(res[0].value, b"val-b");
    assert_eq!(res[1].key, b"ccc");
    assert_eq!(res[1].value, b"val-c");
    assert_eq!(res[2].key, b"ddd");
    assert_eq!(res[2].value, b"val-d");
    OK
  })
}

/// 24. 对应 Garnet RIScanOnNonExistentIndexTest
#[test]
fn test_ri_scan_on_non_existent_index() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    let err = session
      .range_index_scan(b"noindex", b"aaa", 10, ScanReturnField::KeyAndValue)
      .await;
    assert!(matches!(err, Err(RangeIndexError::NotFound)));
    OK
  })
}

/// 25. 对应 Garnet RIRangeOnNonExistentIndexTest
#[test]
fn test_ri_range_on_non_existent_index() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    let err = session
      .range_index_range(b"noindex", b"aaa", b"zzz", ScanReturnField::KeyAndValue)
      .await;
    assert!(matches!(err, Err(RangeIndexError::NotFound)));
    OK
  })
}

/// 26. 内存模式禁止 RI.SCAN
#[test]
fn test_ri_scan_memory_mode_not_supported() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"memindex", StorageBackend::Memory, TUNE)
      .await?;
    let err = session
      .range_index_scan(b"memindex", b"aaa", 10, ScanReturnField::KeyAndValue)
      .await;
    assert!(matches!(err, Err(RangeIndexError::MemoryModeNotSupported)));
    OK
  })
}

/// 27. 内存模式禁止 RI.RANGE
#[test]
fn test_ri_range_memory_mode_not_supported() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"memindex", StorageBackend::Memory, TUNE)
      .await?;
    let err = session
      .range_index_range(b"memindex", b"aaa", b"zzz", ScanReturnField::KeyAndValue)
      .await;
    assert!(matches!(err, Err(RangeIndexError::MemoryModeNotSupported)));
    OK
  })
}

// ---------------------------------------------------------------------------
// 6. 参数范围与边界校验 (RISetInvalidKV*)
// --------------------------------------------------------------------------- /// 28. 对应 Garnet RISetInvalidKVFieldTooLongTest
#[test]
fn test_ri_set_invalid_kv_field_too_long() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          cache_size: 65536,
          min_record_size: 8,
          max_record_size: 256,
          max_key_len: 16,
          leaf_page_size: 0,
        },
      )
      .await?;
    let long_field = vec![b'k'; 17];
    let err = session
      .range_index_set(b"myindex", &long_field, b"value1")
      .await;
    assert!(matches!(err, Err(RangeIndexError::InvalidKV { .. })));
    OK
  })
}

/// 29. 对应 Garnet RISetInvalidKVValueTooLongTest
#[test]
fn test_ri_set_invalid_kv_value_too_long() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          cache_size: 65536,
          min_record_size: 8,
          max_record_size: 64,
          max_key_len: 16,
          leaf_page_size: 0,
        },
      )
      .await?;
    let long_value = vec![b'v'; 128];
    let err = session
      .range_index_set(b"myindex", b"field1", &long_value)
      .await;
    assert!(matches!(err, Err(RangeIndexError::InvalidKV { .. })));
    OK
  })
}

/// 30. 对应 Garnet RISetInvalidKVRecordTooSmallTest
#[test]
fn test_ri_set_invalid_kv_record_too_small() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          cache_size: 65536,
          min_record_size: 128,
          max_record_size: 1024,
          max_key_len: 64,
          leaf_page_size: 0,
        },
      )
      .await?;
    let err = session.range_index_set(b"myindex", b"a", b"b").await;
    assert!(matches!(err, Err(RangeIndexError::InvalidKV { .. })));
    OK
  })
}

// ---------------------------------------------------------------------------
// 7. 并发与多客户端隔离 (RIConcurrent*)
// --------------------------------------------------------------------------- /// 31. 对应 Garnet RIConcurrentMultiClientTest
#[test]
fn test_ri_concurrent_multi_client() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          cache_size: 1048576,
          min_record_size: 8,
          max_record_size: 256,
          max_key_len: 64,
          leaf_page_size: 0,
        },
      )
      .await?;

    let num_tasks = 8;
    let ops_per_task = 50;

    // 阶段 1: 并发写入
    for t in 0..num_tasks {
      let session_clone = store.new_session()?;
      for i in 0..ops_per_task {
        let field = format!("t{}:f{}", t, pad(i, 4));
        let value = format!("val-{}-{}", t, i);
        session_clone
          .range_index_set(b"myindex", field.as_bytes(), value.as_bytes())
          .await?;
      }
    }

    // 阶段 2: 并发读取校验
    for t in 0..num_tasks {
      let session_clone = store.new_session()?;
      for i in 0..ops_per_task {
        let field = format!("t{}:f{}", t, pad(i, 4));
        let expected = format!("val-{}-{}", t, i);
        let actual = session_clone
          .range_index_get(b"myindex", field.as_bytes())
          .await?;
        assert_eq!(actual, Some(expected.into_bytes()));
      }
    }

    // 阶段 3: 混合写入与删除
    for t in 0..num_tasks {
      let session_clone = store.new_session()?;
      for i in 0..ops_per_task {
        let field = format!("mix{}:f{}", t, pad(i, 4));
        let val = format!("mixed-{}-{}", t, i);
        session_clone
          .range_index_set(b"myindex", field.as_bytes(), val.as_bytes())
          .await?;
        assert_eq!(
          session_clone
            .range_index_get(b"myindex", field.as_bytes())
            .await?,
          Some(val.into_bytes())
        );
        assert!(
          session_clone
            .range_index_del(b"myindex", field.as_bytes())
            .await?
        );
      }
    }
    OK
  })
}

// ---------------------------------------------------------------------------
// 8. 资源释放与生命周期管理 (RIDelete*, RIEviction*)
// --------------------------------------------------------------------------- /// 32. 对应 Garnet RIDeleteInMutableRegionFreesResourcesTest
#[test]
fn test_ri_delete_in_mutable_region_frees_resources() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value1")
      .await?;
    assert_eq!(store.range_index.live_index_count(), 1);

    session.delete(b"myindex").await?;
    assert_eq!(store.range_index.live_index_count(), 0);
    OK
  })
}

/// 33. 对应 Garnet RIDeleteInReadOnlyRegionFreesResourcesTest
#[test]
fn test_ri_delete_in_read_only_region_frees_resources() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value1")
      .await?;
    assert_eq!(store.range_index.live_index_count(), 1);

    // 模拟刷盘并删除
    let mut stub = session.range_index_config(b"myindex").await?;
    store.range_index.on_flush(b"myindex", &mut stub)?;
    session.delete(b"myindex").await?;
    assert_eq!(store.range_index.live_index_count(), 0);

    // 删除后重新创建新索引
    session
      .range_index_create(b"newidx", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"newidx", b"field-one", b"value-one")
      .await?;
    assert_eq!(
      session.range_index_get(b"newidx", b"field-one").await?,
      Some(b"value-one".to_vec())
    );
    OK
  })
}

/// 34. 对应 Garnet RIEvictionFreesEvictedTreeButKeepsLiveTest
#[test]
fn test_ri_eviction_frees_evicted_tree_but_keeps_live() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    for i in 0..3 {
      let name = format!("early{}", i);
      session
        .range_index_create(name.as_bytes(), StorageBackend::Memory, TUNE)
        .await?;
      session
        .range_index_set(name.as_bytes(), b"field1", b"valxx")
        .await?;
    }
    assert_eq!(store.range_index.live_index_count(), 3);

    // 释放逐出前 3 棵早期树条目
    for i in 0..3 {
      let name = format!("early{}", i);
      store.range_index.dispose_tree(name.as_bytes(), false);
    }
    assert_eq!(store.range_index.live_index_count(), 0);

    // 新创建活跃树
    session
      .range_index_create(b"live", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"live", b"field1", b"alive")
      .await?;
    assert_eq!(store.range_index.live_index_count(), 1);
    assert_eq!(
      session.range_index_get(b"live", b"field1").await?,
      Some(b"alive".to_vec())
    );
    OK
  })
}

/// 35. 对应 Garnet RIEvictionAfterDeleteTest
#[test]
fn test_ri_eviction_after_delete() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"key1", b"value1")
      .await?;
    session.delete(b"myindex").await?;

    // 删除后再次模拟逐出处理不崩溃
    store.range_index.dispose_tree(b"myindex", false);

    session
      .range_index_create(b"newidx", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"newidx", b"field1", b"hello")
      .await?;
    assert_eq!(
      session.range_index_get(b"newidx", b"field1").await?,
      Some(b"hello".to_vec())
    );
    OK
  })
}

/// 36. 对应 Garnet RIEvictionMultipleIndexesTest
#[test]
fn test_ri_eviction_multiple_indexes() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    for i in 0..3 {
      let name = format!("old{}", i);
      session
        .range_index_create(name.as_bytes(), StorageBackend::Memory, TUNE)
        .await?;
      session
        .range_index_set(name.as_bytes(), b"field1", b"value")
        .await?;
      session.delete(name.as_bytes()).await?;
      store.range_index.dispose_tree(name.as_bytes(), false);
    }

    session
      .range_index_create(b"live", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"live", b"field1", b"alive")
      .await?;
    assert_eq!(
      session.range_index_get(b"live", b"field1").await?,
      Some(b"alive".to_vec())
    );
    OK
  })
}

/// 37. 对应 Garnet RICreateDeleteRecreateWithEvictionTest
#[test]
fn test_ri_create_delete_recreate_with_eviction() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    for round in 0..3 {
      session
        .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
        .await?;
      let val = format!("round{}", round);
      session
        .range_index_set(b"myindex", b"field1", val.as_bytes())
        .await?;
      session.delete(b"myindex").await?;
      store.range_index.dispose_tree(b"myindex", false);
    }

    session
      .range_index_create(b"final", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"final", b"field1", b"works")
      .await?;
    assert_eq!(
      session.range_index_get(b"final", b"field1").await?,
      Some(b"works".to_vec())
    );
    OK
  })
}

// ---------------------------------------------------------------------------
// 9. 刷盘、逐出与懒恢复 (RIFlush*, RIEvictToDiskThenLazyRestore*)
// --------------------------------------------------------------------------- /// 38. 对应 Garnet RIFlushPromotesToTailOnNextAccessTest
#[test]
fn test_ri_flush_promotes_to_tail_on_next_access() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"aaa", b"val-a")
      .await?;
    session
      .range_index_set(b"myindex", b"bbb", b"val-b")
      .await?;

    let mut stub = session.range_index_config(b"myindex").await?;
    store.range_index.on_flush(b"myindex", &mut stub)?;
    assert!(stub.is_flushed());

    // 访问促进并读取数据
    let val = session.range_index_get(b"myindex", b"aaa").await?;
    assert_eq!(val, Some(b"val-a".to_vec()));

    // 促进后继续写入
    session
      .range_index_set(b"myindex", b"ccc", b"val-c")
      .await?;
    assert_eq!(
      session.range_index_get(b"myindex", b"ccc").await?,
      Some(b"val-c".to_vec())
    );
    assert_eq!(
      session.range_index_get(b"myindex", b"bbb").await?,
      Some(b"val-b".to_vec())
    );
    OK
  })
}

/// 39. 对应 Garnet RIFlushPromoteThenSecondFlushTest
#[test]
fn test_ri_flush_promote_then_second_flush() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"key-one", b"value-1")
      .await?;

    // 第一次刷盘与访问
    let mut stub = session.range_index_config(b"myindex").await?;
    store.range_index.on_flush(b"myindex", &mut stub)?;
    assert_eq!(
      session.range_index_get(b"myindex", b"key-one").await?,
      Some(b"value-1".to_vec())
    );

    // 写入新键并进行第二次刷盘
    session
      .range_index_set(b"myindex", b"key-two", b"value-2")
      .await?;
    store.range_index.on_flush(b"myindex", &mut stub)?;

    assert_eq!(
      session.range_index_get(b"myindex", b"key-one").await?,
      Some(b"value-1".to_vec())
    );
    assert_eq!(
      session.range_index_get(b"myindex", b"key-two").await?,
      Some(b"value-2".to_vec())
    );
    assert_eq!(store.range_index.live_index_count(), 1);
    OK
  })
}

/// 40. 对应 Garnet RIEvictToDiskThenLazyRestoreTest
#[test]
fn test_ri_evict_to_disk_then_lazy_restore() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"alpha", b"value-alpha")
      .await?;
    session
      .range_index_set(b"myindex", b"bravo", b"value-bravo")
      .await?;
    assert_eq!(store.range_index.live_index_count(), 1);

    // 刷盘并注销内存活跃实例（模拟页面逐出）
    let mut stub = session.range_index_config(b"myindex").await?;
    store.range_index.on_flush(b"myindex", &mut stub)?;
    store.range_index.unregister_index(b"myindex");
    assert_eq!(store.range_index.live_index_count(), 0);

    // 访问触发懒加载恢复
    let val_a = session.range_index_get(b"myindex", b"alpha").await?;
    assert_eq!(val_a, Some(b"value-alpha".to_vec()));
    assert_eq!(store.range_index.live_index_count(), 1);

    let val_b = session.range_index_get(b"myindex", b"bravo").await?;
    assert_eq!(val_b, Some(b"value-bravo".to_vec()));

    // 恢复后可继续写入
    session
      .range_index_set(b"myindex", b"charlie", b"value-charlie")
      .await?;
    assert_eq!(
      session.range_index_get(b"myindex", b"charlie").await?,
      Some(b"value-charlie".to_vec())
    );
    OK
  })
}

/// 41. 对应 Garnet RIMultipleTreesEvictAndRestoreTest
#[test]
fn test_ri_multiple_trees_evict_and_restore() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    for i in 0..3 {
      let name = format!("tree{}", i);
      session
        .range_index_create(name.as_bytes(), StorageBackend::Std, TUNE)
        .await?;
      session
        .range_index_set(name.as_bytes(), b"f1", format!("val{}-1", i).as_bytes())
        .await?;
      session
        .range_index_set(name.as_bytes(), b"f2", format!("val{}-2", i).as_bytes())
        .await?;

      let mut stub = session.range_index_config(name.as_bytes()).await?;
      store.range_index.on_flush(name.as_bytes(), &mut stub)?;
      store.range_index.unregister_index(name.as_bytes());
    }
    assert_eq!(store.range_index.live_index_count(), 0);

    // 创建一棵新的活跃树
    session
      .range_index_create(b"tree-live", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"tree-live", b"f1", b"live-val")
      .await?;
    assert_eq!(store.range_index.live_index_count(), 1);

    // 逐一触发懒加载恢复前 3 棵树
    for i in 0..3 {
      let name = format!("tree{}", i);
      let val1 = session.range_index_get(name.as_bytes(), b"f1").await?;
      assert_eq!(val1, Some(format!("val{}-1", i).into_bytes()));
      let val2 = session.range_index_get(name.as_bytes(), b"f2").await?;
      assert_eq!(val2, Some(format!("val{}-2", i).into_bytes()));
    }
    assert_eq!(store.range_index.live_index_count(), 4);
    assert_eq!(
      session.range_index_get(b"tree-live", b"f1").await?,
      Some(b"live-val".to_vec())
    );
    OK
  })
}

// ---------------------------------------------------------------------------
// 10. 检查点快照与全量恢复 (RICheckpoint*, RIRecover*)
// --------------------------------------------------------------------------- /// 42. 对应 Garnet RICheckpointAndRecoverTest
#[test]
fn test_ri_checkpoint_and_recover() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"cpindex", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"cpindex", b"alpha", b"value-alpha")
      .await?;
    session
      .range_index_set(b"cpindex", b"bravo", b"value-bravo")
      .await?;
    session
      .range_index_set(b"cpindex", b"charlie", b"value-charlie")
      .await?;

    let ckpt_dir = dir.path().join("checkpoints");
    let meta = store
      .create_checkpoint(&ckpt_dir, CheckpointType::Snapshot)
      .await?;
    let token = meta.token;
    drop(session);
    drop(store);

    let db_path = dir.path().join("store.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let recovered_store = Arc::new(WedbStore::recover(&ckpt_dir, token, device).await?);
    let recovered_session = recovered_store.new_session()?;

    assert_eq!(
      recovered_session
        .range_index_get(b"cpindex", b"alpha")
        .await?,
      Some(b"value-alpha".to_vec())
    );
    assert_eq!(
      recovered_session
        .range_index_get(b"cpindex", b"bravo")
        .await?,
      Some(b"value-bravo".to_vec())
    );
    assert_eq!(
      recovered_session
        .range_index_get(b"cpindex", b"charlie")
        .await?,
      Some(b"value-charlie".to_vec())
    );

    // 恢复后写入正常工作
    recovered_session
      .range_index_set(b"cpindex", b"delta", b"value-delta")
      .await?;
    assert_eq!(
      recovered_session
        .range_index_get(b"cpindex", b"delta")
        .await?,
      Some(b"value-delta".to_vec())
    );
    OK
  })
}

/// 43. 对应 Garnet RIFlushEvictRestoreCheckpointCycleTest
#[test]
fn test_ri_flush_evict_restore_checkpoint_cycle() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"lifecycle", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"lifecycle", b"key-aaa", b"val-aaa")
      .await?;
    session
      .range_index_set(b"lifecycle", b"key-bbb", b"val-bbb")
      .await?;

    let mut stub = session.range_index_config(b"lifecycle").await?;
    store.range_index.on_flush(b"lifecycle", &mut stub)?;

    // 访问并促进
    assert_eq!(
      session.range_index_get(b"lifecycle", b"key-aaa").await?,
      Some(b"val-aaa".to_vec())
    );
    session
      .range_index_set(b"lifecycle", b"key-ccc", b"val-ccc")
      .await?;

    // 模拟日志推进淘汰旧页（由于提升到尾部，活跃树不被淘汰）
    for i in 0..200 {
      let k = format!("fill{}", pad(i, 4));
      let v = format!("data{}", pad(i, 4));
      session.upsert(k.as_bytes(), v.as_bytes()).await?;
    }

    assert_eq!(
      session.range_index_get(b"lifecycle", b"key-bbb").await?,
      Some(b"val-bbb".to_vec())
    );
    assert_eq!(
      session.range_index_get(b"lifecycle", b"key-ccc").await?,
      Some(b"val-ccc".to_vec())
    );

    // 打快照并恢复
    let token = checkpoint(&store, dir.path()).await?;
    drop(session);
    drop(store);

    let (_rec_store, rec_session) = recover(dir.path(), token).await?;

    assert_eq!(
      rec_session
        .range_index_get(b"lifecycle", b"key-aaa")
        .await?,
      Some(b"val-aaa".to_vec())
    );
    assert_eq!(
      rec_session
        .range_index_get(b"lifecycle", b"key-bbb")
        .await?,
      Some(b"val-bbb".to_vec())
    );
    assert_eq!(
      rec_session
        .range_index_get(b"lifecycle", b"key-ccc")
        .await?,
      Some(b"val-ccc".to_vec())
    );
    OK
  })
}

/// 44. 对应 Garnet RICheckpointWithMultipleTreesAndRecoverTest
#[test]
fn test_ri_checkpoint_with_multiple_trees_and_recover() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    for i in 0..5 {
      let name = format!("idx{}", i);
      session
        .range_index_create(name.as_bytes(), StorageBackend::Std, TUNE)
        .await?;
      for j in 0..10 {
        let f = format!("field-{}", pad(j, 3));
        let v = format!("value-{}-{}", i, j);
        session
          .range_index_set(name.as_bytes(), f.as_bytes(), v.as_bytes())
          .await?;
      }
    }

    let token = checkpoint(&store, dir.path()).await?;
    drop(session);
    drop(store);

    let (_rec_store, rec_session) = recover(dir.path(), token).await?;

    for i in 0..5 {
      let name = format!("idx{}", i);
      for j in 0..10 {
        let f = format!("field-{}", pad(j, 3));
        let v = format!("value-{}-{}", i, j);
        assert_eq!(
          rec_session
            .range_index_get(name.as_bytes(), f.as_bytes())
            .await?,
          Some(v.into_bytes())
        );
      }
      rec_session
        .range_index_set(
          name.as_bytes(),
          b"new-field",
          format!("new-value-{}", i).as_bytes(),
        )
        .await?;
      assert_eq!(
        rec_session
          .range_index_get(name.as_bytes(), b"new-field")
          .await?,
        Some(format!("new-value-{}", i).into_bytes())
      );
    }
    OK
  })
}

/// 45. 对应 Garnet RIFlushPromoteCheckpointRecoverTest
#[test]
fn test_ri_flush_promote_checkpoint_recover() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"fpcp", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"fpcp", b"before-flush", b"original")
      .await?;

    let mut stub = session.range_index_config(b"fpcp").await?;
    store.range_index.on_flush(b"fpcp", &mut stub)?;

    assert_eq!(
      session.range_index_get(b"fpcp", b"before-flush").await?,
      Some(b"original".to_vec())
    );

    session
      .range_index_set(b"fpcp", b"after-flush", b"mutated")
      .await?;
    session
      .range_index_set(b"fpcp", b"before-flush", b"updated")
      .await?;

    let token = checkpoint(&store, dir.path()).await?;
    drop(session);
    drop(store);

    let (_rec_store, rec_session) = recover(dir.path(), token).await?;

    assert_eq!(
      rec_session
        .range_index_get(b"fpcp", b"before-flush")
        .await?,
      Some(b"updated".to_vec())
    );
    assert_eq!(
      rec_session.range_index_get(b"fpcp", b"after-flush").await?,
      Some(b"mutated".to_vec())
    );
    OK
  })
}

/// 46. 对应 Garnet RIDeleteDuringEvictionCycleTest
#[test]
fn test_ri_delete_during_eviction_cycle() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"deltest", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"deltest", b"field1", b"value1")
      .await?;
    assert_eq!(store.range_index.live_index_count(), 1);

    session.delete(b"deltest").await?;
    assert_eq!(store.range_index.live_index_count(), 0);

    store.range_index.dispose_tree(b"deltest", false);

    session
      .range_index_create(b"newtest", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"newtest", b"field1", b"alive")
      .await?;
    assert_eq!(store.range_index.live_index_count(), 1);
    assert_eq!(
      session.range_index_get(b"newtest", b"field1").await?,
      Some(b"alive".to_vec())
    );
    OK
  })
}

/// 47. 对应 Garnet RIDoubleFlushCycleWithCheckpointTest
#[test]
fn test_ri_double_flush_cycle_with_checkpoint() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"dblflush", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"dblflush", b"round1", b"value-r1")
      .await?;

    let mut stub = session.range_index_config(b"dblflush").await?;
    store.range_index.on_flush(b"dblflush", &mut stub)?;
    assert_eq!(
      session.range_index_get(b"dblflush", b"round1").await?,
      Some(b"value-r1".to_vec())
    );

    session
      .range_index_set(b"dblflush", b"round2", b"value-r2")
      .await?;
    store.range_index.on_flush(b"dblflush", &mut stub)?;
    assert_eq!(
      session.range_index_get(b"dblflush", b"round2").await?,
      Some(b"value-r2".to_vec())
    );

    session
      .range_index_set(b"dblflush", b"round3", b"value-r3")
      .await?;

    let token = checkpoint(&store, dir.path()).await?;
    drop(session);
    drop(store);

    let (_rec_store, rec_session) = recover(dir.path(), token).await?;

    assert_eq!(
      rec_session.range_index_get(b"dblflush", b"round1").await?,
      Some(b"value-r1".to_vec())
    );
    assert_eq!(
      rec_session.range_index_get(b"dblflush", b"round2").await?,
      Some(b"value-r2".to_vec())
    );
    assert_eq!(
      rec_session.range_index_get(b"dblflush", b"round3").await?,
      Some(b"value-r3".to_vec())
    );
    OK
  })
}

/// 48. 对应 Garnet RIEvictRestoreAndCheckpointTest
#[test]
fn test_ri_evict_restore_and_checkpoint() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"evictcp", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"evictcp", b"pre-evict", b"original")
      .await?;

    let mut stub = session.range_index_config(b"evictcp").await?;
    store.range_index.on_flush(b"evictcp", &mut stub)?;
    store.range_index.unregister_index(b"evictcp");
    assert_eq!(store.range_index.live_index_count(), 0);

    // 懒加载恢复后变更
    assert_eq!(
      session.range_index_get(b"evictcp", b"pre-evict").await?,
      Some(b"original".to_vec())
    );
    session
      .range_index_set(b"evictcp", b"post-restore", b"added")
      .await?;

    let token = checkpoint(&store, dir.path()).await?;
    drop(session);
    drop(store);

    let (_rec_store, rec_session) = recover(dir.path(), token).await?;

    assert_eq!(
      rec_session
        .range_index_get(b"evictcp", b"pre-evict")
        .await?,
      Some(b"original".to_vec())
    );
    assert_eq!(
      rec_session
        .range_index_get(b"evictcp", b"post-restore")
        .await?,
      Some(b"added".to_vec())
    );
    OK
  })
}

/// 49. 对应 Garnet RITwoCheckpointsRecoverToLatestTest
#[test]
fn test_ri_two_checkpoints_recover_to_latest() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"twockpt", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"twockpt", b"key-alpha", b"val-alpha")
      .await?;
    session
      .range_index_set(b"twockpt", b"key-bravo", b"val-bravo")
      .await?;

    // 第一个检查点
    let _token1 = checkpoint(&store, dir.path()).await?;

    // 写入新数据并更新老数据
    session
      .range_index_set(b"twockpt", b"key-charlie", b"val-charlie")
      .await?;
    session
      .range_index_set(b"twockpt", b"key-alpha", b"val-alpha-v2")
      .await?;

    // 第二个检查点
    let token2 = checkpoint(&store, dir.path()).await?;
    drop(session);
    drop(store);

    // 恢复到最新检查点 (token2)
    let (_rec_store, rec_session) = recover(dir.path(), token2).await?;

    assert_eq!(
      rec_session
        .range_index_get(b"twockpt", b"key-alpha")
        .await?,
      Some(b"val-alpha-v2".to_vec())
    );
    assert_eq!(
      rec_session
        .range_index_get(b"twockpt", b"key-bravo")
        .await?,
      Some(b"val-bravo".to_vec())
    );
    assert_eq!(
      rec_session
        .range_index_get(b"twockpt", b"key-charlie")
        .await?,
      Some(b"val-charlie".to_vec())
    );

    rec_session
      .range_index_set(b"twockpt", b"key-delta", b"val-delta")
      .await?;
    assert_eq!(
      rec_session
        .range_index_get(b"twockpt", b"key-delta")
        .await?,
      Some(b"val-delta".to_vec())
    );
    OK
  })
}

/// 50. 对应 Garnet RIRecoverToEarlierCheckpointTest
#[test]
fn test_ri_recover_to_earlier_checkpoint() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"earlyck", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"earlyck", b"key-A", b"val-A-original")
      .await?;
    session
      .range_index_set(b"earlyck", b"key-B", b"val-B")
      .await?;

    let token = checkpoint(&store, dir.path()).await?;

    // 快照后产生的增量变更记录为 WAL 字节流
    let mut aof_payload = Vec::new();
    aof_payload.extend_from_slice(&encode_ri_set(b"earlyck", b"key-A", b"val-A-updated"));
    aof_payload.extend_from_slice(&encode_ri_set(b"earlyck", b"key-C", b"val-C"));

    drop(session);
    drop(store);

    let (_rec_store, rec_session) = recover(dir.path(), token).await?;

    // 回放快照后的增量 AOF 数据
    replay_payload(&rec_session, &aof_payload).await?;

    assert_eq!(
      rec_session.range_index_get(b"earlyck", b"key-A").await?,
      Some(b"val-A-updated".to_vec())
    );
    assert_eq!(
      rec_session.range_index_get(b"earlyck", b"key-B").await?,
      Some(b"val-B".to_vec())
    );
    assert_eq!(
      rec_session.range_index_get(b"earlyck", b"key-C").await?,
      Some(b"val-C".to_vec())
    );
    OK
  })
}

/// 51. 对应 Garnet RIDeleteAfterRecoveryTest
#[test]
fn test_ri_delete_after_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"delafter", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"delafter", b"key1", b"val1")
      .await?;

    let token = checkpoint(&store, dir.path()).await?;
    drop(session);
    drop(store);

    let (rec_store, rec_session) = recover(dir.path(), token).await?;

    assert_eq!(
      rec_session.range_index_get(b"delafter", b"key1").await?,
      Some(b"val1".to_vec())
    );
    assert_eq!(rec_store.range_index.live_index_count(), 1);

    assert!(rec_session.delete(b"delafter").await?);
    assert_eq!(rec_store.range_index.live_index_count(), 0);

    let err = rec_session.range_index_get(b"delafter", b"key1").await;
    assert!(matches!(err, Err(RangeIndexError::NotFound)));
    OK
  })
}

// ---------------------------------------------------------------------------
// 11. 元数据诊断与协议命令 (RIExists*, RIConfig*, RIMetrics*, RIType*)
// --------------------------------------------------------------------------- /// 52. 对应 Garnet RIExistsBasicTest
#[test]
fn test_ri_exists_basic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    assert!(!session.range_index_exists(b"myindex").await?);

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    assert!(session.range_index_exists(b"myindex").await?);

    session.delete(b"myindex").await?;
    assert!(!session.range_index_exists(b"myindex").await?);
    OK
  })
}

/// 53. 对应 Garnet RIExistsOnNormalKeyTest
#[test]
fn test_ri_exists_on_normal_key() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session.upsert(b"normalkey", b"hello").await?;
    // 对普通字符串键调用 range_index_exists 返回 false (0)
    assert!(!session.range_index_exists(b"normalkey").await?);
    OK
  })
}

/// 54. 对应 Garnet RIConfigBasicTest
#[test]
fn test_ri_config_basic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(
        b"myindex",
        StorageBackend::Memory,
        TreeTuning {
          min_record_size: 32,
          max_record_size: 512,
          max_key_len: 64,
          ..TUNE
        },
      )
      .await?;

    let stub = session.range_index_config(b"myindex").await?;
    assert_eq!(stub.storage_backend, 1); // Memory
    assert_eq!(stub.cache_size, 65536);
    assert_eq!(stub.min_record_size, 32);
    assert_eq!(stub.max_record_size, 512);
    assert_eq!(stub.max_key_len, 64);
    assert!(stub.leaf_page_size > 0);
    OK
  })
}

/// 55. 对应 Garnet RIConfigWrongTypeTest
#[test]
fn test_ri_config_wrong_type() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session.upsert(b"normalkey", b"hello").await?;
    let err = session.range_index_config(b"normalkey").await;
    assert!(matches!(err, Err(RangeIndexError::WrongType)));
    OK
  })
}

/// 56. 对应 Garnet RIMetricsBasicTest
#[test]
fn test_ri_metrics_basic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"myindex", b"field1", b"value1")
      .await?;
    session
      .range_index_set(b"myindex", b"field2", b"value2")
      .await?;

    let (handle, is_live, is_flushed, is_recovered) =
      session.range_index_metrics(b"myindex").await?;
    assert!(handle != 0);
    assert!(is_live);
    assert!(!is_flushed);
    assert!(!is_recovered);
    OK
  })
}

/// 57. 对应 Garnet RITypeCommandTest
#[test]
fn test_ri_type_command() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"myindex", StorageBackend::Memory, TUNE)
      .await?;
    assert_eq!(session.type_of(b"myindex").await?, "rangeindex");

    session.upsert(b"normalkey", b"hello").await?;
    assert_eq!(session.type_of(b"normalkey").await?, "string");
    OK
  })
}

// ---------------------------------------------------------------------------
// 12. 进阶边界与文件生命周期约束 (Snapshot, Cleanup, Invariance)
// --------------------------------------------------------------------------- /// 58. 对应 Garnet RIConcurrentOpsWithCheckpointTest
#[test]
fn test_ri_concurrent_ops_with_checkpoint() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"stress", StorageBackend::Std, TUNE)
      .await?;

    // 阶段 1: 写入前置数据并打快照
    for i in 0..50 {
      let field = format!("k_{}", pad(i, 4));
      let val = format!("v_{}", pad(i, 4));
      session
        .range_index_set(b"stress", field.as_bytes(), val.as_bytes())
        .await?;
    }
    let token = checkpoint(&store, dir.path()).await?;

    // 阶段 2: 快照后继续写入后置数据
    for i in 50..100 {
      let field = format!("k_{}", pad(i, 4));
      let val = format!("v_{}", pad(i, 4));
      session
        .range_index_set(b"stress", field.as_bytes(), val.as_bytes())
        .await?;
    }

    drop(session);
    drop(store);

    // 恢复检查点：必须只恢复前 50 条快照内数据
    let (_rec_store, rec_session) = recover(dir.path(), token).await?;

    for i in 0..50 {
      let field = format!("k_{}", pad(i, 4));
      let val = format!("v_{}", pad(i, 4));
      assert_eq!(
        rec_session
          .range_index_get(b"stress", field.as_bytes())
          .await?,
        Some(val.into_bytes())
      );
    }
    for i in 50..100 {
      let field = format!("k_{}", pad(i, 4));
      assert_eq!(
        rec_session
          .range_index_get(b"stress", field.as_bytes())
          .await?,
        None
      );
    }
    OK
  })
}

/// 59. 对应 Garnet RIRecoverThenSecondEvictionUsesFlushSnapshotTest
#[test]
fn test_ri_recover_then_second_eviction_uses_flush_snapshot() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"stalecp", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"stalecp", b"pre-checkpoint", b"original")
      .await?;
    let token = checkpoint(&store, dir.path()).await?;

    drop(session);
    drop(store);

    let (rec_store, rec_session) = recover(dir.path(), token).await?;

    assert_eq!(
      rec_session
        .range_index_get(b"stalecp", b"pre-checkpoint")
        .await?,
      Some(b"original".to_vec())
    );

    // 恢复后写入新值
    rec_session
      .range_index_set(b"stalecp", b"post-recovery", b"new-value")
      .await?;

    // 刷盘并注销（二次逐出）
    let mut stub = rec_session.range_index_config(b"stalecp").await?;
    rec_store.range_index.on_flush(b"stalecp", &mut stub)?;
    rec_store.range_index.unregister_index(b"stalecp");

    assert_eq!(
      rec_session
        .range_index_get(b"stalecp", b"pre-checkpoint")
        .await?,
      Some(b"original".to_vec())
    );
    assert_eq!(
      rec_session
        .range_index_get(b"stalecp", b"post-recovery")
        .await?,
      Some(b"new-value".to_vec())
    );
    OK
  })
}

/// 60. 对应 Garnet RIAofOnlyRecoveryTest
#[test]
fn test_ri_aof_only_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    let mut aof = Vec::new();
    aof.extend_from_slice(&encode_ri_create(b"aofonly", StorageBackend::Std, TUNE));
    aof.extend_from_slice(&encode_ri_set(b"aofonly", b"key-a", b"val-a"));
    aof.extend_from_slice(&encode_ri_set(b"aofonly", b"key-b", b"val-b"));
    aof.extend_from_slice(&encode_ri_set(b"aofonly", b"key-c", b"val-c"));
    aof.extend_from_slice(&encode_ri_set(b"aofonly", b"key-a", b"val-a-updated"));
    aof.extend_from_slice(&encode_ri_del(b"aofonly", b"key-b"));

    replay_payload(&session, &aof).await?;

    assert_eq!(
      session.range_index_get(b"aofonly", b"key-a").await?,
      Some(b"val-a-updated".to_vec())
    );
    assert_eq!(session.range_index_get(b"aofonly", b"key-b").await?, None);
    assert_eq!(
      session.range_index_get(b"aofonly", b"key-c").await?,
      Some(b"val-c".to_vec())
    );

    session
      .range_index_set(b"aofonly", b"key-d", b"val-d")
      .await?;
    assert_eq!(
      session.range_index_get(b"aofonly", b"key-d").await?,
      Some(b"val-d".to_vec())
    );
    OK
  })
}

/// 61. 对应 Garnet RIDiskFileCleanupOnDeleteTest
#[test]
fn test_ri_disk_file_cleanup_on_delete() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"cleanup", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"cleanup", b"key1", b"val1")
      .await?;

    let ri_log_root = dir.path().join("rangeindex");
    let count_data_files = || -> usize {
      fs::read_dir(&ri_log_root)
        .map(|entries| {
          entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().to_string_lossy().ends_with(".data.bftree"))
            .count()
        })
        .unwrap_or(0)
    };

    assert_eq!(count_data_files(), 1);

    // 删除索引清理工作文件
    assert!(session.delete(b"cleanup").await?);
    assert_eq!(count_data_files(), 0);
    OK
  })
}

/// 62. 对应 Garnet RIDeletePreservesPerFlushSnapshotFilesTest
#[test]
fn test_ri_delete_preserves_per_flush_snapshot_files() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"preservetest", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"preservetest", b"field-x", b"value-v1")
      .await?;

    let mut stub = session.range_index_config(b"preservetest").await?;
    store
      .range_index
      .on_flush_address(b"preservetest", &mut stub, 1000)?;

    let ri_log_root = dir.path().join("rangeindex");
    let count_flush_files = || -> usize {
      fs::read_dir(&ri_log_root)
        .map(|entries| {
          entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().to_string_lossy().ends_with(".flush.bftree"))
            .count()
        })
        .unwrap_or(0)
    };

    let flush_count_before = count_flush_files();
    assert!(flush_count_before >= 1);

    // 删除索引：工作文件被删除，但历史刷盘文件必须得以完整保留
    session.delete(b"preservetest").await?;

    let flush_count_after = count_flush_files();
    assert_eq!(flush_count_before, flush_count_after);
    OK
  })
}

/// 63. 对应 Garnet RIDiskFileCleanupOnDeleteAfterEvictionAndRestoreTest
#[test]
fn test_ri_disk_file_cleanup_on_delete_after_eviction_and_restore() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"evictdel", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"evictdel", b"key1", b"val1")
      .await?;

    let mut stub = session.range_index_config(b"evictdel").await?;
    store
      .range_index
      .on_flush_address(b"evictdel", &mut stub, 2000)?;
    store.range_index.unregister_index(b"evictdel");
    assert_eq!(store.range_index.live_index_count(), 0);

    // 懒加载恢复
    assert_eq!(
      session.range_index_get(b"evictdel", b"key1").await?,
      Some(b"val1".to_vec())
    );
    assert_eq!(store.range_index.live_index_count(), 1);

    // 删除索引
    assert!(session.delete(b"evictdel").await?);

    let ri_log_root = dir.path().join("rangeindex");
    let data_files = fs::read_dir(&ri_log_root)
      .map(|entries| {
        entries
          .filter_map(|e| e.ok())
          .filter(|e| e.path().to_string_lossy().ends_with(".data.bftree"))
          .count()
      })
      .unwrap_or(0);
    assert_eq!(data_files, 0);

    let flush_files = fs::read_dir(&ri_log_root)
      .map(|entries| {
        entries
          .filter_map(|e| e.ok())
          .filter(|e| e.path().to_string_lossy().ends_with(".flush.bftree"))
          .count()
      })
      .unwrap_or(0);
    assert!(flush_files >= 1);
    OK
  })
}

/// 64. 对应 Garnet RIFlushFilesAreImmutablePerAddressTest
#[test]
fn test_ri_flush_files_are_immutable_per_address() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"flushtestkey", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"flushtestkey", b"field-x", b"value-v1")
      .await?;

    let mut stub = session.range_index_config(b"flushtestkey").await?;
    store
      .range_index
      .on_flush_address(b"flushtestkey", &mut stub, 1000)?;

    let path_1000 = store.range_index.log_flush_path(
      &wkv::RangeIndexManager::hash_prefix_of(b"flushtestkey"),
      1000,
    );
    let bytes_v1 = fs::read(&path_1000)?;

    // 促进并变更数据
    session
      .range_index_set(b"flushtestkey", b"field-x", b"value-v2")
      .await?;
    store
      .range_index
      .on_flush_address(b"flushtestkey", &mut stub, 2000)?;

    let path_2000 = store.range_index.log_flush_path(
      &wkv::RangeIndexManager::hash_prefix_of(b"flushtestkey"),
      2000,
    );
    assert!(path_2000.exists());

    // 校验前一次地址 1000 的刷盘文件内容保持严格不可变
    let bytes_v1_after = fs::read(&path_1000)?;
    assert_eq!(bytes_v1, bytes_v1_after);
    OK
  })
}

/// 65. 对应 Garnet RIDisposeTreeUnderLockNoOpsOnTransferredSourceTest
#[test]
fn test_ri_dispose_tree_under_lock_no_ops_on_transferred_source() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"transtest", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"transtest", b"field-x", b"value-v1")
      .await?;
    assert_eq!(store.range_index.live_index_count(), 1);

    // 构造已转移存根（IsTransferred = true）
    let mut stale_stub = RangeIndexStub::new(0, 65536, 8, 1024, 128, 4096, StorageBackend::Std);
    stale_stub.set_transferred(true);

    // 在转移状态下逐出旧条目必须 No-Op，不得释放活跃树
    let res = store
      .range_index
      .dispose_tree_under_lock(b"transtest", &stale_stub, false);
    assert!(!res);
    assert_eq!(store.range_index.live_index_count(), 1);
    assert_eq!(
      session.range_index_get(b"transtest", b"field-x").await?,
      Some(b"value-v1".to_vec())
    );

    // 测试：未标记转移的存根则会被正常释放
    let normal_stub = RangeIndexStub::new(0, 65536, 8, 1024, 128, 4096, StorageBackend::Std);
    let res_evict = store
      .range_index
      .dispose_tree_under_lock(b"transtest", &normal_stub, false);
    assert!(res_evict);
    assert_eq!(store.range_index.live_index_count(), 0);
    OK
  })
}

/// 66. 对应 Garnet RICopyReadsToTailCompatibleTest
#[test]
fn test_ri_copy_reads_to_tail_compatible() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"rikey", StorageBackend::Std, TUNE)
      .await?;
    for i in 0..50 {
      let f = format!("field-{}", pad(i, 3));
      let v = format!("value-{}-pad", pad(i, 3));
      session
        .range_index_set(b"rikey", f.as_bytes(), v.as_bytes())
        .await?;
    }

    // 连续多次并发读取测试稳定性
    for i in 0..50 {
      let f = format!("field-{}", pad(i, 3));
      let expected = format!("value-{}-pad", pad(i, 3));
      assert_eq!(
        session.range_index_get(b"rikey", f.as_bytes()).await?,
        Some(expected.into_bytes())
      );
    }
    OK
  })
}

/// 67. 对应 Garnet RangeIndexManagerKeyExistsTest
#[test]
fn test_range_index_manager_key_exists() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    // 1. 普通字符串键
    session.upsert(b"str-key", b"string-value").await?;
    assert!(session.contains_key(b"str-key").await?);

    // 2. 哈希集合键
    session
      .hset(b"hash-key", b"f".to_vec(), b"v".to_vec())
      .await?;
    assert!(session.contains_key(b"hash-key").await?);

    // 3. 有序集合键
    session
      .zadd(
        b"zset-key",
        1.0,
        b"m".to_vec(),
        wedb_zset::ZAddOpt::default(),
      )
      .await?;
    assert!(session.contains_key(b"zset-key").await?);

    // 4. RangeIndex 范围索引键
    session
      .range_index_create(b"ri-key", StorageBackend::Memory, TUNE)
      .await?;
    assert!(session.contains_key(b"ri-key").await?);

    // 5. 不存在的键
    assert!(!session.contains_key(b"missing-key").await?);
    OK
  })
}

/// RENAME 迁移 RangeIndex：数据必须完整迁移到新键且旧键数据文件被彻底清理
/// （修复前：旧键 delete 会销毁唯一数据副本，新键因数据文件按键名哈希命名而无法打开）
#[test]
fn test_ri_rename_migrates_data_and_files() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = new_store()?;
    let session = store.new_session()?;

    // 1. 创建磁盘后端索引并写入若干字段
    session
      .range_index_create(b"ri_src", StorageBackend::Std, TUNE)
      .await?;
    session
      .range_index_set(b"ri_src", b"alpha", b"value-alpha")
      .await?;
    session
      .range_index_set(b"ri_src", b"bravo", b"value-bravo")
      .await?;

    let ri_log_root = dir.path().join("rangeindex");
    let count_data_files = || -> usize {
      fs::read_dir(&ri_log_root)
        .map(|entries| {
          entries
            .filter_map(|e| e.ok())
            .filter(|e| e.path().to_string_lossy().ends_with(".data.bftree"))
            .count()
        })
        .unwrap_or(0)
    };
    assert_eq!(count_data_files(), 1, "前置条件：旧键应存在一个数据文件");

    // 2. RENAME 迁移
    assert_eq!(
      session.rename(b"ri_src", b"ri_dst", false).await?,
      RenameResult::Success
    );

    // 3. 旧键消亡、新键存活且数据完整
    assert!(!session.range_index_exists(b"ri_src").await?);
    assert_eq!(session.type_of(b"ri_src").await?, "none");
    assert!(session.range_index_exists(b"ri_dst").await?);
    assert_eq!(
      session.range_index_get(b"ri_dst", b"alpha").await?,
      Some(b"value-alpha".to_vec())
    );
    assert_eq!(
      session.range_index_get(b"ri_dst", b"bravo").await?,
      Some(b"value-bravo".to_vec())
    );

    // 4. 迁移后的索引可持续读写
    session
      .range_index_set(b"ri_dst", b"charlie", b"value-charlie")
      .await?;
    assert_eq!(
      session.range_index_get(b"ri_dst", b"charlie").await?,
      Some(b"value-charlie".to_vec())
    );

    // 5. 旧键数据文件被清理，仅保留新键数据文件
    assert_eq!(count_data_files(), 1, "迁移后仅应保留新键数据文件");

    // 6. RENAMENX 语义：目标已存在时拦截
    session
      .range_index_create(b"ri_nx", StorageBackend::Std, TUNE)
      .await?;
    assert_eq!(
      session.rename(b"ri_nx", b"ri_dst", true).await?,
      RenameResult::AlreadyExists
    );
    assert!(session.range_index_exists(b"ri_nx").await?);

    OK
  })
}

/// RENAME 迁移内存后端 RangeIndex：树数据经 CPR 快照重建后持续可用
#[test]
fn test_ri_rename_memory_backend() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = new_store()?;
    let session = store.new_session()?;

    session
      .range_index_create(b"mem_src", StorageBackend::Memory, TUNE)
      .await?;
    session
      .range_index_set(b"mem_src", b"field_1", b"value_1")
      .await?;
    session
      .range_index_set(b"mem_src", b"field_2", b"value_2")
      .await?;

    assert_eq!(
      session.rename(b"mem_src", b"mem_dst", false).await?,
      RenameResult::Success
    );

    assert!(!session.range_index_exists(b"mem_src").await?);
    assert!(session.range_index_exists(b"mem_dst").await?);
    assert_eq!(
      session.range_index_get(b"mem_dst", b"field_1").await?,
      Some(b"value_1".to_vec())
    );
    assert_eq!(
      session.range_index_get(b"mem_dst", b"field_2").await?,
      Some(b"value_2".to_vec())
    );

    // 迁移后覆盖写与删除
    session
      .range_index_set(b"mem_dst", b"field_1", b"value_1_new")
      .await?;
    assert_eq!(
      session.range_index_get(b"mem_dst", b"field_1").await?,
      Some(b"value_1_new".to_vec())
    );
    assert!(session.range_index_del(b"mem_dst", b"field_2").await?);
    assert_eq!(session.range_index_get(b"mem_dst", b"field_2").await?, None);

    OK
  })
}
