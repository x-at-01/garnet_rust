//! Redis ZSet 命令语义边界回归 (RespSortedSetTests 与 SortedSetComparer)
use aok::{OK, Void};
use coarsetime::Clock;
use wedb_zset::{
  CompactZSet, CompactZSetExt, Error, ExpireOpt, LexBound, ScoreRange, SortedSetObject, ZAddOpt,
};
use whasher::{HashSet, HashSetExt};

/// ZRANGE / ZREMRANGEBYRANK 负索引越界语义：
/// 负索引换算后仍为负 (如 0 -5 于 3 元素集合) 时区间为空，而非钳位到 0
#[test]
fn zrange_negative_out_of_range_stop_is_empty() -> Void {
  let mut z = SortedSetObject::new();
  for (i, m) in [b"a", b"b", b"c"].iter().enumerate() {
    z.zadd(i as f64, *m, ZAddOpt::default())?;
  }

  // ZRANGE key 0 -5 -> 空 (Redis: start=0 > end=-2)
  assert!(z.zrange(0, -5, false).is_empty());
  assert!(z.zrange_ref(0, -5, false).is_empty());
  assert!(z.zrange(0, -5, true).is_empty());
  assert!(z.zrange(2, -4, false).is_empty());
  assert!(z.zrange(-5, -4, false).is_empty());

  // 边界内负索引不受影响: -5 -> 0, -3 -> 0
  assert_eq!(z.zrange(-5, -3, false), vec![(b"a".to_vec(), 0.0)]);
  assert_eq!(z.zrange(0, -1, false).len(), 3);
  assert_eq!(z.zrange(-1, -1, true), vec![(b"a".to_vec(), 0.0)]);

  // ZREMRANGEBYRANK key 0 -5 -> 删除 0 个
  assert_eq!(z.zremrangebyrank(0, -5), 0);
  assert_eq!(z.len(), 3);
  // 正常负索引删除不受影响
  assert_eq!(z.zremrangebyrank(-1, -1), 1);
  assert_eq!(z.len(), 2);
  assert_eq!(z.zscore(b"c"), None);

  info_log("zrange_negative_out_of_range_stop_is_empty");
  OK
}

/// CompactZSet 与跳表路径保持一致的负索引语义，及编码校验边界
#[test]
fn compact_negative_range_and_codec_guards() -> Void {
  let mut c = CompactZSet::new();
  for i in 0..3u16 {
    c.zadd(
      f64::from(i),
      format!("m{}", i).as_bytes(),
      ZAddOpt::default(),
    )?;
  }

  assert!(c.zrange(0, -5, false).is_empty());
  assert!(c.zrange(0, -5, true).is_empty());
  assert_eq!(c.zrange(-5, -3, false).len(), 1);
  assert_eq!(c.zrange(0, -1, false).len(), 3);
  assert_eq!(c.zrange(-1, -1, true).len(), 1);

  // from_bytes 拒绝尾部多余字节
  let mut raw = c.as_bytes().to_vec();
  raw.push(0xFF);
  assert!(CompactZSet::from_bytes(&raw).is_err());
  // 精确字节可解析且无损
  let restored = CompactZSet::from_bytes(c.as_bytes())?;
  assert_eq!(restored.len(), 3);
  assert_eq!(restored.as_bytes(), c.as_bytes());

  // 成员长度超过 u16::MAX 报错 (紧凑编码长度字段上限)
  let long = vec![b'x'; u16::MAX as usize + 1];
  assert_eq!(
    c.zadd(9.0, &long, ZAddOpt::default()),
    Err(Error::InvalidOpt)
  );

  info_log("compact_negative_range_and_codec_guards");
  OK
}

