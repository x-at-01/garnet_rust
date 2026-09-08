use std::sync::Arc;

use aok::{OK, Void};
use coarsetime::Clock;
use compio::runtime::Runtime;
use log::info;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_hash::ExpireOpt as HashExpireOpt;
use wedb_list::InsertPosition;
use wedb_redis::prelude::*;
use wedb_zset::{ScoreRange, ZAddOpt};
use wkv::{StoreConfig, WedbStore};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 测试 1: Hash 嵌入式端到端测试与清空自动墓碑自愈
#[test]
fn test_embedded_hash_crud_and_auto_tombstone() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("hash.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(
      StoreConfig::new(1024, 64 * 1024, 16, 0.5)?,
      device,
    )?);
    let session = store.new_session()?;

    let key = b"user:profile:1";

    // 初始状态 key 不存在
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.hlen(key).await?, 0);

    // 1. HSET & HGET
    assert!(
      session
        .hset(key, b"name".to_vec(), b"Alice".to_vec())
        .await?
    );
    assert_eq!(session.type_of(key).await?, "hash");
    assert_eq!(session.hget(key, b"name").await?, Some(b"Alice".to_vec()));

    // 2. HSETNX
    assert!(
      !session
        .hsetnx(key, b"name".to_vec(), b"Bob".to_vec())
        .await?
    );
    assert_eq!(session.hget(key, b"name").await?, Some(b"Alice".to_vec()));
    assert!(session.hsetnx(key, b"age".to_vec(), b"20".to_vec()).await?);

    // 3. HMSET & HMGET & HGETALL
    let count = session
      .hmset(
        key,
        vec![
          (b"city".to_vec(), b"Shanghai".to_vec()),
          (b"score".to_vec(), b"100".to_vec()),
        ],
      )
      .await?;
    assert_eq!(count, 2);
    assert_eq!(session.hlen(key).await?, 4);

    let vals = session.hmget(key, &[b"name", b"score", b"unknown"]).await?;
    assert_eq!(
      vals,
      vec![Some(b"Alice".to_vec()), Some(b"100".to_vec()), None]
    );

    let all = session.hgetall(key).await?;
    assert_eq!(all.len(), 4);

    // 4. HINCRBY & HINCRBYFLOAT
    assert_eq!(session.hincrby(key, b"age", 5).await?, 25);
    let fscore = session.hincrbyfloat(key, b"score", 0.5).await?;
    assert!((fscore - 100.5).abs() < 1e-6);

    // 5. HEXPIRE & HTTL & HPERSIST
    let now = Clock::now_since_epoch().as_millis();
    let exp_res = session
      .hexpire(key, b"city", now + 60_000, HashExpireOpt::NONE)
      .await?;
    assert_eq!(exp_res, wedb_hash::ExpireResult::Ok);
    let ttl = session.httl(key, b"city").await?;
    assert!(ttl > 0);
    assert!(session.hpersist(key, b"city").await?);
    assert_eq!(session.httl(key, b"city").await?, -1);

    // 6. HSCAN
    let (next_cursor, scanned) = session.hscan(key, 0, 10, None).await?;
    assert_eq!(next_cursor, 0);
    assert_eq!(scanned.len(), 4);

    // 7. HDEL 部分删除
    let del_cnt = session.hdel(key, &[b"age", b"city"]).await?;
    assert_eq!(del_cnt, 2);
    assert_eq!(session.hlen(key).await?, 2);

    // 8. 清空所有剩余字段触发自动墓碑删除
    let del_rest = session.hdel(key, &[b"name", b"score"]).await?;
    assert_eq!(del_rest, 2);

    // 验证底层 key 已被删除自愈为 None
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.read(key).await?, None);
    assert!(session.load_object(key).await?.is_none());

    info!("测试 1: Hash 嵌入式端到端测试与清空自动墓碑自愈通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 2: List 嵌入式端到端测试与弹出自动墓碑自愈
#[test]
fn test_embedded_list_crud_and_auto_tombstone() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("list.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(
      StoreConfig::new(1024, 64 * 1024, 16, 0.5)?,
      device,
    )?);
    let session = store.new_session()?;

    let key = b"task:queue";

    // 1. LPUSH & RPUSH
    assert_eq!(
      session
        .lpush(key, vec![b"b".to_vec(), b"a".to_vec()])
        .await?,
      2
    ); // [a, b]
    assert_eq!(session.type_of(key).await?, "list");
    assert_eq!(
      session
        .rpush(key, vec![b"c".to_vec(), b"d".to_vec()])
        .await?,
      4
    ); // [a, b, c, d]
    assert_eq!(session.llen(key).await?, 4);

    // 2. LRANGE & LINDEX
    let range = session.lrange(key, 0, -1).await?;
    assert_eq!(
      range,
      vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
    );
    assert_eq!(session.lindex(key, 1).await?, Some(b"b".to_vec()));
    assert_eq!(session.lindex(key, -1).await?, Some(b"d".to_vec()));

    // 3. LSET
    assert!(session.lset(key, 1, b"B".to_vec()).await?);
    assert_eq!(session.lindex(key, 1).await?, Some(b"B".to_vec()));

    // 4. LINSERT & LPOS
    let ins_res = session
      .linsert(key, b"B", b"A2".to_vec(), InsertPosition::Before)
      .await?;
    assert_eq!(ins_res, 5); // [a, A2, B, c, d]
    let pos = session.lpos(key, b"B", 1, None, 0).await?;
    assert_eq!(pos, vec![2]);

    // 5. LTRIM

    session.ltrim(key, 1, 3).await?; // 保留索引 [1..=3]，即 [A2, B, c]
    assert_eq!(session.llen(key).await?, 3);

    // 6. LREM
    let rem_cnt = session.lrem(key, 1, b"B").await?;
    assert_eq!(rem_cnt, 1);
    assert_eq!(session.llen(key).await?, 2); // [A2, c]

    // 7. LPOP & RPOP 弹出直到清空
    let popped_left = session.lpop(key, 1).await?;
    assert_eq!(popped_left, vec![b"A2".to_vec()]);
    assert_eq!(session.llen(key).await?, 1);

    let popped_right = session.rpop(key, 1).await?;
    assert_eq!(popped_right, vec![b"c".to_vec()]);

    // 验证清空后自动墓碑删除
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.read(key).await?, None);
    assert!(session.load_object(key).await?.is_none());

    info!("测试 2: List 嵌入式端到端测试与弹出自动墓碑自愈通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 2.1: LMOVE/RPOPLPUSH 单遍语义回归
/// （同键旋转、跨键转移、源空返回 nil、跨类型 WRONGTYPE 拦截）
#[test]
fn test_embedded_lmove_rpoplpush_semantics() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("lmove.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(
      StoreConfig::new(1024, 64 * 1024, 16, 0.5)?,
      device,
    )?);
    let session = store.new_session()?;

    let src = b"lmove:src";
    let dst = b"lmove:dst";

    // 1. 源不存在：返回 nil 且不创建目标键（不触发目标类型校验）
    assert_eq!(session.lmove(src, dst, true, true).await?, None);
    assert_eq!(session.type_of(dst).await?, "none");

    // 2. 同键旋转：[a,b,c] LMOVE k k LEFT RIGHT => 弹 a 尾插 => [b,c,a]
    session
      .rpush(src, vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()])
      .await?;
    assert_eq!(
      session.lmove(src, src, true, false).await?,
      Some(b"a".to_vec())
    );
    assert_eq!(
      session.lrange(src, 0, -1).await?,
      vec![b"b".to_vec(), b"c".to_vec(), b"a".to_vec()]
    );
    // 同键反向旋转复原: [b,c,a] LMOVE k k RIGHT LEFT => 弹 a 头插 => [a,b,c]
    assert_eq!(
      session.lmove(src, src, false, true).await?,
      Some(b"a".to_vec())
    );
    assert_eq!(
      session.lrange(src, 0, -1).await?,
      vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );
    // 同键同向移动为 peek 语义: LMOVE k k LEFT LEFT 弹出即推回头端, 列表不变
    assert_eq!(
      session.lmove(src, src, true, true).await?,
      Some(b"a".to_vec())
    );
    assert_eq!(
      session.lrange(src, 0, -1).await?,
      vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
    );

    // 3. 跨键转移: LEFT RIGHT 弹 a 尾插 => src [b,c] dst [a]
    assert_eq!(
      session.lmove(src, dst, true, false).await?,
      Some(b"a".to_vec())
    );
    assert_eq!(
      session.lrange(src, 0, -1).await?,
      vec![b"b".to_vec(), b"c".to_vec()]
    );
    assert_eq!(session.lrange(dst, 0, -1).await?, vec![b"a".to_vec()]);
    // 跨键 RIGHT RIGHT 弹 c 尾插 => src [b] dst [a,c]
    assert_eq!(
      session.lmove(src, dst, false, false).await?,
      Some(b"c".to_vec())
    );
    assert_eq!(session.lrange(src, 0, -1).await?, vec![b"b".to_vec()]);
    assert_eq!(
      session.lrange(dst, 0, -1).await?,
      vec![b"a".to_vec(), b"c".to_vec()]
    );

    // 4. RPOPLPUSH 委托同一路径: src [b] 弹 b 头插 dst => src 清空自愈, dst [b,a,c]
    assert_eq!(session.rpoplpush(src, dst).await?, Some(b"b".to_vec()));
    assert_eq!(session.type_of(src).await?, "none");
    assert_eq!(
      session.lrange(dst, 0, -1).await?,
      vec![b"b".to_vec(), b"a".to_vec(), b"c".to_vec()]
    );

    // 5. 源耗尽后再移: 返回 nil 且不创建新目标键
    assert_eq!(session.rpoplpush(src, dst).await?, None);
    assert_eq!(session.type_of(src).await?, "none");

    // 6. 源为字符串裸键: WRONGTYPE, 目标保持原样
    let str_key = b"lmove:str";
    session.upsert(str_key, b"plain").await?;
    let err = session
      .lmove(str_key, dst, true, true)
      .await
      .expect_err("源为字符串应 WRONGTYPE");
    assert!(err.is_wrong_type());
    assert_eq!(session.llen(dst).await?, 3);

    // 7. 目标为字符串裸键: WRONGTYPE 且源不被修改 (类型校验先于弹出)
    session
      .rpush(src, vec![b"x".to_vec(), b"y".to_vec()])
      .await?;
    let err = session
      .rpoplpush(src, str_key)
      .await
      .expect_err("目标为字符串应 WRONGTYPE");
    assert!(err.is_wrong_type());
    assert_eq!(
      session.lrange(src, 0, -1).await?,
      vec![b"x".to_vec(), b"y".to_vec()]
    );
    assert_eq!(session.read(str_key).await?, Some(b"plain".to_vec()));

    info!("测试 2.1: LMOVE/RPOPLPUSH 单遍语义回归通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 3: Set 嵌入式端到端测试与移除自动墓碑自愈
#[test]
fn test_embedded_set_crud_and_auto_tombstone() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("set.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(
      StoreConfig::new(1024, 64 * 1024, 16, 0.5)?,
      device,
    )?);
    let session = store.new_session()?;

    let key = b"tags:article:42";

    // 1. SADD
    let added = session
      .sadd(
        key,
        vec![b"rust".to_vec(), b"database".to_vec(), b"garnet".to_vec()],
      )
      .await?;
    assert_eq!(added, 3);
    assert_eq!(session.type_of(key).await?, "set");
    assert_eq!(session.scard(key).await?, 3);

    // 重复 SADD 应返回 0 新增
    assert_eq!(session.sadd(key, vec![b"rust".to_vec()]).await?, 0);

    // 2. SISMEMBER & SMISMEMBER
    assert!(session.sismember(key, b"rust").await?);
    assert!(!session.sismember(key, b"python").await?);
    assert_eq!(
      session
        .smismember(key, &[b"rust", b"python", b"garnet"])
        .await?,
      vec![true, false, true]
    );

    // 3. SMEMBERS & SSCAN
    let members = session.smembers(key).await?;
    assert_eq!(members.len(), 3);

    let (next_cursor, scanned) = session.sscan(key, 0, 10, None).await?;
    assert_eq!(next_cursor, 0);
    assert_eq!(scanned.len(), 3);

    // 4. SPOP
    let popped = session.spop(key, 1).await?;
    assert_eq!(popped.len(), 1);
    assert_eq!(session.scard(key).await?, 2);

    // 5. SREM 移除剩余元素
    let remaining = session.smembers(key).await?;
    let rem_refs: Vec<&[u8]> = remaining.iter().map(|v| v.as_slice()).collect();
    let removed = session.srem(key, &rem_refs).await?;
    assert_eq!(removed, 2);

    // 验证清空后自动墓碑删除
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.read(key).await?, None);
    assert!(session.load_object(key).await?.is_none());

    info!("测试 3: Set 嵌入式端到端测试与移除自动墓碑自愈通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 4: ZSet 与 Geo 嵌入式端到端测试与清空自动墓碑自愈
#[test]
fn test_embedded_zset_crud_and_auto_tombstone() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("zset.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store = Arc::new(WedbStore::open(
      StoreConfig::new(1024, 64 * 1024, 16, 0.5)?,
      device,
    )?);
    let session = store.new_session()?;

    let key = b"leaderboard";

    // 1. ZADD & ZCARD
    session
      .zadd(key, 100.0, b"alice".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(key, 200.0, b"bob".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(key, 150.0, b"charlie".to_vec(), ZAddOpt::default())
      .await?;

    assert_eq!(session.type_of(key).await?, "zset");
    assert_eq!(session.zcard(key).await?, 3);

    // 2. ZSCORE & ZMSCORE
    assert_eq!(session.zscore(key, b"alice").await?, Some(100.0));
    assert_eq!(session.zscore(key, b"unknown").await?, None);
    assert_eq!(
      session
        .zmscore(key, &[b"alice", b"bob", b"unknown"])
        .await?,
      vec![Some(100.0), Some(200.0), None]
    );

    // 3. ZRANK & ZREVRANK
    assert_eq!(session.zrank(key, b"alice").await?, Some(0));
    assert_eq!(session.zrank(key, b"charlie").await?, Some(1));
    assert_eq!(session.zrank(key, b"bob").await?, Some(2));
    assert_eq!(session.zrevrank(key, b"bob").await?, Some(0));

    // 4. ZRANGE & ZRANGEBYSCORE
    let range = session.zrange(key, 0, -1, false).await?;
    assert_eq!(
      range,
      vec![
        (b"alice".to_vec(), 100.0),
        (b"charlie".to_vec(), 150.0),
        (b"bob".to_vec(), 200.0),
      ]
    );

    let by_score = session
      .zrangebyscore(key, ScoreRange::new(120.0, true, 250.0, true), false, 0, 10)
      .await?;
    assert_eq!(
      by_score,
      vec![(b"charlie".to_vec(), 150.0), (b"bob".to_vec(), 200.0)]
    );

    // 5. ZCOUNT & ZINCRBY
    assert_eq!(
      session
        .zcount(key, ScoreRange::new(100.0, true, 150.0, true))
        .await?,
      2
    );
    let new_score = session.zincrby(key, 50.0, b"alice").await?;
    assert_eq!(new_score, 150.0);

    // 6. ZPOPMIN & ZPOPMAX
    let pop_min = session.zpopmin(key, 1).await?;
    assert_eq!(pop_min.len(), 1);
    assert_eq!(session.zcard(key).await?, 2);

    // 7. ZSCAN
    let (next_cursor, scanned) = session.zscan(key, 0, 10, None).await?;
    assert_eq!(next_cursor, 0);
    assert_eq!(scanned.len(), 2);

    // 8. 清空剩余元素
    session.zremrangebyrank(key, 0, -1).await?;

    // 验证清空后自动墓碑删除
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.read(key).await?, None);
    assert!(session.load_object(key).await?.is_none());

    // 9. GEO 端到端测试
    let geo_key = b"cities:geo";
    // 插入 Palermo 与 Catania
    assert!(
      session
        .geoadd(geo_key, 38.1156879, 13.3612671, b"Palermo".to_vec())
        .await?
    );
    assert!(
      session
        .geoadd(geo_key, 37.5024815, 15.0878329, b"Catania".to_vec())
        .await?
    );

    // GEOPOS 坐标查询
    let pos_palermo = session.geopos(geo_key, b"Palermo").await?.unwrap();
    assert!((pos_palermo.0 - 13.3612671).abs() < 1e-4);
    assert!((pos_palermo.1 - 38.1156879).abs() < 1e-4);

    // GEODIST 距离计算
    let dist = session
      .geodist(geo_key, b"Palermo", b"Catania")
      .await?
      .unwrap();
    assert!((dist - 166274.0).abs() < 200.0);

    // GEOHASH Base32 查询
    let hashes = session.geohash(geo_key, &[b"Palermo", b"Catania"]).await?;
    assert_eq!(hashes.len(), 2);
    assert!(hashes[0].is_some());
    assert!(hashes[1].is_some());

    // 清理 GEO

    session.zrem(geo_key, &[b"Palermo", b"Catania"]).await?;
    assert_eq!(session.type_of(geo_key).await?, "none");

    info!("测试 4: ZSet 与 Geo 嵌入式端到端测试与清空自动墓碑自愈通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 5: 混合冷读取与落盘换页数据持久化恢复无损（ObjectTests）
#[test]
fn test_embedded_hybrid_flush_and_cold_read_recovery() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("hybrid_cold_read.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    // 使用较小的页面以快速触发写盘与缓存管理
    let config = StoreConfig::new(1024, 4096, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device.clone())?);
    let session = store.new_session()?;

    // 1. 写入普通 String
    let k_str = b"sys:config:version";
    let v_str = b"v2.1.0-release";
    session.upsert(k_str, v_str).await?;

    // 2. 写入 Hash
    let k_hash = b"user:meta:888";
    session
      .hset(k_hash, b"theme".to_vec(), b"dark".to_vec())
      .await?;
    session
      .hset(k_hash, b"lang".to_vec(), b"zh-CN".to_vec())
      .await?;
    session.hincrby(k_hash, b"login_count", 42).await?;

    // 3. 写入 List
    let k_list = b"events:audit";
    session
      .rpush(
        k_list,
        vec![b"login".to_vec(), b"pay".to_vec(), b"logout".to_vec()],
      )
      .await?;

    // 4. 写入 Set
    let k_set = b"roles:admin";
    session
      .sadd(
        k_set,
        vec![b"read".to_vec(), b"write".to_vec(), b"admin".to_vec()],
      )
      .await?;

    // 5. 写入 ZSet
    let k_zset = b"scores:rank";
    session
      .zadd(k_zset, 99.5, b"player_1".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(k_zset, 88.0, b"player_2".to_vec(), ZAddOpt::default())
      .await?;

    // 6. 执行全量落盘
    store.flush_all().await?;

    // 7. 模拟重新读取（通过同一个 session 或新 session，直接读取）
    let session2 = store.new_session()?;

    // 校验 String
    assert_eq!(session2.type_of(k_str).await?, "string");
    assert_eq!(session2.read(k_str).await?, Some(v_str.to_vec()));

    // 校验 Hash
    assert_eq!(session2.type_of(k_hash).await?, "hash");
    assert_eq!(
      session2.hget(k_hash, b"theme").await?,
      Some(b"dark".to_vec())
    );
    assert_eq!(
      session2.hget(k_hash, b"lang").await?,
      Some(b"zh-CN".to_vec())
    );
    assert_eq!(
      session2.hget(k_hash, b"login_count").await?,
      Some(b"42".to_vec())
    );

    // 校验 List
    assert_eq!(session2.type_of(k_list).await?, "list");
    assert_eq!(session2.llen(k_list).await?, 3);
    assert_eq!(
      session2.lrange(k_list, 0, -1).await?,
      vec![b"login".to_vec(), b"pay".to_vec(), b"logout".to_vec()]
    );

    // 校验 Set
    assert_eq!(session2.type_of(k_set).await?, "set");
    assert_eq!(session2.scard(k_set).await?, 3);
    assert!(session2.sismember(k_set, b"write").await?);

    // 校验 ZSet
    assert_eq!(session2.type_of(k_zset).await?, "zset");
    assert_eq!(session2.zcard(k_zset).await?, 2);
    assert_eq!(session2.zscore(k_zset, b"player_1").await?, Some(99.5));
    assert_eq!(session2.zrank(k_zset, b"player_2").await?, Some(0));

    info!("测试 5: 混合冷读取与落盘换页数据持久化恢复无损测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
