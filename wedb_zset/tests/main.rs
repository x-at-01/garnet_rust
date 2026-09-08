use aok::{OK, Void};
use log::info;
use wedb_zset::{ExpireOpt, ExpireResult, SortedSetObject, ZAddOpt};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_basic_zadd_and_queries() -> Void {
  let mut zset = SortedSetObject::new();
  assert!(zset.is_empty());
  assert_eq!(zset.len(), 0);

  // ZADD
  let (added, score) = zset.zadd(10.0, b"member1", ZAddOpt::default())?;
  assert_eq!(added, 1);
  assert!((score - 10.0).abs() < 1e-6);

  let (added2, _) = zset.zadd(20.0, b"member2", ZAddOpt::default())?;
  assert_eq!(added2, 1);

  // 更新已存在元素 (无 ch 选项返回 0)
  let (updated, score) = zset.zadd(15.0, b"member1", ZAddOpt::default())?;
  assert_eq!(updated, 0);
  assert!((score - 15.0).abs() < 1e-6);

  // 带 ch 选项更新返回 1
  let (updated_ch, _) = zset.zadd(
    25.0,
    b"member1",
    ZAddOpt {
      ch: true,
      ..Default::default()
    },
  )?;
  assert_eq!(updated_ch, 1);

  // ZCARD
  assert_eq!(zset.len(), 2);

  // ZSCORE & ZMSCORE
  assert_eq!(zset.zscore(b"member1"), Some(25.0));
  assert_eq!(zset.zscore(b"member2"), Some(20.0));
  assert_eq!(zset.zscore(b"nonexistent"), None);

  let ms = zset.zmscore(&[b"member2", b"member1", b"x"]);
  assert_eq!(ms, vec![Some(20.0), Some(25.0), None]);

  // ZREM
  assert_eq!(zset.zrem(&[b"member1", b"none"]), 1);
  assert_eq!(zset.len(), 1);

  info!("test_basic_zadd_and_queries passed");
  OK
}

#[test]
fn test_rank_and_range() -> Void {
  let mut zset = SortedSetObject::new();
  // 插入元素：分数分别为 10, 20, 30, 40
  zset.zadd(10.0, b"a", ZAddOpt::default())?;
  zset.zadd(20.0, b"b", ZAddOpt::default())?;
  zset.zadd(30.0, b"c", ZAddOpt::default())?;
  zset.zadd(40.0, b"d", ZAddOpt::default())?;

  // ZRANK (0-based)
  assert_eq!(zset.zrank(b"a"), Some(0));
  assert_eq!(zset.zrank(b"d"), Some(3));
  assert_eq!(zset.zrank(b"x"), None);

  // ZREVRANK
  assert_eq!(zset.zrevrank(b"d"), Some(0));
  assert_eq!(zset.zrevrank(b"a"), Some(3));

  // ZRANGE 正向
  let range = zset.zrange(1, 2, false);
  assert_eq!(range, vec![(b"b".to_vec(), 20.0), (b"c".to_vec(), 30.0)]);

  // ZRANGE 反向
  let rev_range = zset.zrange(0, 1, true);
  assert_eq!(
    rev_range,
    vec![(b"d".to_vec(), 40.0), (b"c".to_vec(), 30.0)]
  );

  // ZRANGEBYSCORE
  let by_score = zset.zrangebyscore(
    wedb_zset::ScoreRange::new(15.0, true, 35.0, true),
    false,
    0,
    10,
  );
  assert_eq!(by_score, vec![(b"b".to_vec(), 20.0), (b"c".to_vec(), 30.0)]);

  // ZCOUNT
  assert_eq!(
    zset.zcount(wedb_zset::ScoreRange::new(10.0, true, 30.0, true)),
    3
  );
  assert_eq!(
    zset.zcount(wedb_zset::ScoreRange::new(10.0, false, 30.0, false)),
    1
  );

  info!("test_rank_and_range passed");
  OK
}

