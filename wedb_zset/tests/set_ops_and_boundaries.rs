//! 有序集合高级集合运算、极端边界防护与 Geo 批量特性的综合测试
use core::time::Duration;
use std::thread::sleep;

use aok::{OK, Void};
use coarsetime::Clock;
use wedb_zset::{
  CompactZSet, CompactZSetExt, ExpireOpt, ExpireResult, ScoreRange, SortedSetAggregate,
  SortedSetObject, ZAddOpt, geo_distance, geo_filter_radius, simd_batch_distance,
};
use whasher::{HashMap, HashMapExt};

/// 1. ZADD 在 NX / GT / LT 遇到同分时，绝不错误清除成员已有的 TTL
#[test]
fn test_zadd_nx_gt_lt_same_score_preserves_ttl() -> Void {
  let mut z = SortedSetObject::new();
  z.zadd(100.0, b"elem", ZAddOpt::default())?;

  let future = Clock::now_since_epoch().as_millis() + 60_000;
  assert_eq!(
    z.zexpire(b"elem", future, ExpireOpt::default()),
    ExpireResult::Ok
  );
  assert!(z.zttl(b"elem") > 0);

  // 1.1 NX 条件：已存在成员无论分值是否相同均不执行更新，TTL 保持不动
  let (ret, score) = z.zadd(
    100.0,
    b"elem",
    ZAddOpt {
      nx: true,
      ..ZAddOpt::default()
    },
  )?;
  assert_eq!(ret, 0);
  assert_eq!(score, 100.0);
  assert!(z.zttl(b"elem") > 0, "NX 同分不应清除 TTL");

  // 1.2 GT 条件：新分值 <= 原分值时不执行更新，TTL 保持不动
  let (ret, score) = z.zadd(
    100.0,
    b"elem",
    ZAddOpt {
      gt: true,
      ..ZAddOpt::default()
    },
  )?;
  assert_eq!(ret, 0);
  assert_eq!(score, 100.0);
  assert!(z.zttl(b"elem") > 0, "GT 同分不应清除 TTL");

  // 1.3 LT 条件：新分值 >= 原分值时不执行更新，TTL 保持不动
  let (ret, score) = z.zadd(
    100.0,
    b"elem",
    ZAddOpt {
      lt: true,
      ..ZAddOpt::default()
    },
  )?;
  assert_eq!(ret, 0);
  assert_eq!(score, 100.0);
  assert!(z.zttl(b"elem") > 0, "LT 同分不应清除 TTL");

  // 1.4 普通 ZADD (无 NX/GT/LT)：同分写操作按 Redis 规范重置/清除 TTL
  let (ret, score) = z.zadd(100.0, b"elem", ZAddOpt::default())?;
  assert_eq!(ret, 0);
  assert_eq!(score, 100.0);
  assert_eq!(z.zttl(b"elem"), -1, "普通同分 ZADD 应当清除 TTL");

  OK
}

/// 2. ScoreRange 的 NaN、倒置区间以及退化开区间在 range_by_score 与 count_by_score 中的快速短路
#[test]
fn test_score_range_nan_and_inversion_short_circuit() -> Void {
  let mut z = SortedSetObject::new();
  for i in 1..=10 {
    z.zadd(i as f64, format!("m{}", i).as_bytes(), ZAddOpt::default())?;
  }

  // 2.1 NaN 边界
  let nan_min = ScoreRange::new(f64::NAN, true, 10.0, true);
  let nan_max = ScoreRange::new(1.0, true, f64::NAN, true);
  let both_nan = ScoreRange::new(f64::NAN, true, f64::NAN, true);

  assert!(!nan_min.is_valid());
  assert!(!nan_max.is_valid());
  assert!(!both_nan.is_valid());
  assert!(!nan_min.contains(5.0));

  assert_eq!(z.zcount(nan_min), 0);
  assert_eq!(z.zcount(nan_max), 0);
  assert_eq!(z.zcount(both_nan), 0);
  assert!(z.zrangebyscore(nan_min, false, 0, 10).is_empty());
  assert!(z.zrangebyscore(nan_max, true, 0, 10).is_empty());

  // 2.2 倒置区间 [10, 5]
  let inverted = ScoreRange::new(10.0, true, 5.0, true);
  assert!(!inverted.is_valid());
  assert_eq!(z.zcount(inverted), 0);
  assert!(z.zrangebyscore(inverted, false, 0, 10).is_empty());

  // 2.3 退化开区间 (5, 5)
  let deg_open1 = ScoreRange::new(5.0, false, 5.0, true);
  let deg_open2 = ScoreRange::new(5.0, true, 5.0, false);
  let deg_open3 = ScoreRange::new(5.0, false, 5.0, false);
  assert!(!deg_open1.is_valid());
  assert!(!deg_open2.is_valid());
  assert!(!deg_open3.is_valid());
  assert_eq!(z.zcount(deg_open1), 0);
  assert_eq!(z.zcount(deg_open2), 0);
  assert_eq!(z.zcount(deg_open3), 0);

  // 2.4 退化闭区间 [5, 5] 必须合法且精确匹配
  let point = ScoreRange::new(5.0, true, 5.0, true);
  assert!(point.is_valid());
  assert_eq!(z.zcount(point), 1);
  let res = z.zrangebyscore(point, false, 0, 10);
  assert_eq!(res.len(), 1);
  assert_eq!(res[0].0, b"m5".to_vec());

  OK
}

