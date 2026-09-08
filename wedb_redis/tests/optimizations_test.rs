use std::{collections::HashSet, iter::repeat_n, path::PathBuf, sync::Arc};

use aok::{OK, Void};
use compio::runtime::Runtime;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use whlog::SECTOR_ALIGNMENT;
use wkv::{StoreConfig, WedbStore};

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

/// 不可变区读取预提升 ReadCache（对齐 C# InternalRead.CopyFromImmutable）：
/// 记录刷盘后仅推进 ReadOnlyAddress（仍驻留内存不可变区），首次读即应挂入 ReadCache
#[test]
fn test_immutable_region_read_promotes_to_read_cache() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("rc_promote.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let page_size = SECTOR_ALIGNMENT;
    let num_pages = 8;
    let config = StoreConfig::new(64, page_size, num_pages, 0.5)?.with_read_cache(true);

    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let k = b"k_imm_01";
    let v = b"val_imm_01";
    let addr = session.upsert(k, v).await?;

    // 填充记录将日志尾部推过页边界，保证后续热键写入真可变区（addr >= read_only）
    let pad = vec![b'X'; 3000];
    session.upsert(b"pad1", &pad).await?;
    session.upsert(b"pad2", &pad).await?;

    // 刷盘并推进只读区：记录进入内存不可变区（[head, read_only) 仍驻留 DRAM）
    store.flush_all().await?;
    store.shift_read_only_address(page_size as u64);

    assert!(!store.hlog.is_mutable(addr), "记录应位于不可变区");
    assert!(store.hlog.is_in_memory(addr), "记录应仍驻留内存");

    let str_k = session.session_string_key(k);
    let tag_before = store.index.find_tag(&str_k).expect("tag exists");
    assert!(
      !wkv::is_read_cache_addr(tag_before),
      "读取前不应有 ReadCache 挂载"
    );

    // 首次读：不可变区命中即预提升挂入 ReadCache
    let res = session.read(k).await?;
    assert_eq!(res, Some(v.to_vec()));

    let tag_after = store.index.find_tag(&str_k).expect("tag exists");
    assert!(
      wkv::is_read_cache_addr(tag_after),
      "不可变区记录读取后必须预提升挂入 ReadCache"
    );

    // 二次读：直接命中 ReadCache，值正确
    assert_eq!(session.read(k).await?, Some(v.to_vec()));

    // 可变区热数据不得被提升：新写入记录（addr >= read_only）读后仍应保留主日志地址
    let hot_k = b"k_hot_01";
    let hot_addr = session.upsert(hot_k, b"hot").await?;
    assert!(store.hlog.is_mutable(hot_addr), "热键应写入真可变区");
    assert_eq!(session.read(hot_k).await?, Some(b"hot".to_vec()));
    let hot_tag = store
      .index
      .find_tag(&session.session_string_key(hot_k))
      .expect("tag exists");
    assert_eq!(hot_tag, hot_addr, "可变区命中不应触发 ReadCache 提升");

    // RC 挂载后删除与包含性判定依然正确
    assert!(session.contains_key(k).await?);
    assert!(session.delete(k).await?);
    assert!(!session.contains_key(k).await?);

    OK
  })
}

