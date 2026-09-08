//! ZUNIONSTORE/ZINTERSTORE/ZDIFFSTORE/GEOSEARCHSTORE 整体桶锁（dest + 源键排序去重一次性持锁）
//! 并发原子性测试：验证 delete(dest) 与结果写入全程不可被并发写撕裂。
//!
//! 探测手法说明：ghost 配对注入（并发线程反复 ZADD ghost / ZREM ghost 且以 ZREM 收尾）
//! 仅作并发背景噪声——单线程成对注入在「delete 与写入两步」的旧无锁实现上同样通过，
//! 对整体锁无区分力，不作为本组用例的判据。真正有区分力的是：
//! 1. 多线程同 dest 异构 STORE 竞争（ZINTERSTORE vs ZDIFFSTORE 混跑、源集合边改边
//!    STORE）：断言终态恒为某一时间点的完整结果集，杜绝半态混合；
//! 2. dest 自别名用例（dest 同时作为源键）：验证去重加锁路径语义正确且无自锁死锁。

use std::{future::Future, sync::Arc, thread};

use aok::{OK, Void};
use compio::runtime::Runtime;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::{
  AggregateType, GeoSearchCenter, GeoSearchOpt, GeoSearchShape, GeoSortOrder, prelude::*,
};
use wedb_zset::{GeoDistanceUnit, ZAddOpt};
use wkv::{StoreConfig, StoreSession, WedbStore};

/// STORE 线程迭代数
const STORE_ITERS: usize = 25;

/// ghost 注入线程迭代数（每次迭代以 ZREM 配对收尾）
const GHOST_ITERS: usize = 25;

/// 并发线程数（同构 STORE 竞争场景）
const N_THREADS: usize = 4;

/// 并发轮数（多轮循环防偶现）
const ROUNDS: usize = 3;

fn create_test_store() -> aok::Result<(tempfile::TempDir, Arc<WedbStore<SegmentedDevice>>)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("zstore_concurrency.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(4096, 64 * 1024, 128, 0.5)?,
    device,
  )?);
  Ok((dir, store))
}

/// 启动一个独立 OS 线程（自带 compio Runtime 与独立会话）执行异步工作流
///
/// compio thread-per-core 底座的会话 future 非 Send，仅在本线程内 block_on 消费
fn spawn_worker<S, Fut>(
  store: &Arc<WedbStore<SegmentedDevice>>,
  f: S,
) -> thread::JoinHandle<aok::Result<()>>
where
  S: FnOnce(StoreSession<SegmentedDevice>) -> Fut + Send + 'static,
  Fut: Future<Output = aok::Result<()>> + 'static,
{
  let store = Arc::clone(store);
  thread::spawn(move || {
    let rt = Runtime::new()?;
    rt.block_on(async {
      let session = store.new_session()?;
      f(session).await
    })
  })
}

fn join_all(handles: Vec<thread::JoinHandle<aok::Result<()>>>) -> Void {
  for h in handles {
    h.join().unwrap()?;
  }
  OK
}

/// 读取 zset 全量快照并按 member 排序（消除分值序对断言的干扰）
async fn snapshot_sorted(
  session: &StoreSession<SegmentedDevice>,
  key: &[u8],
) -> aok::Result<Vec<(Vec<u8>, f64)>> {
  let mut items = session.zrange(key, 0, -1, false).await?;
  items.sort_by(|a, b| a.0.cmp(&b.0));
  Ok(items)
}

/// 断言 dest 快照与预期 (member, score) 列表完全一致
fn assert_zset_eq(actual: &[(Vec<u8>, f64)], expected: &[(&str, f64)], ctx: &str) {
  let expected: Vec<(Vec<u8>, f64)> = expected
    .iter()
    .map(|(m, s)| (m.as_bytes().to_vec(), *s))
    .collect();
  assert_eq!(actual, &expected, "{ctx}: zset 内容不符");
}

