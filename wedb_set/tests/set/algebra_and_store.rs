//! 集合代数与落盘操作测试（对标 Garnet RespSetTest.cs）

use aok::{OK, Void};
use log::info;
use wedb_set::SetObject;

use super::support::make_set;

/// SUNION 集合并集
/// 对标 C# RespSetTest.cs: CanDoSetUnion, CanDoSetUnionLC
#[test]
fn test_sunion_basic() -> Void {
  let key1 = make_set(&[b"a", b"b", b"c", b"d"]);
  let key2 = make_set(&[b"c"]);
  let key3 = make_set(&[b"a", b"c", b"e"]);

  let union = key1.union(&[&key2, &key3]);
  assert_eq!(union.len(), 5);
  for item in [b"a", b"b", b"c", b"d", b"e"] {
    assert!(union.sismember(item));
  }

  info!("test_sunion_basic 通过");
  OK
}

/// SUNION 单集合或带空集合
/// 对标 C# RespSetTest.cs: SUnionWithFirstKeyNotExisting
#[test]
fn test_sunion_single_and_empty() -> Void {
  let set = make_set(&[b"x", b"y", b"z"]);
  let empty = SetObject::new();

  // 单集合自身并集
  assert_eq!(set.union(&[]), set);

  // 与空集合并集
  assert_eq!(set.union(&[&empty]), set);
  assert_eq!(empty.union(&[&set]), set);

  info!("test_sunion_single_and_empty 通过");
  OK
}

/// SUNION 悬殊规模集合自适应并集优化
/// 对标 C# RespSetTest.cs: CanDoSetUnion (悬殊规模集合自适应并集优化)
#[test]
fn test_sunion_asymmetric_scale() -> Void {
  let small = make_set(&[b"only_in_small"]);

  let mut large = SetObject::with_capacity(1000);
  large.sadd((0..1000u32).map(|i| i.to_le_bytes()));

  let union1 = small.union(&[&large]);
  assert_eq!(union1.len(), 1001);
  assert!(union1.sismember(b"only_in_small"));
  assert!(union1.sismember(&500u32.to_le_bytes()));

  let union2 = large.union(&[&small]);
  assert_eq!(union2.len(), 1001);

  info!("test_sunion_asymmetric_scale 通过");
  OK
}

/// SINTER 集合交集
/// 对标 C# RespSetTest.cs: CanDoSetInter, CanDoSinterLC
#[test]
fn test_sinter_basic() -> Void {
  let key1 = make_set(&[b"a", b"b", b"c", b"d"]);
  let key2 = make_set(&[b"c"]);
  let key3 = make_set(&[b"a", b"c", b"e"]);

  let inter = key1.inter(&[&key2, &key3]);
  assert_eq!(inter.len(), 1);
  assert!(inter.sismember(b"c"));

  info!("test_sinter_basic 通过");
  OK
}

/// SINTER 与空集合或不相交集合
/// 对标 C# RespSetTest.cs: IntersectWithEmptySetReturnEmptySet, SInterWithFirstKeyNotExisting
#[test]
fn test_sinter_empty_and_disjoint() -> Void {
  let set = make_set(&[b"1", b"2", b"3"]);
  let empty = SetObject::new();
  let disjoint = make_set(&[b"4", b"5", b"6"]);

  // 遇到空集短路
  assert!(set.inter(&[&empty]).is_empty());
  assert!(empty.inter(&[&set]).is_empty());

  // 不相交集合交集为空
  assert!(set.inter(&[&disjoint]).is_empty());

  // 单集合自身交集
  assert_eq!(set.inter(&[]), set);

  info!("test_sinter_empty_and_disjoint 通过");
  OK
}

/// SDIFF 集合差集
/// 对标 C# RespSetTest.cs: CanDoSdiff, CanDoSdiffLC
#[test]
fn test_sdiff_basic() -> Void {
  let key1 = make_set(&[b"a", b"b", b"c", b"d"]);
  let key2 = make_set(&[b"c"]);
  let key3 = make_set(&[b"a", b"c", b"e"]);

  let diff = key1.diff(&[&key2, &key3]);
  assert_eq!(diff.len(), 2);
  assert!(diff.sismember(b"b"));
  assert!(diff.sismember(b"d"));
  assert!(!diff.sismember(b"a"));
  assert!(!diff.sismember(b"c"));

  info!("test_sdiff_basic 通过");
  OK
}