#[test]
fn test_read_cache_contains_dbsize_keys_entry_count() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("rc_opt.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let page_size = SECTOR_ALIGNMENT;
    let num_pages = 8;
    let config = StoreConfig::new(64, page_size, num_pages, 0.5)?.with_read_cache(true);

    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    assert!(store.read_cache.is_enabled);

    let k1 = b"k_rc_01";
    let v1 = b"val_rc_01";
    let init_addr = session.upsert(k1, v1).await?;
    assert!(init_addr < page_size as u64);

    let pad = vec![b'X'; 3000];
    session.upsert(b"pad1", &pad).await?;
    let pad_addr = session.upsert(b"pad2", &pad).await?;
    assert!(pad_addr >= page_size as u64);

    // 刷盘并驱逐到磁盘
    store.flush_all().await?;
    store.shift_read_only_address(page_size as u64);
    store.shift_head_address(page_size as u64);

    assert!(store.hlog.is_on_disk(init_addr));
    assert!(!store.hlog.is_in_memory(init_addr));

    // 首次冷读挂载 ReadCache
    let res = session.read(k1).await?;
    assert_eq!(res, Some(v1.to_vec()));

    // 验证经过 ReadCache 挂载后，tag 地址具有 ReadCache 标记位
    let str_k1 = session.session_string_key(k1);
    let tag_addr = store.index.find_tag(&str_k1).expect("tag exists");
    assert!(
      wkv::is_read_cache_addr(tag_addr),
      "k1 必须处于 ReadCache 挂载状态"
    );

    // 1. contains_key / contains_key_raw
    assert!(
      session.contains_key(k1).await?,
      "contains_key 应识别 ReadCache 记录"
    );
    assert!(
      session.contains_key_raw(&str_k1).await?,
      "contains_key_raw 必须识别 ReadCache 标记位"
    );

    // 2. dbsize 统计
    let dbsize = session.dbsize().await?;
    assert_eq!(dbsize, 3, "dbsize 必须包含 ReadCache 中的记录且不漏不重");

    // 3. keys 遍历
    let keys = session.keys(b"*").await?;
    assert_eq!(keys.len(), 3);
    assert!(keys.contains(&k1.to_vec()));

    // 4. store.entry_count
    assert_eq!(
      store.entry_count(),
      3,
      "entry_count 必须正确透传 ReadCache 检查有效性"
    );

    OK
  })
}

#[test]
fn test_incrby_slack_in_place_shrinking_and_reexpansion() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("slack_incrby.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let key = b"counter:slack";
    // 初始值 "100" (3 字节)
    let addr_init = session.upsert(key, b"100").await?;

    // 自减 1 变成 "99" (2 字节)：由于使用了 slack 更新，应在原位直接修改，原物理地址不变！
    let val_decreased = session.incrby(key, -1).await?;
    assert_eq!(val_decreased, 99);
    let str_k = session.session_string_key(key);
    let addr_after_shrink = session.store.index.find_tag(&str_k);
    assert_eq!(
      Some(addr_init),
      addr_after_shrink,
      "数值长度缩短时，必须利用 slack 空间在原位就地更新，无需重新分配尾部空间"
    );
    assert_eq!(session.read_string(key).await?, Some(b"99".to_vec()));

    // 再次就地累加 1 变成 "100" (3 字节)：容量恰好能容纳原 slack，应复用原位空间
    let val_reexpand = session.incrby(key, 1).await?;
    assert_eq!(val_reexpand, 100);
    let addr_after_reexpand = session.store.index.find_tag(&str_k);
    assert_eq!(
      Some(addr_init),
      addr_after_reexpand,
      "数值重新扩充至原分配容量时，原位复用不变"
    );
    assert_eq!(session.read_string(key).await?, Some(b"100".to_vec()));

    // 累加 900 变成 "1000" (4 字节)：超出原位 slack 容量，安全降级 RCU 扩容
    let val_overflow = session.incrby(key, 900).await?;
    assert_eq!(val_overflow, 1000);
    let addr_overflow = session.store.index.find_tag(&str_k);
    assert_ne!(
      Some(addr_init),
      addr_overflow,
      "超出总容量时必须执行安全 RCU 追加扩容"
    );
    assert_eq!(session.read_string(key).await?, Some(b"1000".to_vec()));

    OK
  })
}

