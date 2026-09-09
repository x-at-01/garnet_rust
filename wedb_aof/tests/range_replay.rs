//! RangeIndex 帧家族端到端测试（引擎级）：字段写效果入 AOF，崩溃式退出后
//! 仅靠重放恢复（无 checkpoint）；帧序自洽——索引 stub 的 hlog Upsert 帧先行。

use std::{path::Path, sync::Arc};

use aok::{OK, Result, Void};
use compio::runtime::Runtime;
use log::info;
use tempfile::tempdir;
use wbftree::{StorageBackend, TreeTuning};
use wedb_aof::AofLog;
use wdev::SegmentedDevice;
use wkv::{StoreConfig, WedbStore};

/// 打开基于临时目录的存储引擎（RangeIndex 树目录同置，崩溃后文件留存）
fn open_store(dir: &Path) -> Result<Arc<WedbStore<SegmentedDevice>>> {
  let device = Arc::new(SegmentedDevice::new(
    dir.join("store.db"),
    Some(4 * 1024 * 1024),
    4096,
  )?);
  let config = StoreConfig::minimal()
    .with_range_index_dir(dir.join("range_index"))
    .with_bftree_path(dir.join("bftree").join("shared.data.bftree"));
  Ok(Arc::new(WedbStore::open(config, device)?))
}

#[test]
fn test_range_index_frame_replay_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;

    // 第一轮：创建索引、写入字段、删除其一，提交后崩溃式退出
    {
      let store = open_store(dir.path())?;
      let aof = AofLog::open(dir.path(), 0).await?;
      if let Some(l) = aof.write_listener() {
        store.set_write_listener(l);
      }
      if let Some(l) = aof.bftree_listener() {
        store.bftree.set_write_listener(l);
      }
      if let Some(l) = aof.range_listener() {
        store.set_range_listener(l);
      }

      let session = store.new_session()?;
      let tuning = TreeTuning {
        cache_size: 64 * 1024,
        min_record_size: 8,
        max_record_size: 1024,
        max_key_len: 128,
        leaf_page_size: 0,
      };
      session
        .range_index_create(b"idx", StorageBackend::Disk, tuning)
        .await?;
      session.range_index_set(b"idx", b"field-1", b"value-1").await?;
      session.range_index_set(b"idx", b"field-2", b"value-2").await?;
      session.range_index_del(b"idx", b"field-1").await?;
      aof.commit().await?;
      // drop 即崩溃式退出（AofReplayer 与监听均已注入过，重放写不二次入 AOF）
    }

    // 第二轮：全新开库（无 checkpoint），AOF 重放恢复 RangeIndex 字段
    {
      let store = open_store(dir.path())?;
      let aof = AofLog::open(dir.path(), 0).await?;
      let replayed = aof.recover_and_replay(&store).await?;
      assert!(replayed > 0, "应有帧被重放");

      let session = store.new_session()?;
      assert_eq!(
        session.range_index_get(b"idx", b"field-2").await?,
        Some(b"value-2".to_vec()),
        "存留字段应恢复"
      );
      assert_eq!(
        session.range_index_get(b"idx", b"field-1").await?,
        None,
        "已删字段不得复活"
      );
    }

    info!("RangeIndex 帧家族重放恢复测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
