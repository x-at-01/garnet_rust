use std::{
  fs::{OpenOptions, create_dir, create_dir_all, read, write},
  sync::{Arc, atomic::Ordering},
  thread,
};

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;
use tempfile::tempdir;
use wcpr::{
  CheckpointMeta, CheckpointType, Error, FORMAT_VERSION, index_filename, index_tmp_filename,
  meta_filename, meta_tmp_filename, next_token,
};
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use wedb_zset::ZAddOpt;
use wkv::{CheckpointManager, KEY_ID_ASSIGN_MARGIN, StoreConfig, WedbStore};

/// 测试 1: 完整的 FoldOver 快照持久化与崩溃恢复全流程
/// 覆盖路径：
/// 1. 单机写入历史 KV 数据，包括插入、原位修改与删除（墓碑）。
/// 2. 创建 FoldOver 异步快照，验证索引与元数据文件原子刷盘，TailAddress 与 ReadOnlyAddress 状态封印。
/// 3. 模拟宕机停机（Drop 销毁原实例），新建存储引擎执行崩溃恢复。
/// 4. 验证历史所有 KV 数据的完整性与正确性（删除项读空、历史项可读、未写项读空）。
/// 5. 恢复后继续追加写入新记录，修改历史只读记录（触发 RCU 追加），刷盘持久化并二次恢复验证。
#[test]
fn test_foldover_checkpoint_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("foldover_test.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(1024, 64 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();
    let token;
    let expected_count;

    // 第一阶段：初始化实例并写入历史测试数据
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(WedbStore::open(config.clone(), device)?);
      let session = store.new_session()?;

      // 1. 批量写入 100 条记录
      for i in 0..100 {
        let k = format!("user:{i:04}");
        let v = format!("value_{i:04}");
        session.upsert(k.as_bytes(), v.as_bytes()).await?;
      }

      // 2. 原位覆写部分记录（在可变区更新）
      session.upsert(b"user:0010", b"value_0010_upd").await?;
      session.upsert(b"user:0020", b"value_0020_upd").await?;

      // 3. 删除部分记录（写入墓碑）
      session.delete(b"user:0030").await?;
      session.delete(b"user:0040").await?;

      expected_count = store.entry_count();
      assert_eq!(
        expected_count, 100,
        "哈希索引条目总数应为 100（包含已墓碑化记录指针）"
      );

      // 4. 创建 FoldOver 快照
      let meta = manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;

      token = meta.token;
      assert_eq!(meta.cp_type, CheckpointType::FoldOver);
      assert_eq!(meta.index_meta.entry_count, expected_count);
      assert!(meta.hlog_meta.tail_address > 0);
      assert_eq!(
        meta.hlog_meta.flushed_until_address,
        store.hlog.flushed_until_address()
      );

      // 验证快照物理文件确实已原子落盘
      let index_file = ckpt_dir.join(format!("index_{}.ckpt", token));
      let meta_file = ckpt_dir.join(format!("checkpoint_{}.meta", token));
      assert!(index_file.exists(), "索引快照文件必须存在");
      assert!(meta_file.exists(), "元数据文件必须存在");

      // 验证 FoldOver 封印语义：只读边界必须推进至截断点
      assert!(
        store.read_only_address() >= meta.hlog_meta.tail_address,
        "FoldOver 检查点后 ReadOnlyAddress 必须封印至 TailAddress"
      );

      // 验证封印后更新历史只读记录必须触发 RCU 追加（tail 前移），严禁原位覆写
      let tail_before = store.tail_address();
      session
        .upsert(b"user:0002", b"value_0002_after_ckpt")
        .await?;
      assert!(
        store.tail_address() > tail_before,
        "封印后更新只读记录必须触发 RCU 追加而非原位覆写"
      );

      info!("第一阶段完成: 成功创建 FoldOver 快照 token={token:#x}");
    } // 此处 store、session、device 全部 Drop 销毁，模拟节点崩溃与断电停机

    // 第二阶段：新建引擎执行崩溃恢复，验证历史读取
    let second_token;
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let recovered_store = Arc::new(CheckpointManager::recover(&ckpt_dir, token, device).await?);
      let session = recovered_store.new_session()?;

      // 1. 验证历史有效记录总数
      assert_eq!(
        recovered_store.entry_count(),
        expected_count,
        "恢复后的有效条目数必须与快照前一致"
      );

      // 2. 验证已修改记录
      assert_eq!(
        session.read(b"user:0010").await?,
        Some(b"value_0010_upd".to_vec())
      );
      assert_eq!(
        session.read(b"user:0020").await?,
        Some(b"value_0020_upd".to_vec())
      );

      // 3. 验证被删除记录（读空）
      assert_eq!(session.read(b"user:0030").await?, None);
      assert_eq!(session.read(b"user:0040").await?, None);

      // 4. 验证未修改记录
      for i in 0..100 {
        if i == 10 || i == 20 || i == 30 || i == 40 {
          continue;
        }
        let k = format!("user:{i:04}");
        let expected_v = format!("value_{i:04}");
        let val = session.read(k.as_bytes()).await?;
        assert_eq!(val, Some(expected_v.into_bytes()), "键 {k} 读取内容不匹配");
      }

      // 5. 验证不存在的记录
      assert_eq!(session.read(b"user:non_existent").await?, None);

      info!("第二阶段完成: 历史数据回读校验完全一致");

      // 第三阶段：恢复后继续追加新写入与持久化
      // 1. 修改历史只读记录（触发向后追加 RCU 路径）
      session
        .upsert(b"user:0001", b"value_0001_recovered_upd")
        .await?;
      assert_eq!(
        session.read(b"user:0001").await?,
        Some(b"value_0001_recovered_upd".to_vec())
      );

      // 2. 追加全新的数据项
      for i in 100..150 {
        let k = format!("user:{i:04}");
        let v = format!("value_{i:04}");
        session.upsert(k.as_bytes(), v.as_bytes()).await?;
      }

      for i in 100..150 {
        let k = format!("user:{i:04}");
        let expected_v = format!("value_{i:04}");
        assert_eq!(
          session.read(k.as_bytes()).await?,
          Some(expected_v.into_bytes())
        );
      }

      // 3. 刷盘并创建第二次快照
      recovered_store.flush_all().await?;
      let meta2 = manager
        .create_checkpoint(&recovered_store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;
      second_token = meta2.token;

      info!("第三阶段完成: 恢复后追加写入与二次快照成功 token={second_token:#x}");
    } // 再次模拟停机

    // 第四阶段：从第二次快照恢复并验证全部数据
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store2 = Arc::new(CheckpointManager::recover(&ckpt_dir, second_token, device).await?);
      let session2 = store2.new_session()?;

      // 验证历史更新项
      assert_eq!(
        session2.read(b"user:0001").await?,
        Some(b"value_0001_recovered_upd".to_vec())
      );

      // 验证新追加项
      for i in 100..150 {
        let k = format!("user:{i:04}");
        let expected_v = format!("value_{i:04}");
        assert_eq!(
          session2.read(k.as_bytes()).await?,
          Some(expected_v.into_bytes())
        );
      }

      info!("第四阶段完成: 二次恢复全量数据验证通过");
    }

    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 2: Snapshot 快照类型与崩溃恢复验证