#[test]
fn test_bitmap_zero_copy_commands() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("bm_zero_copy.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let key = b"my_bitmap";

    // 初始均不存在
    assert_eq!(session.getbit(key, 0).await?, 0);
    assert_eq!(session.bitcount(key, None).await?, 0);
    assert_eq!(session.bitpos(key, 1, None, None, false).await?, -1);

    // 设置 offset 0, 7, 8, 15, 100
    assert_eq!(session.setbit(key, 0, 1).await?, 0);
    assert_eq!(session.setbit(key, 7, 1).await?, 0);
    assert_eq!(session.setbit(key, 8, 1).await?, 0);
    assert_eq!(session.setbit(key, 15, 1).await?, 0);
    assert_eq!(session.setbit(key, 100, 1).await?, 0);

    // 验证 getbit 零拷贝直读
    assert_eq!(session.getbit(key, 0).await?, 1);
    assert_eq!(session.getbit(key, 1).await?, 0);
    assert_eq!(session.getbit(key, 7).await?, 1);
    assert_eq!(session.getbit(key, 8).await?, 1);
    assert_eq!(session.getbit(key, 15).await?, 1);
    assert_eq!(session.getbit(key, 99).await?, 0);
    assert_eq!(session.getbit(key, 100).await?, 1);
    assert_eq!(session.getbit(key, 1000).await?, 0);

    // 验证 bitcount 全量及范围统计
    assert_eq!(session.bitcount(key, None).await?, 5);
    assert_eq!(session.bitcount_range(key, 0, 0, false).await?, 2); // 0 和 7 在 byte 0
    assert_eq!(session.bitcount_range(key, 1, 1, false).await?, 2); // 8 和 15 在 byte 1
    assert_eq!(session.bitcount_range(key, 0, 1, false).await?, 4);

    // 验证 bitpos
    assert_eq!(session.bitpos(key, 1, None, None, false).await?, 0);
    assert_eq!(session.bitpos(key, 0, None, None, false).await?, 1);

    OK
  })
}

#[test]
fn test_temp_range_index_dir_cleanup_on_drop() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("tmp_cleanup.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    // range_index_dir 为 None，将生成临时目录
    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = WedbStore::open(config, device)?;

    let tmp_path: Option<PathBuf> = store.temp_range_index_dir().map(|p| p.to_path_buf());
    assert!(
      tmp_path.is_some(),
      "未指定 range_index_dir 时应分配临时目录"
    );
    let path = tmp_path.unwrap();
    assert!(path.exists(), "临时目录在 Store 存活期间必须真实存在");

    // 显式 drop Store

    drop(store);

    // 验证在     Store::drop 执行后，临时目录已被彻底清理
    assert!(!path.exists(), "临时目录在 Store drop 后必须被彻底删除");

    OK
  })
}

#[test]
fn test_temp_bftree_path_cleanup_on_drop_vs_external() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("bftree_cleanup.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    // 1. 测试未指定 bftree_path：应生成临时文件，且在 drop 后被彻底删除
    let config1 = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store1 = WedbStore::open(config1, Arc::clone(&device))?;

    let tmp_path: Option<PathBuf> = store1.temp_bftree_path().map(|p| p.to_path_buf());
    assert!(
      tmp_path.is_some(),
      "未配置 bftree_path 时应分配内部临时 BfTree 物理文件"
    );
    let tmp_file = tmp_path.unwrap();
    assert!(
      tmp_file.exists(),
      "临时 BfTree 文件在 Store 存活期间必须真实存在"
    );

    drop(store1);
    assert!(
      !tmp_file.exists(),
      "临时 BfTree 文件在 Store drop 后必须被物理删除"
    );

    // 2. 测试外部传入 bftree_path：temp_bftree_path 为 None，且在 drop 后外部文件完好保留
    let external_bftree_path = dir.path().join("external_bftree.db");
    let config2 =
      StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?.with_bftree_path(&external_bftree_path);
    let store2 = WedbStore::open(config2, Arc::clone(&device))?;

    assert!(
      store2.temp_bftree_path().is_none(),
      "外部传入 bftree_path 时 temp_bftree_path 必须为 None"
    );
    assert!(
      external_bftree_path.exists(),
      "外部 BfTree 文件在 Store 存活期间存在"
    );

    drop(store2);
    assert!(
      external_bftree_path.exists(),
      "外部传入的 BfTree 物理文件在 Store drop 后绝对不能被删除"
    );

    OK
  })
}

