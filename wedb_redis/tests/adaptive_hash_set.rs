use std::{iter::repeat_n, sync::Arc, time::Duration};

use aok::{OK, Void};
use compio::{runtime::Runtime, time::sleep};
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_hash::{ExpireOpt as HashExpireOpt, ExpireResult as HashExpireResult};
use wedb_redis::prelude::*;
use wkv::{
  HASH_MAX_COMPACT_ENTRIES, HASH_MAX_COMPACT_VALUE, MAX_COMPACT_TOTAL_BYTES,
  SET_MAX_COMPACT_ENTRIES, SET_MAX_COMPACT_VALUE, StorageEncoding, StoreConfig, WedbStore,
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

/// 辅助函数：快速创建测试用 WedbStore
fn create_test_store() -> aok::Result<(tempfile::TempDir, Arc<WedbStore<SegmentedDevice>>)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("adaptive.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(4096, 64 * 1024, 128, 0.5)?,
    device,
  )?);
  Ok((dir, store))
}

/// 测试 1: 小 Hash 紧凑内联生命周期完整验证
#[test]
fn test_adaptive_hash_compact_lifecycle() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:compact:lifecycle";

    // 初始状态
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.hlen(key).await?, 0);

    // 1. 初始 HSET
    assert!(session.hset(key, b"f1".to_vec(), b"v1".to_vec()).await?);
    assert_eq!(session.type_of(key).await?, "hash");

    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.encoding(), StorageEncoding::Compact);
    assert_eq!(session.hlen(key).await?, 1);
    assert!(session.hexists(key, b"f1").await?);
    assert!(!session.hexists(key, b"f2").await?);
    assert_eq!(session.hstrlen(key, b"f1").await?, 2);
    assert_eq!(session.hget(key, b"f1").await?, Some(b"v1".to_vec()));

    // 覆盖更新现有字段
    assert!(
      !session
        .hset(key, b"f1".to_vec(), b"v1_updated".to_vec())
        .await?
    );
    assert_eq!(
      session.hget(key, b"f1").await?,
      Some(b"v1_updated".to_vec())
    );
    assert_eq!(session.hlen(key).await?, 1);

    // 2. HMSET & HMGET
    session
      .hmset(
        key,
        vec![
          (b"f2".to_vec(), b"v2".to_vec()),
          (b"f3".to_vec(), b"v3".to_vec()),
        ],
      )
      .await?;
    assert_eq!(session.hlen(key).await?, 3);
    let mget_res = session
      .hmget(key, &[b"f1", b"f2", b"f3", b"f_none"])
      .await?;
    assert_eq!(
      mget_res,
      vec![
        Some(b"v1_updated".to_vec()),
        Some(b"v2".to_vec()),
        Some(b"v3".to_vec()),
        None,
      ]
    );

    // 3. HINCRBY & HINCRBYFLOAT
    assert_eq!(session.hincrby(key, b"counter", 10).await?, 10);
    assert_eq!(session.hincrby(key, b"counter", -3).await?, 7);
    let float_res = session.hincrbyfloat(key, b"f_float", 2.5).await?;
    assert!((float_res - 2.5).abs() < 1e-6);
    let float_res2 = session.hincrbyfloat(key, b"f_float", 1.25).await?;
    assert!((float_res2 - 3.75).abs() < 1e-6);

    // 4. HEXPIRE, HTTL, HPERSIST
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let exp_res = session
      .hexpire(key, b"f2", now + 3_600_000, HashExpireOpt::NONE)
      .await?;
    assert_eq!(exp_res, HashExpireResult::Ok);
    let ttl2 = session.httl(key, b"f2").await?;
    assert!(ttl2 > 0 && ttl2 <= 3_600_000);
    let ttl3 = session.httl(key, b"f3").await?;
    assert_eq!(ttl3, -1);
    let persist_res = session.hpersist(key, b"f2").await?;
    assert!(persist_res);
    let ttl2_after = session.httl(key, b"f2").await?;
    assert_eq!(ttl2_after, -1);

    // 5. HGETALL, HKEYS & HVALS
    let all = session.hgetall(key).await?;
    assert_eq!(all.len(), 5); // f1, f2, f3, counter, f_float
    let keys = session.hkeys(key).await?;
    assert_eq!(keys.len(), 5);
    let vals = session.hvals(key).await?;
    assert_eq!(vals.len(), 5);

    // 6. HSCAN (全量与分页游标早停验证)
    let (cur, scanned) = session.hscan(key, 0, 100, Some(b"*")).await?;
    assert_eq!(cur, 0);
    assert_eq!(scanned.len(), 5);
    let (cur_p1, p1) = session.hscan(key, 0, 2, Some(b"*")).await?;
    assert_eq!(cur_p1, 2);
    assert_eq!(p1.len(), 2);
    let (cur_p2, p2) = session.hscan(key, cur_p1, 10, Some(b"*")).await?;
    assert_eq!(cur_p2, 0);
    assert_eq!(p2.len(), 3);

    // 确认在此期间依然保持 Compact 编码
    let meta_before_del = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_before_del.encoding(), StorageEncoding::Compact);

    // 7. HDEL 部分删除
    let deleted = session.hdel(key, &[b"counter", b"f_float"]).await?;
    assert_eq!(deleted, 2);
    assert_eq!(session.hlen(key).await?, 3);

    // 8. HDEL 删空剩余字段，验证 Meta 自动清除
    let deleted_rest = session.hdel(key, &[b"f1", b"f2", b"f3"]).await?;
    assert_eq!(deleted_rest, 3);
    assert_eq!(session.type_of(key).await?, "none");
    assert!(session.load_meta(key).await?.is_none());

    // 9. 紧凑模式单键快速 drop / delete
    let drop_key = b"hash:compact:drop";
    session.hset(drop_key, b"k".to_vec(), b"v".to_vec()).await?;
    assert_eq!(
      session.load_meta(drop_key).await?.expect("meta").encoding(),
      StorageEncoding::Compact
    );
    assert!(session.delete(drop_key).await?);
    assert_eq!(session.type_of(drop_key).await?, "none");
    assert!(session.load_meta(drop_key).await?.is_none());

    OK
  })
}