#[test]
fn test_snapshot_checkpoint_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("snapshot_test.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(512, 64 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();
    let token;

    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(WedbStore::open(config.clone(), device)?);
      let session = store.new_session()?;

      session.upsert(b"item:alpha", b"val_alpha").await?;
      session.upsert(b"item:beta", b"val_beta").await?;
      session.upsert(b"item:gamma", b"val_gamma").await?;

      let meta = manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::Snapshot)
        .await?;
      token = meta.token;
      assert_eq!(meta.cp_type, CheckpointType::Snapshot);
    }

    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let recovered = Arc::new(CheckpointManager::recover(&ckpt_dir, token, device).await?);
      let session = recovered.new_session()?;

      assert_eq!(
        session.read(b"item:alpha").await?,
        Some(b"val_alpha".to_vec())
      );
      assert_eq!(
        session.read(b"item:beta").await?,
        Some(b"val_beta".to_vec())
      );
      assert_eq!(
        session.read(b"item:gamma").await?,
        Some(b"val_gamma".to_vec())
      );

      // 恢复后追加写入
      session.upsert(b"item:delta", b"val_delta").await?;
      assert_eq!(
        session.read(b"item:delta").await?,
        Some(b"val_delta".to_vec())
      );
    }

    info!("Snapshot 快照与恢复测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 3: 跨多页换页与哈希索引溢出桶（OverflowPool）崩溃恢复
#[test]
fn test_multi_page_and_overflow_bucket_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("multipage_test.db");
    let ckpt_dir = dir.path().join("checkpoints");

    // 使用较小的桶数（64）与页面尺寸（8KB），强制触发大量溢出桶和跨多页换页
    let config = StoreConfig::new(64, 8192, 16, 0.5)?;
    let manager = CheckpointManager::new();
    let token;
    let total_records = 300;

    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(WedbStore::open(config.clone(), device)?);
      let session = store.new_session()?;

      // 写入 300 条约 100 字节的记录，占用约 30KB（跨越至少 4 个 8KB 页）
      for i in 0..total_records {
        let k = format!("key_{i:05}");
        let v = format!("payload_data_for_key_{i:05}_{}", "x".repeat(50));
        session.upsert(k.as_bytes(), v.as_bytes()).await?;
      }

      let overflow_count = store.index.overflow_pool.allocated_count();
      assert!(
        overflow_count > 0,
        "在仅 64 个主桶中插入 300 项必定触发溢出桶分配，实际 overflow_count={overflow_count}"
      );

      let meta = manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;
      token = meta.token;
      assert!(meta.index_meta.overflow_count > 0);
      assert_eq!(meta.index_meta.entry_count, total_records);

      info!(
        "多页溢出写入完成: total={total_records}, overflow_count={overflow_count}, tail={:#x}",
        meta.hlog_meta.tail_address
      );
    }

    // 崩溃恢复
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let recovered = Arc::new(CheckpointManager::recover(&ckpt_dir, token, device).await?);
      let session = recovered.new_session()?;

      assert_eq!(recovered.entry_count(), total_records);

      // 验证全部记录在跨页和溢出链场景下的准确读取
      for i in 0..total_records {
        let k = format!("key_{i:05}");
        let expected_v = format!("payload_data_for_key_{i:05}_{}", "x".repeat(50));
        let val = session.read(k.as_bytes()).await?;
        assert_eq!(
          val,
          Some(expected_v.into_bytes()),
          "多页恢复后记录 {k} 读取内容不匹配"
        );
      }

      // 继续追加写入触发新换页
      for i in total_records..total_records + 50 {
        let k = format!("key_{i:05}");
        let v = format!("extra_payload_{i:05}");
        session.upsert(k.as_bytes(), v.as_bytes()).await?;
      }

      for i in total_records..total_records + 50 {
        let k = format!("key_{i:05}");
        let expected_v = format!("extra_payload_{i:05}");
        assert_eq!(
          session.read(k.as_bytes()).await?,
          Some(expected_v.into_bytes())
        );
      }

      recovered.flush_all().await?;
      info!("多页溢出桶恢复及追加写入验证成功");
    }

    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 4: 快照元数据检索、Token 校验与异常容错测试