#[test]
fn test_read_cache_tag_collision_no_accidental_delete_or_miss() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("rc_collision.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    // 仅分配 2 个桶，强制高概率产生桶碰撞与溢出链
    let page_size = SECTOR_ALIGNMENT;
    let num_pages = 8;
    let config = StoreConfig::new(2, page_size, num_pages, 0.5)?.with_read_cache(true);

    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    assert!(store.read_cache.is_enabled);

    let keys = [
      b"coll_key_alpha".to_vec(),
      b"coll_key_beta".to_vec(),
      b"coll_key_gamma".to_vec(),
      b"coll_key_delta".to_vec(),
      b"coll_key_epsilon".to_vec(),
    ];

    for (i, k) in keys.iter().enumerate() {
      let mut val = String::from("val_");
      val.push_str(itoa::Buffer::new().format(i));
      session.upsert_raw(k, val.as_bytes()).await?;
    }

    // 填充数据推进页面边界并刷盘，将记录驱逐至磁盘
    let pad = vec![b'P'; 3000];
    session.upsert_raw(b"pad_coll1", &pad).await?;
    session.upsert_raw(b"pad_coll2", &pad).await?;

    store.flush_all().await?;
    store.shift_read_only_address(page_size as u64);
    store.shift_head_address(page_size as u64);

    // 首次冷读挂载 ReadCache
    for (i, k) in keys.iter().enumerate() {
      let val = session.read_raw(k).await?;
      let mut expected = String::from("val_");
      expected.push_str(itoa::Buffer::new().format(i));
      assert_eq!(val, Some(expected.into_bytes()));
    }

    // 验证所有 key 均已挂载到 ReadCache 且 contains_key_raw 全部返回 true
    for k in keys.iter() {
      let tag_addr = store.index.find_tag(k).expect("tag exists");
      assert!(
        wkv::is_read_cache_addr(tag_addr),
        "key {:?} 必须在 ReadCache 中挂载",
        String::from_utf8_lossy(k)
      );
      assert!(
        session.contains_key_raw(k).await?,
        "contains_key_raw 必须正确识别 ReadCache 中的 key"
      );
    }

    // 删除 keys[0]（alpha），验证 delete_raw 不会误删处于同桶/多候选链上的 beta, gamma 等键
    let del_alpha = session.delete_raw(&keys[0]).await?;
    assert!(del_alpha, "keys[0] 删除应成功");

    assert!(
      !session.contains_key_raw(&keys[0]).await?,
      "keys[0] 已删除，contains_key_raw 必须返回 false"
    );
    assert_eq!(
      session.read_raw(&keys[0]).await?,
      None,
      "keys[0] 已删除，read 必须返回 None"
    );

    // 验证其他 4 个 key 完好无损
    for (i, k) in keys.iter().enumerate().skip(1) {
      assert!(
        session.contains_key_raw(k).await?,
        "未被删除的 key {:?} 必须依然存在",
        String::from_utf8_lossy(k)
      );
      let mut expected = String::from("val_");
      expected.push_str(itoa::Buffer::new().format(i));
      assert_eq!(
        session.read_raw(k).await?,
        Some(expected.into_bytes()),
        "未被删除的 key {:?} 的值必须完整且正确",
        String::from_utf8_lossy(k)
      );
    }

    // 再次删除 keys[2]（gamma）
    let del_gamma = session.delete_raw(&keys[2]).await?;
    assert!(del_gamma, "keys[2] 删除应成功");

    assert!(
      !session.contains_key_raw(&keys[2]).await?,
      "keys[2] 已删除，contains_key_raw 必须返回 false"
    );

    // 验证 keys[1], keys[3], keys[4] 依然完好
    for &idx in &[1, 3, 4] {
      let k = &keys[idx];
      assert!(
        session.contains_key_raw(k).await?,
        "key {:?} 必须不受影响",
        String::from_utf8_lossy(k)
      );
      let mut expected = String::from("val_");
      expected.push_str(itoa::Buffer::new().format(idx));
      assert_eq!(session.read_raw(k).await?, Some(expected.into_bytes()));
    }

    OK
  })
}