/// 测试 2: Hash 数量阈值跃迁 (128 Compact -> 129 Flattened)
#[test]
fn test_adaptive_hash_promotion_threshold_count() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:threshold:count";

    // 写入 512 个字段 (阈值以内，使用 2 字节 field 与空 value 确保总大小 3586 字节 <= MAX_COMPACT_TOTAL_BYTES)
    for i in 0..HASH_MAX_COMPACT_ENTRIES {
      let field = (i as u16).to_be_bytes();
      let value = b"";
      assert!(session.hset(key, field, value).await?);
    }

    assert_eq!(session.hlen(key).await?, HASH_MAX_COMPACT_ENTRIES);
    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.encoding(), StorageEncoding::Compact);
    assert_eq!(session.hlen(key).await?, HASH_MAX_COMPACT_ENTRIES);

    // 写入第 513 个字段，触发自动跃迁
    let f_513 = (HASH_MAX_COMPACT_ENTRIES as u16).to_be_bytes();
    let v_513 = b"val_513";
    assert!(session.hset(key, f_513, v_513).await?);

    // 验证跃迁后编码为 Flattened，chunk_id 已分配
    let meta_promoted = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_promoted.encoding(), StorageEncoding::Flattened);
    let chunk_info = session.get_collection_chunk_info(key).await?;
    assert!(
      chunk_info.is_some(),
      "chunk info should exist after promotion"
    );
    assert!(chunk_info.unwrap().0 > 0, "chunk_id must be assigned (> 0)");
    assert_eq!(session.hlen(key).await?, HASH_MAX_COMPACT_ENTRIES + 1);

    // 验证全部 513 个字段均可读取
    for i in 0..HASH_MAX_COMPACT_ENTRIES {
      let f = (i as u16).to_be_bytes();
      let v = session.hget(key, &f).await?;
      assert_eq!(v, Some(b"".to_vec()));
    }
    let v_last = session.hget(key, &f_513).await?;
    assert_eq!(v_last, Some(v_513.to_vec()));

    // 在 Flattened 模式下继续追加写入
    let f_extra = b"f_extra";
    let v_extra = b"v_extra";
    assert!(session.hset(key, f_extra, v_extra).await?);
    assert_eq!(session.hlen(key).await?, HASH_MAX_COMPACT_ENTRIES + 2);

    // 在 Flattened 模式下删除字段
    assert_eq!(session.hdel(key, &[f_extra]).await?, 1);
    assert_eq!(session.hlen(key).await?, HASH_MAX_COMPACT_ENTRIES + 1);

    // 删除整个 Flattened 键，验证资源妥善清理
    assert!(session.delete(key).await?);
    assert_eq!(session.type_of(key).await?, "none");

    OK
  })
}

/// 测试 3: Hash 大 Value 阈值跃迁 (> 64 字节)
#[test]
fn test_adaptive_hash_promotion_threshold_large_value() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // 场景 A: 从 Compact 跃迁到 Flattened
    let key1 = b"hash:large_val:promote";
    session
      .hset(key1, b"small_f".to_vec(), b"small_v".to_vec())
      .await?;
    assert_eq!(
      session.load_meta(key1).await?.unwrap().encoding(),
      StorageEncoding::Compact
    );

    // 写入超过 64 字节的大 Value
    let large_val = vec![b'A'; HASH_MAX_COMPACT_VALUE + 1];
    session
      .hset(key1, b"large_f".to_vec(), large_val.clone())
      .await?;

    let meta1 = session.load_meta(key1).await?.unwrap();
    assert_eq!(meta1.encoding(), StorageEncoding::Flattened);
    assert_eq!(session.hlen(key1).await?, 2);
    assert_eq!(
      session.hget(key1, b"small_f").await?,
      Some(b"small_v".to_vec())
    );
    assert_eq!(session.hget(key1, b"large_f").await?, Some(large_val));

    // 场景 B: 初始 HSET 大 Value 直接初始化为 Flattened 模式
    let key2 = b"hash:large_val:direct";
    let large_val2 = vec![b'B'; HASH_MAX_COMPACT_VALUE + 10];
    session
      .hset(key2, b"f_big".to_vec(), large_val2.clone())
      .await?;

    let meta2 = session.load_meta(key2).await?.unwrap();
    assert_eq!(meta2.encoding(), StorageEncoding::Flattened);
    assert_eq!(session.hlen(key2).await?, 1);
    assert_eq!(session.hget(key2, b"f_big").await?, Some(large_val2));

    // 场景 C: 初始 HMSET 包含大 Value 直接初始化为 Flattened 模式
    let key3 = b"hash:large_val:hmset_direct";
    let large_val3 = vec![b'C'; 100];
    session
      .hmset(
        key3,
        vec![
          (b"f1".to_vec(), b"v1".to_vec()),
          (b"f2".to_vec(), large_val3.clone()),
        ],
      )
      .await?;

    let meta3 = session.load_meta(key3).await?.unwrap();
    assert_eq!(meta3.encoding(), StorageEncoding::Flattened);
    assert_eq!(session.hlen(key3).await?, 2);
    assert_eq!(session.hget(key3, b"f2").await?, Some(large_val3));

    OK
  })
}

