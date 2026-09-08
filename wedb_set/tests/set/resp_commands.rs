//! 单集合 RESP 命令测试（对标 Garnet RespSetTest.cs）

use aok::{OK, Void};
use log::info;
use wedb_set::{CompactSet, MAX_RAND_SAMPLE_LIMIT, SetObject};

use super::support::{make_seq_set, sorted_members};

/// SADD 基础语义与去重
/// 对标 C# RespSetTest.cs: CandDoSaddBasic
#[test]
fn test_sadd_basic() -> Void {
  let mut set = SetObject::new();
  assert_eq!(set.len(), 0);
  assert!(set.is_empty());

  // 添加单个元素
  assert_eq!(set.sadd([b"Hello"]), 1);
  assert_eq!(set.len(), 1);

  // 添加第二元素
  assert_eq!(set.sadd([b"World"]), 1);
  assert_eq!(set.len(), 2);

  // 重复添加相同元素返回 0
  assert_eq!(set.sadd([b"World"]), 0);
  assert_eq!(set.len(), 2);

  // 空成员切片添加返回 0
  assert_eq!(set.sadd::<&[u8]>([]), 0);
  assert_eq!(set.len(), 2);

  info!("test_sadd_basic 通过");
  OK
}

/// SADD 批量添加与批内重复去重
/// 对标 C# RespSetTest.cs: CanAddAndListMembers, CanAddAndListMembersLC
#[test]
fn test_sadd_batch_and_dedup() -> Void {
  let mut set = SetObject::new();

  // 批量添加不重复成员
  let added = set.sadd([
    &b"ItemOne"[..],
    &b"ItemTwo"[..],
    &b"ItemThree"[..],
    &b"ItemFour"[..],
  ]);
  assert_eq!(added, 4);
  assert_eq!(set.len(), 4);

  // 批内含有重复成员：仅计首次新增
  let added_dup = set.sadd([&b"ItemOne"[..], &b"ItemFive"[..], &b"ItemFive"[..]]);
  assert_eq!(added_dup, 1);
  assert_eq!(set.len(), 5);

  // 排序成员列表比对验证
  let members = sorted_members(&set);
  assert_eq!(
    members,
    [
      &b"ItemFive"[..],
      &b"ItemFour"[..],
      &b"ItemOne"[..],
      &b"ItemThree"[..],
      &b"ItemTwo"[..],
    ]
  );

  info!("test_sadd_batch_and_dedup 通过");
  OK
}

/// SREM 移除成员
/// 对标 C# RespSetTest.cs: CanRemoveField, CanDoSREMLC
#[test]
fn test_srem_basic() -> Void {
  let mut set = SetObject::new();
  set.sadd([&b"one"[..], &b"two"[..], &b"three"[..], &b"four"[..]]);
  assert_eq!(set.len(), 4);

  // 移除存在的成员与不存在的成员
  assert_eq!(set.srem(&[b"one", b"three", b"nonexistent"]), 2);
  assert_eq!(set.len(), 2);
  assert!(!set.sismember(b"one"));
  assert!(!set.sismember(b"three"));
  assert!(set.sismember(b"two"));
  assert!(set.sismember(b"four"));

  // 再次移除相同元素返回 0
  assert_eq!(set.srem(&[b"one"]), 0);
  assert_eq!(set.len(), 2);

  // 从空集合移除返回 0
  let mut empty = SetObject::new();
  assert_eq!(empty.srem(&[b"any"]), 0);

  info!("test_srem_basic 通过");
  OK
}

/// SCARD 集合基数统计
/// 对标 C# RespSetTest.cs: CanDoSCARDCommandLC, CanDoSCARDCommandWhenKeyDoesNotExistLC
#[test]
fn test_scard_basic() -> Void {
  let mut set = SetObject::new();
  assert_eq!(set.len(), 0);
  assert!(set.is_empty());

  let mut itoa_buf = itoa::Buffer::new();
  for i in 0..10 {
    let s = itoa_buf.format(i);
    let mut m = Vec::with_capacity(2 + s.len());
    m.extend_from_slice(b"m_");
    m.extend_from_slice(s.as_bytes());
    set.sadd([m]);
    assert_eq!(set.len(), i + 1);
  }

  // 移除后基数减少
  set.srem(&[b"m_0", b"m_1"]);
  assert_eq!(set.len(), 8);

  info!("test_scard_basic 通过");
  OK
}