/// 3. 多集合并集 (ZUNION) 的权重、SUM/MIN/MAX 聚合与过期条目过滤
#[test]
fn test_sorted_set_union() -> Void {
  let mut z1 = SortedSetObject::new();
  z1.zadd(1.0, b"a", ZAddOpt::default())?;
  z1.zadd(2.0, b"b", ZAddOpt::default())?;

  let mut z2 = SortedSetObject::new();
  z2.zadd(10.0, b"b", ZAddOpt::default())?;
  z2.zadd(20.0, b"c", ZAddOpt::default())?;

  // 3.1 默认权重 1.0, SUM: a=1, b=2+10=12, c=20
  let u_sum = SortedSetObject::union(&[(&z1, 1.0), (&z2, 1.0)], SortedSetAggregate::Sum);
  assert_eq!(u_sum.zscore_ref(b"a"), Some(1.0));
  assert_eq!(u_sum.zscore_ref(b"b"), Some(12.0));
  assert_eq!(u_sum.zscore_ref(b"c"), Some(20.0));
  assert_eq!(u_sum.len_ref(), 3);

  // 3.2 权重加权与 MIN / MAX 聚合
  // z1 weight=2.0 (a:2, b:4); z2 weight=0.5 (b:5, c:10)
  let u_min = SortedSetObject::union(&[(&z1, 2.0), (&z2, 0.5)], SortedSetAggregate::Min);
  assert_eq!(u_min.zscore_ref(b"a"), Some(2.0));
  assert_eq!(u_min.zscore_ref(b"b"), Some(4.0)); // min(4, 5)
  assert_eq!(u_min.zscore_ref(b"c"), Some(10.0));

  let u_max = SortedSetObject::union(&[(&z1, 2.0), (&z2, 0.5)], SortedSetAggregate::Max);
  assert_eq!(u_max.zscore_ref(b"b"), Some(5.0)); // max(4, 5)

  // 3.3 排除已过期未清退的幽灵条目
  let mut z3 = SortedSetObject::new();
  z3.zadd(99.0, b"ghost", ZAddOpt::default())?;
  let past = Clock::now_since_epoch().as_millis() - 1000;
  // 直接模拟到期未物理清除
  z3.zadd(50.0, b"valid", ZAddOpt::default())?;
  z3.zexpire(b"ghost", past + 10, ExpireOpt::default()); // 已过去的时间
  let u_ghost = SortedSetObject::union(&[(&z3, 1.0)], SortedSetAggregate::Sum);
  assert_eq!(u_ghost.zscore_ref(b"ghost"), None);
  assert_eq!(u_ghost.zscore_ref(b"valid"), Some(50.0));

  // 3.4 SUM 聚合合成 NaN (+inf 与 -inf 相加) 的条目被显式剔除
  let mut za = SortedSetObject::new();
  za.zadd(f64::INFINITY, b"x", ZAddOpt::default())?;
  let mut zb = SortedSetObject::new();
  zb.zadd(f64::NEG_INFINITY, b"x", ZAddOpt::default())?;
  let u_nan = SortedSetObject::union(&[(&za, 1.0), (&zb, 1.0)], SortedSetAggregate::Sum);
  assert_eq!(u_nan.len_ref(), 0);
  assert_eq!(u_nan.zscore_ref(b"x"), None);
  // MIN/MAX 聚合不存在 NaN 合成问题：min(+inf, -inf) = -inf
  let u_min = SortedSetObject::union(&[(&za, 1.0), (&zb, 1.0)], SortedSetAggregate::Min);
  assert_eq!(u_min.zscore_ref(b"x"), Some(f64::NEG_INFINITY));

  // 3.5 ZINTER 同样剔除 SUM 合成的 NaN
  let mut zc = SortedSetObject::new();
  zc.zadd(f64::INFINITY, b"y", ZAddOpt::default())?;
  let mut zd = SortedSetObject::new();
  zd.zadd(f64::NEG_INFINITY, b"y", ZAddOpt::default())?;
  let i_nan = SortedSetObject::inter(&[(&zc, 1.0), (&zd, 1.0)], SortedSetAggregate::Sum);
  assert_eq!(i_nan.len_ref(), 0);

  OK
}