/// 测试 4: 小 Set 紧凑内联生命周期完整验证
#[test]
fn test_adaptive_set_compact_lifecycle() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"set:compact:lifecycle";

    // 初始状态
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.scard(key).await?, 0);

    // 1. SADD 元素与去重
    assert_eq!(session.sadd(key, [b"m1", b"m2", b"m3"]).await?, 3);
    assert_eq!(session.sadd(key, [b"m1", b"m4"]).await?, 1); // m1 重复，只新增 m4
    assert_eq!(session.type_of(key).await?, "set");

    // 验证处于 Compact 模式
    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.encoding(), StorageEncoding::Compact);
    assert_eq!(session.scard(key).await?, 4);

    // 2. SISMEMBER & SMISMEMBER
    assert!(session.sismember(key, b"m1").await?);
    assert!(!session.sismember(key, b"m99").await?);
    assert_eq!(
      session.smismember(key, &[b"m1", b"m99", b"m2"]).await?,
      vec![true, false, true]
    );

    // 3. SMEMBERS
    let mut members = session.smembers(key).await?;
    members.sort();
    assert_eq!(
      members,
      vec![
        b"m1".to_vec(),
        b"m2".to_vec(),
        b"m3".to_vec(),
        b"m4".to_vec()
      ]
    );

    // 4. SRANDMEMBER
    let rand_m = session.srandmember(key, 2).await?;
    assert_eq!(rand_m.len(), 2);
    for m in &rand_m {
      assert!(session.sismember(key, m).await?);
    }

    // 5. SPOP
    let popped = session.spop(key, 1).await?;
    assert_eq!(popped.len(), 1);
    assert_eq!(session.scard(key).await?, 3);
    assert!(!session.sismember(key, &popped[0]).await?);

    // 6. SREM
    let rem_cnt = session.srem(key, &[b"m1", b"m2", b"m99"]).await?;
    // m1 或 m2 可能之前被 pop 走了一个，或者都在
    assert!((1..=2).contains(&rem_cnt));

    // 7. SMOVE
    let dest_key = b"set:compact:dest";
    let remaining = session.smembers(key).await?;
    assert!(!remaining.is_empty());
    let target_elem = remaining[0].clone();
    assert!(session.smove(key, dest_key, &target_elem).await?);
    assert!(!session.sismember(key, &target_elem).await?);
    assert!(session.sismember(dest_key, &target_elem).await?);
    assert_eq!(session.scard(dest_key).await?, 1);

    // 8. 删空剩余元素，Meta 自动清除
    let rest = session.smembers(key).await?;
    let rest_slices: Vec<&[u8]> = rest.iter().map(|v| v.as_slice()).collect();
    if !rest_slices.is_empty() {
      session.srem(key, &rest_slices).await?;
    }
    assert_eq!(session.scard(key).await?, 0);
    assert_eq!(session.type_of(key).await?, "none");
    assert!(session.load_meta(key).await?.is_none());

    // 9. SSCAN 在 Compact 模式下遍历 dest_key 与分页游标验证
    let (cur, scanned) = session.sscan(dest_key, 0, 10, Some(b"*")).await?;
    assert_eq!(cur, 0);
    assert_eq!(scanned.len(), 1);
    assert_eq!(scanned[0], target_elem);

    session
      .sadd(dest_key, [b"elem_a", b"elem_b", b"elem_c"])
      .await?;
    let (c1, p1) = session.sscan(dest_key, 0, 2, Some(b"*")).await?;
    assert_eq!(c1, 2);
    assert_eq!(p1.len(), 2);
    let (c2, p2) = session.sscan(dest_key, c1, 10, Some(b"*")).await?;
    assert_eq!(c2, 0);
    assert_eq!(p2.len(), 2);

    // 10. 紧凑模式单键快速 drop / delete
    assert!(session.delete(dest_key).await?);
    assert_eq!(session.type_of(dest_key).await?, "none");

    OK
  })
}