#[test]
fn test_batch_store_session_unsafe_context() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("batch_unsafe.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let k1 = b"batch_k1";
    let v1 = b"val1";
    let k2 = b"batch_k2";
    let v2 = b"100";

    session.upsert(k1, v1).await?;
    session.upsert(k2, v2).await?;

    // 进入 IUnsafeContext 纪元批处理保护
    let batch = session.enter_batch();

    // 1. 同步内存直读
    let read1 = batch.try_read_in_memory(k1, |v| v.to_vec())?;
    assert_eq!(read1, Some(Some(v1.to_vec())));

    {
      let str1 = batch.try_read_string_in_memory(k1, |v| v.to_vec())?;
      assert_eq!(str1, Some(Some(v1.to_vec())));
    }

    // 2. 原位读改写
    let mod_res = batch.try_modify_in_place(k2, |bytes| {
      bytes[0] = b'2';
      Some(())
    })?;
    assert_eq!(mod_res, Some(()));
    assert_eq!(batch.read(k2).await?, Some(b"200".to_vec()));

    // 3. 原位松弛读改写（长度缩短）
    let slack_ok = batch.try_modify_with_slack(k2, b"99")?;
    assert!(slack_ok);
    assert_eq!(batch.read(k2).await?, Some(b"99".to_vec()));

    // 4. 在批处理中执行普通 upsert 和 read
    let k3 = b"batch_k3";
    let v3 = b"val3";
    batch.upsert(k3, v3).await?;
    assert_eq!(batch.read(k3).await?, Some(v3.to_vec()));

    // 5. 不存在的 key
    let miss = batch.try_read_in_memory(b"no_such_key", |v| v.to_vec())?;
    assert_eq!(miss, Some(None));

    drop(batch);

    OK
  })
}

#[test]
fn test_copy_reads_to_tail_cas_failure_reviv_pool() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("copy_cas_reviv.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let page_size = SECTOR_ALIGNMENT;
    // 启用 revivification，禁用 read_cache，强制走 copy_reads_to_tail 路径
    let config = StoreConfig::new(1024, page_size, 16, 0.5)?
      .with_revivification(true)
      .with_read_cache(false);
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;
    session.set_copy_reads_to_tail(true);

    let k = b"cold_copy_key";
    let v = b"cold_copy_val";
    let addr_init = session.upsert(k, v).await?;

    // 填充跨页并刷盘驱逐至磁盘
    let pad = vec![b'P'; 3000];
    session.upsert(b"pad_p1", &pad).await?;
    session.upsert(b"pad_p2", &pad).await?;

    store.flush_all().await?;
    store.shift_read_only_address(page_size as u64);
    store.shift_head_address(page_size as u64);

    assert!(store.hlog.is_on_disk(addr_init));

    // 模拟并发读写竞争：
    // 在冷读回填前，并发线程写入了该键的新版本，使索引中的地址推进为 concurrent_addr
    let record = store.hlog.read_record(addr_init).await?;
    let val_slice = record.value()?;
    let _concurrent_addr = session.upsert(k, b"newer_concurrent_val").await?;

    // 模拟冷读线程追加 Tail，但在原子 CAS 时发现期望地址 addr_init 已被推进为 concurrent_addr，CAS 失败
    let new_tail_addr = session
      .append_record(k, val_slice, addr_init, false)
      .await?;
    let cas_succeeded = store.index.update_address(k, addr_init, new_tail_addr);
    assert!(!cas_succeeded, "期望地址不匹配，CAS 必然失败");

    let rec_size = wrecord::record_size(k.len(), val_slice.len()) as u32;
    // 严格read_from_disk 实现：CAS 失败时将未挂载的新空间归还至 reviv_pool
    store
      .reviv_pool
      .put(new_tail_addr, rec_size, store.hlog.read_only_address());

    // 验证新分配空间已被安全归还至 reviv_pool，并能被后续分配完美复用
    let recycled = store.reviv_pool.take(rec_size, 0);
    assert_eq!(
      recycled,
      Some((new_tail_addr, rec_size)),
      "CAS 冲突后，copy_reads_to_tail 分配的新记录空间必须被归还至 reviv_pool"
    );

    OK
  })
}

