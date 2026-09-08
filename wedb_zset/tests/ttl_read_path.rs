//! 只读 (_ref) 路径在成员已过期但尚未物理清退时的语义对拍：
//! 排名/计数/区间必须以存活成员为序
use core::{hint::spin_loop, time::Duration};
use std::thread::sleep;

use aok::{OK, Void};
use coarsetime::Clock;
use log::info;
use wedb_zset::{ExpireOpt, ExpireResult, LexBound, ScoreRange, SortedSetObject, ZAddOpt};

/// 将成员置为「已过期但未清退」：设置近期 TTL 后忙等时钟越过期限
fn expire_without_purge(z: &mut SortedSetObject, member: &[u8]) -> Void {
  let deadline = Clock::now_since_epoch().as_millis() + 15;
  assert_eq!(
    z.zexpire(member, deadline, ExpireOpt::default()),
    ExpireResult::Ok
  );
  let mut guard = 0;
  while Clock::now_since_epoch().as_millis() <= deadline {
    spin_loop();
    sleep(Duration::from_millis(2));
    guard += 1;
    assert!(guard < 10_000, "时钟未推进，测试环境异常");
  }
  OK
}

#[test]
fn readonly_paths_skip_expired_not_yet_purged() -> Void {
  let mut z = SortedSetObject::new();
  // a(0) b(1) c(2) d(3) e(4)，其中 b 与 d 过期未清退
  for (i, m) in [b"a", b"b", b"c", b"d", b"e"].iter().enumerate() {
    z.zadd(i as f64, *m, ZAddOpt::default())?;
  }
  expire_without_purge(&mut z, b"b")?;
  expire_without_purge(&mut z, b"d")?;

  // 只读视图：长度、计数均以存活成员为基准
  assert_eq!(z.len_ref(), 3);
  assert_eq!(
    z.zcount_ref(ScoreRange::new(
      f64::NEG_INFINITY,
      true,
      f64::INFINITY,
      true
    )),
    3
  );
  assert_eq!(z.zcount_ref(ScoreRange::new(1.5, true, 10.0, true)), 2);
  assert_eq!(z.zcount_ref(ScoreRange::new(0.0, true, 1.0, true)), 1);

  // ZRANGE 排名窗口以存活成员为序 (C#: Where(!IsExpired) 在 Skip/Take 之前)
  assert_eq!(
    z.zrange_ref(0, -1, false),
    vec![
      (b"a".to_vec(), 0.0),
      (b"c".to_vec(), 2.0),
      (b"e".to_vec(), 4.0)
    ]
  );
  assert_eq!(
    z.zrange_ref(0, -1, true),
    vec![
      (b"e".to_vec(), 4.0),
      (b"c".to_vec(), 2.0),
      (b"a".to_vec(), 0.0)
    ]
  );
  assert_eq!(z.zrange_ref(1, 1, false), vec![(b"c".to_vec(), 2.0)]);
  assert!(z.zrange_ref(3, 10, false).is_empty());

  // ZRANGEBYSCORE 过滤先于 LIMIT
  assert_eq!(
    z.zrangebyscore_ref(ScoreRange::new(0.0, true, 10.0, true), false, 0, 10),
    vec![
      (b"a".to_vec(), 0.0),
      (b"c".to_vec(), 2.0),
      (b"e".to_vec(), 4.0)
    ]
  );
  assert_eq!(
    z.zrangebyscore_ref(ScoreRange::new(0.0, true, 10.0, true), false, 1, 1),
    vec![(b"c".to_vec(), 2.0)]
  );
  assert_eq!(
    z.zrangebyscore_ref(ScoreRange::new(0.0, true, 10.0, true), true, 0, 2),
    vec![(b"e".to_vec(), 4.0), (b"c".to_vec(), 2.0)]
  );

  // ZRANK / ZREVRANK 只在存活成员中累计
  assert_eq!(z.zrank_ref(b"a"), Some(0));
  assert_eq!(z.zrank_ref(b"c"), Some(1));
  assert_eq!(z.zrank_ref(b"e"), Some(2));
  assert_eq!(z.zrank_ref(b"b"), None);
  assert_eq!(z.zrevrank_ref(b"a"), Some(2));
  assert_eq!(z.zrevrank_ref(b"e"), Some(0));
  assert_eq!(z.zrevrank_ref(b"d"), None);

  // ZMSCORE 对过期成员返回 nil
  assert_eq!(
    z.zmscore_ref(&[b"a", b"b", b"c"]),
    vec![Some(0.0), None, Some(2.0)]
  );

  // 清退后 mut 路径与只读结果完全一致
  z.delete_expired();
  assert_eq!(z.len(), 3);
  assert_eq!(z.zcount(ScoreRange::new(0.0, true, 10.0, true)), 3);
  assert_eq!(
    z.zrange(0, -1, false),
    vec![
      (b"a".to_vec(), 0.0),
      (b"c".to_vec(), 2.0),
      (b"e".to_vec(), 4.0)
    ]
  );
  assert_eq!(z.zrank(b"c"), Some(1));
  assert_eq!(z.zrevrank(b"a"), Some(2));

  info!("readonly_paths_skip_expired_not_yet_purged passed");
  OK
}