/// ±inf 分数排序、ZCOUNT 无穷边界、ZINCRBY 无穷运算与 ±0.0 归一
#[test]
fn infinite_scores_and_zero_normalization() -> Void {
  let mut z = SortedSetObject::new();
  z.zadd(f64::INFINITY, b"c", ZAddOpt::default())?;
  z.zadd(f64::NEG_INFINITY, b"z", ZAddOpt::default())?;
  z.zadd(0.0, b"m", ZAddOpt::default())?;
  // 同分成员按字节字典序决序C# SortedSetComparer)

  z.zadd(0.0, b"a", ZAddOpt::default())?;

  let members: Vec<Vec<u8>> = z.zrange(0, -1, false).into_iter().map(|(m, _)| m).collect();
  assert_eq!(
    members,
    vec![b"z".to_vec(), b"a".to_vec(), b"m".to_vec(), b"c".to_vec()]
  );

  assert_eq!(
    z.zcount(ScoreRange::new(
      f64::NEG_INFINITY,
      true,
      f64::INFINITY,
      true
    )),
    4
  );
  // 开区间 (-inf, +inf) 排除恰好位于边界的 ±inf 分数
  assert_eq!(
    z.zcount(ScoreRange::new(
      f64::NEG_INFINITY,
      false,
      f64::INFINITY,
      false
    )),
    2
  );
  assert_eq!(
    z.zrangebyscore(
      ScoreRange::new(f64::NEG_INFINITY, true, f64::INFINITY, true),
      true,
      0,
      10
    )
    .len(),
    4
  );

  // ZINCRBY: 有限 + inf = inf；inf + (-inf) = NaN 报错
  assert_eq!(z.zincrby(b"m", f64::INFINITY)?, f64::INFINITY);
  assert_eq!(z.zincrby(b"m", f64::INFINITY)?, f64::INFINITY);
  assert_eq!(z.zincrby(b"z", f64::INFINITY), Err(Error::InvalidScore));
  assert_eq!(z.zincrby(b"m", f64::NEG_INFINITY), Err(Error::InvalidScore));

  // ±0.0 归一为同一分值：重复添加视为未变更
  let mut z2 = SortedSetObject::new();
  z2.zadd(-0.0, b"k", ZAddOpt::default())?;
  let (ret, score) = z2.zadd(0.0, b"k", ZAddOpt::default())?;
  assert_eq!(ret, 0);
  assert_eq!(score, 0.0);
  assert_eq!(z2.len(), 1);
  assert_eq!(z2.zscore(b"k"), Some(0.0));

  info_log("infinite_scores_and_zero_normalization");
  OK
}

