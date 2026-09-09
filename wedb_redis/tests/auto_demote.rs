use std::{iter::repeat_n, sync::Arc};

use aok::{OK, Void};
use compio::runtime::Runtime;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use wedb_zset::ZAddOpt;
use wkv::{StoreConfig, WedbStore};
use wval::{CollectionType, StorageEncoding};

/// 十进制补零到 width 位
fn pad(v: impl itoa::Integer, width: usize) -> String {
  let mut buf = itoa::Buffer::new();
  let digits = buf.format(v);
  let mut s = String::with_capacity(width.max(digits.len()));
  s.extend(repeat_n('0', width.saturating_sub(digits.len())));
  s.push_str(digits);
  s
}

fn create_test_store() -> aok::Result<(tempfile::TempDir, Arc<WedbStore<SegmentedDevice>>)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("auto_demote.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(4096, 64 * 1024, 128, 0.5)?,
    device,
  )?);
  Ok((dir, store))
}

#[test]
fn test_auto_demote_hash() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"test:demote:hash";

    // 1. 写入 15 个正常字段
    for i in 0..15 {
      let field = format!("f_{}", pad(i, 2)).into_bytes();
      let val = format!("val_{}", pad(i, 2)).into_bytes();
      session.hset(key, field, val).await?;
    }
    let meta_c = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_c.encoding(), StorageEncoding::Compact);

    // 写入一个超长值的字段（val > 64 字节），触发升级为 Flattened
    let long_val = vec![b'v'; 70];
    session.hset(key, b"f_long", long_val).await?;
    assert_eq!(session.hlen(key).await?, 16);
    let meta_f = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_f.encoding(), StorageEncoding::Flattened);

    // 2. 删除超长字段，此时剩余 15 个短字段（<= 16 且无超长条目），触发 Auto-Demote 降级收缩为 Compact
    let del_cnt = session.hdel(key, &[b"f_long".as_slice()]).await?;
    assert_eq!(del_cnt, 1);
    assert_eq!(session.hlen(key).await?, 15);

    let meta_demoted = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_demoted.encoding(), StorageEncoding::Compact);
    assert_eq!(meta_demoted.size, 15);
    // 崩溃一致性新顺序（先写 meta 后删数据）：降级必须递增版本隔离旧子键，类型保持 Hash
    assert_eq!(
      meta_demoted.version,
      meta_f.version + 1,
      "降级路径必须递增 meta.version"
    );
    assert_eq!(meta_demoted.collection_type, CollectionType::Hash);

    // 验证剩余数据查询正确
    assert_eq!(session.hget(key, b"f_00").await?, Some(b"val_00".to_vec()));
    assert_eq!(session.hget(key, b"f_14").await?, Some(b"val_14".to_vec()));
    assert_eq!(session.hget(key, b"f_long").await?, None);

    // 3. 删空全部字段，验证严格删空语义（无幽灵 Meta）
    let all_remaining: Vec<Vec<u8>> = (0..15)
      .map(|i| format!("f_{}", pad(i, 2)).into_bytes())
      .collect();
    let all_refs: Vec<&[u8]> = all_remaining.iter().map(|f| f.as_slice()).collect();
    let del_all = session.hdel(key, &all_refs).await?;
    assert_eq!(del_all, 15);
    assert_eq!(session.hlen(key).await?, 0);
    assert!(session.load_meta(key).await?.is_none());

    OK
  })
}