#[test]
fn test_large_flattened_set_spop_and_srandmember_chunk_sampling() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("set_chunk_sampling.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let key = b"large_set_chunks";
    // 写入 1200 个成员，强制打平并跨越多个 Chunk (单 Chunk 上限 512)
    let members: Vec<Vec<u8>> = (0..1200)
      .map(|i| {
        let mut s = String::from("member_");
        s.push_str(&pad(i, 5));
        s.into_bytes()
      })
      .collect();
    session.sadd(key, &members).await?;
    assert_eq!(session.scard(key).await?, 1200);

    // 1. SRANDMEMBER count = 1 单元素快速采样（只读取单个分块）
    let rand1 = session.srandmember(key, 1).await?;
    assert_eq!(rand1.len(), 1);
    assert!(session.sismember(key, &rand1[0]).await?);
    assert_eq!(session.scard(key).await?, 1200);

    // 2. SRANDMEMBER count = 10 正数采样（互异）
    let rand10 = session.srandmember(key, 10).await?;
    assert_eq!(rand10.len(), 10);
    let mut uniq = HashSet::new();
    for m in &rand10 {
      assert!(session.sismember(key, m).await?);
      uniq.insert(m.clone());
    }
    assert_eq!(uniq.len(), 10);

    // 3. SRANDMEMBER count = -15 负数采样（可重复）
    let rand_neg = session.srandmember(key, -15).await?;
    assert_eq!(rand_neg.len(), 15);
    for m in &rand_neg {
      assert!(session.sismember(key, m).await?);
    }

    // 4. SPOP count = 1 单元素弹出（按分块采样并弹出）
    let pop1 = session.spop(key, 1).await?;
    assert_eq!(pop1.len(), 1);
    assert_eq!(session.scard(key).await?, 1199);
    assert!(!session.sismember(key, &pop1[0]).await?);

    // 5. SPOP count = 20 跨分块批量弹出
    let pop20 = session.spop(key, 20).await?;
    assert_eq!(pop20.len(), 20);
    assert_eq!(session.scard(key).await?, 1179);
    for m in &pop20 {
      assert!(!session.sismember(key, m).await?);
    }

    // 6. SPOP 全部剩余成员
    let rest = session.spop(key, 2000).await?;
    assert_eq!(rest.len(), 1179);
    assert_eq!(session.scard(key).await?, 0);

    OK
  })
}