/// SDIFF 与空集合或自身
/// 对标 C# RespSetTest.cs: SDiffWithFirstKeyNotExisting
#[test]
fn test_sdiff_empty_and_self() -> Void {
  let set = make_set(&[b"1", b"2", b"3"]);
  let empty = SetObject::new();

  // 排除空集等于自身
  assert_eq!(set.diff(&[&empty, &empty]), set);

  // 单集合无额外参数
  assert_eq!(set.diff(&[]), set);

  // 排除自身为空
  assert!(set.diff(&[&set]).is_empty());

  info!("test_sdiff_empty_and_self 通过");
  OK
}

/// SINTERCARD 集合交集基数与 LIMIT 短路
/// 对标 C# RespSetTest.cs: CanDoSinterCard, CanDoSinterCardLC
#[test]
fn test_sintercard_semantics() -> Void {
  let set1 = make_set(&[b"1", b"2", b"3", b"4"]);
  let set2 = make_set(&[b"2", b"3", b"5"]);
  let set3 = make_set(&[b"2", b"3", b"6"]);

  // 全量交集 {2, 3}，基数 2
  assert_eq!(set1.intercard(&[&set2, &set3], 0), 2);
  // limit = 1 短路退出
  assert_eq!(set1.intercard(&[&set2, &set3], 1), 1);
  // limit 大于实际交集数
  assert_eq!(set1.intercard(&[&set2, &set3], 10), 2);

  // 空集合短路
  let empty = SetObject::new();
  assert_eq!(set1.intercard(&[&empty], 0), 0);
  assert_eq!(empty.intercard(&[&set1], 0), 0);

  // 不相交集合
  let disjoint = make_set(&[b"99", b"100"]);
  assert_eq!(set1.intercard(&[&disjoint], 0), 0);

  info!("test_sintercard_semantics 通过");
  OK
}

/// SUNIONSTORE 并集落盘存储
/// 对标 C# RespSetTest.cs: CanDoSetUnionStore, CanDoSunionStoreLC, SUnionStoreWithFirstKeyNotExisting
#[test]
fn test_sunion_store_semantics() -> Void {
  let key1 = make_set(&[b"a", b"b", b"c"]);
  let key2 = make_set(&[b"c", b"d"]);

  let mut dst = SetObject::new();
  assert_eq!(dst.union_store(&key1, &[&key2]), 4);
  assert_eq!(dst.len(), 4);
  for item in [b"a", b"b", b"c", b"d"] {
    assert!(dst.sismember(item));
  }

  // union_store_in_place 就地覆写
  let mut s2 = key2.clone();
  let key3 = make_set(&[b"d", b"e"]);
  assert_eq!(s2.union_store_in_place(&[&key3]), 3);
  assert_eq!(s2.len(), 3);
  assert!(s2.sismember(b"c"));
  assert!(s2.sismember(b"d"));
  assert!(s2.sismember(b"e"));

  // 源为空时覆写目标集合为空
  let empty = SetObject::new();
  let mut overwrite_dst = make_set(&[b"x", b"y"]);
  assert_eq!(overwrite_dst.union_store(&empty, &[]), 0);
  assert!(overwrite_dst.is_empty());

  info!("test_sunion_store_semantics 通过");
  OK
}

/// SINTERSTORE 交集落盘存储
/// 对标 C# RespSetTest.cs: CanDoSetInterStore, CanDoSinterStoreLC, IntersectAndStoreWithNotExisingSetsOverwitesDestinationSet, SInterStoreWithFirstKeyNotExisting
#[test]
fn test_sinter_store_semantics() -> Void {
  // 覆盖目标集合为空
  let mut key = make_set(&[b"a"]);
  let key1 = SetObject::new();
  let key2 = SetObject::new();
  assert_eq!(key.inter_store(&key1, &[&key2]), 0);
  assert!(key.is_empty());

  // 正常交集落盘
  let key1 = make_set(&[b"a", b"b", b"c", b"d"]);
  let key2 = make_set(&[b"c"]);
  let key3 = make_set(&[b"a", b"c", b"e"]);

  let mut dst = SetObject::new();
  assert_eq!(dst.inter_store(&key1, &[&key2, &key3]), 1);
  assert_eq!(dst.members_ref(), [&b"c"[..]]);

  // inter_store_in_place 就地交集覆写
  let mut s1 = key1.clone();
  assert_eq!(s1.inter_store_in_place(&[&key2]), 1);
  assert_eq!(s1.len(), 1);
  assert!(s1.sismember(b"c"));

  info!("test_sinter_store_semantics 通过");
  OK
}