/// ZADD GT/LT/NX/XX/CH 条件语义与同分清除成员 TTL
#[test]
fn zadd_gt_lt_ch_and_ttl_clearing() -> Void {
  let mut z = SortedSetObject::new();
  z.zadd(10.0, b"a", ZAddOpt::default())?;

  // GT: 新分更低不更新
  assert_eq!(
    z.zadd(
      5.0,
      b"a",
      ZAddOpt {
        gt: true,
        ch: true,
        ..ZAddOpt::default()
      }
    )?,
    (0, 10.0)
  );
  // GT: 新分更高则更新并计入 CH
  assert_eq!(
    z.zadd(
      20.0,
      b"a",
      ZAddOpt {
        gt: true,
        ch: true,
        ..ZAddOpt::default()
      }
    )?,
    (1, 20.0)
  );
  // LT: 新分更高不更新；新分更低则更新
  assert_eq!(
    z.zadd(
      25.0,
      b"a",
      ZAddOpt {
        lt: true,
        ch: true,
        ..ZAddOpt::default()
      }
    )?,
    (0, 20.0)
  );
  assert_eq!(
    z.zadd(
      15.0,
      b"a",
      ZAddOpt {
        lt: true,
        ch: true,
        ..ZAddOpt::default()
      }
    )?,
    (1, 15.0)
  );
  // NX: 已存在不更新
  assert_eq!(
    z.zadd(
      99.0,
      b"a",
      ZAddOpt {
        nx: true,
        ..ZAddOpt::default()
      }
    )?,
    (0, 15.0)
  );
  // XX: 不存在不新增
  assert_eq!(
    z.zadd(
      1.0,
      b"new",
      ZAddOpt {
        xx: true,
        ..ZAddOpt::default()
      }
    )?,
    (0, 1.0)
  );
  assert_eq!(z.zscore(b"new"), None);

  // 同分重复 ZADD 清除成员 TTL
  let future = Clock::now_since_epoch().as_millis() + 60_000;
  assert_eq!(
    z.zexpire(b"a", future, ExpireOpt::default()),
    wedb_zset::ExpireResult::Ok
  );
  assert!(z.zttl(b"a") > 0);
  let (ret, _) = z.zadd(15.0, b"a", ZAddOpt::default())?;
  assert_eq!(ret, 0);
  assert_eq!(z.zttl(b"a"), -1);

  // INCR 条件不满足: 分值保持不动
  let (ret, score) = z.zadd(
    -100.0,
    b"a",
    ZAddOpt {
      incr: true,
      gt: true,
      ..ZAddOpt::default()
    },
  )?;
  assert_eq!(ret, 0);
  assert_eq!(score, 15.0);
  assert_eq!(z.zscore(b"a"), Some(15.0));

  // INCR 实际变更同样清除 TTL (C#: INCR changed -> TryRemoveExpiration)
  assert_eq!(
    z.zexpire(b"a", future, ExpireOpt::default()),
    wedb_zset::ExpireResult::Ok
  );
  let (_, score) = z.zadd(
    1.0,
    b"a",
    ZAddOpt {
      incr: true,
      ..ZAddOpt::default()
    },
  )?;
  assert_eq!(score, 16.0);
  assert_eq!(z.zttl(b"a"), -1);

  // ZINCRBY 不触碰成员 TTLC# Zincrby 全程不调用 TryRemoveExpiration)
  assert_eq!(
    z.zexpire(b"a", future, ExpireOpt::default()),
    wedb_zset::ExpireResult::Ok
  );
  assert_eq!(z.zincrby(b"a", 1.0)?, 17.0);
  assert!(z.zttl(b"a") > 0);

  info_log("zadd_gt_lt_ch_and_ttl_clearing");
  OK
}

/// ZSCAN 游标翻页：无 pattern 全量覆盖、带 pattern 精确过滤且游标最终归零
#[test]
fn zscan_pagination_covers_all_members() -> Void {
  let mut z = SortedSetObject::new();
  for i in 0..25u32 {
    z.zadd(f64::from(i), format!("m{i:02}"), ZAddOpt::default())?;
  }

  let mut cursor = 0usize;
  let mut seen = HashSet::new();
  loop {
    let (next, items) = z.zscan(cursor, 6, None);
    for (m, _) in items {
      seen.insert(m);
    }
    if next == 0 {
      break;
    }
    cursor = next;
  }
  assert_eq!(seen.len(), 25);

  // 带 pattern: m1? 仅匹配 m10..m19 共 10 个
  let mut cursor = 0usize;
  let mut seen = HashSet::new();
  loop {
    let (next, items) = z.zscan(cursor, 4, Some(b"m1?"));
    for (m, _) in items {
      seen.insert(m);
    }
    if next == 0 {
      break;
    }
    cursor = next;
  }
  assert_eq!(seen.len(), 10);
  for i in 10..20u32 {
    assert!(seen.contains(&format!("m{}", i).into_bytes()));
  }

  info_log("zscan_pagination_covers_all_members");
  OK
}

