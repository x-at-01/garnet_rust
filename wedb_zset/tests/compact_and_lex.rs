use std::cmp::Ordering;

use aok::{OK, Void};
use log::info;
use wedb_zset::{
  CompactZSet, CompactZSetExt, Error, GeoBoundingBox, LexBound, SortableFloat, SortedSetObject,
  ZAddOpt, geo_distance, get_distance_when_in_rectangle, is_point_within_radius,
};
use whasher::HashSet;

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 1. 测试 SortableFloat 的 IEEE 754 全序、-0.0/+0.0 规范化与 Hash/Ord 一致性
#[test]
fn test_sortable_float_wrapper() -> Void {
  let f_neg_zero = SortableFloat(-0.0);
  let f_pos_zero = SortableFloat(0.0);

  // -0.0 与 +0.0 规范化后严格相等
  assert_eq!(f_neg_zero, f_pos_zero);
  assert_eq!(f_neg_zero.cmp(&f_pos_zero), Ordering::Equal);

  let mut set = HashSet::default();
  set.insert(f_neg_zero);
  assert!(set.contains(&f_pos_zero));

  let f_neg_inf = SortableFloat(f64::NEG_INFINITY);
  let f_neg_10 = SortableFloat(-10.0);
  let f_neg_0_1 = SortableFloat(-0.1);
  let f_pos_0_1 = SortableFloat(0.1);
  let f_pos_10 = SortableFloat(10.0);
  let f_pos_inf = SortableFloat(f64::INFINITY);

  assert!(f_neg_inf < f_neg_10);
  assert!(f_neg_10 < f_neg_0_1);
  assert!(f_neg_0_1 < f_neg_zero);
  assert!(f_pos_zero < f_pos_0_1);
  assert!(f_pos_0_1 < f_pos_10);
  assert!(f_pos_10 < f_pos_inf);

  info!("test_sortable_float_wrapper passed");
  OK
}

/// 2. 测试 ZADD 参数选项组合 (XX, NX, GT, LT, CH, INCR) 与边界冲突
#[test]
fn test_zadd_options_semantics() -> Void {
  let mut zset = SortedSetObject::new();

  // 1. 互斥参数校验
  assert!(
    zset
      .zadd(
        1.0,
        b"a",
        ZAddOpt {
          nx: true,
          xx: true,
          ..Default::default()
        }
      )
      .is_err()
  );
  assert!(
    zset
      .zadd(
        1.0,
        b"a",
        ZAddOpt {
          gt: true,
          lt: true,
          ..Default::default()
        }
      )
      .is_err()
  );
  assert!(
    zset
      .zadd(
        1.0,
        b"a",
        ZAddOpt {
          nx: true,
          gt: true,
          ..Default::default()
        }
      )
      .is_err()
  );

  // 2. INCR 选项
  // 成员不存在时自增 -> 直接插入该分值
  let (added, score) = zset.zadd(
    10.0,
    b"user1",
    ZAddOpt {
      incr: true,
      ..Default::default()
    },
  )?;
  assert_eq!(added, 1);
  assert_eq!(score, 10.0);

  // 成员已存在时自增 -> 累加
  let (added, score) = zset.zadd(
    5.5,
    b"user1",
    ZAddOpt {
      incr: true,
      ..Default::default()
    },
  )?;
  assert_eq!(added, 1);
  assert_eq!(score, 15.5);

  // INCR + XX (成员存在时累加)
  let (added, score) = zset.zadd(
    -2.5,
    b"user1",
    ZAddOpt {
      incr: true,
      xx: true,
      ..Default::default()
    },
  )?;
  assert_eq!(added, 1);
  assert_eq!(score, 13.0);

  // INCR + XX (成员不存在时无操作)
  let (added, _) = zset.zadd(
    100.0,
    b"nonexistent",
    ZAddOpt {
      incr: true,
      xx: true,
      ..Default::default()
    },
  )?;
  assert_eq!(added, 0);
  assert_eq!(zset.zscore(b"nonexistent"), None);

  // INCR + GT (累加后如果未大于当前分数则不更新)
  let (added, score) = zset.zadd(
    -5.0,
    b"user1",
    ZAddOpt {
      incr: true,
      gt: true,
      ..Default::default()
    },
  )?;
  assert_eq!(added, 0);
  assert_eq!(score, 13.0); // 维持 13.0
  assert_eq!(zset.zscore(b"user1"), Some(13.0));

  info!("test_zadd_options_semantics passed");
  OK
}