/// 测试 5: Set 数量阈值跃迁 (128 Compact -> 129 Flattened)
#[test]
fn test_adaptive_set_promotion_threshold_count() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"set:threshold:count";

    // 批量写入 128 个元素 (阈值以内)
    let members: Vec<Vec<u8>> = (0..SET_MAX_COMPACT_ENTRIES)
      .map(|i| format!("m_{}", pad(i, 4)).into_bytes())
      .collect();
    assert_eq!(
      session.sadd(key, members.clone()).await?,
      SET_MAX_COMPACT_ENTRIES
    );

    assert_eq!(session.scard(key).await?, SET_MAX_COMPACT_ENTRIES);
    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.encoding(), StorageEncoding::Compact);

    // 写入第 129 个元素，触发自动跃迁
    let m_129 = format!("m_{}", pad(SET_MAX_COMPACT_ENTRIES, 4)).into_bytes();
    assert_eq!(session.sadd(key, [m_129.clone()]).await?, 1);

    // 验证跃迁后编码为 Flattened，chunk_id 已分配
    let meta_promoted = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_promoted.encoding(), StorageEncoding::Flattened);
    let chunk_info = session.get_collection_chunk_info(key).await?;
    assert!(
      chunk_info.is_some(),
      "chunk info should exist after promotion"
    );
    assert!(chunk_info.unwrap().0 > 0, "chunk_id must be assigned (> 0)");
    assert_eq!(session.scard(key).await?, SET_MAX_COMPACT_ENTRIES + 1);

    // 验证全部 129 个元素 sismember 均为 true
    for i in 0..=SET_MAX_COMPACT_ENTRIES {
      let m = format!("m_{}", pad(i, 4)).into_bytes();
      assert!(
        session.sismember(key, &m).await?,
        "member {:?} must exist",
        String::from_utf8_lossy(&m)
      );
    }

    // 在 Flattened 模式下继续追加写入
    let m_extra = b"m_extra";
    assert_eq!(session.sadd(key, [m_extra]).await?, 1);
    assert_eq!(session.scard(key).await?, SET_MAX_COMPACT_ENTRIES + 2);

    // 在 Flattened 模式下删除元素
    assert_eq!(session.srem(key, &[m_extra]).await?, 1);
    assert_eq!(session.scard(key).await?, SET_MAX_COMPACT_ENTRIES + 1);

    // 删除整个 Flattened 键，验证资源妥善清理
    assert!(session.delete(key).await?);
    assert_eq!(session.type_of(key).await?, "none");

    OK
  })
}

/// 测试 6: Set 大元素阈值跃迁 (> 64 字节)
#[test]
fn test_adaptive_set_promotion_threshold_large_member() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // 场景 A: 从 Compact 跃迁到 Flattened
    let key1 = b"set:large_mem:promote";
    assert_eq!(session.sadd(key1, [b"small_m"]).await?, 1);
    assert_eq!(
      session.load_meta(key1).await?.unwrap().encoding(),
      StorageEncoding::Compact
    );

    // 写入超过 64 字节的大成员
    let large_mem = vec![b'S'; SET_MAX_COMPACT_VALUE + 1];
    assert_eq!(session.sadd(key1, [large_mem.as_slice()]).await?, 1);

    let meta1 = session.load_meta(key1).await?.unwrap();
    assert_eq!(meta1.encoding(), StorageEncoding::Flattened);
    assert_eq!(session.scard(key1).await?, 2);
    assert!(session.sismember(key1, b"small_m").await?);
    assert!(session.sismember(key1, &large_mem).await?);

    // 场景 B: 初始 SADD 大成员直接初始化为 Flattened 模式
    let key2 = b"set:large_mem:direct";
    let large_mem2 = vec![b'Z'; SET_MAX_COMPACT_VALUE + 10];
    assert_eq!(session.sadd(key2, [large_mem2.as_slice()]).await?, 1);

    let meta2 = session.load_meta(key2).await?.unwrap();
    assert_eq!(meta2.encoding(), StorageEncoding::Flattened);
    assert_eq!(session.scard(key2).await?, 1);
    assert!(session.sismember(key2, &large_mem2).await?);

    // 场景 C: 初始批量 SADD 包含大成员直接初始化为 Flattened 模式
    let key3 = b"set:large_mem:batch_direct";
    let large_mem3 = vec![b'K'; 80];
    assert_eq!(
      session
        .sadd(key3, [b"m_tiny".as_slice(), large_mem3.as_slice()])
        .await?,
      2
    );

    let meta3 = session.load_meta(key3).await?.unwrap();
    assert_eq!(meta3.encoding(), StorageEncoding::Flattened);
    assert_eq!(session.scard(key3).await?, 2);
    assert!(session.sismember(key3, b"m_tiny").await?);
    assert!(session.sismember(key3, &large_mem3).await?);

    OK
  })
}

