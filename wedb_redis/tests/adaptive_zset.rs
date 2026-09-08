use std::{iter::repeat_n, sync::Arc};

use aok::{OK, Void};
use compio::runtime::Runtime;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use wedb_zset::{ScoreRange, ZAddOpt};
use wkv::{
  StoreConfig, StoreSession, WedbStore, ZSET_MAX_COMPACT_ENTRIES, ZSET_MAX_COMPACT_MEMBER,
};
use wrecord::StorageEncoding;

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
  let db_path = dir.path().join("adaptive_zset.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(4096, 64 * 1024, 128, 0.5)?,
    device,
  )?);
  Ok((dir, store))
}

/// 测试 1: 小有序集合紧凑内联生命周期完整验证
#[test]
fn test_adaptive_zset_compact_lifecycle() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:compact:lifecycle";

    // 初始状态
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.zcard(key).await?, 0);
    assert_eq!(session.zscore(key, b"m1").await?, None);

    // 1. 初始 ZADD 写入紧凑模式
    let (added, score) = session
      .zadd(key, 10.0, b"m1".to_vec(), ZAddOpt::default())
      .await?;
    assert_eq!(added, 1);
    assert_eq!(score, 10.0);
    assert_eq!(session.type_of(key).await?, "zset");

    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.encoding(), StorageEncoding::Compact);
    assert_eq!(session.zcard(key).await?, 1);
    assert_eq!(session.zscore(key, b"m1").await?, Some(10.0));

    // 2. 批量 ZMADD 添加多个元素保持 Compact 模式
    let zmadd_res = session
      .zmadd(
        key,
        vec![(20.0, b"m2".to_vec()), (30.0, b"m3".to_vec())],
        ZAddOpt::default(),
      )
      .await?;
    assert_eq!(zmadd_res, 2);
    assert_eq!(session.zcard(key).await?, 3);

    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.encoding(), StorageEncoding::Compact);

    // 3. ZMSCORE 批量查询
    let scores = session
      .zmscore(key, &[b"m1", b"m2", b"m3", b"nonexistent"])
      .await?;
    assert_eq!(scores, vec![Some(10.0), Some(20.0), Some(30.0), None]);

    // 4. ZRANK 与 ZREVRANK
    assert_eq!(session.zrank(key, b"m1").await?, Some(0));
    assert_eq!(session.zrank(key, b"m2").await?, Some(1));
    assert_eq!(session.zrank(key, b"m3").await?, Some(2));
    assert_eq!(session.zrank(key, b"nonexistent").await?, None);

    assert_eq!(session.zrevrank(key, b"m3").await?, Some(0));
    assert_eq!(session.zrevrank(key, b"m2").await?, Some(1));
    assert_eq!(session.zrevrank(key, b"m1").await?, Some(2));
    assert_eq!(session.zrevrank(key, b"nonexistent").await?, None);

    // 5. ZCOUNT 分数区间统计
    assert_eq!(
      session
        .zcount(key, ScoreRange::new(10.0, true, 30.0, true))
        .await?,
      3
    );
    assert_eq!(
      session
        .zcount(key, ScoreRange::new(10.0, false, 30.0, false))
        .await?,
      1
    );
    assert_eq!(
      session
        .zcount(key, ScoreRange::new(10.0, true, 20.0, false))
        .await?,
      1
    );

    // 6. ZINCRBY 分数增减
    let new_score = session.zincrby(key, 5.0, b"m1").await?;
    assert_eq!(new_score, 15.0);
    assert_eq!(session.zscore(key, b"m1").await?, Some(15.0));

    // zincrby 新增成员
    let new_m_score = session.zincrby(key, 50.0, b"m4").await?;
    assert_eq!(new_m_score, 50.0);
    assert_eq!(session.zcard(key).await?, 4);

    // 7. ZRANGE 正序与逆序
    let range_all = session.zrange(key, 0, -1, false).await?;
    assert_eq!(
      range_all,
      vec![
        (b"m1".to_vec(), 15.0),
        (b"m2".to_vec(), 20.0),
        (b"m3".to_vec(), 30.0),
        (b"m4".to_vec(), 50.0)
      ]
    );

    let rev_range = session.zrange(key, 0, 1, true).await?;
    assert_eq!(
      rev_range,
      vec![(b"m4".to_vec(), 50.0), (b"m3".to_vec(), 30.0)]
    );

    // 8. ZRANGEBYSCORE 分数范围查询
    let score_range = session
      .zrangebyscore(key, ScoreRange::new(15.0, true, 30.0, true), false, 0, 10)
      .await?;
    assert_eq!(
      score_range,
      vec![
        (b"m1".to_vec(), 15.0),
        (b"m2".to_vec(), 20.0),
        (b"m3".to_vec(), 30.0)
      ]
    );

    // 9. ZSCAN 游标分页与正则通配
    let (next_cursor, scanned) = session.zscan(key, 0, 10, Some(b"m*")).await?;
    assert_eq!(next_cursor, 0);
    assert_eq!(scanned.len(), 4);

    // 10. ZPOPMIN 与 ZPOPMAX
    let pop_min = session.zpopmin(key, 1).await?;
    assert_eq!(pop_min, vec![(b"m1".to_vec(), 15.0)]);
    assert_eq!(session.zcard(key).await?, 3);

    let pop_max = session.zpopmax(key, 1).await?;
    assert_eq!(pop_max, vec![(b"m4".to_vec(), 50.0)]);
    assert_eq!(session.zcard(key).await?, 2);

    // 11. ZREMRANGEBYRANK 与 ZREMRANGEBYSCORE
    let rem_rank = session.zremrangebyrank(key, 0, 0).await?;
    assert_eq!(rem_rank, 1);
    assert_eq!(session.zcard(key).await?, 1);
    assert_eq!(session.zscore(key, b"m2").await?, None);
    assert_eq!(session.zscore(key, b"m3").await?, Some(30.0));

    // 12. ZREM 移除最后一个成员
    let rem_cnt = session.zrem(key, &[b"m3"]).await?;
    assert_eq!(rem_cnt, 1);
    assert_eq!(session.zcard(key).await?, 0);

    // 紧凑模式完全清空后零残留验证
    let meta_k = StoreSession::<SegmentedDevice>::meta_key(key);
    assert_eq!(session.read_raw(&meta_k).await?, None);

    OK
  })
}