#[test]
fn test_metadata_management_and_errors() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("meta_err_test.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(256, 16 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();

    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;
    session.upsert(b"foo", b"bar").await?;

    // 1. 创建两个不同 Token 的快照
    let meta1 = manager
      .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
      .await?;
    let meta2 = manager
      .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
      .await?;

    // 2. 验证 list_checkpoints 与 find_latest_checkpoint
    let tokens = CheckpointManager::<SegmentedDevice>::list_checkpoints(&ckpt_dir)?;
    assert_eq!(tokens.len(), 2);
    assert_eq!(tokens[0], meta1.token);
    assert_eq!(tokens[1], meta2.token);

    let latest = CheckpointManager::<SegmentedDevice>::find_latest_checkpoint(&ckpt_dir)?;
    assert_eq!(latest, Some(meta2.token));

    // 3. 验证未存在的 Token 恢复时报错 MetaNotFound
    let non_exist_token = 999_999_999u128;
    let err = CheckpointManager::recover(&ckpt_dir, non_exist_token, Arc::clone(&device)).await;
    assert!(
      matches!(err, Err(Error::MetaNotFound(_))),
      "期望 MetaNotFound 错误"
    );

    info!("快照元数据检索与异常处理测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 5: 最新有效检查点自动回退恢复与孤儿目录清理
/// 覆盖路径C# GetClosestHybridLogCheckpointInfo 对无效 Token 的容错跳过语义）：
/// 1. 依次写入 3 份数据并创建 3 个检查点（token1 < token2 < token3）；
/// 2. 损坏最新检查点（token3）的索引快照文件（截断字节，触发文件长度校验失败）；
///    篡改次新检查点（token2）的元数据为非法 JSON（触发反序列化失败）；
/// 3. `recover_latest` 必须自动回退至最老的有效检查点（token1）并完整回读数据；
/// 4. 目录中不存在任何检查点时，`recover_latest` 必须返回 `NoValidCheckpoint`；
/// 5. 残留孤儿 token 子目录（meta 已丢失）与临时文件必须被 `purge_all` 彻底清理。
#[test]
fn test_recover_latest_fallback_and_orphan_cleanup() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("recover_latest.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(512, 16 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();

    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;

    // 1. 三个递增版本检查点
    let mut tokens = Vec::new();
    for round in 0..3 {
      session
        .upsert(
          format!("rk:{}", round).as_bytes(),
          format!("rv:{}", round).as_bytes(),
        )
        .await?;
      let meta = manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;
      tokens.push(meta.token);
    }
    assert_eq!(tokens.len(), 3);

    // 2. 损坏最新检查点（token3）的索引快照文件：截断 16 字节触发长度校验失败
    let index3 = ckpt_dir.join(format!("index_{}.ckpt", tokens[2]));
    let file = OpenOptions::new().read(true).write(true).open(&index3)?;
    let cur_len = file.metadata()?.len();
    file.set_len(cur_len - 16)?;
    drop(file);

    // 3. 篡改次新检查点（token2）的元数据为非法 JSON
    let meta2_path = ckpt_dir.join(format!("checkpoint_{}.meta", tokens[1]));
    write(&meta2_path, b"{ corrupted json !!!")?;

    // 4. recover_latest 必须回退至最老的有效检查点 token1，且只含第一轮数据
    let recovered =
      Arc::new(CheckpointManager::recover_latest(&ckpt_dir, Arc::clone(&device)).await?);
    let s = recovered.new_session()?;
    assert_eq!(s.read(b"rk:0").await?, Some(b"rv:0".to_vec()));
    assert_eq!(s.read(b"rk:1").await?, None, "回退恢复严禁泄露后续写入");
    assert_eq!(s.read(b"rk:2").await?, None, "回退恢复严禁泄露后续写入");

    // 5. 全部检查点均无效时返回 NoValidCheckpoint
    let empty_dir = dir.path().join("empty_ckpt");
    let err = CheckpointManager::recover_latest(&empty_dir, Arc::clone(&device)).await;
    assert!(
      matches!(err, Err(Error::NoValidCheckpoint(_))),
      "期望 NoValidCheckpoint 错误"
    );

    // 6. 孤儿 token 子目录与临时文件必须被 purge_all 彻底清理
    let orphan_dir = ckpt_dir.join("424242");
    create_dir_all(&orphan_dir)?;
    write(orphan_dir.join("residue.bftree"), b"orphan rangeindex")?;
    write(ckpt_dir.join("checkpoint_777.meta.tmp"), b"half written")?;

    manager.purge_all_checkpoints(&ckpt_dir)?;
    assert!(!orphan_dir.exists(), "孤儿 token 子目录必须被删除");
    assert!(!ckpt_dir.join("checkpoint_777.meta.tmp").exists());
    assert!(
      CheckpointManager::<SegmentedDevice>::list_checkpoints(&ckpt_dir)?.is_empty(),
      "purge_all 必须彻底清空所有快照"
    );

    info!("最新有效检查点回退恢复与孤儿清理测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 6: FlushedUntilAddress 恢复钳制与地址校验防御（第 2 轮修复回归）
/// 覆盖路径：
/// 1. 封印后并发 RCU 追加可能使持久化 FlushedUntilAddress 超前截断点 tail——恢复时
///    必须钳制至 tail（`[begin, tail)` 已由 flush_all + device.sync 保证落盘，钳制只会
///    引发冗余重刷而绝不漏刷，同时维持 flushed_until <= tail 单调不变式）；
/// 2. FlushedUntilAddress 低于 HeadAddress（已驱逐页未落盘）必须被拦截报错。
#[test]
fn test_flushed_until_clamp_and_validation() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("flushed_clamp.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(256, 16 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();

    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;
    session.upsert(b"clamp", b"me").await?;

    let meta = manager
      .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
      .await?;
    let token = meta.token;
    let tail = meta.hlog_meta.tail_address;
    let meta_file = ckpt_dir.join(format!("checkpoint_{}.meta", token));

    // 1. FlushedUntilAddress 超前截断点：恢复成功且被钳制至 tail
    //    （语义字段篡改后必须重新封签，使本用例聚焦地址语义而非封签校验）
    {
      let mut tampered = meta.clone();
      tampered.hlog_meta.flushed_until_address = tail + 8;
      tampered.seal();
      write(&meta_file, sonic_rs::to_vec_pretty(&tampered)?)?;

      let recovered = CheckpointManager::recover(&ckpt_dir, token, Arc::clone(&device)).await?;
      assert_eq!(
        recovered.hlog.flushed_until_address(),
        tail,
        "FlushedUntilAddress 必须被钳制至截断点"
      );
      let s = Arc::new(recovered).new_session()?;
      assert_eq!(s.read(b"clamp").await?, Some(b"me".to_vec()));
    }

    // 2. FlushedUntilAddress 低于 HeadAddress（head <= flushed 不变式被破坏）：恢复被拦截
    {
      let mut tampered = meta.clone();
      tampered.hlog_meta.head_address = tail;
      tampered.hlog_meta.flushed_until_address = meta.hlog_meta.begin_address;
      tampered.seal();
      write(&meta_file, sonic_rs::to_vec_pretty(&tampered)?)?;

      let err = CheckpointManager::recover(&ckpt_dir, token, Arc::clone(&device)).await;
      assert!(
        matches!(err, Err(Error::InvalidRecoveryAddress(msg)) if msg.contains("低于 HeadAddress")),
        "期望 FlushedUntilAddress 低于 HeadAddress 被拦截"
      );
    }

    // 还原元数据并验证正常恢复
    write(&meta_file, sonic_rs::to_vec_pretty(&meta)?)?;
    let rec = CheckpointManager::recover(&ckpt_dir, token, Arc::clone(&device)).await?;
    assert_eq!(rec.entry_count(), 1);

    info!("FlushedUntilAddress 钳制与校验防御测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 7: 元数据格式版本兼容门控（第 2 轮修复回归）
/// 1. 超前版本（format_version 大于当前支持版本）必须拒绝恢复，防止新版字段被旧引擎
///    按旧语义错误解读；
/// 2. 早期未携带版本号字段（format_version 缺失，serde 缺省为 0）的遗留元数据必须照常恢复。
#[test]
fn test_meta_format_version_gate() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("meta_version.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(256, 16 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();

    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;
    session.upsert(b"ver", b"check").await?;

    let meta = manager
      .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
      .await?;
    let token = meta.token;
    let meta_file = ckpt_dir.join(format!("checkpoint_{}.meta", token));
    assert_eq!(meta.format_version, FORMAT_VERSION);

    // 1. 超前版本拒绝恢复
    let mut future = meta.clone();
    future.format_version = FORMAT_VERSION + 1;
    write(&meta_file, sonic_rs::to_vec_pretty(&future)?)?;

    let err = CheckpointManager::recover(&ckpt_dir, token, Arc::clone(&device)).await;
    assert!(
      matches!(
        err,
        Err(Error::UnsupportedMetaVersion { actual, supported })
          if actual == FORMAT_VERSION + 1 && supported == FORMAT_VERSION
      ),
      "期望超前元数据版本被拒绝"
    );

    // 2. 遗留元数据缺失版本字段：从 pretty JSON 中剥离末尾两字段构造遗留格式，
    //    照常恢复（serde 缺省 format_version = 0、封签字段缺失同被剥离）。u128
    // token 无法经过无类型 Value 中转，故直接做文本行过滤并消去前一行的悬挂逗号。
    let pretty = String::from_utf8(sonic_rs::to_vec_pretty(&meta)?)?;
    let legacy_json = pretty
      .lines()
      .filter(|line| {
        let t = line.trim_start();
        !(t.starts_with("\"format_version\"") || t.starts_with("\"integrity_crc32\""))
      })
      .collect::<Vec<&str>>()
      .join("\n")
      .replacen(",\n}", "\n}", 1);
    assert!(!legacy_json.contains("format_version"));
    assert!(!legacy_json.contains("integrity_crc32"));
    write(&meta_file, legacy_json)?;

    let recovered = CheckpointManager::recover(&ckpt_dir, token, Arc::clone(&device)).await?;
    assert_eq!(recovered.entry_count(), 1);
    let s = Arc::new(recovered).new_session()?;
    assert_eq!(s.read(b"ver").await?, Some(b"check".to_vec()));

    info!("元数据格式版本兼容门控测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 8: purge_outdated 保留最新 N 版的磁盘空间回收（第 2 轮修复回归）
/// 1. 连续创建 4 个检查点；
/// 2. `purge_outdated(keep=2)` 精准清理最旧的 2 个并返回其 Token 列表；
/// 3. 被清理 Token 恢复报 MetaNotFound，最新检查点经 recover_latest 完好恢复；
/// 4. `keep=0` 等价于全量回收；保留量不小于现存数量时不动任何文件。
#[test]
fn test_purge_outdated_retention() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("purge_outdated.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(256, 16 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();

    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;

    let mut tokens = Vec::new();
    for round in 0..4 {
      session
        .upsert(
          format!("pk:{}", round).as_bytes(),
          format!("pv:{}", round).as_bytes(),
        )
        .await?;
      let meta = manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;
      tokens.push(meta.token);
    }

    // 1. keep=2：清理最旧的 2 个，保留最新 2 个
    let purged = CheckpointManager::<SegmentedDevice>::purge_outdated(&ckpt_dir, 2)?;
    assert_eq!(purged, vec![tokens[0], tokens[1]]);
    let listed = CheckpointManager::<SegmentedDevice>::list_checkpoints(&ckpt_dir)?;
    assert_eq!(listed, vec![tokens[2], tokens[3]]);
    assert!(
      !ckpt_dir.join(format!("index_{}.ckpt", tokens[0])).exists(),
      "被清理检查点的索引快照文件必须物理删除"
    );

    // 2. 被清理 Token 恢复报 MetaNotFound
    let err = CheckpointManager::recover(&ckpt_dir, tokens[0], Arc::clone(&device)).await;
    assert!(matches!(err, Err(Error::MetaNotFound(_))));

    // 3. 最新检查点经 recover_latest 完好恢复
    let latest = CheckpointManager::recover_latest(&ckpt_dir, Arc::clone(&device)).await?;
    let s = Arc::new(latest).new_session()?;
    assert_eq!(s.read(b"pk:3").await?, Some(b"pv:3".to_vec()));

    // 4. 保留量不小于现存数量：不清理任何文件
    let untouched = CheckpointManager::<SegmentedDevice>::purge_outdated(&ckpt_dir, 8)?;
    assert!(untouched.is_empty());
    assert_eq!(
      CheckpointManager::<SegmentedDevice>::list_checkpoints(&ckpt_dir)?.len(),
      2
    );

    // 5. keep=0：全量回收
    let purged_all = CheckpointManager::<SegmentedDevice>::purge_outdated(&ckpt_dir, 0)?;
    assert_eq!(purged_all, vec![tokens[2], tokens[3]]);
    assert!(
      CheckpointManager::<SegmentedDevice>::list_checkpoints(&ckpt_dir)?.is_empty(),
      "keep=0 必须清空全部检查点"
    );

    info!("purge_outdated 保留式磁盘回收测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 9: CheckpointMeta bitcode 序列化往返与二进制元数据自愈恢复
#[test]
fn test_checkpoint_meta_bitcode_roundtrip_and_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("bitcode_ckpt.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(256, 16 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();

    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;
    session.upsert(b"bitcode_key", b"bitcode_val").await?;

    let meta = manager
      .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
      .await?;
    let token = meta.token;

    // 1. 验证 bitcode 编码与解码往返一致性
    let bc_bytes = meta.encode_bitcode();
    assert!(!bc_bytes.is_empty());
    let decoded_meta = CheckpointMeta::decode_bitcode(&bc_bytes)?;
    assert_eq!(decoded_meta, meta);

    // 2. 用 bitcode 二进制字节直接覆盖 checkpoint_{token}.meta 文件
    let meta_file = ckpt_dir.join(meta_filename(token));
    write(&meta_file, &bc_bytes)?;

    // 3. 验证 CheckpointManager::recover 能够通过 decode_auto 自动识别并成功从 bitcode 恢复
    let recovered = CheckpointManager::recover(&ckpt_dir, token, Arc::clone(&device)).await?;
    assert_eq!(recovered.entry_count(), 1);
    let s = Arc::new(recovered).new_session()?;
    assert_eq!(s.read(b"bitcode_key").await?, Some(b"bitcode_val".to_vec()));

    // 4. 损坏 bitcode 数据：校验错误拦截
    write(&meta_file, [0xFF, 0xFF, 0x00, 0x12])?;
    let err = CheckpointManager::recover(&ckpt_dir, token, Arc::clone(&device)).await;
    assert!(err.is_err(), "损坏的元数据字节必须报错");

    info!("CheckpointMeta bitcode 往返与自愈恢复测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 10: 共享 BfTree 持久化闭环（Flattened ZSet 与系统元数据，对标 Garnet 共享树快照/恢复）
/// 1. 配置 bftree_path，ZADD 150 个 >64B 成员触发直接 Flattened 进共享 BfTree；
/// 2. 直写共享树系统元数据键（模拟 ACL / 集群配置存储）；
/// 3. 创建 FoldOver Checkpoint：共享树 CPR 快照必须落盘至 `{token}/bftree/shared.bftree`；
/// 4. Drop 全部实例模拟停机；`recover_latest` 恢复后 ZSCORE/ZCARD/系统键 1:1 一致，
///    且活动基文件必须预置在持久工作路径（绝不在会被 purge 回收的 Token 目录内）；
/// 5. purge_outdated 清理旧 Token 后二次 Checkpoint + 恢复仍成功。
#[test]
fn test_shared_bftree_checkpoint_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("shared_bftree.db");
    let ckpt_dir = dir.path().join("checkpoints");
    let bftree_work_path = dir.path().join("bftree").join("shared.data.bftree");

    let config = StoreConfig::new(512, 64 * 1024, 16, 0.5)?
      .with_range_index_dir(dir.path())
      .with_bftree_path(&bftree_work_path);
    let manager = CheckpointManager::new();
    let members: Vec<String> = (0..150)
      .map(|i| format!("member_{i:04}_{}", "x".repeat(72)))
      .collect();

    // 第一阶段：写入并快照
    let token1;
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(WedbStore::open(config.clone(), device)?);
      let session = store.new_session()?;

      for (i, m) in members.iter().enumerate() {
        session
          .zadd(b"lb:hot", i as f64, m, ZAddOpt::default())
          .await?;
      }
      // 成员 > ZSET_MAX_COMPACT_MEMBER(64B)：必须直接走 Flattened 共享 BfTree
      assert_eq!(session.zcard(b"lb:hot").await?, members.len());
      assert_eq!(
        session.zscore(b"lb:hot", members[7].as_bytes()).await?,
        Some(7.0)
      );

      // 系统元数据键（模拟 ACL 存储直写共享树）
      store.bftree.insert(b"acl:user:alice", b"on ~* +@all");
      let (_, acl_check) = store.bftree.read(b"acl:user:alice");
      assert_eq!(acl_check.as_deref(), Some(&b"on ~* +@all"[..]));

      let meta = manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;
      token1 = meta.token;
      let snap = ckpt_dir
        .join(token1.to_string())
        .join("bftree")
        .join("shared.bftree");
      assert!(snap.exists(), "共享 BfTree 快照必须落盘至 token 目录");
      info!("第一阶段完成: 共享 BfTree Checkpoint token={token1:#x}");
    } // store / session / device 全部 Drop，模拟节点崩溃

    // 第二阶段：恢复并校验，随后二次快照并清理旧 Token
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(CheckpointManager::recover_latest(&ckpt_dir, device).await?);
      let session = store.new_session()?;

      assert_eq!(session.zcard(b"lb:hot").await?, members.len());
      assert_eq!(
        session.zscore(b"lb:hot", members[7].as_bytes()).await?,
        Some(7.0)
      );
      let (_, acl_val) = store.bftree.read(b"acl:user:alice");
      assert_eq!(
        acl_val.as_deref(),
        Some(&b"on ~* +@all"[..]),
        "共享树系统元数据必须随 Checkpoint 恢复"
      );
      assert!(bftree_work_path.exists(), "恢复必须预置共享树持久工作文件");

      // 恢复后继续写入 + 二次快照
      session
        .zadd(b"lb:hot", 999.0, "member_new_0999", ZAddOpt::default())
        .await?;
      manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;
      // 清理旧 Token（含旧共享树快照文件）：活动基文件在工作路径，不受影响
      let purged = CheckpointManager::<SegmentedDevice>::purge_outdated(&ckpt_dir, 1)?;
      assert_eq!(purged, vec![token1]);
      info!("第二阶段完成: 恢复校验通过，旧 Token 已回收");
    } // 二次模拟停机

    // 第三阶段：purge 旧 Token 后从最新快照恢复，全量校验
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(CheckpointManager::recover_latest(&ckpt_dir, device).await?);
      let session = store.new_session()?;
      assert_eq!(session.zcard(b"lb:hot").await?, members.len() + 1);
      assert_eq!(
        session.zscore(b"lb:hot", b"member_new_0999").await?,
        Some(999.0)
      );
      assert_eq!(
        session.zscore(b"lb:hot", members[7].as_bytes()).await?,
        Some(7.0)
      );
      info!("第三阶段完成: purge 后恢复全量校验通过");
    }

    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 11: 旧版 StoreMeta 缺失 bftree_path 时的恢复防御
/// 1. 配置 bftree_path 写入共享树数据并创建 Checkpoint（token 目录含共享树快照）；
/// 2. 篡改 meta 将 `bftree_path` 置 None（等价旧版本引擎未记录该字段）；
/// 3. 恢复必须报错拒绝——静默跳过 = 丢弃可恢复的 ACL / Flattened ZSet 数据；
/// 4. 还原记录后恢复成功，共享树数据 1:1 回读。
#[test]
fn test_recover_rejects_legacy_meta_without_bftree_path() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("legacy_meta.db");
    let ckpt_dir = dir.path().join("checkpoints");
    let bftree_work_path = dir.path().join("bftree").join("shared.data.bftree");

    let config = StoreConfig::new(512, 64 * 1024, 16, 0.5)?
      .with_range_index_dir(dir.path())
      .with_bftree_path(&bftree_work_path);
    let manager = CheckpointManager::new();
    let token;

    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(WedbStore::open(config.clone(), device)?);
      store.bftree.insert(b"legacy:key", b"legacy:value");
      let meta = manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;
      token = meta.token;
    }

    let meta_file = ckpt_dir.join(meta_filename(token));

    // 1. 剥离 bftree_path 记录 (置 None，等价旧版本引擎未记录该字段)：恢复必须拒绝
    {
      let meta = CheckpointMeta::decode_auto(&read(&meta_file)?)?;
      let mut tampered = meta;
      assert!(
        tampered.store_meta.bftree_path.is_some(),
        "前置：meta 已记录 bftree_path"
      );
      tampered.store_meta.bftree_path = None;
      // 语义字段篡改后重新封签，使本用例聚焦 bftree_path 语义校验而非封签比对
      tampered.seal();
      write(&meta_file, sonic_rs::to_vec_pretty(&tampered)?)?;

      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let err = CheckpointManager::recover(&ckpt_dir, token, device).await;
      assert!(
        err.is_err(),
        "meta 缺失 bftree_path 但存在共享树快照，必须拒绝恢复"
      );
    }

    // 2. 还原 bftree_path 记录：恢复成功且共享树数据完整
    {
      let meta = CheckpointMeta::decode_auto(&read(&meta_file)?)?;
      let mut restored = meta;
      assert!(restored.store_meta.bftree_path.is_none());
      restored.store_meta.bftree_path = Some(bftree_work_path.to_string_lossy().into_owned());
      restored.seal();
      write(&meta_file, sonic_rs::to_vec_pretty(&restored)?)?;

      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(CheckpointManager::recover(&ckpt_dir, token, device).await?);
      let (_, v) = store.bftree.read(b"legacy:key");
      assert_eq!(v.as_deref(), Some(&b"legacy:value"[..]));
    }

    info!("旧版 meta 缺失 bftree_path 恢复防御测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 12: key_id 分配水位持久化与墙钟回退防御
/// 1. checkpoint 往返：创建集合（key_id 增长）→ 打 checkpoint → StoreMeta.next_key_id
///    必须不小于已推进的水位；
/// 2. 恢复水位：recover 后 next_key_id 必须 ≥ persisted + KEY_ID_ASSIGN_MARGIN，
///    且新分配的 key_id 单调大于持久化前所有值；
/// 3. 墙钟回退模拟：不真改时钟——把 meta 水位篡改为超出当前时钟域的高值 F（等效上一
///    进程在「未来」时间戳分配过集合，本进程时钟回退后 generate_initial_key_id 只能
///    给出低值），篡改后重新封签（模拟「水位被篡改但封签一致」的攻击者），恢复后
///    fetch_max 必须以水位为准，新分配 key_id 严格大于 F；
/// 4. 封签防线：篡改水位后不重新封签直接写盘 → 恢复必须因封签比对失败而报错，
///    杜绝静默接受被篡改的水位。
/// 5. fetch_max 单调性：低值水位下限不得回退已推进的水位。
#[test]
fn test_key_id_watermark_persist_and_clock_rollback() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("key_id_watermark.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(512, 64 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();
    let token;
    let persisted;

    // 第一阶段：创建 Flattened 集合驱动 key_id 增长 → checkpoint → 断言水位持久化
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(WedbStore::open(config.clone(), device)?);
      let session = store.new_session()?;

      let before = store.next_key_id.load(Ordering::Relaxed);
      // 成员 > ZSET_MAX_COMPACT_MEMBER(64B) 强制走 Flattened 路径：每个新集合分配一个 key_id
      for i in 0..3u64 {
        let member = format!("member_{i}_{}", "x".repeat(72));
        session
          .zadd(
            format!("coll:{i}").as_bytes(),
            i as f64,
            member,
            ZAddOpt::default(),
          )
          .await?;
      }
      let after = store.next_key_id.load(Ordering::Relaxed);
      assert!(after > before, "创建集合必须推进 key_id 分配水位");

      let meta = manager
        .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
        .await?;
      token = meta.token;
      persisted = meta.store_meta.next_key_id;
      assert!(
        persisted >= after,
        "StoreMeta.next_key_id 必须不小于 checkpoint 时点已推进的水位"
      );
    } // store / session / device 全部 Drop，模拟停机

    // 第二阶段：恢复水位 —— fetch_max(墙钟时间戳, persisted + MARGIN)
    {
      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(CheckpointManager::recover(&ckpt_dir, token, device).await?);
      let floor = store.next_key_id.load(Ordering::Relaxed);
      let expected = persisted.saturating_add(KEY_ID_ASSIGN_MARGIN);
      assert!(
        floor >= expected,
        "恢复后水位必须不低于 persisted + KEY_ID_ASSIGN_MARGIN"
      );

      // 低值下限不得回退已推进的水位（fetch_max 单调语义）
      store.raise_key_id_floor(1);
      assert_eq!(
        store.next_key_id.load(Ordering::Relaxed),
        floor,
        "低值水位下限严禁回退已推进的水位"
      );

      let session = store.new_session()?;
      session
        .zadd(
          b"coll:new",
          9.0,
          format!("member_new_{}", "x".repeat(72)),
          ZAddOpt::default(),
        )
        .await?;
      let allocated = store.next_key_id.load(Ordering::Relaxed) - 1;
      assert!(
        allocated > persisted,
        "新分配 key_id 必须严格大于持久化前的所有值"
      );
    }

    // 第三阶段：墙钟回退模拟 —— 篡改 meta 水位为超出当前时钟域的高值 F（1 << 62 远大于
    // 墙钟毫秒 << 16）。篡改后重新封签再写盘，模拟「水位被篡改但封签一致」的攻击者：
    // 封签校验必然放行，恢复后水位必须以 F + MARGIN 为准，新分配 key_id 严格大于 F
    {
      let fake_high = 1u64 << 62;
      let meta_file = ckpt_dir.join(meta_filename(token));
      let mut meta = CheckpointMeta::decode_auto(&read(&meta_file)?)?;
      meta.store_meta.next_key_id = fake_high;
      meta.seal();
      write(&meta_file, sonic_rs::to_vec_pretty(&meta)?)?;

      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let store = Arc::new(CheckpointManager::recover(&ckpt_dir, token, device).await?);
      assert_eq!(
        store.next_key_id.load(Ordering::Relaxed),
        fake_high.saturating_add(KEY_ID_ASSIGN_MARGIN),
        "回退场景下水位必须抬升至持久化值 + 分配余量"
      );

      let session = store.new_session()?;
      session
        .zadd(
          b"coll:rollback",
          1.0,
          format!("member_rb_{}", "x".repeat(72)),
          ZAddOpt::default(),
        )
        .await?;
      let allocated = store.next_key_id.load(Ordering::Relaxed) - 1;
      assert!(
        allocated > fake_high,
        "回退场景下新分配 key_id 必须严格大于历史最高水位，严禁复用"
      );
    }

    // 第四阶段：封签防线 —— 篡改水位后不重新封签直接写盘，恢复必须因封签比对
    // 失败而报 MetaChecksumMismatch，杜绝静默接受被篡改的水位
    {
      let meta_file = ckpt_dir.join(meta_filename(token));
      let mut meta = CheckpointMeta::decode_auto(&read(&meta_file)?)?;
      meta.store_meta.next_key_id += 1;
      write(&meta_file, sonic_rs::to_vec_pretty(&meta)?)?;

      let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
      let err = CheckpointManager::recover(&ckpt_dir, token, device)
        .await
        .err()
        .expect("篡改水位后封签不一致，恢复必须失败");
      assert!(
        matches!(err, Error::MetaChecksumMismatch { .. }),
        "恢复应报 MetaChecksumMismatch: {err:?}"
      );
    }

    info!("key_id 水位持久化与墙钟回退防御测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 13: 跨进程重启的 Token 下界钳制（墙钟回拨防御，第 2 轮复审回归）
/// 1. 目录预置高水位 Token 的 meta 文件（模拟重启前由更快的墙钟签发的检查点）；
/// 2. 新签发 Token 必须严格大于目录现存最大 Token，连续签发严格递增不撞号
///    （进程内守卫吸收下界抬升，回拨 + 并发两路均不会撞号）；
/// 3. 预置的垃圾 meta 不阻断 recover_latest 的由新到旧容错回退。
#[test]
fn test_token_floor_after_restart_rollback() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("token_floor.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(256, 16 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();

    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;
    session.upsert(b"floor", b"probe").await?;

    // 预置高水位 Token 的垃圾 meta（list_checkpoints 仅按文件名解析 Token）
    let seeded: u128 = 1 << 100;
    create_dir_all(&ckpt_dir)?;
    write(
      ckpt_dir.join(format!("checkpoint_{seeded}.meta")),
      b"not a meta",
    )?;

    let m1 = manager
      .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
      .await?;
    assert!(m1.token > seeded, "签发 Token 必须高于目录现存最大 Token");

    let m2 = manager
      .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
      .await?;
    assert!(m2.token > m1.token, "连续签发必须严格递增不撞号");

    // 垃圾 meta 不阻断回退：由新到旧跳过不可解析 Token，落回真实检查点
    let recovered = CheckpointManager::recover_latest(&ckpt_dir, Arc::clone(&device)).await?;
    assert_eq!(recovered.entry_count(), 1);
    let s = Arc::new(recovered).new_session()?;
    assert_eq!(s.read(b"floor").await?, Some(b"probe".to_vec()));

    info!("跨进程 Token 下界钳制测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 14: SAVE 与周期快照并发触发的串行化（第 2 轮复审回归）
/// 两个 OS 线程各自持 compio Runtime 并发创建检查点：进程级闸门保证在 WedbStore
/// 状态机上的变更串行叠加，签发闸门保证 Token 唯一；两个检查点文件集互不相交且
/// 均完整可恢复，前台写入在每个检查点中可见。
#[test]
fn test_concurrent_checkpoints_serialize() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("concurrent_ckpt.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(256, 16 * 1024, 16, 0.5)?;
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;
    session.upsert(b"cc", b"shared").await?;

    let (h1, h2) = {
      let store1 = Arc::clone(&store);
      let path = ckpt_dir.clone();
      let h1 = thread::spawn(move || -> aok::Result<u128> {
        let rt = Runtime::new()?;
        Ok(
          rt.block_on(CheckpointManager::new().create_checkpoint(
            &store1,
            &path,
            CheckpointType::FoldOver,
          ))?
          .token,
        )
      });
      let store2 = Arc::clone(&store);
      let path = ckpt_dir.clone();
      let h2 = thread::spawn(move || -> aok::Result<u128> {
        let rt = Runtime::new()?;
        Ok(
          rt.block_on(CheckpointManager::new().create_checkpoint(
            &store2,
            &path,
            CheckpointType::Snapshot,
          ))?
          .token,
        )
      });
      (h1, h2)
    };
    let t1 = h1.join().unwrap()?;
    let t2 = h2.join().unwrap()?;
    assert_ne!(t1, t2, "并发检查点 Token 必须唯一");

    // 两个检查点均完整可恢复，前台写入在各自一致性点均可见
    for t in [t1, t2] {
      let recovered = CheckpointManager::recover(&ckpt_dir, t, Arc::clone(&device)).await?;
      assert_eq!(recovered.entry_count(), 1);
      let s = Arc::new(recovered).new_session()?;
      assert_eq!(s.read(b"cc").await?, Some(b"shared".to_vec()));
    }

    info!("并发检查点串行化测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 15: 检查点创建失败的残留清场（磁盘满 ENOSPC 类失败路径代理注入，第 2 轮复审回归）
/// 以目录形态预先占用 index 临时文件路径注入必然写失败：失败错误上浮、本 Token 全部
/// 残留（含目录形态 `.tmp`）被 best-effort 回收，清场后目录可继续正常创建与恢复。
#[test]
fn test_failed_checkpoint_residue_cleanup() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("residue_cleanup.db");
    let ckpt_dir = dir.path().join("checkpoints");

    let config = StoreConfig::new(256, 16 * 1024, 16, 0.5)?;
    let manager = CheckpointManager::new();

    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(config.clone(), Arc::clone(&device))?);
    let session = store.new_session()?;
    session.upsert(b"keep", b"me").await?;

    // 目录形态注入：占用 index tmp 路径令快照写入必然失败（File::create 返回 EISDIR）
    create_dir_all(&ckpt_dir)?;
    let bad_token = next_token();
    create_dir(ckpt_dir.join(index_tmp_filename(bad_token)))?;

    let err = manager
      .create_checkpoint_with_token(&store, &ckpt_dir, CheckpointType::FoldOver, bad_token)
      .await;
    assert!(
      matches!(err, Err(Error::Io(_))),
      "注入的写失败必须以 I/O 错误上浮"
    );

    // 本 Token 文件集全部回收，目录形态 `.tmp` 残留也不例外
    assert!(!ckpt_dir.join(meta_filename(bad_token)).exists());
    assert!(!ckpt_dir.join(index_filename(bad_token)).exists());
    assert!(!ckpt_dir.join(meta_tmp_filename(bad_token)).exists());
    assert!(
      !ckpt_dir.join(index_tmp_filename(bad_token)).exists(),
      "目录形态残留必须被回收"
    );
    assert!(!ckpt_dir.join(bad_token.to_string()).exists());

    // 清场后目录继续正常工作
    let meta = manager
      .create_checkpoint(&store, &ckpt_dir, CheckpointType::FoldOver)
      .await?;
    let recovered = CheckpointManager::recover(&ckpt_dir, meta.token, Arc::clone(&device)).await?;
    assert_eq!(recovered.entry_count(), 1);

    info!("失败残留清场测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