/// 测试 7: Compact 模式下字段过期覆盖写入时元数据大小一致性验证
#[test]
fn test_adaptive_hash_compact_expired_field_overwrite_and_meta_size() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:compact:expired_overwrite";

    // 1. 写入 f1 并设置未来的过期时间（10ms），休眠 20ms 触发惰性过期
    assert!(session.hset(key, b"f1".to_vec(), b"v1".to_vec()).await?);
    assert_eq!(session.hlen(key).await?, 1);

    let now = coarsetime::Clock::now_since_epoch().as_millis();
    assert_eq!(
      session
        .hexpire(key, b"f1", now + 10, HashExpireOpt::NONE)
        .await?,
      HashExpireResult::Ok
    );

    sleep(Duration::from_millis(25)).await;

    // 惰性过期后读取应为 None
    assert_eq!(session.hget(key, b"f1").await?, None);

    // 2. 覆盖写入已惰性过期的 f1（此时物理条目尚存但逻辑已过期），必须返回 is_new=true，且 meta.size 严谨同步为 1
    assert!(
      session
        .hset(key, b"f1".to_vec(), b"v1_revived".to_vec())
        .await?
    );
    assert_eq!(
      session.hget(key, b"f1").await?,
      Some(b"v1_revived".to_vec())
    );
    assert_eq!(session.hlen(key).await?, 1);
    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.size, 1);
    assert_eq!(meta.encoding(), StorageEncoding::Compact);

    // 3. 再次设置短暂过期并休眠，测试 HSETNX 覆盖惰性过期字段
    let now2 = coarsetime::Clock::now_since_epoch().as_millis();
    assert_eq!(
      session
        .hexpire(key, b"f1", now2 + 10, HashExpireOpt::NONE)
        .await?,
      HashExpireResult::Ok
    );
    sleep(Duration::from_millis(25)).await;

    assert!(
      session
        .hsetnx(key, b"f1".to_vec(), b"v1_hsetnx".to_vec())
        .await?
    );
    assert_eq!(session.hget(key, b"f1").await?, Some(b"v1_hsetnx".to_vec()));
    assert_eq!(session.hlen(key).await?, 1);
    let meta2 = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta2.size, 1);

    // 4. HMSET 混合写入（已有字段 f1 与新字段 f2）
    session
      .hmset(
        key,
        vec![
          (b"f1".to_vec(), b"v1_final".to_vec()),
          (b"f2".to_vec(), b"v2".to_vec()),
        ],
      )
      .await?;
    assert_eq!(session.hlen(key).await?, 2);
    let meta3 = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta3.size, 2);

    // 5. 主动过期：设置过去的时间戳，立即删除字段并返回 KeyAlreadyExpired
    let past_time = coarsetime::Clock::now_since_epoch()
      .as_millis()
      .saturating_sub(1000);
    assert_eq!(
      session
        .hexpire(key, b"f2", past_time, HashExpireOpt::NONE)
        .await?,
      HashExpireResult::KeyAlreadyExpired
    );
    assert_eq!(session.hget(key, b"f2").await?, None);
    assert_eq!(session.hlen(key).await?, 1);

    OK
  })
}

/// 测试 8: Set 跃迁后因重复成员导致新增为 0 时的元数据原子持久化验证
#[test]
fn test_adaptive_set_promotion_with_duplicate_members() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"set:promote:dup_members";

    // 写入 128 个元素 (Compact 满载)
    let members: Vec<Vec<u8>> = (0..SET_MAX_COMPACT_ENTRIES)
      .map(|i| format!("m_{}", pad(i, 4)).into_bytes())
      .collect();
    assert_eq!(
      session.sadd(key, members.clone()).await?,
      SET_MAX_COMPACT_ENTRIES
    );

    let meta_before = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_before.encoding(), StorageEncoding::Compact);

    // 再次调用 SADD，传入包含已存在元素的数组，因总数超限触发跃迁，但实际新增为 0
    let dup_batch: Vec<Vec<u8>> = (0..10)
      .map(|i| format!("m_{}", pad(i, 4)).into_bytes())
      .collect();
    let added = session.sadd(key, dup_batch).await?;
    assert_eq!(added, 0);

    // 验证跃迁后编码已成功持久化为 Flattened，绝不回退至旧 Compact 状态
    let meta_after = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(
      meta_after.encoding(),
      StorageEncoding::Flattened,
      "元数据必须正确保存为 Flattened 模式"
    );
    assert_eq!(session.scard(key).await?, SET_MAX_COMPACT_ENTRIES);

    // 验证所有 128 个成员均可正常检索
    for m in &members {
      assert!(session.sismember(key, m).await?);
    }

    OK
  })
}

