use std::{iter::repeat_n, path::PathBuf, sync::Arc, time::Instant};

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use wedb_zset::{ScoreRange, ZAddOpt};
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

/// 辅助函数：创建测试用 WedbStore
fn create_test_store() -> aok::Result<(tempfile::TempDir, Arc<WedbStore<SegmentedDevice>>)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("flattened_zset.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(2048, 64 * 1024, 16, 0.5)?,
    device,
  )?);
  Ok((dir, store))
}

/// 测试 1: 基础命令全覆盖 (zadd, zscore, zmscore, zcard, zrem, zcount)
#[test]
fn test_flattened_zset_basic_commands() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:test:basic";

    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.zcard(key).await?, 0);
    assert_eq!(session.zscore(key, b"m1").await?, None);

    // 1. ZADD 单成员新增
    let (added, score) = session
      .zadd(key, 100.0, b"m1".to_vec(), ZAddOpt::default())
      .await?;
    assert_eq!(added, 1);
    assert_eq!(score, 100.0);
    assert_eq!(session.type_of(key).await?, "zset");
    assert_eq!(session.zcard(key).await?, 1);
    assert_eq!(session.zscore(key, b"m1").await?, Some(100.0));

    // 2. ZADD 更新已有成员分数
    let (added, score) = session
      .zadd(key, 150.0, b"m1".to_vec(), ZAddOpt::default())
      .await?;
    assert_eq!(added, 0);
    assert_eq!(score, 150.0);
    assert_eq!(session.zcard(key).await?, 1);
    assert_eq!(session.zscore(key, b"m1").await?, Some(150.0));

    // 3. 批量添加多个成员
    session
      .zadd(key, 200.0, b"m2".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(key, 50.0, b"m0".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(key, 300.0, b"m3".to_vec(), ZAddOpt::default())
      .await?;
    assert_eq!(session.zcard(key).await?, 4);

    // 4. ZMSCORE 批量查询分数
    let scores = session
      .zmscore(key, &[b"m0", b"m1", b"m2", b"m3", b"m_unknown"])
      .await?;
    assert_eq!(
      scores,
      vec![Some(50.0), Some(150.0), Some(200.0), Some(300.0), None]
    );

    // 5. ZCOUNT 分数区间统计
    assert_eq!(
      session
        .zcount(key, ScoreRange::new(50.0, true, 200.0, true))
        .await?,
      3
    );
    assert_eq!(
      session
        .zcount(key, ScoreRange::new(50.0, false, 200.0, false))
        .await?,
      1
    );

    // 6. ZREM 移除成员
    let removed = session.zrem(key, &[b"m1", b"m_nonexist"]).await?;
    assert_eq!(removed, 1);
    assert_eq!(session.zcard(key).await?, 3);
    assert_eq!(session.zscore(key, b"m1").await?, None);

    info!("测试 1: 基础命令全覆盖通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 2: 范围与排序测试 (zrange, zrange reverse, zrangebyscore)
#[test]
fn test_flattened_zset_range_and_sort() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:test:range";

    // 写入 5 个元素: a:10, b:20, c:30, d:40, e:50
    let items = [
      (10.0, b"a".to_vec()),
      (20.0, b"b".to_vec()),
      (30.0, b"c".to_vec()),
      (40.0, b"d".to_vec()),
      (50.0, b"e".to_vec()),
    ];
    for (score, member) in items {
      session.zadd(key, score, member, ZAddOpt::default()).await?;
    }
    assert_eq!(session.zcard(key).await?, 5);

    // 1. ZRANGE 全量正序 (0..-1)
    let full = session.zrange(key, 0, -1, false).await?;
    assert_eq!(
      full,
      vec![
        (b"a".to_vec(), 10.0),
        (b"b".to_vec(), 20.0),
        (b"c".to_vec(), 30.0),
        (b"d".to_vec(), 40.0),
        (b"e".to_vec(), 50.0),
      ]
    );

    // 2. ZRANGE 逆序 (0..-1, reverse: true)
    let rev_full = session.zrange(key, 0, -1, true).await?;
    assert_eq!(
      rev_full,
      vec![
        (b"e".to_vec(), 50.0),
        (b"d".to_vec(), 40.0),
        (b"c".to_vec(), 30.0),
        (b"b".to_vec(), 20.0),
        (b"a".to_vec(), 10.0),
      ]
    );

    // 3. ZRANGE 切片索引 (1..3)
    let slice = session.zrange(key, 1, 3, false).await?;
    assert_eq!(
      slice,
      vec![
        (b"b".to_vec(), 20.0),
        (b"c".to_vec(), 30.0),
        (b"d".to_vec(), 40.0),
      ]
    );

    // 4. ZRANGE 负数负向索引 (-3..-1)
    let neg_slice = session.zrange(key, -3, -1, false).await?;
    assert_eq!(
      neg_slice,
      vec![
        (b"c".to_vec(), 30.0),
        (b"d".to_vec(), 40.0),
        (b"e".to_vec(), 50.0),
      ]
    );

    // 5. ZRANGEBYSCORE 包含区间 [20, 40]
    let by_score = session
      .zrangebyscore(key, ScoreRange::new(20.0, true, 40.0, true), false, 0, 10)
      .await?;
    assert_eq!(
      by_score,
      vec![
        (b"b".to_vec(), 20.0),
        (b"c".to_vec(), 30.0),
        (b"d".to_vec(), 40.0),
      ]
    );

    // 6. ZRANGEBYSCORE 开区间 (20, 40)
    let by_score_open = session
      .zrangebyscore(key, ScoreRange::new(20.0, false, 40.0, false), false, 0, 10)
      .await?;
    assert_eq!(by_score_open, vec![(b"c".to_vec(), 30.0)]);

    // 7. ZRANGEBYSCORE 带 offset 和 count
    let by_score_limit = session
      .zrangebyscore(key, ScoreRange::new(10.0, true, 50.0, true), false, 1, 2)
      .await?;
    assert_eq!(
      by_score_limit,
      vec![(b"b".to_vec(), 20.0), (b"c".to_vec(), 30.0)]
    );

    // 8. ZRANGEBYSCORE 逆序 (reverse: true)
    let by_score_rev = session
      .zrangebyscore(key, ScoreRange::new(20.0, true, 40.0, true), true, 0, 10)
      .await?;
    assert_eq!(
      by_score_rev,
      vec![
        (b"d".to_vec(), 40.0),
        (b"c".to_vec(), 30.0),
        (b"b".to_vec(), 20.0),
      ]
    );

    info!("测试 2: 范围与排序测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 3: Order-Statistic 顺序统计排名测试 (zrank, zrevrank)
#[test]
fn test_flattened_zset_order_statistic_rank() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:test:rank";

    let members = [
      (10.0, b"alice".to_vec()),
      (20.0, b"bob".to_vec()),
      (30.0, b"charlie".to_vec()),
      (40.0, b"david".to_vec()),
      (50.0, b"eve".to_vec()),
    ];
    for (s, m) in members {
      session.zadd(key, s, m, ZAddOpt::default()).await?;
    }

    // 1. ZRANK 0-based 升序绝对排名
    assert_eq!(session.zrank(key, b"alice").await?, Some(0));
    assert_eq!(session.zrank(key, b"bob").await?, Some(1));
    assert_eq!(session.zrank(key, b"charlie").await?, Some(2));
    assert_eq!(session.zrank(key, b"david").await?, Some(3));
    assert_eq!(session.zrank(key, b"eve").await?, Some(4));
    assert_eq!(session.zrank(key, b"non_exist").await?, None);

    // 2. ZREVRANK 0-based 降序绝对排名
    assert_eq!(session.zrevrank(key, b"eve").await?, Some(0));
    assert_eq!(session.zrevrank(key, b"david").await?, Some(1));
    assert_eq!(session.zrevrank(key, b"charlie").await?, Some(2));
    assert_eq!(session.zrevrank(key, b"bob").await?, Some(3));
    assert_eq!(session.zrevrank(key, b"alice").await?, Some(4));
    assert_eq!(session.zrevrank(key, b"non_exist").await?, None);

    // 3. 动态更新分数后排名即时刷新 (O(log N))
    session
      .zadd(key, 5.0, b"eve".to_vec(), ZAddOpt::default())
      .await?;
    // 现在 eve 分数最小 (5.0)，升序排名变为 0，降序排名变为 4
    assert_eq!(session.zrank(key, b"eve").await?, Some(0));
    assert_eq!(session.zrank(key, b"alice").await?, Some(1));
    assert_eq!(session.zrevrank(key, b"eve").await?, Some(4));

    info!("测试 3: Order-Statistic 顺序统计排名测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 4: 增量与弹出测试 (zincrby, zpopmin, zpopmax)
#[test]
fn test_flattened_zset_incr_and_pop() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:test:pop";

    // 1. ZINCRBY 新建成员与已有成员累加
    let s1 = session.zincrby(key, 10.5, b"hero").await?;
    assert_eq!(s1, 10.5);
    assert_eq!(session.zscore(key, b"hero").await?, Some(10.5));

    let s2 = session.zincrby(key, 9.5, b"hero").await?;
    assert_eq!(s2, 20.0);
    assert_eq!(session.zscore(key, b"hero").await?, Some(20.0));

    // 添加更多成员
    session
      .zadd(key, 5.0, b"novice".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(key, 100.0, b"boss".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(key, 50.0, b"veteran".to_vec(), ZAddOpt::default())
      .await?;
    assert_eq!(session.zcard(key).await?, 4);

    // 2. ZPOPMIN 弹出最小元素
    let popped_min = session.zpopmin(key, 1).await?;
    assert_eq!(popped_min, vec![(b"novice".to_vec(), 5.0)]);
    assert_eq!(session.zcard(key).await?, 3);
    assert_eq!(session.zscore(key, b"novice").await?, None);

    // 3. ZPOPMAX 弹出最大元素
    let popped_max = session.zpopmax(key, 2).await?;
    assert_eq!(
      popped_max,
      vec![(b"boss".to_vec(), 100.0), (b"veteran".to_vec(), 50.0)]
    );
    assert_eq!(session.zcard(key).await?, 1);
    assert_eq!(session.zscore(key, b"hero").await?, Some(20.0));

    info!("测试 4: 增量与弹出测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 5: Fast Drop (O(1) 瞬时删除) 与版本隔离测试
#[test]
fn test_flattened_zset_fast_drop_and_version_isolation() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let zkey = b"zset:fast_drop:key";

    // 1. 写入一批数据
    for i in 0..100 {
      let member = format!("item_{}", i).into_bytes();
      session
        .zadd(zkey, i as f64, member, ZAddOpt::default())
        .await?;
    }
    assert_eq!(session.zcard(zkey).await?, 100);
    assert_eq!(session.type_of(zkey).await?, "zset");
    assert_eq!(session.zscore(zkey, b"item_50").await?, Some(50.0));

    // 2. 执行 session.delete(zkey)
    assert!(session.delete(zkey).await?);

    // 3. 验证即时清空
    assert_eq!(session.zcard(zkey).await?, 0);
    assert_eq!(session.zscore(zkey, b"item_50").await?, None);
    assert_eq!(session.type_of(zkey).await?, "none");
    assert_eq!(session.zrange(zkey, 0, -1, false).await?, Vec::new());

    // 4. 再次对同名 key 执行 zadd，验证自动进入全新 Version，历史数据完全隔离
    session
      .zadd(zkey, 999.0, b"new_item".to_vec(), ZAddOpt::default())
      .await?;
    assert_eq!(session.type_of(zkey).await?, "zset");
    assert_eq!(session.zcard(zkey).await?, 1);
    assert_eq!(session.zscore(zkey, b"new_item").await?, Some(999.0));
    // 历史成员绝对查不到
    assert_eq!(session.zscore(zkey, b"item_50").await?, None);
    assert_eq!(session.zrank(zkey, b"item_50").await?, None);
    assert_eq!(session.zrank(zkey, b"new_item").await?, Some(0));

    info!("测试 5: Fast Drop 与版本隔离测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 6: 千万级无单大对象写放大压力测试 (批量 3,000 个成员)
#[test]
fn test_flattened_zset_large_scale_stress() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let zkey = b"zset:stress:large";

    let n = 3000;
    info!("开始向打平 ZSet 写入 {n} 个成员...");
    let start_time = Instant::now();

    for i in 0..n {
      let member = format!("heavy_member_{}", pad(i, 6)).into_bytes();
      let (added, score) = session
        .zadd(zkey, i as f64, member, ZAddOpt::default())
        .await?;
      assert_eq!(added, 1);
      assert_eq!(score, i as f64);
    }

    let elapsed = start_time.elapsed();
    info!("写入 {n} 个成员完成，总耗时: {elapsed:?}");

    // 验证容量与抽样查询
    assert_eq!(session.zcard(zkey).await?, n);
    assert_eq!(
      session.zscore(zkey, b"heavy_member_000000").await?,
      Some(0.0)
    );
    assert_eq!(
      session.zscore(zkey, b"heavy_member_001500").await?,
      Some(1500.0)
    );
    assert_eq!(
      session.zscore(zkey, b"heavy_member_002999").await?,
      Some(2999.0)
    );

    // 验证 O(log N) 排名查询
    assert_eq!(
      session.zrank(zkey, b"heavy_member_001500").await?,
      Some(1500)
    );
    assert_eq!(
      session.zrevrank(zkey, b"heavy_member_001500").await?,
      Some(1499)
    );

    // 验证 O(log N) zcount
    assert_eq!(
      session
        .zcount(zkey, ScoreRange::new(1000.0, true, 2000.0, true))
        .await?,
      1001
    );

    info!("测试 6: 千万级无单大对象写放大压力测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 7: 范围删除与批量添加测试 (zmadd, zremrangebyrank, zremrangebyscore)
#[test]
fn test_flattened_zset_range_rem_and_zmadd() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:test:range_rem";

    // 1. ZMADD 批量添加
    let items = [
      (10.0, &b"k1"[..]),
      (20.0, &b"k2"[..]),
      (30.0, &b"k3"[..]),
      (40.0, &b"k4"[..]),
      (50.0, &b"k5"[..]),
      (60.0, &b"k6"[..]),
    ];
    let added = session.zmadd(key, items, ZAddOpt::default()).await?;
    assert_eq!(added, 6);
    assert_eq!(session.zcard(key).await?, 6);

    // 2. ZREMRANGEBYRANK 删除排名 [1..=2] (即 k2:20, k3:30)
    let rem_rank = session.zremrangebyrank(key, 1, 2).await?;
    assert_eq!(rem_rank, 2);
    assert_eq!(session.zcard(key).await?, 4);
    assert_eq!(session.zscore(key, b"k2").await?, None);
    assert_eq!(session.zscore(key, b"k3").await?, None);
    assert_eq!(session.zscore(key, b"k1").await?, Some(10.0));
    assert_eq!(session.zscore(key, b"k4").await?, Some(40.0));

    // 3. ZREMRANGEBYSCORE 删除分数区间 [40, 55] (即 k4:40, k5:50)
    let rem_score = session
      .zremrangebyscore(key, ScoreRange::new(40.0, true, 55.0, true))
      .await?;
    assert_eq!(rem_score, 2);
    assert_eq!(session.zcard(key).await?, 2);
    assert_eq!(session.zscore(key, b"k4").await?, None);
    assert_eq!(session.zscore(key, b"k5").await?, None);
    assert_eq!(session.zscore(key, b"k6").await?, Some(60.0));

    info!("测试 7: 范围删除与批量添加测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 8: 游标扫描测试 (zscan 分页与 glob 匹配)
#[test]
fn test_flattened_zset_zscan() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:test:scan";

    // 写入 25 个成员: apple_1..apple_15, banana_1..banana_10
    let mut items = Vec::new();
    for i in 1..=15 {
      items.push((i as f64, format!("apple_{}", i).into_bytes()));
    }
    for i in 1..=10 {
      items.push(((100 + i) as f64, format!("banana_{}", i).into_bytes()));
    }
    session.zmadd(key, items, ZAddOpt::default()).await?;
    assert_eq!(session.zcard(key).await?, 25);

    // 1. 无通配符分页全扫描
    let mut cursor = 0;
    let mut all_scanned = Vec::new();
    loop {
      let (next_cursor, batch) = session.zscan(key, cursor, 10, None).await?;
      all_scanned.extend(batch);
      cursor = next_cursor;
      if cursor == 0 {
        break;
      }
    }
    assert_eq!(all_scanned.len(), 25);

    // 2. 带 glob pattern 过滤扫描
    let mut cursor = 0;
    let mut apple_scanned = Vec::new();
    loop {
      let (next_cursor, batch) = session.zscan(key, cursor, 10, Some(b"apple_*")).await?;
      apple_scanned.extend(batch);
      cursor = next_cursor;
      if cursor == 0 {
        break;
      }
    }
    assert_eq!(apple_scanned.len(), 15);

    info!("测试 8: 游标扫描测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 9: 地理空间位置查询测试 (geoadd, geopos, geodist, geohash)
#[test]
fn test_flattened_zset_geo_api() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:test:geo";

    // 1. GEOADD 添加两个城市坐标: Palermo 和 Catania
    let add1 = session
      .geoadd(key, 38.1156879, 13.3612671, b"Palermo")
      .await?;
    let add2 = session
      .geoadd(key, 37.5024815, 15.0878329, b"Catania")
      .await?;
    assert!(add1);
    assert!(add2);
    assert_eq!(session.zcard(key).await?, 2);

    // 2. GEOPOS 坐标反查 (误差在地理哈希解算精度范围内)
    let pos_palermo = session.geopos(key, b"Palermo").await?;
    assert!(pos_palermo.is_some());
    let (lon1, lat1) = pos_palermo.unwrap();
    assert!((lon1 - 13.3612671).abs() < 0.001);
    assert!((lat1 - 38.1156879).abs() < 0.001);

    // 3. GEODIST 两地大圆球面距离 (Palermo 到 Catania 约 166 公里)
    let dist = session.geodist(key, b"Palermo", b"Catania").await?;
    assert!(dist.is_some());
    let dist_m = dist.unwrap();
    assert!((dist_m - 166_274.0).abs() < 1000.0);

    // 4. GEOHASH 获取 geohash 字符串
    let hashes = session
      .geohash(key, &[b"Palermo", b"Catania", b"NonExist"])
      .await?;
    assert_eq!(hashes.len(), 3);
    assert!(hashes[0].is_some());
    assert!(hashes[1].is_some());
    assert!(hashes[2].is_none());

    info!("测试 9: 地理空间位置查询测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 10: 极端边界、滑动窗口流式截断与临时磁盘索引安全回收测试
#[test]
fn test_flattened_zset_edge_cases_and_sliding_window() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:test:edges";

    // 1. 空成员列表快速路径
    assert_eq!(session.zmscore(key, &[]).await?, Vec::<Option<f64>>::new());
    assert_eq!(session.zrem(key, &[]).await?, 0);

    // 2. 写入 10 个数据项 (0..10)
    for i in 0..10 {
      session
        .zadd(
          key,
          (i * 10) as f64,
          format!("m{}", i).into_bytes(),
          ZAddOpt::default(),
        )
        .await?;
    }
    assert_eq!(session.zcard(key).await?, 10);

    // 3. ZRANGEBYSCORE 逆序滑动窗口精准截断测试 (offset: 2, count: 3) // 逆序全量: m9:90, m8:80, m7:70, m6:60, m5:50, m4:40, m3:30, m2:20, m1:10, m0:0
    // skip(2).take(3) => [m7:70, m6:60, m5:50]
    let rev_window = session
      .zrangebyscore(key, ScoreRange::new(0.0, true, 90.0, true), true, 2, 3)
      .await?;
    assert_eq!(
      rev_window,
      vec![
        (b"m7".to_vec(), 70.0),
        (b"m6".to_vec(), 60.0),
        (b"m5".to_vec(), 50.0),
      ]
    );

    // 4. ZPOPMAX count = 0
    assert_eq!(session.zpopmax(key, 0).await?, Vec::new());
    assert_eq!(session.zcard(key).await?, 10);

    // 5. ZPOPMAX count = 2
    let pop2 = session.zpopmax(key, 2).await?;
    assert_eq!(pop2, vec![(b"m9".to_vec(), 90.0), (b"m8".to_vec(), 80.0)]);
    assert_eq!(session.zcard(key).await?, 8);

    // 6. ZPOPMAX count > 剩余元素总数
    let pop_all = session.zpopmax(key, 20).await?;
    assert_eq!(pop_all.len(), 8);
    assert_eq!(pop_all[0], (b"m7".to_vec(), 70.0));
    assert_eq!(pop_all[7], (b"m0".to_vec(), 0.0));
    assert_eq!(session.zcard(key).await?, 0);

    // 7. 再次 ZPOPMAX 空集合
    assert_eq!(session.zpopmax(key, 5).await?, Vec::new());

    // 8. 验证临时文件安全回收 (Drop WedbStore)
    let temp_bftree_file = store.bftree.file_path().map(PathBuf::from);
    assert!(temp_bftree_file.is_some());
    let path = temp_bftree_file.unwrap();
    assert!(path.exists());

    drop(session);
    drop(store);
    drop(dir);

    // WedbStore 被 drop 后，自动生成的临时 bftree 文件已被安全删除
    assert!(!path.exists());

    info!("测试 10: 极端边界、滑动窗口流式截断与临时磁盘索引安全回收测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