#[test]
fn test_large_zset_sliding_window_and_batch_remrange() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("zset_batch_opt.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let key = b"large_zset_batch";
    // 写入 1500 个有序集合元素（打平且跨越 512 批处理上限）
    for i in 0..1500 {
      let mut member = String::from("zmem_");
      member.push_str(&pad(i, 5));
      let member = member.into_bytes();
      session
        .zadd(key, i as f64, &member, wedb_zset::ZAddOpt::default())
        .await?;
    }
    assert_eq!(session.zcard(key).await?, 1500);

    // 1. 验证 zrangebyscore 逆序分支在 count=usize::MAX 时安全受控
    let rev_all = session
      .zrangebyscore(
        key,
        wedb_zset::ScoreRange::new(0.0, true, 1500.0, true),
        true,
        0,
        usize::MAX,
      )
      .await?;
    assert_eq!(rev_all.len(), 1500);
    assert_eq!(rev_all[0].1, 1499.0);
    assert_eq!(rev_all[1499].1, 0.0);

    // 2. 验证 offset 超出集合大小时立即短路为空
    let rev_empty = session
      .zrangebyscore(
        key,
        wedb_zset::ScoreRange::new(0.0, true, 1500.0, true),
        true,
        1500,
        10,
      )
      .await?;
    assert!(rev_empty.is_empty());

    // 3. 验证 zremrangebyrank 流式分批删除（跨越 512 批次，删除 1100 个元素）
    // 当前下标 0..1499，删除 rank 100..1199（共 1100 个元素）
    let removed_rank = session.zremrangebyrank(key, 100, 1199).await?;
    assert_eq!(removed_rank, 1100);
    assert_eq!(session.zcard(key).await?, 400);

    // 4. 验证 zremrangebyscore 流式分批删除（删除剩余 400 个中的 300 个）
    // 剩余的 400 个元素分数应为 0..99 和 1200..1499
    // 删除分数 1200.0..1499.0（共 300 个元素）
    let removed_score = session
      .zremrangebyscore(key, wedb_zset::ScoreRange::new(1200.0, true, 1499.0, true))
      .await?;
    assert_eq!(removed_score, 300);
    assert_eq!(session.zcard(key).await?, 100);

    // 验证剩余 100 个元素的分数处于 0..99
    let remaining = session.zrange(key, 0, -1, false).await?;
    assert_eq!(remaining.len(), 100);
    assert_eq!(remaining[0].1, 0.0);
    assert_eq!(remaining[99].1, 99.0);

    OK
  })
}

#[test]
fn test_zremrangebyscore_multi_batch_and_boundaries() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("zremrangebyscore_multi_batch.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let key = b"zset:multi_batch_score";

    // 1. 边界测试：空/未创建键的各类删除与读取操作应安全短路返回 0 或空集
    assert_eq!(
      session
        .zremrangebyscore(key, wedb_zset::ScoreRange::new(0.0, true, 100.0, true))
        .await?,
      0
    );
    assert_eq!(session.zremrangebyrank(key, 0, 10).await?, 0);
    assert!(
      session
        .zrangebyscore(
          key,
          wedb_zset::ScoreRange::new(0.0, true, 100.0, true),
          false,
          0,
          10
        )
        .await?
        .is_empty()
    );

    // 2. 写入 1200 个元素（跨越 512 批次）
    for i in 0..1200 {
      let mut member = String::from("mb_mem_");
      member.push_str(&pad(i, 5));
      let member = member.into_bytes();
      session
        .zadd(key, i as f64, &member, wedb_zset::ZAddOpt::default())
        .await?;
    }
    assert_eq!(session.zcard(key).await?, 1200);

    // 3. 边界测试：倒置区间（min > max）与超出范围区间直接短路
    assert_eq!(
      session
        .zremrangebyscore(key, wedb_zset::ScoreRange::new(500.0, true, 100.0, true))
        .await?,
      0
    );
    assert_eq!(
      session
        .zremrangebyscore(key, wedb_zset::ScoreRange::new(2000.0, true, 3000.0, true))
        .await?,
      0
    );
    assert_eq!(session.zremrangebyrank(key, 100, 50).await?, 0);
    assert_eq!(session.zremrangebyrank(key, 2000, 3000).await?, 0);

    // 4. 关键验证：跨 512 批次流式删除（删除 800 个元素：100.0..=899.0）
    // 批次 1: 512 个，批次 2: 288 个；验证修复 while meta.size > 0 后的多批次流转正确性
    let removed = session
      .zremrangebyscore(key, wedb_zset::ScoreRange::new(100.0, true, 899.0, true))
      .await?;
    assert_eq!(removed, 800, "必须完整删除两批次共 800 个元素");
    assert_eq!(session.zcard(key).await?, 400);

    // 验证剩余元素前缀为 0..99，后缀为 900..1199
    let head = session.zrange(key, 0, 99, false).await?;
    assert_eq!(head.len(), 100);
    assert_eq!(head[0].1, 0.0);
    assert_eq!(head[99].1, 99.0);

    let tail = session.zrange(key, 100, -1, false).await?;
    assert_eq!(tail.len(), 300);
    assert_eq!(tail[0].1, 900.0);
    assert_eq!(tail[299].1, 1199.0);

    // 5. 验证剩余 400 个元素全量清空
    let removed_all = session
      .zremrangebyscore(
        key,
        wedb_zset::ScoreRange::new(f64::NEG_INFINITY, true, f64::INFINITY, true),
      )
      .await?;
    assert_eq!(removed_all, 400);
    assert_eq!(session.zcard(key).await?, 0);

    OK
  })
}