/// SDIFFSTORE 差集落盘存储
/// 对标 C# RespSetTest.cs: CanDoSdiffStoreOverwrittenKey, CanDoSdiffStoreLC, SDiffStoreWithFirstKeyNotExisting
#[test]
fn test_sdiff_store_semantics() -> Void {
  let key1 = make_set(&[b"a", b"b", b"c", b"d"]);
  let key2 = make_set(&[b"c"]);
  let key3 = make_set(&[b"a", b"c", b"e"]);

  let mut dst = SetObject::new();
  assert_eq!(dst.diff_store(&key1, &[&key2, &key3]), 2);
  assert_eq!(dst.len(), 2);
  assert!(dst.sismember(b"b"));
  assert!(dst.sismember(b"d"));

  // diff_store_in_place 就地差集覆写
  let mut s1 = key1.clone();
  assert_eq!(s1.diff_store_in_place(&[&key2, &key3]), 2);
  assert_eq!(s1, dst);

  // 差集排除自身必为空
  assert_eq!(dst.diff_store_in_place(&[&s1]), 0);
  assert!(dst.is_empty());

  info!("test_sdiff_store_semantics 通过");
  OK
}

/// 聚合命令对称性与自引用最小集优化路径
/// 对标 C# RespSetTest.cs: CanDoSetInter, CanDoSdiff (聚合命令对称性与自引用最小集优化路径)
#[test]
fn test_aggregate_symmetry_edges() -> Void {
  let small = make_set(&[b"c"]);
  let big = make_set(&[b"a", b"b", b"c", b"d"]);

  // self 为最小集时走快捷路径
  let inter = small.inter(&[&big]);
  assert_eq!(inter.len(), 1);
  assert!(inter.sismember(b"c"));

  // intercard 同路径
  assert_eq!(small.intercard(&[&big], 0), 1);
  assert_eq!(big.intercard(&[&small], 1), 1);

  // 遇空集结果为空
  let empty = SetObject::new();
  assert!(big.inter(&[&empty]).is_empty());

  // diff 全为空集等于自身副本
  assert_eq!(big.diff(&[&empty, &empty]), big);

  // 聚合结果与成员顺序无关，且不修改源集合
  let inter2 = big.inter(&[&small]);
  assert_eq!(inter.len(), inter2.len());
  assert_eq!(big.len(), 4);
  assert_eq!(small.len(), 1);

  info!("test_aggregate_symmetry_edges 通过");
  OK
}