/// 4. 多集合交集 (ZINTER) 与交集基数 (ZINTERCARD) 及其 limit 提前短路
#[test]
fn test_sorted_set_inter_and_inter_card() -> Void {
  let mut z1 = SortedSetObject::new();
  z1.zadd(1.0, b"a", ZAddOpt::default())?;
  z1.zadd(2.0, b"b", ZAddOpt::default())?;
  z1.zadd(3.0, b"c", ZAddOpt::default())?;

  let mut z2 = SortedSetObject::new();
  z2.zadd(10.0, b"b", ZAddOpt::default())?;
  z2.zadd(20.0, b"c", ZAddOpt::default())?;
  z2.zadd(30.0, b"d", ZAddOpt::default())?;

  let mut z3 = SortedSetObject::new();
  z3.zadd(100.0, b"b", ZAddOpt::default())?;
  z3.zadd(200.0, b"c", ZAddOpt::default())?;

  // 4.1 ZINTER 交集: 共有成员 b, c
  let inter_sum = SortedSetObject::inter(
    &[(&z1, 1.0), (&z2, 1.0), (&z3, 1.0)],
    SortedSetAggregate::Sum,
  );
  assert_eq!(inter_sum.len_ref(), 2);
  assert_eq!(inter_sum.zscore_ref(b"b"), Some(112.0)); // 2 + 10 + 100
  assert_eq!(inter_sum.zscore_ref(b"c"), Some(223.0)); // 3 + 20 + 200
  assert_eq!(inter_sum.zscore_ref(b"a"), None);
  assert_eq!(inter_sum.zscore_ref(b"d"), None);

  // 4.2 任意集合为空时立即返回空
  let empty = SortedSetObject::new();
  let inter_empty = SortedSetObject::inter(&[(&z1, 1.0), (&empty, 1.0)], SortedSetAggregate::Sum);
  assert_eq!(inter_empty.len_ref(), 0);

  // 4.3 ZINTERCARD 交集基数与 limit 提前短路
  assert_eq!(SortedSetObject::inter_card(&[&z1, &z2, &z3], 0), 2);
  assert_eq!(SortedSetObject::inter_card(&[&z1, &z2, &z3], 1), 1); // 达到 limit=1 立即短路
  assert_eq!(SortedSetObject::inter_card(&[&z1, &z2, &z3], 5), 2);
  assert_eq!(SortedSetObject::inter_card(&[&z1, &empty], 0), 0);

  OK
}