/// 测试 9: SPOP 与 SRANDMEMBER 紧凑与打平双模随机性、基数及全量清空验证
#[test]
fn test_adaptive_set_spop_and_srandmember_dual_mode() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // --- 1. 紧凑模式下的 SPOP 与 SRANDMEMBER ---
    let c_key = b"set:compact:rand_pop";
    session
      .sadd(
        c_key,
        [b"item_1", b"item_2", b"item_3", b"item_4", b"item_5"],
      )
      .await?;

    // count = 0 边界
    assert!(session.spop(c_key, 0).await?.is_empty());
    assert!(session.srandmember(c_key, 0).await?.is_empty());
    assert_eq!(session.scard(c_key).await?, 5);

    // srandmember count = 1
    let rand_one = session.srandmember(c_key, 1).await?;
    assert_eq!(rand_one.len(), 1);
    assert!(session.sismember(c_key, &rand_one[0]).await?);
    assert_eq!(session.scard(c_key).await?, 5);

    // srandmember count = 3
    let rand_multi = session.srandmember(c_key, 3).await?;
    assert_eq!(rand_multi.len(), 3);
    for m in &rand_multi {
      assert!(session.sismember(c_key, m).await?);
    }
    assert_eq!(session.scard(c_key).await?, 5);

    // spop count = 1
    let popped_one = session.spop(c_key, 1).await?;
    assert_eq!(popped_one.len(), 1);
    assert_eq!(session.scard(c_key).await?, 4);
    assert!(!session.sismember(c_key, &popped_one[0]).await?);

    // spop count = 2
    let popped_two = session.spop(c_key, 2).await?;
    assert_eq!(popped_two.len(), 2);
    assert_eq!(session.scard(c_key).await?, 2);
    for m in &popped_two {
      assert!(!session.sismember(c_key, m).await?);
    }

    // spop 超过剩余数量（全量弹出）
    let popped_rest = session.spop(c_key, 10).await?;
    assert_eq!(popped_rest.len(), 2);
    assert_eq!(session.scard(c_key).await?, 0);
    assert_eq!(session.type_of(c_key).await?, "none");
    assert!(session.load_meta(c_key).await?.is_none());

    // --- 2. 打平模式下的 SPOP 与 SRANDMEMBER ---
    let f_key = b"set:flattened:rand_pop";
    let f_members: Vec<Vec<u8>> = (0..150)
      .map(|i| format!("f_mem_{}", pad(i, 4)).into_bytes())
      .collect();
    session.sadd(f_key, f_members).await?;
    assert_eq!(session.scard(f_key).await?, 150);
    assert_eq!(
      session.load_meta(f_key).await?.unwrap().encoding(),
      StorageEncoding::Flattened
    );

    let f_rand = session.srandmember(f_key, 5).await?;
    assert_eq!(f_rand.len(), 5);
    for m in &f_rand {
      assert!(session.sismember(f_key, m).await?);
    }
    assert_eq!(session.scard(f_key).await?, 150);

    let f_pop = session.spop(f_key, 5).await?;
    assert_eq!(f_pop.len(), 5);
    assert_eq!(session.scard(f_key).await?, 145);
    for m in &f_pop {
      assert!(!session.sismember(f_key, m).await?);
    }

    OK
  })
}

/// 测试 10: SMISMEMBER 优化与各种边界条件
#[test]
fn test_adaptive_set_smismember_batch_and_edge_cases() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // 1. 不存在的键
    let none_key = b"set:not_exists";
    assert_eq!(
      session.smismember(none_key, &[b"a", b"b"]).await?,
      vec![false, false]
    );
    assert!(session.smismember(none_key, &[]).await?.is_empty());

    // 2. Compact 模式批量判断
    let key = b"set:smismember:compact";
    session
      .sadd(
        key,
        [b"alpha".as_slice(), b"beta".as_slice(), b"gamma".as_slice()],
      )
      .await?;
    let flags = session
      .smismember(key, &[b"alpha", b"delta", b"beta", b"omega"])
      .await?;
    assert_eq!(flags, vec![true, false, true, false]);

    // 3. Flattened 模式批量判断
    let f_key = b"set:smismember:flattened";
    let f_mems: Vec<Vec<u8>> = (0..140)
      .map(|i| format!("fm_{}", pad(i, 3)).into_bytes())
      .collect();
    session.sadd(f_key, f_mems).await?;

    let f_flags = session
      .smismember(f_key, &[b"fm_000", b"fm_999", b"fm_139", b"fm_non"])
      .await?;
    assert_eq!(f_flags, vec![true, false, true, false]);

    OK
  })
}

/// 测试 11: SSCAN 紧凑模式与打平模式通配符过滤与分页游标状态机严格对齐
#[test]
fn test_adaptive_set_sscan_dual_mode_with_pattern() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // 1. Compact 模式带通配符分页扫描
    let c_key = b"set:sscan:compact_pat";
    session
      .sadd(
        c_key,
        [
          b"user:active:101",
          b"user:banned:201",
          b"user:active:102",
          b"user:banned:202",
          b"user:active:103",
        ],
      )
      .await?;

    let (c1, p1) = session.sscan(c_key, 0, 2, Some(b"user:active:*")).await?;
    assert!(c1 > 0, "游标必须递进");
    assert_eq!(p1.len(), 2);
    for item in &p1 {
      assert!(item.starts_with(b"user:active:"));
    }

    let (c2, p2) = session.sscan(c_key, c1, 10, Some(b"user:active:*")).await?;
    assert_eq!(c2, 0, "完成全集合扫描后游标归零");
    assert_eq!(p2.len(), 1);
    assert_eq!(p2[0], b"user:active:103");

    // 2. Flattened 模式跨分块通配符分页扫描
    let f_key = b"set:sscan:flattened_pat";
    let mut total_active = 0;
    let mut f_mems = Vec::new();
    for i in 0..150 {
      if i % 2 == 0 {
        f_mems.push(format!("order:paid:{}", pad(i, 4)).into_bytes());
        total_active += 1;
      } else {
        f_mems.push(format!("order:unpaid:{}", pad(i, 4)).into_bytes());
      }
    }
    session.sadd(f_key, f_mems).await?;

    let mut scanned_orders = Vec::new();
    let mut cursor = 0;
    loop {
      let (next_cur, items) = session
        .sscan(f_key, cursor, 20, Some(b"order:paid:*"))
        .await?;
      scanned_orders.extend(items);
      if next_cur == 0 {
        break;
      }
      cursor = next_cur;
    }

    assert_eq!(scanned_orders.len(), total_active);
    for order in &scanned_orders {
      assert!(order.starts_with(b"order:paid:"));
    }

    OK
  })
}