/// 原地操作在自别名、悬殊基数与重复指针下的极值边界
/// 对标 C# RespSetTest.cs: CanDoSetUnionStore, CanDoSetInterStore, CanDoSdiffStore (原地操作在自别名、悬殊基数与重复指针下的极值边界)
#[test]
fn test_in_place_extreme_edges() -> Void {
  // 1. 自别名差集就地覆盖
  let mut s1 = make_set(&[b"a", b"b", b"c"]);
  let s1_ref = unsafe { &*(&s1 as *const SetObject) };
  assert_eq!(s1.diff_store_in_place(&[s1_ref]), 0);
  assert!(s1.is_empty());

  // 2. 自别名交集就地覆盖
  let mut s2 = make_set(&[b"x", b"y"]);
  let s2_ref = unsafe { &*(&s2 as *const SetObject) };
  assert_eq!(s2.inter_store_in_place(&[s2_ref, s2_ref]), 2);
  assert_eq!(s2.len(), 2);
  assert!(s2.sismember(b"x"));
  assert!(s2.sismember(b"y"));

  // 3. 自别名并集就地覆盖
  assert_eq!(s2.union_store_in_place(&[s2_ref, s2_ref]), 2);
  assert_eq!(s2.len(), 2);

  // 4. 悬殊基数：self 极大，other 极小 (小集合驱动自适应切换)
  let mut huge = SetObject::with_capacity(5000);
  huge.sadd((0..5000u32).map(|i| i.to_le_bytes()));
  let mut tiny = SetObject::new();
  tiny.sadd([10u32.to_le_bytes(), 20u32.to_le_bytes()]);

  assert_eq!(huge.inter_store_in_place(&[&tiny]), 2);
  assert_eq!(huge.len(), 2);
  assert!(huge.sismember(&10u32.to_le_bytes()));
  assert!(huge.sismember(&20u32.to_le_bytes()));

  // 5. 悬殊基数：self 极小，other 极大 (原地 retain 零分配分支)
  let mut huge2 = SetObject::with_capacity(5000);
  huge2.sadd((0..5000u32).map(|i| i.to_le_bytes()));
  let mut tiny2 = SetObject::new();
  tiny2.sadd([10u32.to_le_bytes(), 9999u32.to_le_bytes()]);

  assert_eq!(tiny2.inter_store_in_place(&[&huge2]), 1);
  assert_eq!(tiny2.len(), 1);
  assert!(tiny2.sismember(&10u32.to_le_bytes()));

  // 6. diff_store_in_place 原地过滤
  let mut base = make_set(&[b"1", b"2", b"3", b"4", b"5"]);
  let remove_set = make_set(&[b"2", b"4"]);
  assert_eq!(base.diff_store_in_place(&[&remove_set]), 3);
  assert_eq!(base.len(), 3);
  assert!(base.sismember(b"1"));
  assert!(base.sismember(b"3"));
  assert!(base.sismember(b"5"));
  assert!(!base.sismember(b"2"));
  assert!(!base.sismember(b"4"));

  info!("test_in_place_extreme_edges 通过");
  OK
}

/// 栈优先容器跨越 16 探针阈值平滑退化为堆的正确性
/// 对标 C# RespSetTest.cs: CanDoSetUnion, CanDoSetInter, CanDoSdiff (栈优先容器跨越 16 探针阈值平滑退化为堆的正确性)
#[test]
fn test_probes_overflow_smooth_heap_expansion() -> Void {
  let base = make_set(&[b"common", b"base_only", b"survivor"]);

  // 构造 24 个集合，验证超出 16 探针限制后的平滑退化
  let other_sets: Vec<SetObject> = (0..24)
    .map(|i| {
      let mut s = SetObject::new();
      let mut b = itoa::Buffer::new();
      let num = b.format(i);
      let mut u = Vec::with_capacity(7 + num.len());
      u.extend_from_slice(b"unique_");
      u.extend_from_slice(num.as_bytes());
      s.sadd([b"common".as_slice(), &u]);
      s
    })
    .collect();

  let other_refs: Vec<&SetObject> = other_sets.iter().collect();

  // 1. SINTER: 24 个集合均包含 "common"，交集恰为 {"common"}
  let inter = base.inter(&other_refs);
  assert_eq!(inter.len(), 1);
  assert!(inter.sismember(b"common"));

  // 2. SINTERCARD: 24 个集合交集基数
  assert_eq!(base.intercard(&other_refs, 0), 1);
  assert_eq!(base.intercard(&other_refs, 5), 1);

  // 3. SDIFF: base 排除 24 个集合后保留 base_only 与 survivor
  let diff = base.diff(&other_refs);
  assert_eq!(diff.len(), 2);
  assert!(diff.sismember(b"base_only"));
  assert!(diff.sismember(b"survivor"));
  assert!(!diff.sismember(b"common"));

  // 4. SUNION: base + 24 个集合的并集 (common + base_only + survivor + 24 unique = 27)
  let union = base.union(&other_refs);
  assert_eq!(union.len(), 27);

  // 5. inter_store_in_place 跨越 16 个探针
  let mut in_place_inter = base.clone();
  assert_eq!(in_place_inter.inter_store_in_place(&other_refs), 1);
  assert_eq!(in_place_inter.members_ref(), [&b"common"[..]]);

  // 6. diff_store_in_place 跨越 16 个探针
  let mut in_place_diff = base.clone();
  assert_eq!(in_place_diff.diff_store_in_place(&other_refs), 2);
  assert!(in_place_diff.sismember(b"base_only"));
  assert!(in_place_diff.sismember(b"survivor"));

  info!("test_probes_overflow_smooth_heap_expansion 通过");
  OK
}