/// 5. 多集合差集 (ZDIFF)
#[test]
fn test_sorted_set_diff() -> Void {
  let mut z1 = SortedSetObject::new();
  z1.zadd(1.0, b"a", ZAddOpt::default())?;
  z1.zadd(2.0, b"b", ZAddOpt::default())?;
  z1.zadd(3.0, b"c", ZAddOpt::default())?;

  let mut z2 = SortedSetObject::new();
  z2.zadd(10.0, b"b", ZAddOpt::default())?;

  let mut z3 = SortedSetObject::new();
  z3.zadd(20.0, b"d", ZAddOpt::default())?;

  // z1 - z2 - z3 => 排除 b，剩余 a, c
  let diff_res = SortedSetObject::diff(&z1, &[&z2, &z3]);
  assert_eq!(diff_res.len_ref(), 2);
  assert_eq!(diff_res.zscore_ref(b"a"), Some(1.0));
  assert_eq!(diff_res.zscore_ref(b"c"), Some(3.0));
  assert_eq!(diff_res.zscore_ref(b"b"), None);

  // others 为空时返回原集合克隆
  let diff_self = SortedSetObject::diff(&z1, &[]);
  assert_eq!(diff_self.len_ref(), 3);

  OK
}

/// 6. Geo SIMD 别名与批量范围过滤 (geo_filter_radius)
#[test]
fn test_geo_simd_alias_and_filter_radius() -> Void {
  let center_lat = 39.9042;
  let center_lon = 116.4074; // 北京天安门

  let lats = [39.9042, 39.9087, 31.2304]; // 天安门, 景山 (~500m), 上海 (~1068km)
  let lons = [116.4074, 116.3975, 121.4737];
  let mut distances = [0.0; 3];

  // 验证 simd_batch_distance 别名
  simd_batch_distance(center_lat, center_lon, &lats, &lons, &mut distances);
  assert_eq!(distances[0], 0.0);
  assert!(distances[1] > 400.0 && distances[1] < 1500.0);
  assert!(distances[2] > 1_000_000.0);

  // 验证 geo_filter_radius (半径 2000 米): 仅前两点满足
  let filtered = geo_filter_radius(center_lat, center_lon, 2000.0, &lats, &lons);
  assert_eq!(filtered.len(), 2);
  assert_eq!(filtered[0].0, 0);
  assert_eq!(filtered[1].0, 1);
  for (idx, dist) in filtered {
    let scalar_dist = geo_distance(center_lat, center_lon, lats[idx], lons[idx]);
    assert!((dist - scalar_dist).abs() < 1e-9);
  }

  OK
}

/// 7. GEOADD 覆盖更新不触碰成员 TTL
#[test]
fn test_geoadd_update_preserves_member_ttl() -> Void {
  let mut z = SortedSetObject::new();
  assert!(z.geoadd(39.9042, 116.4074, b"beijing")?);
  let future = Clock::now_since_epoch().as_millis() + 60_000;
  assert_eq!(
    z.zexpire(b"beijing", future, ExpireOpt::default()),
    ExpireResult::Ok
  );

  // 7.1 覆盖更新坐标：分值变更但 TTL 保持不动
  let (added, changed) = z.geoadd_opts(31.2304, 121.4737, b"beijing", false, false)?;
  assert_eq!((added, changed), (0, 1));
  let new_score = z.zscore_ref(b"beijing").unwrap();
  assert!(z.zttl(b"beijing") > 0, "GEOADD 覆盖更新不应清除成员 TTL");

  // 7.2 同坐标重复 GEOADD：无操作，TTL 保持不动
  let (added, changed) = z.geoadd_opts(31.2304, 121.4737, b"beijing", false, false)?;
  assert_eq!((added, changed), (0, 0));
  assert!(z.zttl(b"beijing") > 0);

  // 7.3 NX 命中已存在成员：无操作，TTL 保持不动
  let (added, changed) = z.geoadd_opts(22.5431, 114.0579, b"beijing", true, false)?;
  assert_eq!((added, changed), (0, 0));
  assert!(z.zttl(b"beijing") > 0);
  assert_eq!(z.zscore_ref(b"beijing"), Some(new_score));

  // 7.4组：普通 ZADD 同分写按 Redis 规范清除成员 TTL
  let (ret, _) = z.zadd(new_score, b"beijing", ZAddOpt::default())?;
  assert_eq!(ret, 0);
  assert_eq!(z.zttl(b"beijing"), -1);

  OK
}

/// 用 itoa 构造带数字后缀的成员字节串（替代测试热路径的 format!）
fn member_suffix(prefix: &str, n: usize) -> Vec<u8> {
  let mut s = String::from(prefix);
  let mut ibuf = itoa::Buffer::new();
  s.push_str(ibuf.format(n));
  s.into_bytes()
}