/// 测试 12: Set Fast Drop (O(1) 瞬时删除) 与重新插入的版本隔离验证
#[test]
fn test_adaptive_set_fast_drop_and_resurrection() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"set:fast_drop:resurrection";

    // 写入 150 个元素 (Flattened 模式)
    let f_mems: Vec<Vec<u8>> = (0..150)
      .map(|i| format!("old_member_{}", pad(i, 4)).into_bytes())
      .collect();
    session.sadd(key, f_mems).await?;
    assert_eq!(session.scard(key).await?, 150);

    let meta_old = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_old.encoding(), StorageEncoding::Flattened);
    let old_version = meta_old.version;

    // Fast Drop: 删除该集合
    assert!(session.delete(key).await?);
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.scard(key).await?, 0);

    // 验证旧版本号已递增，size 为 0
    let meta_dropped = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_dropped.size, 0);
    assert_eq!(meta_dropped.version, old_version + 1);

    // 复活（写入新成员）
    session
      .sadd(key, [b"new_member_1", b"new_member_2"])
      .await?;
    assert_eq!(session.scard(key).await?, 2);
    assert_eq!(session.type_of(key).await?, "set");

    // 验证旧成员完全不可见，无历史残留泄漏
    assert!(session.sismember(key, b"new_member_1").await?);
    assert!(session.sismember(key, b"new_member_2").await?);
    assert!(!session.sismember(key, b"old_member_0000").await?);
    assert!(!session.sismember(key, b"old_member_0149").await?);

    let members = session.smembers(key).await?;
    assert_eq!(members.len(), 2);

    OK
  })
}

/// 测试 13: HINCRBY / HINCRBYFLOAT 在紧凑模式和打平模式下保留 TTL 验证
#[test]
fn test_adaptive_hash_hincrby_preserves_ttl() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:hincrby:ttl";

    // 1. Compact 模式下自增并验证 TTL 保留
    session.hset(key, b"cnt", b"10").await?;
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let exp_time = now + 600_000;
    assert_eq!(
      session
        .hexpire(key, b"cnt", exp_time, HashExpireOpt::NONE)
        .await?,
      HashExpireResult::Ok
    );

    // 自增后 TTL 必须保留（非 -1 或 -2）
    assert_eq!(session.hincrby(key, b"cnt", 5).await?, 15);
    let ttl = session.httl(key, b"cnt").await?;
    assert!(
      ttl > 0 && ttl <= 600_000,
      "TTL must be preserved after HINCRBY"
    );

    let float_res = session.hincrbyfloat(key, b"cnt", 2.5).await?;
    assert!((float_res - 17.5).abs() < 1e-6);
    let ttl_float = session.httl(key, b"cnt").await?;
    assert!(
      ttl_float > 0 && ttl_float <= 600_000,
      "TTL must be preserved after HINCRBYFLOAT"
    );

    // 2. NaN / Infinite 浮点数报错拦截
    assert!(session.hincrbyfloat(key, b"cnt", f64::NAN).await.is_err());
    assert!(
      session
        .hincrbyfloat(key, b"cnt", f64::INFINITY)
        .await
        .is_err()
    );

    // 3. Flattened 模式下自增并验证 TTL 保留
    let flat_key = b"hash:flat:hincrby:ttl";
    let large_val = vec![b'X'; HASH_MAX_COMPACT_VALUE + 1];
    session.hset(flat_key, b"pad", &large_val).await?;
    session.hset(flat_key, b"flat_cnt", b"100").await?;

    let meta = session.load_meta(flat_key).await?.expect("meta exists");
    assert_eq!(meta.encoding(), StorageEncoding::Flattened);

    let exp_flat = now + 500_000;
    assert_eq!(
      session
        .hexpire(flat_key, b"flat_cnt", exp_flat, HashExpireOpt::NONE)
        .await?,
      HashExpireResult::Ok
    );

    assert_eq!(session.hincrby(flat_key, b"flat_cnt", 50).await?, 150);
    let ttl_flat = session.httl(flat_key, b"flat_cnt").await?;
    assert!(
      ttl_flat > 0 && ttl_flat <= 500_000,
      "TTL must be preserved after Flattened HINCRBY"
    );

    OK
  })
}

/// 测试 14: HDEL 对已过期字段返回 0 且妥善清理存储
#[test]
fn test_adaptive_hash_hdel_expired_returns_zero() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:hdel:expired";

    session.hset(key, b"f_active", b"v1").await?;
    session.hset(key, b"f_exp", b"v2").await?;
    assert_eq!(session.hlen(key).await?, 2);

    let now = coarsetime::Clock::now_since_epoch().as_millis();
    // f_exp 设置短暂过期
    assert_eq!(
      session
        .hexpire(key, b"f_exp", now + 10, HashExpireOpt::NONE)
        .await?,
      HashExpireResult::Ok
    );
    sleep(Duration::from_millis(25)).await;

    // HDEL 对已过期字段应返回 0（Redis 规范：已过期字段视同不存在）
    let del_cnt = session.hdel(key, &[b"f_exp"]).await?;
    assert_eq!(del_cnt, 0);
    assert_eq!(session.hlen(key).await?, 1);
    assert_eq!(session.hget(key, b"f_active").await?, Some(b"v1".to_vec()));

    // HDEL 对正常有效字段返回 1
    let del_active = session.hdel(key, &[b"f_active"]).await?;
    assert_eq!(del_active, 1);
    assert_eq!(session.hlen(key).await?, 0);
    assert_eq!(session.type_of(key).await?, "none");

    OK
  })
}