/// SISMEMBER 单成员探测
/// 对标 C# RespSetTest.cs: CanCheckIfMemberExistsInSet, CanCheckIfMemberExistsInSetLC
#[test]
fn test_sismember_basic() -> Void {
  let mut set = SetObject::new();
  set.sadd([&b"apple"[..], &b"banana"[..], &b"cherry"[..]]);

  assert!(set.sismember(b"apple"));
  assert!(set.sismember(b"banana"));
  assert!(set.sismember(b"cherry"));
  assert!(!set.sismember(b"durian"));
  assert!(!set.sismember(b""));

  info!("test_sismember_basic 通过");
  OK
}

/// SMISMEMBER 批量成员探测
/// 对标 C# RespSetTest.cs: CheckIfMemberExistsInSetLC, CheckIfMemberExistsWithNoExistKey
#[test]
fn test_smismember_batch() -> Void {
  let mut set = SetObject::new();
  set.sadd([
    &b"one"[..],
    &b"two"[..],
    &b"three"[..],
    &b"four"[..],
    &b"five"[..],
  ]);

  // 批量存在性探测按输入顺序返回布尔值
  let res = set.smismember(&[b"one", b"six", b"three", b"seven", b"five"]);
  assert_eq!(res, [true, false, true, false, true]);

  // 空集合探测全 false
  let empty = SetObject::new();
  assert_eq!(empty.smismember(&[b"a", b"b"]), [false, false]);

  info!("test_smismember_batch 通过");
  OK
}

/// SMOVE 集合间成员原子转移
/// 对标 C# RespSetTest.cs: CanDoSmoveBasic, CanDoSMOVECommandLC
#[test]
fn test_smove_basic() -> Void {
  let mut source = SetObject::new();
  source.sadd([&b"oneS"[..], &b"twoS"[..], &b"threeS"[..], &b"common"[..]]);

  let mut dest = SetObject::new();
  dest.sadd([&b"oneD"[..], &b"twoD"[..], &b"common"[..]]);

  // 转移存在的成员
  assert!(source.smove(&mut dest, b"oneS"));
  assert!(!source.sismember(b"oneS"));
  assert!(dest.sismember(b"oneS"));

  // 转移已在目标集合中存在的公共成员
  assert!(source.smove(&mut dest, b"common"));
  assert!(!source.sismember(b"common"));
  assert!(dest.sismember(b"common"));

  // 转移源集合中不存在的成员返回 false
  assert!(!source.smove(&mut dest, b"not_in_source"));

  info!("test_smove_basic 通过");
  OK
}

/// SMOVE 自别名防御（源与目标指向同一集合实例时安全短路）
/// 对标 C# RespSetTest.cs: CanDoSmoveBasic (自别名与边界防御)
#[test]
fn test_smove_self_aliasing_guard() -> Void {
  let mut set = SetObject::new();
  set.sadd([b"item1", b"item2"]);

  let set_ptr = &mut set as *mut SetObject;
  let self_ref = unsafe { &mut *set_ptr };

  assert!(!set.smove(self_ref, b"item1"));
  assert_eq!(set.len(), 2);
  assert!(set.sismember(b"item1"));
  assert!(set.sismember(b"item2"));

  info!("test_smove_self_aliasing_guard 通过");
  OK
}

/// SPOP 随机弹出成员
/// 对标 C# RespSetTest.cs: CanDoSPOPCommandLC, CanDoSPOPWithCountCommandLC, CanDoSPOPWithMoreCountThanSetSizeCommandLC, CanDoSPOPCommandWhenKeyDoesNotExistLC
#[test]
fn test_spop_semantics() -> Void {
  let mut set = make_seq_set("elem_", 20);
  assert_eq!(set.len(), 20);

  // count = 0 返回空且集合不变
  assert!(set.spop(0).is_empty());
  assert_eq!(set.len(), 20);

  // 弹出单个元素
  let single = set.spop(1);
  assert_eq!(single.len(), 1);
  assert_eq!(set.len(), 19);
  assert!(!set.sismember(&single[0]));

  // 弹出多个元素 (count < total / 2)
  let popped = set.spop(5);
  assert_eq!(popped.len(), 5);
  assert_eq!(set.len(), 14);
  for item in &popped {
    assert!(!set.sismember(item));
  }

  // 弹出多个元素 (count >= total / 2 大比例摘除路径)
  let popped_half = set.spop(8);
  assert_eq!(popped_half.len(), 8);
  assert_eq!(set.len(), 6);
  for item in &popped_half {
    assert!(!set.sismember(item));
  }

  // 弹出数量超出剩余总量：全部弹出且集合为空
  let popped_all = set.spop(100);
  assert_eq!(popped_all.len(), 6);
  assert!(set.is_empty());

  // 空集合弹出返回空
  assert!(set.spop(1).is_empty());

  info!("test_spop_semantics 通过");
  OK
}