/// 3. 测试 ZRANGEBYLEX, ZLEXCOUNT, ZREMRANGEBYLEX
#[test]
fn test_lexicographical_operations() -> Void {
  let mut zset = SortedSetObject::new();
  // 当所有元素分值相同时，严格按字典序排序
  let names = ["alpha", "beta", "charlie", "delta", "echo", "foxtrot"];
  for &name in &names {
    zset.zadd(0.0, name.as_bytes(), ZAddOpt::default())?;
  }

  // 1. 解析 LexBound
  let min_inc = LexBound::parse(b"[beta")?;
  let max_inc = LexBound::parse(b"[echo")?;
  let min_exc = LexBound::parse(b"(beta")?;
  let max_exc = LexBound::parse(b"(echo")?;
  let unbound = LexBound::parse(b"+")?;
  let neg_unbound = LexBound::parse(b"-")?;

  // 2. 闭区间 [beta, echo] -> beta, charlie, delta, echo (4个)
  let range1 = zset.zrangebylex(&min_inc, &max_inc, false, 0, 10);
  let res1: Vec<Vec<u8>> = range1.into_iter().map(|(m, _)| m).collect();
  assert_eq!(
    res1,
    vec![
      b"beta".to_vec(),
      b"charlie".to_vec(),
      b"delta".to_vec(),
      b"echo".to_vec()
    ]
  );
  assert_eq!(zset.zlexcount(&min_inc, &max_inc), 4);

  // 3. 开区间 (beta, echo) -> charlie, delta (2个)
  let range2 = zset.zrangebylex(&min_exc, &max_exc, false, 0, 10);
  let res2: Vec<Vec<u8>> = range2.into_iter().map(|(m, _)| m).collect();
  assert_eq!(res2, vec![b"charlie".to_vec(), b"delta".to_vec()]);
  assert_eq!(zset.zlexcount(&min_exc, &max_exc), 2);

  // 4. 反向字典序
  let rev_range = zset.zrangebylex(&neg_unbound, &unbound, true, 0, 3);
  let rev_res: Vec<Vec<u8>> = rev_range.into_iter().map(|(m, _)| m).collect();
  assert_eq!(
    rev_res,
    vec![b"foxtrot".to_vec(), b"echo".to_vec(), b"delta".to_vec()]
  );

  // 5. ZREMRANGEBYLEX
  let removed = zset.zremrangebylex(&min_inc, &max_inc);
  assert_eq!(removed, 4);
  assert_eq!(zset.len(), 2);
  assert_eq!(zset.zscore(b"alpha"), Some(0.0));
  assert_eq!(zset.zscore(b"foxtrot"), Some(0.0));

  info!("test_lexicographical_operations passed");
  OK
}

/// 4. 测试 CompactZSet 原位操作与二分检索
#[test]
fn test_compact_zset_operations() -> Void {
  let mut compact = CompactZSet::new();
  assert!(compact.is_empty());
  assert_eq!(compact.len(), 0);

  // 插入元素
  assert_eq!(
    compact.zadd(50.0, b"member50", ZAddOpt::default())?,
    (1, 50.0)
  );
  assert_eq!(
    compact.zadd(10.0, b"member10", ZAddOpt::default())?,
    (1, 10.0)
  );
  assert_eq!(
    compact.zadd(30.0, b"member30", ZAddOpt::default())?,
    (1, 30.0)
  );
  assert_eq!(
    compact.zadd(20.0, b"member20", ZAddOpt::default())?,
    (1, 20.0)
  );
  assert_eq!(compact.len(), 4);

  // 校验二分查找有序性
  assert_eq!(compact.zrank(b"member10"), Some(0));
  assert_eq!(compact.zrank(b"member20"), Some(1));
  assert_eq!(compact.zrank(b"member30"), Some(2));
  assert_eq!(compact.zrank(b"member50"), Some(3));
  assert_eq!(compact.zrank(b"nonexistent"), None);

  // 分数查询
  assert_eq!(compact.zscore(b"member20"), Some(20.0));

  // 更新已有元素分数 (移至新顺序)
  assert_eq!(
    compact.zadd(
      60.0,
      b"member20",
      ZAddOpt {
        ch: true,
        ..Default::default()
      }
    )?,
    (1, 60.0)
  );
  assert_eq!(compact.len(), 4);
  assert_eq!(compact.zrank(b"member20"), Some(3)); // 变成最大项

  // ZRANGE 切片
  let range = compact.zrange(0, 1, false);
  assert_eq!(
    range,
    vec![(b"member10".to_vec(), 10.0), (b"member30".to_vec(), 30.0)]
  );

  // ZRANGE 反向
  let rev_range = compact.zrange(0, 1, true);
  assert_eq!(
    rev_range,
    vec![(b"member20".to_vec(), 60.0), (b"member50".to_vec(), 50.0)]
  );

  // 序列化与从切片解析
  let raw = compact.as_bytes();
  let restored = CompactZSet::from_bytes(raw)?;
  assert_eq!(restored.len(), 4);
  assert_eq!(restored.zscore(b"member50"), Some(50.0));

  // 与 SortedSetObject 双向无缝转换
  let sso = compact.to_sorted_set()?;
  assert_eq!(sso.len_ref(), 4);
  let back_compact = CompactZSet::from_sorted_set(&sso)?;
  assert_eq!(back_compact.len(), 4);

  // 删除元素
  assert!(compact.remove(b"member30")?);
  assert_eq!(compact.len(), 3);
  assert_eq!(compact.zscore(b"member30"), None);

  info!("test_compact_zset_operations passed");
  OK
}