#[test]
fn test_incr_pop_and_rand() -> Void {
  let mut zset = SortedSetObject::new();
  zset.zadd(10.0, b"m1", ZAddOpt::default())?;

  // ZINCRBY
  let new_s = zset.zincrby(b"m1", 5.5)?;
  assert!((new_s - 15.5).abs() < 1e-6);
  let new_s2 = zset.zincrby(b"new_m", 3.0)?;
  assert!((new_s2 - 3.0).abs() < 1e-6);

  // ZPOPMIN
  let pop_min = zset.zpopmin(1);
  assert_eq!(pop_min, vec![(b"new_m".to_vec(), 3.0)]);
  assert_eq!(zset.len(), 1);

  // ZPOPMAX
  let pop_max = zset.zpopmax(1);
  assert_eq!(pop_max, vec![(b"m1".to_vec(), 15.5)]);
  assert!(zset.is_empty());

  info!("test_incr_pop_and_rand passed");
  OK
}

#[test]
fn test_geospatial() -> Void {
  let mut zset = SortedSetObject::new();
  // 北京坐标：纬度 39.9042, 经度 116.4074
  // 上海坐标：纬度 31.2304, 经度 121.4737
  assert!(zset.geoadd(39.9042, 116.4074, b"Beijing")?);
  assert!(zset.geoadd(31.2304, 121.4737, b"Shanghai")?);

  // GEOPOS
  let pos = zset.geopos(b"Beijing").unwrap();
  assert!((pos.0 - 39.9042).abs() < 0.01);
  assert!((pos.1 - 116.4074).abs() < 0.01);

  // GEODIST (北京到上海距离大约 1060 ~ 1080 公里)
  let dist = zset.geodist(b"Beijing", b"Shanghai").unwrap();
  assert!(dist > 1_000_000.0 && dist < 1_200_000.0);

  info!("test_geospatial passed");
  OK
}

#[test]
fn test_expiration_and_serialization() -> Void {
  let mut zset = SortedSetObject::new();
  zset.zadd(100.0, b"alpha", ZAddOpt::default())?;
  zset.zadd(200.0, b"beta", ZAddOpt::default())?;

  let future = coarsetime::Clock::now_since_epoch().as_millis() + 10_000;
  assert_eq!(
    zset.zexpire(b"beta", future, ExpireOpt::default()),
    ExpireResult::Ok
  );
  assert!(zset.zttl(b"beta") > 0);

  let mut buf = Vec::new();
  zset.serialize(&mut buf);

  let mut restored = SortedSetObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 2);
  assert_eq!(restored.zscore(b"alpha"), Some(100.0));
  assert_eq!(restored.zscore(b"beta"), Some(200.0));
  assert_eq!(restored.zttl(b"alpha"), -1);
  assert!(restored.zttl(b"beta") > 0);

  info!("test_expiration_and_serialization passed");
  OK
}