/// 测试 2: 128 -> 129 数量超限自动跃迁至 wbftree 验证
#[test]
fn test_adaptive_zset_count_promotion_to_bftree() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:count:promotion";

    // 1. 连续插入 128 个元素，保持在 Compact 紧凑内联模式
    for i in 0..ZSET_MAX_COMPACT_ENTRIES {
      let m = format!("elem_{}", pad(i, 4)).into_bytes();
      let (added, score) = session
        .zadd(key, (i as f64) * 1.5, m, ZAddOpt::default())
        .await?;
      assert_eq!(added, 1);
      assert_eq!(score, (i as f64) * 1.5);
    }

    assert_eq!(session.zcard(key).await?, ZSET_MAX_COMPACT_ENTRIES);
    let meta_before = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_before.encoding(), StorageEncoding::Compact);

    // 2. 插入第 129 个元素，触发自动原子跃迁 (Promotion)
    let overflow_member = b"elem_overflow".to_vec();
    let (added, score) = session
      .zadd(key, 9999.0, overflow_member.clone(), ZAddOpt::default())
      .await?;
    assert_eq!(added, 1);
    assert_eq!(score, 9999.0);

    assert_eq!(session.zcard(key).await?, ZSET_MAX_COMPACT_ENTRIES + 1);

    // 验证编码跃迁为 Flattened
    let meta_after = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_after.encoding(), StorageEncoding::Flattened);

    // 3. 验证 100% 数据一致性
    for i in 0..ZSET_MAX_COMPACT_ENTRIES {
      let m = format!("elem_{}", pad(i, 4)).into_bytes();
      let expected_score = (i as f64) * 1.5;
      assert_eq!(session.zscore(key, &m).await?, Some(expected_score));
      assert_eq!(session.zrank(key, &m).await?, Some(i));
    }
    assert_eq!(session.zscore(key, &overflow_member).await?, Some(9999.0));
    assert_eq!(
      session.zrank(key, &overflow_member).await?,
      Some(ZSET_MAX_COMPACT_ENTRIES)
    );

    // 4. 跃迁后继续追加新元素
    let (added_more, _) = session
      .zadd(key, 10000.0, b"elem_more".to_vec(), ZAddOpt::default())
      .await?;
    assert_eq!(added_more, 1);
    assert_eq!(session.zcard(key).await?, ZSET_MAX_COMPACT_ENTRIES + 2);

    OK
  })
}