/// 5. 测试 GeoBoundingBox 外接矩形过滤与优化验证
#[test]
fn test_geo_bounding_box_and_optimization() -> Void {
  let center_lat = 39.9042;
  let center_lon = 116.4074;
  let radius = 50_000.0; // 50 km

  let bbox = GeoBoundingBox::from_radius(center_lat, center_lon, radius);
  assert!(bbox.contains(center_lat, center_lon));

  // 北京中心偏移 0.1 度 (约 11 km) 应当在包围盒与圆形内
  let p_near = (center_lat + 0.1, center_lon + 0.1);
  assert!(bbox.contains(p_near.0, p_near.1));
  let dist = geo_distance(center_lat, center_lon, p_near.0, p_near.1);
  assert!(dist < radius);
  assert_eq!(
    is_point_within_radius(radius, center_lat, center_lon, p_near.0, p_near.1),
    Some(dist)
  );

  // 纬度超出半径的点 (偏移 1 度约 111 km > 50 km) 快速初筛过滤
  let p_far_lat = (center_lat + 1.0, center_lon);
  assert!(!bbox.contains(p_far_lat.0, p_far_lat.1));
  assert_eq!(
    is_point_within_radius(radius, center_lat, center_lon, p_far_lat.0, p_far_lat.1),
    None
  );

  // 相同点零距离快速路径
  assert_eq!(
    geo_distance(center_lat, center_lon, center_lat, center_lon),
    0.0
  );

  // 矩形检测与中心点经线距离
  let rect_dist = get_distance_when_in_rectangle(
    100_000.0, 100_000.0, center_lat, center_lon, p_near.0, p_near.1,
  );
  assert!(rect_dist.is_some());

  info!("test_geo_bounding_box_and_optimization passed");
  OK
}

/// 6. CompactZSet 与 SortedSetObject 的 ZADD 条件判定次序一致性
///
/// INCR 合成 NaN 必须先于 NX 短路报错
#[test]
fn test_compact_zadd_nx_incr_nan_precedence() -> Void {
  let mut compact = CompactZSet::new();
  // 写入 +inf 分值成员
  assert_eq!(
    compact.zadd(f64::INFINITY, b"inf_member", ZAddOpt::default())?,
    (1, f64::INFINITY)
  );

  // NX + INCR：inf + (-inf) 合成 NaN，NaN 检查先于 NX 短路 → 报 InvalidScore
  assert_eq!(
    compact.zadd(
      f64::NEG_INFINITY,
      b"inf_member",
      ZAddOpt {
        nx: true,
        incr: true,
        ..ZAddOpt::default()
      }
    ),
    Err(Error::InvalidScore)
  );
  // 成员分值保持不变
  assert_eq!(compact.zscore(b"inf_member"), Some(f64::INFINITY));

  // NX + INCR 且结果合法：仍然 NX 短路无操作
  assert_eq!(
    compact.zadd(
      5.0,
      b"inf_member",
      ZAddOpt {
        nx: true,
        incr: true,
        ..ZAddOpt::default()
      }
    )?,
    (0, f64::INFINITY)
  );

  // 普通 INCR 合成 NaN 同样报错
  assert_eq!(
    compact.zadd(
      f64::NEG_INFINITY,
      b"inf_member",
      ZAddOpt {
        incr: true,
        ..ZAddOpt::default()
      }
    ),
    Err(Error::InvalidScore)
  );

  info!("test_compact_zadd_nx_incr_nan_precedence passed");
  OK
}