#[test]
fn readonly_lex_paths_skip_expired_not_yet_purged() -> Void {
  let mut z = SortedSetObject::new();
  // 全零分：纯字典序 a b c d e，其中 b 与 d 过期未清退
  for m in [b"a", b"b", b"c", b"d", b"e"] {
    z.zadd(0.0, m, ZAddOpt::default())?;
  }
  expire_without_purge(&mut z, b"b")?;
  expire_without_purge(&mut z, b"d")?;

  let min = LexBound::parse(b"-")?;
  let max = LexBound::parse(b"+")?;

  // ZLEXCOUNT 扣除已过期成员
  assert_eq!(z.zlexcount_ref(&min, &max), 3);
  assert_eq!(
    z.zlexcount_ref(&LexBound::parse(b"[a")?, &LexBound::parse(b"[c")?),
    2
  );
  assert_eq!(
    z.zlexcount_ref(&LexBound::parse(b"(b")?, &LexBound::parse(b"(d")?),
    1
  );

  // ZRANGEBYLEX 过滤先于 LIMIT
  let members: Vec<Vec<u8>> = z
    .zrangebylex_ref(&min, &max, false, 0, 10)
    .into_iter()
    .map(|(m, _)| m)
    .collect();
  assert_eq!(members, vec![b"a".to_vec(), b"c".to_vec(), b"e".to_vec()]);

  let members: Vec<Vec<u8>> = z
    .zrangebylex_ref(&min, &max, false, 1, 1)
    .into_iter()
    .map(|(m, _)| m)
    .collect();
  assert_eq!(members, vec![b"c".to_vec()]);

  let members: Vec<Vec<u8>> = z
    .zrangebylex_ref(&min, &max, true, 0, 2)
    .into_iter()
    .map(|(m, _)| m)
    .collect();
  assert_eq!(members, vec![b"e".to_vec(), b"c".to_vec()]);

  // 清退后 mut 路径一致
  z.delete_expired();
  assert_eq!(z.zlexcount(&min, &max), 3);
  let members: Vec<Vec<u8>> = z
    .zrangebylex(&min, &max, true, 0, 10)
    .into_iter()
    .map(|(m, _)| m)
    .collect();
  assert_eq!(members, vec![b"e".to_vec(), b"c".to_vec(), b"a".to_vec()]);

  info!("readonly_lex_paths_skip_expired_not_yet_purged passed");
  OK
}

#[test]
fn geo_read_paths_skip_expired_members() -> Void {
  let mut z = SortedSetObject::new();
  z.geoadd(39.9042, 116.4074, b"beijing")?;
  z.geoadd(31.2304, 121.4737, b"shanghai")?;
  expire_without_purge(&mut z, b"shanghai")?;

  // GEOHASH / GEOPOS 批量对过期成员返回 nil (与 ZSCORE 只读语义一致)
  assert!(z.geohash_ref(&[b"beijing", b"shanghai"])[0].is_some());
  assert_eq!(z.geohash_ref(&[b"beijing", b"shanghai"])[1], None);
  let pos = z.geopos_many(&[b"beijing", b"shanghai", b"none"]);
  assert!(pos[0].is_some());
  assert_eq!(pos[1], None);
  assert_eq!(pos[2], None);
  // 过期成员 GEODIST 视同不存在
  assert_eq!(z.geodist_ref(b"beijing", b"shanghai"), None);

  info!("geo_read_paths_skip_expired_members passed");
  OK
}

#[test]
fn zrandmember_read_path_skips_expired_members() -> Void {
  let mut z = SortedSetObject::new();
  z.zadd(10.0, b"alive1", ZAddOpt::default())?;
  z.zadd(20.0, b"alive2", ZAddOpt::default())?;
  z.zadd(30.0, b"expired1", ZAddOpt::default())?;
  expire_without_purge(&mut z, b"expired1")?;

  // zrandmember_ref 单项抽样绝不返回过期条目
  for _ in 0..50 {
    let sampled = z.zrandmember_ref(1, true);
    assert_eq!(sampled.len(), 1);
    assert_ne!(sampled[0].0, b"expired1".to_vec());
  }

  // zrandmember_ref 多项唯一抽样 (count > 0)
  let sampled_all = z.zrandmember_ref(5, true);
  assert_eq!(sampled_all.len(), 2);
  for (m, _) in &sampled_all {
    assert_ne!(m, &b"expired1".to_vec());
  }

  // zrandmember_ref 允许重复抽样 (count < 0)
  let sampled_rep = z.zrandmember_ref(-5, false);
  assert_eq!(sampled_rep.len(), 5);
  for (m, _) in &sampled_rep {
    assert_ne!(m, &b"expired1".to_vec());
  }

  info!("zrandmember_read_path_skips_expired_members passed");
  OK
}
