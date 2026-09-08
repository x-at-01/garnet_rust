//! 跳表 span 增量维护回归测试：随机插入流下 rank/len 全量对拍黄金模型（覆盖 insert 层高分支与 span 修正路径）
use std::cmp::Ordering;

use wedb_zset::{SkipList, SortedSetObject, ZAddOpt};

/// 构造带数字后缀的成员字节串，替代 format!
fn member_suffix(prefix: &str, n: usize) -> Vec<u8> {
  let mut s = String::from(prefix);
  let mut ibuf = itoa::Buffer::new();
  s.push_str(ibuf.format(n));
  s.into_bytes()
}

#[test]
fn stress_span_rank_consistency() {
  let mut rng = fastrand::Rng::new();
  let mut list = SkipList::new();
  let mut golden: Vec<(f64, Vec<u8>)> = Vec::new();

  for i in 0..2000usize {
    let score = (rng.usize(0..100)) as f64;
    let member = member_suffix("m", i);
    list.insert(score, member.clone());
    golden.push((score, member));
    golden.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap().then_with(|| a.1.cmp(&b.1)));

    // 每隔若干步全量校验 get_by_rank 与长度
    if i % 50 == 0 {
      assert_eq!(list.len(), golden.len(), "长度不一致 @ {i}");
      for (rank, (s, m)) in golden.iter().enumerate() {
        let got = list.get_by_rank(rank).map(|(gm, gs)| (gs.to_bits(), gm));
        assert_eq!(
          got,
          Some((s.to_bits(), m.as_slice())),
          "rank {rank} 不一致 @ step {i}"
        );
      }
    }
  }

  // get_rank 全量校验
  for (rank, (s, m)) in golden.iter().enumerate() {
    assert_eq!(list.get_rank(*s, m), Some(rank));
  }
}

#[test]
fn stress_zset_object_rank_path() {
  let mut zs = SortedSetObject::new();
  let mut rng = fastrand::Rng::new();
  for i in 0..500usize {
    let score = (rng.usize(0..20)) as f64;
    let member = member_suffix("k", i);
    let (added, _) = zs.zadd(score, member, ZAddOpt::default()).expect("zadd ok");
    assert_eq!(added, 1);
  }
  let mut prev: Option<(Vec<u8>, f64)> = None;
  let len = zs.len();
  for r in 0..len {
    let cur = zs.zrange_ref(r as isize, r as isize, false);
    assert_eq!(cur.len(), 1, "rank {r} 应恰有一个元素");
    let cur = cur.into_iter().next().unwrap();
    if let Some(p) = &prev {
      let ord = p.1.total_cmp(&cur.1).then_with(|| p.0.cmp(&cur.0));
      assert_eq!(ord, Ordering::Less, "排序破坏 @ rank {r}");
    }
    prev = Some(cur);
  }
}

#[test]
fn test_pop_first_and_pop_last_span_integrity() {
  let mut list = SkipList::new();
  for i in 0..100 {
    list.insert(i as f64, member_suffix("elem", i));
  }

  // 验证 pop_first 连续弹出并保持 span 与 rank 严格一致
  for i in 0..20 {
    let popped = list.pop_first().unwrap();
    assert_eq!(popped.1, i as f64);
    assert_eq!(popped.0, member_suffix("elem", i));
    assert_eq!(list.len(), 100 - 1 - i);
    if let Some((first_m, first_s)) = list.get_by_rank(0) {
      assert_eq!(first_s, (i + 1) as f64);
      assert_eq!(first_m, member_suffix("elem", i + 1).as_slice());
    }
  }

  // 验证 pop_last 零克隆连续弹出并保持 span 与 rank 严格一致
  for i in 0..20 {
    let expected_val = 99 - i;
    let popped = list.pop_last().unwrap();
    assert_eq!(popped.1, expected_val as f64);
    assert_eq!(popped.0, member_suffix("elem", expected_val));
    assert_eq!(list.len(), 80 - 1 - i);
    if let Some((last_m, last_s)) = list.get_by_rank(list.len() - 1) {
      assert_eq!(last_s, (expected_val - 1) as f64);
      assert_eq!(last_m, member_suffix("elem", expected_val - 1).as_slice());
    }
  }

  // 交替弹出直至清空
  while !list.is_empty() {
    if list.len().is_multiple_of(2) {
      list.pop_first();
    } else {
      list.pop_last();
    }
  }
  assert!(list.is_empty());
  assert_eq!(list.len(), 0);
  assert_eq!(list.pop_first(), None);
  assert_eq!(list.pop_last(), None);
}

#[test]
fn test_ttl_cleanup_and_reset_to_none() {
  let mut zs = SortedSetObject::new();
  zs.zadd(10.0, b"k1", ZAddOpt::default()).unwrap();
  zs.zadd(20.0, b"k2", ZAddOpt::default()).unwrap();

  let future = coarsetime::Clock::now_since_epoch().as_millis() + 100_000;
  zs.zexpire(b"k1", future, wedb_zset::ExpireOpt::default());
  assert!(zs.zttl(b"k1") > 0);

  // persist 移除仅有的过期成员后，内部结构彻底回收重置为 None
  assert!(zs.zpersist(b"k1"));
  assert_eq!(zs.zttl(b"k1"), -1);
  assert_eq!(zs.len_ref(), 2);
}