/// 忙等时钟越过截止时间，制造「已过期但未物理清退」状态
fn spin_past_deadline(deadline: u64) -> Void {
  let mut guard = 0;
  while Clock::now_since_epoch().as_millis() <= deadline {
    sleep(Duration::from_millis(2));
    guard += 1;
    assert!(guard < 10_000, "时钟未推进，测试环境异常");
  }
  OK
}

/// 将成员置为「已过期但未清退」：设置近期 TTL 后忙等时钟越过期限
fn expire_without_purge(z: &mut SortedSetObject, member: &[u8]) -> Void {
  let deadline = Clock::now_since_epoch().as_millis() + 15;
  assert_eq!(
    z.zexpire(member, deadline, ExpireOpt::default()),
    ExpireResult::Ok
  );
  spin_past_deadline(deadline)
}

/// 8. ZDIFF 差分测试（黄金模型对拍）：多集合 / 过期成员 / 交叉成员 / 单集合 / 自差 / STORE 计数
#[test]
fn zdiff_differential_against_golden_model() -> Void {
  let mut rng = fastrand::Rng::new();
  const UNIVERSE: usize = 24;
  const ROUNDS: usize = 16;

  // 黄金模型：member -> (score, expired)，与真实集合逐轮同步构建
  for _ in 0..ROUNDS {
    let mut golden: Vec<HashMap<Vec<u8>, (f64, bool)>> = (0..3).map(|_| HashMap::new()).collect();
    let mut real: Vec<SortedSetObject> = (0..3).map(|_| SortedSetObject::new()).collect();

    // 全轮次内共享的近期截止时间：设置 TTL 后统一越线一次
    let deadline = Clock::now_since_epoch().as_millis() + 15;
    for set_idx in 0..3 {
      for i in 0..UNIVERSE {
        // 约 55% 成员命中，分数取 0..50 整数（避免浮点合成噪声）
        if rng.usize(0..100) < 55 {
          let score = rng.usize(0..50) as f64;
          let m = member_suffix("m", i);
          real[set_idx].zadd(score, m.clone(), ZAddOpt::default())?;
          golden[set_idx].insert(m, (score, false));
        }
      }
      // 约 1/2 存活成员共享近期截止时间，稍后统一转为「已过期未清退」
      for i in 0..UNIVERSE {
        let m = member_suffix("m", i);
        if golden[set_idx].contains_key(&m) && rng.bool() {
          assert_eq!(
            real[set_idx].zexpire(&m, deadline, ExpireOpt::default()),
            ExpireResult::Ok
          );
          golden[set_idx].get_mut(&m).unwrap().1 = true;
        }
      }
    }
    spin_past_deadline(deadline)?;

    let alive_in = |g: &HashMap<Vec<u8>, (f64, bool)>, m: &Vec<u8>| {
      g.get(m).is_some_and(|&(_, expired)| !expired)
    };

    // 期望差集：A 存活且不在 B/C 任一存活集合中，分值取 A
    let mut expected: Vec<(f64, Vec<u8>)> = golden[0]
      .iter()
      .filter(|&(m, &(_, expired))| {
        !expired && !alive_in(&golden[1], m) && !alive_in(&golden[2], m)
      })
      .map(|(m, &(s, _))| (s, m.clone()))
      .collect();
    expected.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

    // 对拍 1：ZDIFF 成员集合、顺序与分值逐项一致
    let got = SortedSetObject::diff(&real[0], &[&real[1], &real[2]]);
    let items = got.zrange_ref(0, -1, false);
    assert_eq!(items.len(), expected.len());
    for ((em, es), (gs, gm)) in items.iter().zip(&expected) {
      assert_eq!(em, gm);
      assert_eq!(es.to_bits(), gs.to_bits());
    }

    // 对拍 2：ZDIFFSTORE 返回计数 = 结果基数，内容一致
    let mut dest = SortedSetObject::new();
    assert_eq!(
      dest.zdiffstore(&[&real[0], &real[1], &real[2]])?,
      expected.len()
    );
    let stored = dest.zrange_ref(0, -1, false);
    assert_eq!(stored.len(), expected.len());
    for ((em, es), (gs, gm)) in stored.iter().zip(&expected) {
      assert_eq!(em, gm);
      assert_eq!(es.to_bits(), gs.to_bits());
    }

    // 对拍 3：单集合差集 = 存活成员克隆
    let mut expected_a: Vec<(f64, Vec<u8>)> = golden[0]
      .iter()
      .filter(|&(_, &(_, expired))| !expired)
      .map(|(m, &(s, _))| (s, m.clone()))
      .collect();
    expected_a.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
    let single = SortedSetObject::diff(&real[0], &[]);
    assert_eq!(single.zrange_ref(0, -1, false).len(), expected_a.len());
    assert_eq!(single.len_ref(), expected_a.len());

    // 对拍 4：自差恒空（first 同时出现在 others 中：A - A = ∅，A - A - B = ∅）
    assert_eq!(SortedSetObject::diff(&real[0], &[&real[0]]).len_ref(), 0);
    assert_eq!(
      SortedSetObject::diff(&real[0], &[&real[0], &real[1]]).len_ref(),
      0
    );
  }

  OK
}