#[test]
fn test_glob_match_non_recursive_and_redos_prevention() -> Void {
  use wedb_zset::glob_match;

  // 1. 基础通配符与边界情况（目标为空仅空模式命中，对齐 C# GlobUtils L19/L158）
  assert!(!glob_match(b"*", b""));
  assert!(glob_match(b"*", b"hello"));
  assert!(glob_match(b"", b""));
  assert!(!glob_match(b"", b"a"));
  assert!(glob_match(b"h?llo", b"hello"));
  assert!(!glob_match(b"h?llo", b"hllo"));

  // 2. 字符集匹配 [a-z], [^0-9], [0-9]（'!' 为字面集合成员，仅 '^' 取反）
  assert!(glob_match(b"[a-z]ello", b"hello"));
  assert!(!glob_match(b"[0-9]ello", b"hello"));
  assert!(!glob_match(b"[!0-9]ello", b"hello"));
  assert!(glob_match(b"[!0-9]ello", b"!ello"));
  assert!(glob_match(b"[^0-9]ello", b"hello"));

  // 3. 复杂反向回溯模式（防 ReDoS 恶劣回溯与栈溢出测试）
  let pattern = b"*a*a*a*a*b";
  let target = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab";
  assert!(glob_match(pattern, target));

  let no_match_target = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac";
  assert!(!glob_match(pattern, no_match_target));

  // 4. 多重星号与尾部星号
  assert!(glob_match(b"***hello***", b"hello"));
  assert!(glob_match(b"foo*bar*baz", b"fooxxxbar123baz"));
  assert!(!glob_match(b"foo*bar*baz", b"fooxxxbar123bazz"));

  // 5. 方括号内反斜杠转义：下一字节按字面参与匹配
  assert!(glob_match(b"[\\]]", b"]"));
  assert!(!glob_match(b"[\\]]", b"\\"));
  assert!(!glob_match(b"[\\]]", b"a"));
  assert!(glob_match(b"a[\\-]c", b"a-c"));
  assert!(!glob_match(b"a[\\-]c", b"a\\c"));
  // 转义使 '^' 与 '-' 失去特殊含义
  assert!(glob_match(b"[\\^a]", b"^"));
  assert!(glob_match(b"[\\^a]", b"a"));
  assert!(!glob_match(b"[\\^a]", b"b"));
  // 范围与转义混排互不干扰
  assert!(glob_match(b"[a-c\\-]x", b"-x"));
  assert!(glob_match(b"[a-c\\-]x", b"bx"));

  // 6. 区间右端 ']' 仅作区间上界（C# L75-95），集合为 {']'..'a'}
  assert!(glob_match(b"[a-]", b"a"));
  assert!(glob_match(b"[a-]", b"]"));
  assert!(glob_match(b"[a-]", b"_"));
  assert!(!glob_match(b"[a-]", b"-"));
  assert!(!glob_match(b"[a-]", b"b"));
  // 区间后继字节并入集合：集合 {']'..'a'} ∪ {'z'} 消费 "[a-]z" 整段，无字面后缀
  assert!(glob_match(b"x[a-]z", b"xz"));
  assert!(!glob_match(b"x[a-]z", b"x^z"));
  assert!(!glob_match(b"x[a-]z", b"x-z"));
  // 反转类同理：'a-]' 消费为区间，']' 不终止集合
  assert!(glob_match(b"[^a-]", b"-"));
  assert!(!glob_match(b"[^a-]", b"_"));

  info!("test_glob_match_non_recursive_and_redos_prevention passed");
  OK
}

#[test]
fn test_zcount_and_extreme_boundary() -> Void {
  use wedb_zset::{ScoreRange, SortedSetObject};

  let mut zset = SortedSetObject::new();
  for i in 1..=100 {
    let member = format!("m{}", i).into_bytes();
    let _ = zset.zadd(i as f64, member, wedb_zset::ZAddOpt::default());
  }

  // 1. O(log N) count_by_score 跨度计算验证
  // 全包含
  assert_eq!(zset.zcount(ScoreRange::new(1.0, true, 100.0, true)), 100);
  // 开区间 (10.0, 20.0) -> 11..=19 共 9 个
  assert_eq!(zset.zcount(ScoreRange::new(10.0, false, 20.0, false)), 9);
  // 闭区间 [10.0, 20.0] -> 10..=20 共 11 个
  assert_eq!(zset.zcount(ScoreRange::new(10.0, true, 20.0, true)), 11);
  // 单点闭区间 [15.0, 15.0]
  assert_eq!(zset.zcount(ScoreRange::new(15.0, true, 15.0, true)), 1);
  // 单点开区间 (15.0, 15.0)
  assert_eq!(zset.zcount(ScoreRange::new(15.0, false, 15.0, false)), 0);
  // 无交集区间
  assert_eq!(zset.zcount(ScoreRange::new(200.0, true, 300.0, true)), 0);
  assert_eq!(zset.zcount(ScoreRange::new(50.0, true, 40.0, true)), 0);

  // 2. 极端边界
  // isize::MIN / isize::MAX 防护
  let range_min = zset.zrange(isize::MIN, -1, false);
  assert_eq!(range_min.len(), 100);

  let range_inverted = zset.zrange(isize::MAX, isize::MIN, false);
  assert!(range_inverted.is_empty());

  // 3. ZRANDMEMBER 零分配采样验证
  assert_eq!(zset.zrandmember(1, false).len(), 1);
  assert_eq!(zset.zrandmember(5, true).len(), 5);
  assert_eq!(zset.zrandmember(-10, false).len(), 10);
  assert!(!zset.zrandmember(isize::MIN, false).is_empty());

  info!("test_zcount_and_extreme_boundary passed");
  OK
}