/// SRANDMEMBER 随机采样
/// 对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandSE, CanDoSRANDMEMBERWithCountCommandLC
#[test]
fn test_srandmember_semantics() -> Void {
  let set = make_seq_set("rand_", 50);

  // count = 0 返回空
  assert!(set.srandmember(0).is_empty());

  // count = 1
  let sample1 = set.srandmember(1);
  assert_eq!(sample1.len(), 1);
  assert!(set.sismember(&sample1[0]));
  assert_eq!(set.len(), 50); // 不删除

  // count > 0: 互不重复采样
  let mut sample_distinct = set.srandmember(10);
  assert_eq!(sample_distinct.len(), 10);
  sample_distinct.sort_unstable();
  sample_distinct.dedup();
  assert_eq!(sample_distinct.len(), 10);

  // count >= total: 返回全部元素且互不重复
  let all = set.srandmember(100);
  assert_eq!(all.len(), 50);

  // count < 0: 允许重复采样
  let sample_dup = set.srandmember(-30);
  assert_eq!(sample_dup.len(), 30);
  for item in &sample_dup {
    assert!(set.sismember(item));
  }

  // 单元素集合负 count 快速路径
  let mut solo = SetObject::new();
  solo.sadd([b"solo_item"]);
  let samples = solo.srandmember(-10);
  assert_eq!(samples.len(), 10);
  for s in samples {
    assert_eq!(s, b"solo_item");
  }

  // 空集合采样返回空
  let empty = SetObject::new();
  assert!(empty.srandmember(5).is_empty());
  assert!(empty.srandmember(-5).is_empty());

  info!("test_srandmember_semantics 通过");
  OK
}

/// SRANDMEMBER 反向位排除快速抽样 (N <= 64 且 count > total / 2)
/// 对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandLC
#[test]
fn test_srandmember_reverse_bitmask() -> Void {
  let mut mid_set = SetObject::with_capacity(60);
  mid_set.sadd((0..60u32).map(|i| i.to_le_bytes()));

  // 抽取 58 个（利用 exclude = 2 的快速排除）
  let mut picked = mid_set.srandmember(58);
  assert_eq!(picked.len(), 58);
  picked.sort_unstable();
  picked.dedup();
  assert_eq!(picked.len(), 58);

  info!("test_srandmember_reverse_bitmask 通过");
  OK
}

/// srandmember_ref 零拷贝只读批量借用
/// 对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandLC (零拷贝只读批量借用)
#[test]
fn test_srandmember_ref_zero_copy() -> Void {
  let mut set = SetObject::new();
  assert!(set.srandmember_ref(5).is_empty());

  let mut itoa_buf = itoa::Buffer::new();
  for i in 0..10u32 {
    let s = itoa_buf.format(i);
    let mut elem = Vec::with_capacity(5 + s.len());
    elem.extend_from_slice(b"elem_");
    elem.extend_from_slice(s.as_bytes());
    set.sadd([elem]);
  }

  // count == 0
  assert!(set.srandmember_ref(0).is_empty());

  // count == 1
  let single = set.srandmember_ref(1);
  assert_eq!(single.len(), 1);
  assert!(set.sismember(single[0]));

  // count == 5 < total
  let mut five = set.srandmember_ref(5);
  assert_eq!(five.len(), 5);
  for item in &five {
    assert!(set.sismember(item));
  }
  five.sort_unstable();
  five.dedup();
  assert_eq!(five.len(), 5);

  // count >= total
  let mut all = set.srandmember_ref(20);
  assert_eq!(all.len(), 10);
  all.sort_unstable();
  all.dedup();
  assert_eq!(all.len(), 10);

  info!("test_srandmember_ref_zero_copy 通过");
  OK
}