/// 测试 3: 超过 64 字节的长成员触发自动跃迁至 wbftree
#[test]
fn test_adaptive_zset_large_member_promotion() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // 场景 A: 初始写入即超长 (65 字节) -> 直接以 Flattened 模式创建
    let key_a = b"zset:large:initial";
    let long_member_65 = vec![b'x'; ZSET_MAX_COMPACT_MEMBER + 1];
    let (added, _) = session
      .zadd(key_a, 100.0, long_member_65.clone(), ZAddOpt::default())
      .await?;
    assert_eq!(added, 1);

    let meta_a = session.load_meta(key_a).await?.expect("meta exists");
    assert_eq!(meta_a.encoding(), StorageEncoding::Flattened);
    assert_eq!(session.zscore(key_a, &long_member_65).await?, Some(100.0));
    assert_eq!(session.zcard(key_a).await?, 1);

    // 场景 B: 先写入若干短成员 (Compact 模式)，随后写入一个 65 字节成员 -> 自动跃迁
    let key_b = b"zset:large:transition";
    session
      .zadd(key_b, 1.0, b"short_1".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(key_b, 2.0, b"short_2".to_vec(), ZAddOpt::default())
      .await?;

    let meta_b1 = session.load_meta(key_b).await?.expect("meta exists");
    assert_eq!(meta_b1.encoding(), StorageEncoding::Compact);

    // 插入 65 字节超长成员
    let (added_long, _) = session
      .zadd(key_b, 50.0, long_member_65.clone(), ZAddOpt::default())
      .await?;
    assert_eq!(added_long, 1);

    let meta_b2 = session.load_meta(key_b).await?.expect("meta exists");
    assert_eq!(meta_b2.encoding(), StorageEncoding::Flattened);
    assert_eq!(session.zcard(key_b).await?, 3);

    // 验证所有成员数据准确无损
    assert_eq!(session.zscore(key_b, b"short_1").await?, Some(1.0));
    assert_eq!(session.zscore(key_b, b"short_2").await?, Some(2.0));
    assert_eq!(session.zscore(key_b, &long_member_65).await?, Some(50.0));
    assert_eq!(session.zrank(key_b, &long_member_65).await?, Some(2));

    OK
  })
}

/// 测试 4: Fast Drop 彻底物理清理 (Compact) 与 Version Bump 瞬时隔离 (Flattened)
#[test]
fn test_adaptive_zset_fast_drop_and_isolation() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // 1. Compact 模式下的 delete(key) 彻底物理清理 (零残留)
    let compact_key = b"zset:delete:compact";
    session
      .zadd(compact_key, 1.0, b"c1".to_vec(), ZAddOpt::default())
      .await?;
    session
      .zadd(compact_key, 2.0, b"c2".to_vec(), ZAddOpt::default())
      .await?;

    let meta_c = session.load_meta(compact_key).await?.expect("meta exists");
    assert_eq!(meta_c.encoding(), StorageEncoding::Compact);

    let meta_k = StoreSession::<SegmentedDevice>::meta_key(compact_key);
    assert!(session.read_raw(&meta_k).await?.is_some());

    // 删除键
    assert!(session.delete(compact_key).await?);
    // 验证元数据记录彻底物理删除（零残留）
    assert_eq!(session.read_raw(&meta_k).await?, None);
    assert_eq!(session.zcard(compact_key).await?, 0);

    // 2. Flattened 模式下的 delete(key) 执行 Version Bump 瞬时隔离
    let flattened_key = b"zset:delete:flattened";
    let long_m = vec![b'k'; ZSET_MAX_COMPACT_MEMBER + 1];
    session
      .zadd(flattened_key, 10.0, long_m.clone(), ZAddOpt::default())
      .await?;

    let meta_f = session
      .load_meta(flattened_key)
      .await?
      .expect("meta exists");
    assert_eq!(meta_f.encoding(), StorageEncoding::Flattened);
    let original_version = meta_f.version;

    let meta_kf = StoreSession::<SegmentedDevice>::meta_key(flattened_key);
    assert!(session.read_raw(&meta_kf).await?.is_some());

    // 删除打平存储键
    assert!(session.delete(flattened_key).await?);

    // 验证元数据保留并递增了版本号（Fast Drop 瞬时隔离）
    let meta_after = session
      .load_meta(flattened_key)
      .await?
      .expect("meta exists");
    assert_eq!(meta_after.size, 0);
    assert_eq!(meta_after.version, original_version + 1);
    assert_eq!(session.zcard(flattened_key).await?, 0);
    assert_eq!(session.zscore(flattened_key, &long_m).await?, None);

    OK
  })
}