#[test]
fn test_zunionstore_concurrent_ghost_churn_atomic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let s1 = b"zu:s1".to_vec();
    let s2 = b"zu:s2".to_vec();
    let dest = b"zu:dest".to_vec();
    // 固定源集合：并集 (Sum) = {a:1, b:2+3=5, c:4}
    {
      let session = store.new_session()?;
      session
        .zmadd(
          &s1,
          [(1.0, &b"a"[..]), (2.0, &b"b"[..])],
          ZAddOpt::default(),
        )
        .await?;
      session
        .zmadd(
          &s2,
          [(3.0, &b"b"[..]), (4.0, &b"c"[..])],
          ZAddOpt::default(),
        )
        .await?;
    }

    for round in 0..ROUNDS {
      // 预置陈旧成员：验证 STORE 必然先 delete(dest) 再写入
      {
        let session = store.new_session()?;
        session
          .zadd(&dest, 9.0, &b"stale"[..], ZAddOpt::default())
          .await?;
      }

      let mut handles = Vec::new();
      // N_THREADS 个 STORE 线程并发 ZUNIONSTORE 同一 dest（源固定 → 终态唯一确定）
      for _ in 0..N_THREADS {
        let (s1, s2, dest) = (s1.clone(), s2.clone(), dest.clone());
        handles.push(spawn_worker(&store, move |session| async move {
          for _ in 0..STORE_ITERS {
            session
              .zunionstore(&dest, &[&s1, &s2], &[], AggregateType::Sum)
              .await?;
          }
          OK
        }));
      }
      // ghost 线程：ZADD/ZREM 配对注入且以 ZREM 收尾（撕裂窗口的探测器）
      {
        let dest = dest.clone();
        handles.push(spawn_worker(&store, move |session| async move {
          for _ in 0..GHOST_ITERS {
            session
              .zadd(&dest, 1.0, &b"ghost"[..], ZAddOpt::default())
              .await?;
            session.zrem(&dest, &[b"ghost"]).await?;
          }
          OK
        }));
      }
      join_all(handles)?;

      let session = store.new_session()?;
      let snap = snapshot_sorted(&session, &dest).await?;
      assert_zset_eq(
        &snap,
        &[("a", 1.0), ("b", 5.0), ("c", 4.0)],
        &format!("round={round}"),
      );
    }
    OK
  })
}

/// 源集合边改边 STORE：断言 dest 恒为某一时间点源快照的完整并集（无跨时间混合半态）
#[test]
fn test_zunionstore_source_mutation_race_snapshot_atomic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let s1 = b"zm:s1".to_vec();
    let s2 = b"zm:s2".to_vec();
    let dest = b"zm:dest".to_vec();
    {
      let session = store.new_session()?;
      session
        .zadd(&s1, 1.0, &b"a"[..], ZAddOpt::default())
        .await?;
      session
        .zadd(&s2, 2.0, &b"b"[..], ZAddOpt::default())
        .await?;
    }

    let mut handles = Vec::new();
    // 变更线程：成员 m 反复进出 s2，配对收尾后 s2 终态 = {b:2}
    {
      let s2 = s2.clone();
      handles.push(spawn_worker(&store, move |session| async move {
        for _ in 0..(2 * STORE_ITERS) {
          session
            .zadd(&s2, 7.0, &b"m"[..], ZAddOpt::default())
            .await?;
          session.zrem(&s2, &[b"m"]).await?;
        }
        OK
      }));
    }
    // STORE 线程：持续 ZUNIONSTORE
    {
      let (s1, s2, dest) = (s1.clone(), s2.clone(), dest.clone());
      handles.push(spawn_worker(&store, move |session| async move {
        for _ in 0..(2 * STORE_ITERS * N_THREADS) {
          session
            .zunionstore(&dest, &[&s1, &s2], &[], AggregateType::Sum)
            .await?;
        }
        OK
      }));
    }
    join_all(handles)?;

    // 合法终态仅两种：m 已被清除后的并集，或最后一次 STORE 摄取的含 m 快照
    let session = store.new_session()?;
    let snap = snapshot_sorted(&session, &dest).await?;
    let without_m = vec![(b"a".to_vec(), 1.0), (b"b".to_vec(), 2.0)];
    let with_m = vec![
      (b"a".to_vec(), 1.0),
      (b"b".to_vec(), 2.0),
      (b"m".to_vec(), 7.0),
    ];
    assert!(
      snap == without_m || snap == with_m,
      "dest 出现跨时间撕裂半态: {snap:?}"
    );
    OK
  })
}