/// 大集合反向 Floyd 抽样线性有序生成 (N > 512, count > total / 2)
/// 对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandLC (大集合反向 Floyd 抽样线性有序生成)
#[test]
fn test_reverse_floyd_sampling_large() -> Void {
  let mut set = SetObject::with_capacity(1000);
  set.sadd((0..1000u32).map(|i| i.to_le_bytes()));
  assert_eq!(set.len(), 1000);

  // 抽取 995 个元素（触发 N > 512 场景 4 的反向 exclude = 5 分支）
  let mut picked = set.srandmember(995);
  assert_eq!(picked.len(), 995);

  picked.sort_unstable();
  picked.dedup();
  assert_eq!(picked.len(), 995);

  // 用 srandmember_ref 同步验证
  let mut picked_refs = set.srandmember_ref(995);
  assert_eq!(picked_refs.len(), 995);
  picked_refs.sort_unstable();
  picked_refs.dedup();
  assert_eq!(picked_refs.len(), 995);

  info!("test_reverse_floyd_sampling_large 通过");
  OK
}

/// 反向 Floyd 区间扩展在 10,000 规模高比例抽样下的正确性
/// 对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandLC (反向 Floyd 区间扩展高比例抽样)
#[test]
fn test_reverse_floyd_interval_extreme_scale() -> Void {
  let total = 10_000;
  let count = 9_990; // exclude = 10，触发区间扩展加速

  let mut set = SetObject::with_capacity(total);
  set.sadd((0..total as u32).map(|i| i.to_le_bytes()));
  assert_eq!(set.len(), total);

  // 抽取 9,990 个不重复元素
  let mut picked = set.srandmember(count as isize);
  assert_eq!(picked.len(), count);

  picked.sort_unstable();
  picked.dedup();
  assert_eq!(picked.len(), count);

  // 验证零拷贝 srandmember_ref 与其一致性
  let mut picked_refs = set.srandmember_ref(count);
  assert_eq!(picked_refs.len(), count);

  picked_refs.sort_unstable();
  picked_refs.dedup();
  assert_eq!(picked_refs.len(), count);

  info!("test_reverse_floyd_interval_extreme_scale 通过");
  OK
}

/// N > 512 大集合的正向 Floyd 抽样、大比例 spop 摘除与负 count 堆引用表
/// 对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandLC, CanDoSPOPWithCountCommandLC
#[test]
fn test_large_set_floyd_forward_and_spop() -> Void {
  let mut set = SetObject::with_capacity(1000);
  set.sadd((0..1000u32).map(|i| i.to_le_bytes()));

  // 1. 正向 Floyd：K=100 > 64 且 K <= total / 2
  let mut picked = set.srandmember(100);
  assert_eq!(picked.len(), 100);
  for item in &picked {
    assert!(set.sismember(item));
  }
  picked.sort_unstable();
  picked.dedup();
  assert_eq!(picked.len(), 100);

  // 2. spop 单趟原位摘除：count > 64 且 count < total / 2
  let popped = set.spop(100);
  assert_eq!(popped.len(), 100);
  assert_eq!(set.len(), 900);
  for item in &popped {
    assert!(!set.sismember(item));
  }

  // 3. srandmember 负 count 在 total > 512 时走堆引用表分支
  let samples = set.srandmember(-50);
  assert_eq!(samples.len(), 50);
  for item in &samples {
    assert!(set.sismember(item));
  }

  info!("test_large_set_floyd_forward_and_heap_pop 通过");
  OK
}