#[test]
fn test_auto_demote_set() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"test:demote:set";

    // 1. 写入 15 个正常成员
    let members: Vec<Vec<u8>> = (0..15)
      .map(|i| format!("m_{}", pad(i, 2)).into_bytes())
      .collect();
    session.sadd(key, members).await?;
    let meta_c = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_c.encoding(), StorageEncoding::Compact);

    // 写入一个超长成员（> 64 字节），触发升级为 Flattened
    let long_mem = vec![b'm'; 70];
    session.sadd(key, [long_mem.clone()]).await?;
    assert_eq!(session.scard(key).await?, 16);
    let meta_f = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_f.encoding(), StorageEncoding::Flattened);

    // 2. 删除超长成员，此时剩余 15 个短成员（<= 16），触发 Auto-Demote 降级收缩为 Compact
    let rem_cnt = session.srem(key, &[long_mem.as_slice()]).await?;
    assert_eq!(rem_cnt, 1);
    assert_eq!(session.scard(key).await?, 15);

    let meta_demoted = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_demoted.encoding(), StorageEncoding::Compact);
    assert_eq!(meta_demoted.size, 15);
    // 崩溃一致性新顺序（先写 meta 后删数据）：降级必须递增版本隔离旧子键，类型保持 Set
    assert_eq!(
      meta_demoted.version,
      meta_f.version + 1,
      "降级路径必须递增 meta.version"
    );
    assert_eq!(meta_demoted.collection_type, CollectionType::Set);

    // 验证剩余元素依然能正确命中
    assert!(session.sismember(key, b"m_00").await?);
    assert!(session.sismember(key, b"m_14").await?);
    assert!(!session.sismember(key, &long_mem).await?);

    // 3. 删空全部剩余元素，验证严格删空语义
    let rem_remaining: Vec<Vec<u8>> = (0..15)
      .map(|i| format!("m_{}", pad(i, 2)).into_bytes())
      .collect();
    let rem_refs: Vec<&[u8]> = rem_remaining.iter().map(|m| m.as_slice()).collect();
    let rem_all = session.srem(key, &rem_refs).await?;
    assert_eq!(rem_all, 15);
    assert_eq!(session.scard(key).await?, 0);
    assert!(session.load_meta(key).await?.is_none());

    OK
  })
}

#[test]
fn test_auto_demote_zset() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"test:demote:zset";

    // 1. 写入 15 个短成员
    for i in 0..15 {
      let member = format!("zm_{}", pad(i, 2)).into_bytes();
      session
        .zadd(key, (i as f64) * 10.0, member, ZAddOpt::default())
        .await?;
    }
    let meta_c = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_c.encoding(), StorageEncoding::Compact);

    // 写入一个超长成员（> 64 字节），触发升级为 Flattened (BfTree)
    let long_mem = vec![b'z'; 70];
    session
      .zadd(key, 999.0, long_mem.clone(), ZAddOpt::default())
      .await?;
    assert_eq!(session.zcard(key).await?, 16);
    let meta_f = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_f.encoding(), StorageEncoding::Flattened);

    // 2. 删除超长成员，此时剩余 15 个短成员（<= 16），触发 Auto-Demote 降级收缩为 Compact 并清理 BfTree
    let rem_cnt = session.zrem(key, &[long_mem.as_slice()]).await?;
    assert_eq!(rem_cnt, 1);
    assert_eq!(session.zcard(key).await?, 15);

    let meta_demoted = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_demoted.encoding(), StorageEncoding::Compact);
    assert_eq!(meta_demoted.size, 15);
    // 崩溃一致性新顺序（先写 meta 后清 BfTree）：降级必须递增版本隔离旧双键，类型保持 ZSet
    assert_eq!(
      meta_demoted.version,
      meta_f.version + 1,
      "降级路径必须递增 meta.version"
    );
    assert_eq!(meta_demoted.collection_type, CollectionType::ZSet);

    // 验证查询和排名在 Compact 模式下依然准确
    assert_eq!(session.zscore(key, &long_mem).await?, None);
    assert_eq!(session.zscore(key, b"zm_00").await?, Some(0.0));
    assert_eq!(session.zscore(key, b"zm_14").await?, Some(140.0));
    assert_eq!(session.zrank(key, b"zm_00").await?, Some(0));
    assert_eq!(session.zrank(key, b"zm_14").await?, Some(14));

    // 3. 删空全部剩余元素，验证严格删空语义与 BfTree 完全清空
    let rem_last: Vec<Vec<u8>> = (0..15)
      .map(|i| format!("zm_{}", pad(i, 2)).into_bytes())
      .collect();
    let rem_last_refs: Vec<&[u8]> = rem_last.iter().map(|m| m.as_slice()).collect();
    let rem_final = session.zrem(key, &rem_last_refs).await?;
    assert_eq!(rem_final, 15);
    assert_eq!(session.zcard(key).await?, 0);
    assert!(session.load_meta(key).await?.is_none());

    OK
  })
}