/// 测试 5: 复杂条件 ZADD (nx, xx, gt, lt, ch) 双模一致性验证
#[test]
fn test_adaptive_zset_complex_zadd_conditions() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:conditions:test";

    // 1. NX 选项（只在不存在时插入）
    let (added, score) = session
      .zadd(
        key,
        100.0,
        b"m1".to_vec(),
        ZAddOpt {
          nx: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 1);
    assert_eq!(score, 100.0);

    // 已存在成员在 NX 模式下不修改，返回原分值且 added 为 0
    let (added, score) = session
      .zadd(
        key,
        200.0,
        b"m1".to_vec(),
        ZAddOpt {
          nx: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 0);
    assert_eq!(score, 100.0);
    assert_eq!(session.zscore(key, b"m1").await?, Some(100.0));

    // 2. XX 选项（只在已存在时更新）
    // 不存在成员在 XX 模式下不添加
    let (added, _) = session
      .zadd(
        key,
        50.0,
        b"m_nonexist".to_vec(),
        ZAddOpt {
          xx: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 0);
    assert_eq!(session.zscore(key, b"m_nonexist").await?, None);

    // 已存在成员在 XX 模式下更新
    let (added, score) = session
      .zadd(
        key,
        150.0,
        b"m1".to_vec(),
        ZAddOpt {
          xx: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 0);
    assert_eq!(score, 150.0);
    assert_eq!(session.zscore(key, b"m1").await?, Some(150.0));

    // 3. GT 选项（只在分值大于当前分值时更新）
    // 尝试传入更小分值（120 < 150）-> 不更新
    let (added, score) = session
      .zadd(
        key,
        120.0,
        b"m1".to_vec(),
        ZAddOpt {
          gt: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 0);
    assert_eq!(score, 150.0);
    assert_eq!(session.zscore(key, b"m1").await?, Some(150.0));

    // 尝试传入更大分值（180 > 150）-> 更新
    let (added, score) = session
      .zadd(
        key,
        180.0,
        b"m1".to_vec(),
        ZAddOpt {
          gt: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 0);
    assert_eq!(score, 180.0);
    assert_eq!(session.zscore(key, b"m1").await?, Some(180.0));

    // 4. LT 选项（只在分值小于当前分值时更新）
    // 尝试传入更大分值（200 > 180）-> 不更新
    let (added, score) = session
      .zadd(
        key,
        200.0,
        b"m1".to_vec(),
        ZAddOpt {
          lt: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 0);
    assert_eq!(score, 180.0);
    assert_eq!(session.zscore(key, b"m1").await?, Some(180.0));

    // 尝试传入更小分值（160 < 180）-> 更新
    let (added, score) = session
      .zadd(
        key,
        160.0,
        b"m1".to_vec(),
        ZAddOpt {
          lt: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 0);
    assert_eq!(score, 160.0);
    assert_eq!(session.zscore(key, b"m1").await?, Some(160.0));

    // 5. CH 选项（更新分值时返回计数 1）
    let (ch_added, score) = session
      .zadd(
        key,
        170.0,
        b"m1".to_vec(),
        ZAddOpt {
          ch: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(ch_added, 1);
    assert_eq!(score, 170.0);

    // 分值无变化时 CH 模式返回 0
    let (ch_added, score) = session
      .zadd(
        key,
        170.0,
        b"m1".to_vec(),
        ZAddOpt {
          ch: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(ch_added, 0);
    assert_eq!(score, 170.0);

    OK
  })
}

/// 测试 6: ZMADD 在跃迁后无 size_delta 时，确保持久化 StorageEncoding::Flattened 元数据
#[test]
fn test_adaptive_zmadd_promotion_with_no_size_delta() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:zmadd:promotion_zero_delta";

    // 1. 初始化一个小 ZSet (Compact 模式)
    let added = session
      .zmadd(
        key,
        vec![(10.0, b"elem_small_1"), (20.0, b"elem_small_2")],
        ZAddOpt::default(),
      )
      .await?;
    assert_eq!(added, 2);
    let meta_before = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_before.encoding(), StorageEncoding::Compact);

    // 2. 调用 zmadd 传入一个长成员（> 64B），但使用 XX 选项（仅更新已存在成员，新成员被忽略）
    // 此时 items 包含长成员，触发跃迁逻辑，但因为 xx 拦截，没有任何元素被新插入或更新 (size_delta = 0)
    let long_member = vec![b'x'; 70];
    let added = session
      .zmadd(
        key,
        vec![(99.0, long_member.as_slice())],
        ZAddOpt {
          xx: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(added, 0);

    // 3. 验证元数据已成功原子持久化为 Flattened，未丢失跃迁状态
    let meta_after = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_after.encoding(), StorageEncoding::Flattened);
    assert_eq!(meta_after.size, 2);

    // 4. 原有数据完全正确可用
    assert_eq!(session.zscore(key, b"elem_small_1").await?, Some(10.0));
    assert_eq!(session.zscore(key, b"elem_small_2").await?, Some(20.0));
    assert_eq!(session.zcard(key).await?, 2);

    OK
  })
}

/// 测试 7: 紧凑模式下连续内存切片原地操作 (zremrangebyrank, zpopmin, zpopmax, zremrangebyscore)
#[test]
fn test_adaptive_zset_continuous_drain_operations() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:compact:continuous_drain";

    // 1. 插入 10 个元素，分值 10.0 到 100.0
    let mut items = Vec::new();
    for i in 1..=10 {
      let m = format!("k{}", pad(i, 2)).into_bytes();
      items.push(((i * 10) as f64, m));
    }
    session.zmadd(key, items, ZAddOpt::default()).await?;

    let meta = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta.encoding(), StorageEncoding::Compact);
    assert_eq!(session.zcard(key).await?, 10);

    // 2. 测试 zremrangebyrank 单次连续切片原地 drain (移除 rank 3 到 5，对应 k04, k05, k06)
    let rem_rank = session.zremrangebyrank(key, 3, 5).await?;
    assert_eq!(rem_rank, 3);
    assert_eq!(session.zcard(key).await?, 7);
    assert_eq!(session.zscore(key, b"k04").await?, None);
    assert_eq!(session.zscore(key, b"k05").await?, None);
    assert_eq!(session.zscore(key, b"k06").await?, None);
    assert_eq!(session.zscore(key, b"k03").await?, Some(30.0));
    assert_eq!(session.zscore(key, b"k07").await?, Some(70.0));

    // 3. 测试 zpopmin 单次连续头部切片原地 drain (弹出最小 2 个：k01, k02)
    let pop_min = session.zpopmin(key, 2).await?;
    assert_eq!(
      pop_min,
      vec![(b"k01".to_vec(), 10.0), (b"k02".to_vec(), 20.0)]
    );
    assert_eq!(session.zcard(key).await?, 5);

    // 4. 测试 zpopmax O(1) 尾部 truncate (弹出最大 2 个：k10, k09)
    let pop_max = session.zpopmax(key, 2).await?;
    assert_eq!(
      pop_max,
      vec![(b"k10".to_vec(), 100.0), (b"k09".to_vec(), 90.0)]
    );
    assert_eq!(session.zcard(key).await?, 3); // 剩余 k03(30.0), k07(70.0), k08(80.0)

    // 5. 测试 zremrangebyscore 单次连续切片原地 drain (按分值区间 [60.0, 75.0] 移除 k07)
    let rem_score = session
      .zremrangebyscore(
        key,
        ScoreRange {
          min: 60.0,
          max: 75.0,
          min_inclusive: true,
          max_inclusive: true,
        },
      )
      .await?;
    assert_eq!(rem_score, 1);
    assert_eq!(session.zcard(key).await?, 2); // 剩余 k03(30.0), k08(80.0)

    // 5.1 测试 zremrangebyscore 开区间边界严格判断与提前截断 (30.0, 80.0)
    let rem_exclusive = session
      .zremrangebyscore(
        key,
        ScoreRange {
          min: 30.0,
          max: 80.0,
          min_inclusive: false,
          max_inclusive: false,
        },
      )
      .await?;
    assert_eq!(rem_exclusive, 0);
    assert_eq!(session.zcard(key).await?, 2);

    // 6. 测试 zrangebyscore 零拷贝反向引用收集与分页
    let range_res = session
      .zrangebyscore(
        key,
        ScoreRange {
          min: 0.0,
          max: 100.0,
          min_inclusive: true,
          max_inclusive: true,
        },
        true, // reverse
        0,
        10,
      )
      .await?;
    assert_eq!(
      range_res,
      vec![(b"k08".to_vec(), 80.0), (b"k03".to_vec(), 30.0)]
    );

    // 7. 再次验证 Compact 模式未改变
    let meta_final = session.load_meta(key).await?.expect("meta exists");
    assert_eq!(meta_final.encoding(), StorageEncoding::Compact);

    OK
  })
}

/// 测试: 批量 ZMADD 无 CH 更新已有成员分数必须持久化（返回 0 但数据不得丢失，Redis ZADD 语义）
#[test]
fn test_zmadd_score_update_without_ch_persisted() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:zmadd:noch";

    // 紧凑模式写入 2 个成员
    let added = session
      .zmadd(
        key,
        vec![(1.0, b"m".to_vec()), (2.0, b"n".to_vec())],
        ZAddOpt::default(),
      )
      .await?;
    assert_eq!(added, 2);

    // 无 CH 更新已有成员分数：返回 0，但分数必须真正落盘
    let ret = session
      .zmadd(key, vec![(9.0, b"m".to_vec())], ZAddOpt::default())
      .await?;
    assert_eq!(ret, 0, "无 CH 时返回新增数 0");
    assert_eq!(
      session.zscore(key, b"m").await?,
      Some(9.0),
      "无 CH 的分数更新不得丢失"
    );
    assert_eq!(session.zcard(key).await?, 2, "更新不得改变基数");

    // GT 选项拦截的低分更新：返回 0 且分数保持不变
    let ret = session
      .zmadd(
        key,
        vec![(5.0, b"m".to_vec())],
        ZAddOpt {
          gt: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(ret, 0);
    assert_eq!(
      session.zscore(key, b"m").await?,
      Some(9.0),
      "GT 拦截后分数不变"
    );

    // 带 CH 更新：返回 1 且分数持久化
    let ret = session
      .zmadd(
        key,
        vec![(7.0, b"m".to_vec())],
        ZAddOpt {
          ch: true,
          ..Default::default()
        },
      )
      .await?;
    assert_eq!(ret, 1);
    assert_eq!(session.zscore(key, b"m").await?, Some(7.0));

    OK
  })
}

/// 测试: ZINTERCARD LIMIT 提前终止语义Redis/Garnet）
#[test]
fn test_zintercard_limit_semantics() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    let a = b"zc:a";
    let b = b"zc:b";
    let c = b"zc:c";
    session
      .zmadd(
        a,
        vec![
          (1.0, b"m1".as_slice()),
          (2.0, b"m2".as_slice()),
          (3.0, b"m3".as_slice()),
          (4.0, b"m4".as_slice()),
        ],
        ZAddOpt::default(),
      )
      .await?;
    session
      .zmadd(
        b,
        vec![
          (1.0, b"m2".as_slice()),
          (2.0, b"m3".as_slice()),
          (9.0, b"x".as_slice()),
        ],
        ZAddOpt::default(),
      )
      .await?;
    session
      .zmadd(
        c,
        vec![(5.0, b"m2".as_slice()), (6.0, b"m3".as_slice())],
        ZAddOpt::default(),
      )
      .await?;

    // 完整交集 {m2, m3}
    assert_eq!(session.zintercard(&[a, b, c], 0).await?, 2);
    assert_eq!(session.zintercard(&[a, b, c], 10).await?, 2);
    // LIMIT 封顶
    assert_eq!(session.zintercard(&[a, b, c], 1).await?, 1);
    // 单键
    assert_eq!(session.zintercard(&[a], 0).await?, 4);
    // 空键列表
    assert_eq!(session.zintercard(&[], 0).await?, 0);

    OK
  })
}