/// SSCAN 游标遍历与 glob 模式过滤
/// 对标 C# RespSetTest.cs: CanDoSScanWithCursor, CanUseSScanNoParameters, CanUseSScanWithMatch
#[test]
fn test_sscan_semantics() -> Void {
  let set = make_seq_set("prefix_", 25);
  assert_eq!(set.len(), 25);

  // 1. 无 pattern 游标全量遍历
  let mut all_scanned = Vec::new();
  let mut cursor = 0;
  loop {
    let (next_cur, items) = set.sscan(cursor, 10, None);
    all_scanned.extend(items);
    cursor = next_cur;
    if cursor == 0 {
      break;
    }
  }
  assert_eq!(all_scanned.len(), 25);

  // 2. count = 0 走默认每页 10 条（对标 CanUseSScanNoParameters 无 COUNT 参数）
  let (cur_default, page) = set.sscan(0, 0, None);
  assert_eq!(page.len(), 10);
  assert!(cur_default > 0);

  // 3. 带 glob pattern 过滤
  let (cur, items) = set.sscan(0, 50, Some(b"prefix_1*"));
  assert_eq!(cur, 0);
  // prefix_1, prefix_10 到 prefix_19 共 11 项
  for item in &items {
    assert!(item.starts_with(b"prefix_1"));
  }

  // 4. sscan_ref 零拷贝切片遍历
  let (cur_ref, refs) = set.sscan_ref(0, 50, Some(b"prefix_2*"));
  assert_eq!(cur_ref, 0);
  for r in &refs {
    assert!(r.starts_with(b"prefix_2"));
  }

  // 5. 全不命中模式：游标走完全程后回零且结果为空（防死循环回归）
  let (cur_none, none_items) = set.sscan(0, 10, Some(b"zzz*"));
  assert_eq!(cur_none, 0);
  assert!(none_items.is_empty());

  // 6. 末段游标起步：仅返回剩余成员后回零
  let (cur_tail, tail_items) = set.sscan(20, 10, None);
  assert_eq!(tail_items.len(), 5);
  assert_eq!(cur_tail, 0);

  info!("test_sscan_semantics 通过");
  OK
}

/// 四条抽样路径的分布均匀性回归（SRANDMEMBER 正/负 count、random_member、SPOP 单弹）
/// 对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandLC (分布均匀性回归)
#[test]
fn test_sampling_distribution_uniformity() -> Void {
  const TOTAL: usize = 8;
  const TRIALS: usize = 8000;
  // 均匀分布 p = 1/8 下均值 1000、标准差约 29.6，[700, 1300] 约 ±10σ，
  // 健康采样器越界概率可忽略，而任何退化采样器（恒定/单侧偏斜）必然越界
  const LOW: usize = 700;
  const HIGH: usize = 1300;

  let build = || {
    let mut set = SetObject::with_capacity(TOTAL);
    set.sadd((0..TOTAL as u8).map(|i| vec![i]));
    set
  };

  let check = |name: &str, sample: Vec<u8>| {
    for i in 0..TOTAL as u8 {
      let got = sample.iter().filter(|&&m| m == i).count();
      assert!(
        (LOW..=HIGH).contains(&got),
        "{name} 成员 {i} 出现 {got} 次，偏离均匀区间 [{LOW}, {HIGH}]"
      );
    }
  };

  // 1. srandmember(1)：sample_distinct_indices 升序抽样路径
  let set = build();
  check(
    "srandmember(1)",
    (0..TRIALS).map(|_| set.srandmember(1)[0][0]).collect(),
  );

  // 2. random_member：iter().nth 随机访问路径
  check(
    "random_member",
    (0..TRIALS)
      .filter_map(|_| set.random_member().map(|m| m[0]))
      .collect(),
  );

  // 3. srandmember(-1)：允许重复采样路径
  check(
    "srandmember(-1)",
    (0..TRIALS).map(|_| set.srandmember(-1)[0][0]).collect(),
  );

  // 4. spop(1)：升序抽样 + 单趟摘除路径（每次在独立副本上弹出）
  check(
    "spop(1)",
    (0..TRIALS)
      .map(|_| {
        let mut s = build();
        s.spop(1)[0][0]
      })
      .collect(),
  );

  info!("test_sampling_distribution_uniformity 通过");
  OK
}