/// ZINTERSTORE 与 ZDIFFSTORE 竞争同一 dest：终态必为两个完整结果集之一，绝不混合
#[test]
fn test_zinter_zdiff_store_race_same_dest_atomic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let s1 = b"zd:s1".to_vec();
    let s2 = b"zd:s2".to_vec();
    let dest = b"zd:dest".to_vec();
    // 交集 (Sum) = {b:4, c:6}；差集 = {a:1}
    {
      let session = store.new_session()?;
      session
        .zmadd(
          &s1,
          [(1.0, &b"a"[..]), (2.0, &b"b"[..]), (3.0, &b"c"[..])],
          ZAddOpt::default(),
        )
        .await?;
      session
        .zmadd(
          &s2,
          [(2.0, &b"b"[..]), (3.0, &b"c"[..]), (4.0, &b"d"[..])],
          ZAddOpt::default(),
        )
        .await?;
    }

    for round in 0..ROUNDS {
      let mut handles = Vec::new();
      for i in 0..N_THREADS {
        let (s1, s2, dest) = (s1.clone(), s2.clone(), dest.clone());
        handles.push(spawn_worker(&store, move |session| async move {
          for _ in 0..STORE_ITERS {
            if i % 2 == 0 {
              session
                .zinterstore(&dest, &[&s1, &s2], &[], AggregateType::Sum)
                .await?;
            } else {
              session.zdiffstore(&dest, &[&s1, &s2]).await?;
            }
          }
          OK
        }));
      }
      join_all(handles)?;

      let session = store.new_session()?;
      let snap = snapshot_sorted(&session, &dest).await?;
      let inter = vec![(b"b".to_vec(), 4.0), (b"c".to_vec(), 6.0)];
      let diff = vec![(b"a".to_vec(), 1.0)];
      assert!(
        snap == inter || snap == diff,
        "round={round} dest 出现交集/差集混合半态: {snap:?}"
      );
    }
    OK
  })
}

/// GEOSEARCHSTORE 并发原子性：固定源下终态精确等于检索结果集，ghost 注入无残留；
/// 另单线程验证 STOREDIST 距离写回语义
#[test]
fn test_geosearchstore_concurrent_atomic_and_store_dist() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let src = b"geo:src".to_vec();
    let dest = b"geo:dest".to_vec();
    {
      let session = store.new_session()?;
      // near1 距中心 0m，near2 约 157m，far 约 637km（20km 半径外）
      session
        .geoadd_multi(
          &src,
          &[
            (1.000, 1.000, b"near1"),
            (1.001, 1.001, b"near2"),
            (5.000, 5.000, b"far"),
          ],
          false,
          false,
          false,
        )
        .await?;
    }
    let opts = GeoSearchOpt {
      center: GeoSearchCenter::Coord { lon: 1.0, lat: 1.0 },
      shape: GeoSearchShape::Radius {
        radius: 20.0,
        unit: GeoDistanceUnit::KM,
      },
      sort: GeoSortOrder::Asc,
      count: None,
      count_any: false,
    };

    for round in 0..ROUNDS {
      let mut handles = Vec::new();
      for _ in 0..N_THREADS {
        let (src, dest) = (src.clone(), dest.clone());
        handles.push(spawn_worker(&store, move |session| async move {
          for _ in 0..STORE_ITERS {
            session
              .geosearchstore(&dest, &src, opts, false, GeoDistanceUnit::M)
              .await?;
          }
          OK
        }));
      }
      {
        let dest = dest.clone();
        handles.push(spawn_worker(&store, move |session| async move {
          for _ in 0..GHOST_ITERS {
            session
              .zadd(&dest, 1.0, &b"ghost"[..], ZAddOpt::default())
              .await?;
            session.zrem(&dest, &[b"ghost"]).await?;
          }
          OK
        }));
      }
      join_all(handles)?;

      let session = store.new_session()?;
      // 终态精确 = {near1, near2}（分值为回写的 geohash，须与源集合一致）
      let snap = snapshot_sorted(&session, &dest).await?;
      let near1_score = session.zscore(&src, b"near1").await?;
      let near2_score = session.zscore(&src, b"near2").await?;
      let expected = vec![
        (b"near1".to_vec(), near1_score.unwrap()),
        (b"near2".to_vec(), near2_score.unwrap()),
      ];
      assert_eq!(snap, expected, "round={round} geosearchstore 终态撕裂");
    }

    // STOREDIST 语义：分值改为到中心的距离（单位 M）
    {
      let session = store.new_session()?;
      let dist_dest = b"geo:dest_dist".to_vec();
      let count = session
        .geosearchstore(&dist_dest, &src, opts, true, GeoDistanceUnit::M)
        .await?;
      assert_eq!(count, 2);
      // geohash 52 位量化存在亚米级误差，near1 距离应为亚米级而非精确 0
      let near1_dist = session.zscore(&dist_dest, b"near1").await?;
      assert!(
        near1_dist.is_some_and(|d| (0.0..1.0).contains(&d)),
        "near1 距离应近 0: {near1_dist:?}"
      );
      let near2_dist = session.zscore(&dist_dest, b"near2").await?;
      assert!(
        near2_dist.is_some_and(|d| d > 100.0 && d < 200.0),
        "near2 距离应约 157m: {near2_dist:?}"
      );
    }
    OK
  })
}