/// 测试 15: HEXPIRE 对不存在的键返回 KeyNotFound
#[test]
fn test_adaptive_hash_hexpire_key_not_found() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:nonexistent";

    let now = coarsetime::Clock::now_since_epoch().as_millis();
    let res = session
      .hexpire(key, b"f", now + 10_000, HashExpireOpt::NONE)
      .await?;
    assert_eq!(res, HashExpireResult::KeyNotFound);

    OK
  })
}

/// 测试 16: Compact 模式物理空间压缩与 meta.size 严密同步
#[test]
fn test_adaptive_hash_compact_purge_physical_compaction() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:compact:purge_compaction";

    // 写入 5 个字段
    for i in 0..5 {
      let f = format!("f_{}", i).into_bytes();
      let v = format!("v_{}", i).into_bytes();
      session.hset(key, f, v).await?;
    }
    assert_eq!(session.hlen(key).await?, 5);

    // 将 f_0, f_1 设置为过去时间戳触发过期
    let now = coarsetime::Clock::now_since_epoch().as_millis();
    for i in 0..2 {
      let f = format!("f_{}", i).into_bytes();
      session
        .hexpire(key, &f, now.saturating_sub(100), HashExpireOpt::NONE)
        .await?;
    }

    // 此时写入新字段 f_new，触发底层 purge_expired 物理空间原地压缩与 meta.size 同步
    session.hset(key, b"f_new", b"v_new").await?;

    // 剩余有效字段为: f_2, f_3, f_4, f_new，总计 4 个
    assert_eq!(session.hlen(key).await?, 4);
    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.size, 4);
    assert_eq!(meta.encoding(), StorageEncoding::Compact);

    let all = session.hgetall(key).await?;
    assert_eq!(all.len(), 4);
    let keys = session.hkeys(key).await?;
    assert_eq!(keys.len(), 4);
    assert!(!keys.contains(&b"f_0".to_vec()));
    assert!(!keys.contains(&b"f_1".to_vec()));
    assert!(keys.contains(&b"f_new".to_vec()));

    OK
  })
}

/// 测试 17: Hash 紧凑编码总大小安全门限 (MAX_COMPACT_TOTAL_BYTES 4096 字节防爆跃迁)
#[test]
fn test_adaptive_hash_total_bytes_safety_threshold() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:safety_threshold:bytes";
    assert_eq!(MAX_COMPACT_TOTAL_BYTES, 4096);

    // 每个条目约 88 字节：23 字节 key + 60 字节 value + 5 字节开销
    // 写入 80 个条目，总字节数约 7000 字节，远超 4096 字节安全门限；
    // 但项数（80）远小于 512，单 value 长度（60）小于 64。
    for i in 0..80 {
      let field = format!("field_safety_check_{}", pad(i, 4)).into_bytes();
      let value = vec![b'x'; 60];
      session.hset(key, field, value).await?;
    }

    assert_eq!(session.hlen(key).await?, 80);
    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(
      meta.encoding(),
      StorageEncoding::Flattened,
      "总字节数突破 4096 字节后必须安全自动跃迁为 Flattened"
    );

    // 验证所有 80 个字段均可正常读出
    for i in 0..80 {
      let field = format!("field_safety_check_{}", pad(i, 4)).into_bytes();
      let val = session.hget(key, &field).await?;
      assert_eq!(val, Some(vec![b'x'; 60]));
    }

    OK
  })
}

/// 测试 18: Set 紧凑编码总大小安全门限 (MAX_COMPACT_TOTAL_BYTES 4096 字节防爆跃迁)
#[test]
fn test_adaptive_set_total_bytes_safety_threshold() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"set:safety_threshold:bytes";

    // 写入 70 个长度为 60 字节的 member，总大小约 70 * 62 = 4340 > 4096；
    // 项数（70）小于 128，单 member（60）小于 64。
    let members: Vec<Vec<u8>> = (0..70)
      .map(|i| {
        format!(
          "member_safety_check_{}_{}",
          pad(i, 4),
          "y".repeat(34).as_str()
        )
        .into_bytes()
      })
      .collect();

    for m in &members {
      session.sadd(key, [m.as_slice()]).await?;
    }

    assert_eq!(session.scard(key).await?, 70);
    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(
      meta.encoding(),
      StorageEncoding::Flattened,
      "Set 总字节数突破 4096 字节后必须安全自动跃迁为 Flattened"
    );

    // 验证所有 70 个元素均可正常查出
    for m in &members {
      assert!(session.sismember(key, m).await?);
    }

    OK
  })
}