/// 零拷贝只读切片 API 与迭代器 trait 测试
/// 对标 C# RespSetTest.cs: CanAddAndListMembers (零拷贝只读切片 API 与迭代器 trait 测试)
#[test]
fn test_zero_copy_and_iterators() -> Void {
  let empty = SetObject::new();
  assert!(empty.random_member().is_none());

  let mut set = SetObject::new();
  set.sadd([b"solo"]);
  assert_eq!(set.random_member(), Some(&b"solo"[..]));

  set.sadd([&b"second"[..], &b"third"[..]]);
  let picked = set.random_member().unwrap();
  assert!(set.sismember(picked));

  // &SetObject IntoIterator
  let mut count = 0;
  for m in &set {
    assert!(set.sismember(m));
    count += 1;
  }
  assert_eq!(count, 3);

  // FromIterator (collect::<SetObject>())
  let collected: SetObject = [b"x".as_slice(), b"y".as_slice(), b"z".as_slice()]
    .into_iter()
    .collect();
  assert_eq!(collected.len(), 3);
  assert!(collected.sismember(b"x"));

  // SetObject IntoIterator (消费所有权)
  let drained_items: Vec<Vec<u8>> = collected.into_iter().collect();
  assert_eq!(drained_items.len(), 3);

  // &CompactSet IntoIterator
  let mut compact = CompactSet::new();
  compact.insert(b"alpha")?;
  compact.insert(b"beta")?;
  let mut c_count = 0;
  for m in &compact {
    assert!(compact.contains(m));
    c_count += 1;
  }
  assert_eq!(c_count, 2);

  info!("test_zero_copy_and_iterators 通过");
  OK
}

/// 清空集合与各种移除操作后的内存与物理收敛
/// 对标 C# RespSetTest.cs: CanRemoveField (清空集合与物理收敛)
#[test]
fn test_clear_and_physical_convergence() -> Void {
  let mut set = make_seq_set("item_", 100);
  assert_eq!(set.len(), 100);

  // 1. clear 物理收敛
  set.clear();
  assert!(set.is_empty());
  assert_eq!(set.len(), 0);

  // 2. srem 清空时的物理收敛
  set.sadd([b"x", b"y"]);
  assert_eq!(set.srem(&[b"x", b"y"]), 2);
  assert!(set.is_empty());

  // 3. spop 清空时的物理收敛
  set.sadd([b"item_1"]);
  let popped = set.spop(1);
  assert_eq!(popped.len(), 1);
  assert!(set.is_empty());

  // 4. smove 清空源集合时的物理收敛
  set.sadd([b"item_move"]);
  let mut dest = SetObject::new();
  assert!(set.smove(&mut dest, b"item_move"));
  assert!(set.is_empty());
  assert_eq!(dest.len(), 1);

  // 5. CompactSet clear
  let mut compact = CompactSet::new();
  compact.insert(b"test1")?;
  compact.insert(b"test2")?;
  assert_eq!(compact.len(), 2);
  compact.clear();
  assert!(compact.is_empty());
  assert_eq!(compact.len(), 0);

  info!("test_clear_and_physical_convergence 通过");
  OK
}

/// 允许重复的零拷贝采样与 members_ref
/// 对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandLC (允许重复的零拷贝采样与 members_ref)
#[test]
fn test_members_ref_and_dup_ref() -> Void {
  let mut set = SetObject::new();
  assert!(set.members_ref().is_empty());
  assert!(set.srandmember_dup_ref(5).is_empty());

  set.sadd([&b"item_a"[..], &b"item_b"[..], &b"item_c"[..]]);
  assert_eq!(set.members_ref().len(), 3);
  for m in set.members_ref() {
    assert!(set.sismember(m));
  }

  // 允许重复的零拷贝采样
  let dup_samples = set.srandmember_dup_ref(10);
  assert_eq!(dup_samples.len(), 10);
  for s in dup_samples {
    assert!(set.sismember(s));
  }

  // 单元素集合允许重复采样
  let mut single = SetObject::new();
  single.sadd([b"solo"]);
  let single_dup = single.srandmember_dup_ref(5);
  assert_eq!(single_dup.len(), 5);
  for s in single_dup {
    assert_eq!(s, b"solo");
    assert!(single.sismember(s));
  }

  // 重复采样上限保护：恶意大数被截断至 MAX_RAND_SAMPLE_LIMIT，防内存耗尽
  let capped = single.srandmember_dup_ref(2_000_000_000);
  assert_eq!(capped.len(), MAX_RAND_SAMPLE_LIMIT);
  for s in &capped[..16] {
    assert_eq!(*s, b"solo");
  }

  info!("test_members_ref_and_dup_ref 通过");
  OK
}