/// 空成员迁移后仍可查分、排名及删除，避免两种编码接受域不一致。
#[test]
fn test_empty_member_survives_promotion() -> Void {
  Runtime::new()?.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"zset:empty:promotion";
    session.zadd(key, 1.0, b"", ZAddOpt::default()).await?;
    let large = vec![b'x'; ZSET_MAX_COMPACT_MEMBER + 1];
    session.zadd(key, 2.0, &large, ZAddOpt::default()).await?;
    assert_eq!(
      session.load_meta(key).await?.unwrap().encoding(),
      StorageEncoding::Flattened
    );
    assert_eq!(session.zscore(key, b"").await?, Some(1.0));
    assert_eq!(session.zrank(key, b"").await?, Some(0));
    assert_eq!(session.zrem(key, &[b"".as_slice()]).await?, 1);
    assert_eq!(session.zcard(key).await?, 1);
    OK
  })
}

/// ZADD 选项在紧凑与打平编码中保持一致，失败不得改动已有分值。
#[test]
fn test_zadd_options_across_encodings() -> Void {
  Runtime::new()?.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    for member in [b"m".to_vec(), vec![b'x'; ZSET_MAX_COMPACT_MEMBER + 1]] {
      let key = member.as_slice();
      session.zadd(key, 10.0, &member, ZAddOpt::default()).await?;
      let incr = ZAddOpt {
        incr: true,
        ..Default::default()
      };
      assert_eq!(session.zadd(key, 2.0, &member, incr).await?, (1, 12.0));
      assert_eq!(
        session
          .zadd(key, -1.0, &member, ZAddOpt { gt: true, ..incr })
          .await?,
        (0, 12.0)
      );
      assert_eq!(session.zmadd(key, [(3.0, &member)], incr).await?, 1);
      assert_eq!(session.zscore(key, &member).await?, Some(15.0));
      let invalid = ZAddOpt {
        gt: true,
        lt: true,
        ..Default::default()
      };
      assert!(session.zadd(key, 20.0, &member, invalid).await.is_err());
      assert!(
        session
          .zmadd(key, [(20.0, &member)], invalid)
          .await
          .is_err()
      );
      assert!(
        session
          .zmadd(key, [(1.0, &member), (2.0, &member)], incr)
          .await
          .is_err()
      );
      assert_eq!(session.zscore(key, &member).await?, Some(15.0));
      session
        .zadd(key, f64::INFINITY, &member, ZAddOpt::default())
        .await?;
      assert!(
        session
          .zadd(key, f64::NEG_INFINITY, &member, incr)
          .await
          .is_err()
      );
      assert_eq!(session.zscore(key, &member).await?, Some(f64::INFINITY));
    }
    let xx = ZAddOpt {
      xx: true,
      ..Default::default()
    };
    assert_eq!(session.zmadd(b"missing", [(1.0, b"m")], xx).await?, 0);
    assert_eq!(session.type_of(b"missing").await?, "none");
    OK
  })
}

/// stop 小于负长度时范围为空，读删语义须一致。
#[test]
fn test_rank_stop_before_first_member() -> Void {
  Runtime::new()?.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    for member in [b"m".to_vec(), vec![b'x'; ZSET_MAX_COMPACT_MEMBER + 1]] {
      let key = member.as_slice();
      session.zadd(key, 1.0, &member, ZAddOpt::default()).await?;
      for stop in [-2, isize::MIN] {
        for reverse in [false, true] {
          assert!(session.zrange(key, 0, stop, reverse).await?.is_empty());
        }
        assert_eq!(session.zremrangebyrank(key, 0, stop).await?, 0);
        assert_eq!(session.zcard(key).await?, 1);
      }
    }
    OK
  })
}