/// 9. ZREM / ZPOPMIN / ZPOPMAX：已过期未清退成员视同不存在，绝不计数或弹出
#[test]
fn zrem_and_pop_do_not_count_expired_members() -> Void {
  // ZREM：过期成员就地物理清退但不计入删除数
  let mut z = SortedSetObject::new();
  z.zadd(1.0, b"gone", ZAddOpt::default())?;
  z.zadd(2.0, b"alive", ZAddOpt::default())?;
  expire_without_purge(&mut z, b"gone")?;
  assert_eq!(z.zrem(&[b"gone", b"missing"]), 0);
  assert_eq!(z.zscore(b"gone"), None);
  assert_eq!(z.zrem(&[b"alive"]), 1);
  assert_eq!(z.len(), 0);

  // ZPOPMAX：最高分成员已过期，弹出集合中绝不出现且不影响存活成员计数
  let mut z2 = SortedSetObject::new();
  for (i, m) in [b"lo".as_slice(), b"mid", b"hi", b"ghost"]
    .iter()
    .enumerate()
  {
    z2.zadd(i as f64, *m, ZAddOpt::default())?;
  }
  expire_without_purge(&mut z2, b"ghost")?;
  assert_eq!(
    z2.zpopmax(10),
    vec![
      (b"hi".to_vec(), 2.0),
      (b"mid".to_vec(), 1.0),
      (b"lo".to_vec(), 0.0)
    ]
  );
  assert!(z2.is_empty());

  // ZPOPMIN：过期成员不参与弹出
  let mut z3 = SortedSetObject::new();
  z3.zadd(5.0, b"x", ZAddOpt::default())?;
  z3.zadd(6.0, b"ghost", ZAddOpt::default())?;
  expire_without_purge(&mut z3, b"ghost")?;
  assert_eq!(z3.zpopmin(10), vec![(b"x".to_vec(), 5.0)]);
  assert!(z3.is_empty());

  OK
}

/// 10. RESP 层契约回归：ZADD INCR+XX 未命中返回 (0, 增量)，上层据 flag=0 写 nil
#[test]
fn zadd_incr_xx_miss_returns_zero_with_increment() -> Void {
  let mut z = SortedSetObject::new();
  z.zadd(10.0, b"m", ZAddOpt::default())?;

  // 命中：INCR+XX 累加并计 1
  assert_eq!(
    z.zadd(
      2.5,
      b"m",
      ZAddOpt {
        incr: true,
        xx: true,
        ..ZAddOpt::default()
      }
    ),
    Ok((1, 12.5))
  );
  // 未命中：第二字段原样返回增量（上层不消费，仅 flag=0 决定写 nil）
  assert_eq!(
    z.zadd(
      7.5,
      b"ghost",
      ZAddOpt {
        incr: true,
        xx: true,
        ..ZAddOpt::default()
      }
    ),
    Ok((0, 7.5))
  );
  assert_eq!(z.zscore(b"ghost"), None);

  // 紧凑编码路径契约一致
  let mut c = CompactZSet::new();
  assert_eq!(
    c.zadd(
      3.0,
      b"m",
      ZAddOpt {
        incr: true,
        xx: true,
        ..ZAddOpt::default()
      }
    ),
    Ok((0, 3.0))
  );
  assert_eq!(c.zscore(b"m"), None);

  OK
}