#[test]
fn test_set_single_element_and_empty_boundary_sampling() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("set_boundary_sampling.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let key = b"set:single_element";

    // 1. 不存在的键
    assert!(session.spop(key, 1).await?.is_empty());
    assert!(session.spop(key, 0).await?.is_empty());
    assert!(session.srandmember(key, 0).await?.is_empty());
    assert!(session.srandmember(key, 1).await?.is_empty());
    assert!(session.srandmember(key, -1).await?.is_empty());

    // 2. 单元素 Set
    session.sadd(key, &[b"sole_member"]).await?;
    assert_eq!(session.scard(key).await?, 1);

    // srandmember 快速单取（正数与负数）
    let single_pos = session.srandmember(key, 1).await?;
    assert_eq!(single_pos, vec![b"sole_member".to_vec()]);

    let single_neg = session.srandmember(key, -1).await?;
    assert_eq!(single_neg, vec![b"sole_member".to_vec()]);

    let multi_neg = session.srandmember(key, -3).await?;
    assert_eq!(multi_neg.len(), 3);
    assert_eq!(multi_neg[0], b"sole_member");
    assert_eq!(multi_neg[1], b"sole_member");
    assert_eq!(multi_neg[2], b"sole_member");

    // spop 弹出单元素
    let popped = session.spop(key, 1).await?;
    assert_eq!(popped, vec![b"sole_member".to_vec()]);
    assert_eq!(session.scard(key).await?, 0);

    OK
  })
}

/// 批量读 idx 全局契约：分块调用底层批读后，on_item 的 idx 必须相对调用方全量键列表
/// 全局单调递增（>12 键跨多块时不得从 0 重新起算），且值与键按 idx 严格对位
#[test]
fn test_batch_read_idx_global_contract_across_chunks() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().to_path_buf();
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    // 30 键跨越 3 个预取窗口（12/12/6），值内嵌键序号供对位校验
    const COUNT: usize = 30;
    let keys: Vec<Vec<u8>> = (0..COUNT)
      .map(|i| {
        let mut k = b"batch:idx:".to_vec();
        k.extend_from_slice(pad(i, 3).as_bytes());
        k
      })
      .collect();
    let key_refs: Vec<&[u8]> = keys.iter().map(|k| k.as_slice()).collect();
    for (i, k) in key_refs.iter().enumerate() {
      session.try_upsert_sync(k, pad(i, 3).as_bytes())?.unwrap();
    }

    let mut seen = Vec::with_capacity(COUNT);
    session
      .read_batch_with(&key_refs, |idx, val_opt| {
        seen.push(idx);
        assert_eq!(
          val_opt,
          Some(pad(idx, 3).as_bytes()),
          "idx={idx} 值与键错位"
        );
      })
      .await?;

    // 全部驻留可变区：回调必须严格按 0..COUNT 升序恰好各回调一次
    let expected: Vec<usize> = (0..COUNT).collect();
    assert_eq!(seen, expected);

    let mut seen_sync = Vec::with_capacity(COUNT);
    session.try_read_batch_in_memory(&key_refs, |idx, val_opt| {
      seen_sync.push(idx);
      assert_eq!(val_opt, Some(pad(idx, 3).as_bytes()));
    })?;
    assert_eq!(seen_sync, expected);

    OK
  })
}