/// ZRANGEBYLEX 反向 + LIMIT 组合与 ZLEXCOUNT (CanDoZRevRangeByLex)
#[test]
fn zrevrangebylex_with_limit() -> Void {
  let mut z = SortedSetObject::new();
  for m in [b"a", b"b", b"c", b"d", b"e", b"f", b"g"] {
    z.zadd(0.0, m, ZAddOpt::default())?;
  }

  let min = LexBound::parse(b"(a")?;
  let max = LexBound::parse(b"(a")?;
  // ZREVRANGEBYLEX key (a (a -> 空
  assert!(z.zrangebylex(&min, &max, true, 0, 10).is_empty());

  let all_min = LexBound::parse(b"-")?;
  let all_max = LexBound::parse(b"+")?;
  let rev: Vec<Vec<u8>> = z
    .zrangebylex(&all_min, &all_max, true, 0, 3)
    .into_iter()
    .map(|(m, _)| m)
    .collect();
  assert_eq!(rev, vec![b"g".to_vec(), b"f".to_vec(), b"e".to_vec()]);

  // LIMIT offset 作用于降序序列
  let rev_skip: Vec<Vec<u8>> = z
    .zrangebylex(&all_min, &all_max, true, 5, 10)
    .into_iter()
    .map(|(m, _)| m)
    .collect();
  assert_eq!(rev_skip, vec![b"b".to_vec(), b"a".to_vec()]);

  assert_eq!(z.zlexcount(&all_min, &all_max), 7);

  info_log("zrevrangebylex_with_limit");
  OK
}

fn info_log(name: &str) {
  log::info!("{name} passed");
}

/// ZRANGEBYLEX ±无穷边界语义 (CanDoZRangeByLex)：
/// min 为 "+" 或 max 为 "-" 时区间恒为空；"- +" 才是全区间；"[+"/"[-" 按负无穷处理
#[test]
fn zrangebylex_infinite_bounds_semantics() -> Void {
  let mut z = SortedSetObject::new();
  for m in [b"a", b"b", b"c", b"d", b"e", b"f", b"g"] {
    z.zadd(0.0, m, ZAddOpt::default())?;
  }

  let neg = LexBound::parse(b"-")?;
  let pos = LexBound::parse(b"+")?;
  let inc_c = LexBound::parse(b"[c")?;
  let inc_a = LexBound::parse(b"[a")?;

  // 正常区间: "- [c" -> a b c
  assert_eq!(z.zlexcount(&neg, &inc_c), 3);

  // min = "+" 恒空 (C#: min=InfiniteMax 短路)
  assert!(z.zrangebylex(&pos, &inc_c, false, 0, 10).is_empty());
  assert!(z.zrangebylex(&pos, &pos, false, 0, 10).is_empty());
  assert_eq!(z.zlexcount(&pos, &pos), 0);

  // max = "-" 恒空 (C#: max=InfiniteMin 短路)
  assert!(z.zrangebylex(&inc_a, &neg, false, 0, 10).is_empty());
  assert!(z.zrangebylex(&neg, &neg, false, 0, 10).is_empty());
  assert_eq!(z.zlexcount(&inc_a, &neg), 0);
  assert_eq!(z.zlexcount(&neg, &neg), 0);

  // "- +" 全区间 7 个；反向亦全量
  assert_eq!(z.zlexcount(&neg, &pos), 7);
  assert_eq!(z.zrangebylex(&neg, &pos, false, 0, 10).len(), 7);
  assert_eq!(z.zrangebylex(&neg, &pos, true, 0, 10).len(), 7);

  // C# 兼容怪癖："[+"/"[-" 作为 min 按负无穷处理 (ZRANGE board [+ + BYLEX -> 全量)
  let bracket_plus = LexBound::parse(b"[+")?;
  assert_eq!(z.zrangebylex(&bracket_plus, &pos, false, 0, 10).len(), 7);
  let bracket_minus = LexBound::parse(b"[-")?;
  assert_eq!(z.zrangebylex(&bracket_minus, &pos, false, 0, 10).len(), 7);

  // ZREMRANGEBYLEX：min = "+" 时无成员被删
  assert_eq!(z.zremrangebylex(&pos, &inc_c), 0);
  assert_eq!(z.zremrangebylex(&neg, &inc_c), 3);
  assert_eq!(z.len_ref(), 4);

  info_log("zrangebylex_infinite_bounds_semantics");
  OK
}