/// dest 同时作为源键（别名去重加锁路径）：确定性验证自并集语义且无死锁
#[test]
fn test_zstore_dest_alias_self_union_deterministic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let k = b"alias:k".to_vec();
    let s = b"alias:s".to_vec();
    {
      let session = store.new_session()?;
      session
        .zmadd(&k, [(1.0, &b"p"[..]), (2.0, &b"q"[..])], ZAddOpt::default())
        .await?;
      session
        .zmadd(&s, [(3.0, &b"q"[..]), (4.0, &b"r"[..])], ZAddOpt::default())
        .await?;
    }

    // 反复 ZUNIONSTORE k [k, s]：每轮 q 分值 += 3、r 分值 += 4（Sum），p 保持
    let session = store.new_session()?;
    for i in 1..=3 {
      let count = session
        .zunionstore(&k, &[&k, &s], &[], AggregateType::Sum)
        .await?;
      assert_eq!(count, 3, "第 {i} 轮成员数应为 3");
      let q = 2.0 + 3.0 * i as f64;
      let r = 4.0 * i as f64;
      let snap = snapshot_sorted(&session, &k).await?;
      assert_zset_eq(
        &snap,
        &[("p", 1.0), ("q", q), ("r", r)],
        &format!("第 {i} 轮"),
      );
    }

    // 单源自交集：ZINTERSTORE k2 [k2] 应恒等于自身
    let k2 = b"alias:k2".to_vec();
    session
      .zadd(&k2, 1.0, &b"x"[..], ZAddOpt::default())
      .await?;
    let count = session
      .zinterstore(&k2, &[&k2], &[], AggregateType::Sum)
      .await?;
    assert_eq!(count, 1);
    let snap = snapshot_sorted(&session, &k2).await?;
    assert_zset_eq(&snap, &[("x", 1.0)], "单源自交集");

    // 空交集短路：ZDIFFSTORE 输出为空时 dest 应被整体删除
    let empty_dest = b"alias:empty".to_vec();
    session
      .zadd(&empty_dest, 1.0, &b"junk"[..], ZAddOpt::default())
      .await?;
    let count = session.zdiffstore(&empty_dest, &[&s, &s]).await?;
    assert_eq!(count, 0);
    assert!(
      snapshot_sorted(&session, &empty_dest).await?.is_empty(),
      "空差集结果必须清空 dest（含预置 junk）"
    );
    OK
  })
}

/// ZUNIONSTORE Sum 聚合 +inf 与 -inf 产生 NaN：聚合路径绕过 zmadd 入口校验，
/// 必须在清空 dest 之前拦截并报 InvalidScore（错误口径与 zmadd 入口一致），
/// dest 原有内容不得被破坏
#[test]
fn test_zunionstore_nan_aggregation_rejected_dest_intact() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let s1 = b"nan:s1".to_vec();
    let s2 = b"nan:s2".to_vec();
    let dest = b"nan:dest".to_vec();
    {
      let session = store.new_session()?;
      // ±inf 是合法分值（zmadd 入口仅拒 NaN），Sum 聚合 (+inf)+(-inf) 产生 NaN
      session
        .zmadd(
          &s1,
          [(f64::INFINITY, &b"x"[..]), (1.0, &b"keep"[..])],
          ZAddOpt::default(),
        )
        .await?;
      session
        .zmadd(&s2, [(f64::NEG_INFINITY, &b"x"[..])], ZAddOpt::default())
        .await?;
      // 预置哨兵成员：dest 若被误清空即无法通过后续断言
      session
        .zadd(&dest, 7.0, &b"sentinel"[..], ZAddOpt::default())
        .await?;
    }

    let session = store.new_session()?;
    let err = session
      .zunionstore(&dest, &[&s1, &s2], &[], AggregateType::Sum)
      .await;
    assert!(
      matches!(
        err,
        Err(wedb_redis::Error::ZSet(wedb_zset::Error::InvalidScore))
      ),
      "Sum 聚合产生 NaN 必须报 InvalidScore: {err:?}"
    );

    // dest 未被破坏：delete 尚未执行，哨兵成员原样保留
    let snap = snapshot_sorted(&session, &dest).await?;
    assert_zset_eq(&snap, &[("sentinel", 7.0)], "NaN 拦截后 dest 必须原样保留");
    OK
  })
}
