// 对标 C#: test/standalone/Garnet.test.collections/RespListTests.cs
// LINSERT / LREM 语义：前后插入、pivot 未命中、首个匹配插入、
// 各方向限额删除与 int.MinValue 全量删除。

use aok::{OK, Void};
use log::info;
use wedb_list::{InsertPosition, ListObject};

/// BasicRPUSHAndLINSERT：BEFORE/AFTER 插入后按索引校验插入位置
#[test]
fn basic_rpush_and_linsert() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"val_0", b"val_1", b"val_2"]);

  // BEFORE val_1 插入 -> [val_0, val_test1, val_1, val_2]，插入点索引 1
  assert_eq!(
    list.linsert(b"val_1", b"val_test1", InsertPosition::Before),
    4
  );
  assert_eq!(list.lindex(1), Some(b"val_test1".as_slice()));

  // AFTER val_0 插入 -> [val_0, val_test2, val_test1, val_1, val_2]，插入点索引 1
  assert_eq!(
    list.linsert(b"val_0", b"val_test2", InsertPosition::After),
    5
  );
  assert_eq!(list.lindex(1), Some(b"val_test2".as_slice()));

  info!("BasicRPUSHAndLINSERT 语义通过：前后插入定位正确");
  OK
}

/// CanDoLInsertBeforeAndAfterLC：LINSERT BEFORE/AFTER 返回新长度，重复 pivot 仅首个匹配生效
#[test]
fn can_do_linsert_before_and_after_lc() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"Hello", b"World"]);

  // LINSERT mylist BEFORE World There -> [Hello, There, World]，新长度 3
  assert_eq!(list.linsert(b"World", b"There", InsertPosition::Before), 3);
  assert_eq!(
    list.lrange(0, -1),
    vec![b"Hello".to_vec(), b"There".to_vec(), b"World".to_vec()]
  );

  // LINSERT mylist AFTER World Bye -> [Hello, There, World, Bye]，新长度 4
  assert_eq!(list.linsert(b"World", b"Bye", InsertPosition::After), 4);
  assert_eq!(
    list.lrange(0, -1),
    vec![
      b"Hello".to_vec(),
      b"There".to_vec(),
      b"World".to_vec(),
      b"Bye".to_vec()
    ]
  );

  // 转写补充：重复 pivot 仅在首个匹配处插入
  let mut dup = ListObject::new();
  dup.rpush([&b"dup"[..], &b"other"[..], &b"dup"[..]]);
  assert_eq!(dup.linsert(b"dup", b"new", InsertPosition::After), 4);
  assert_eq!(
    dup.lrange(0, -1),
    vec![
      b"dup".to_vec(),
      b"new".to_vec(),
      b"other".to_vec(),
      b"dup".to_vec()
    ]
  );

  info!("CanDoLInsertBeforeAndAfterLC 语义通过：前后插入与首个匹配 pivot");
  OK
}

/// CanDoLInsertWithNoElementLC：pivot 不存在返回 -1，空列表同样返回 -1
#[test]
fn can_do_linsert_with_no_element_lc() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"Hello", b"World"]);

  // pivot "There" 不存在 -> -1 且列表不变
  assert_eq!(list.linsert(b"There", b"Today", InsertPosition::Before), -1);
  assert_eq!(list.len(), 2);

  // 空列表插入 -> -1
  let mut empty = ListObject::new();
  assert_eq!(empty.linsert(b"none", b"val", InsertPosition::Before), -1);
  assert_eq!(empty.linsert(b"none", b"val", InsertPosition::After), -1);
  assert!(empty.is_empty());
  OK
}

/// BasicRPUSHAndLREM：正向限额、逆向限额与全量删除的组合
#[test]
fn basic_rpush_and_lrem() -> Void {
  let mut list = ListObject::new();
  // C# 构造: [val_0, val_0, val_2, val_2, val_4, val_4]
  list.rpush([b"val_0", b"val_0", b"val_2", b"val_2", b"val_4", b"val_4"]);

  // LREM val_0 2 -> 删除前 2 个
  assert_eq!(list.lrem(2, b"val_0"), 2);
  assert_eq!(
    list.lrange(0, -1),
    vec![
      b"val_2".to_vec(),
      b"val_2".to_vec(),
      b"val_4".to_vec(),
      b"val_4".to_vec()
    ]
  );

  // LREM val_4 -1 -> 自尾删除 1 个，剩 3 个
  assert_eq!(list.lrem(-1, b"val_4"), 1);
  assert_eq!(list.len(), 3);
  assert_eq!(
    list.lrange(0, -1),
    vec![b"val_2".to_vec(), b"val_2".to_vec(), b"val_4".to_vec()]
  );

  // LREM val_2 0 -> 全量删除 2 个
  assert_eq!(list.lrem(0, b"val_2"), 2);

  // LREM val_4 0 -> 删除最后 1 个，列表变空（键被移除）
  assert_eq!(list.lrem(0, b"val_4"), 1);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);

  info!("BasicRPUSHAndLREM 语义通过：正向、逆向与全量删除");
  OK
}

/// LREMWithIntMinValueCountRemovesAllMatches：int.MinValue 计数全量删除且不溢出
#[test]
fn lrem_with_int_min_value_count_removes_all_matches() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"a", b"b", b"a", b"c", b"a"]);

  // int.MinValue 的绝对值无法被 int 表示，历史上 C# 端 Math.Abs 溢出崩溃；
  // 对象层必须等价于全量删除
  assert_eq!(list.lrem(i32::MIN as isize, b"a"), 3);
  assert_eq!(list.lrange(0, -1), vec![b"b".to_vec(), b"c".to_vec()]);

  // isize::MIN 同样等价全量删除
  let mut list2 = ListObject::new();
  list2.rpush([b"k", b"v", b"k"]);
  assert_eq!(list2.lrem(isize::MIN, b"k"), 2);
  assert_eq!(list2.lrange(0, -1), vec![b"v".to_vec()]);

  // isize::MAX 计数亦等价全量删除
  let mut list3 = ListObject::new();
  list3.rpush([b"k", b"v", b"k"]);
  assert_eq!(list3.lrem(isize::MAX, b"k"), 2);
  assert_eq!(list3.lrange(0, -1), vec![b"v".to_vec()]);

  info!("LREMWithIntMinValueCountRemovesAllMatches 语义通过：极值计数全量删除不溢出");
  OK
}

/// lrem_reverse_optimization：负数计数的逆向快速查找路径（转写新增）
#[test]
fn lrem_reverse_optimization() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"a", b"b", b"a", b"c", b"a", b"d"]);

  // count = -1：仅删除自尾起的第一个匹配
  assert_eq!(list.lrem(-1, b"a"), 1);
  assert_eq!(
    list.lrange(0, -1),
    vec![
      b"a".to_vec(),
      b"b".to_vec(),
      b"a".to_vec(),
      b"c".to_vec(),
      b"d".to_vec()
    ]
  );

  // count = -100：限额超过实际匹配数时删除全部匹配
  assert_eq!(list.lrem(-100, b"a"), 2);
  assert_eq!(
    list.lrange(0, -1),
    vec![b"b".to_vec(), b"c".to_vec(), b"d".to_vec()]
  );
  OK
}

/// lrem_all_modes_differential：全量 (0)、正限额、负限额与 isize 极值计数的结果互洽（转写新增）
#[test]
fn lrem_all_modes_differential() -> Void {
  let make_data = || {
    let mut list = ListObject::new();
    list.rpush([b"x", b"a", b"x", b"b", b"x", b"c", b"x"]);
    list
  };

  // 全量 count = 0 删除所有 "x"
  let mut l0 = make_data();
  assert_eq!(l0.lrem(0, b"x"), 4);
  assert_eq!(
    l0.lrange(0, -1),
    vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
  );

  // isize::MIN 与 isize::MAX 的删除结果必须与 count = 0 完全一致
  let mut l_min = make_data();
  assert_eq!(l_min.lrem(isize::MIN, b"x"), 4);
  assert_eq!(l_min, l0);

  let mut l_max = make_data();
  assert_eq!(l_max.lrem(isize::MAX, b"x"), 4);
  assert_eq!(l_max, l0);

  // 正向删除 2 个 -> 保留后两个 "x"
  let mut l_pos = make_data();
  assert_eq!(l_pos.lrem(2, b"x"), 2);
  assert_eq!(
    l_pos.lrange(0, -1),
    vec![
      b"a".to_vec(),
      b"b".to_vec(),
      b"x".to_vec(),
      b"c".to_vec(),
      b"x".to_vec()
    ]
  );

  // 负向删除 2 个 -> 保留前两个 "x"（删除的是全局最后 2 个匹配）
  let mut l_neg = make_data();
  assert_eq!(l_neg.lrem(-2, b"x"), 2);
  assert_eq!(
    l_neg.lrange(0, -1),
    vec![
      b"x".to_vec(),
      b"a".to_vec(),
      b"x".to_vec(),
      b"b".to_vec(),
      b"c".to_vec()
    ]
  );

  // 不存在的元素任何模式均返回 0 且列表不变
  let mut l_none = make_data();
  assert_eq!(l_none.lrem(0, b"nonexistent"), 0);
  assert_eq!(l_none.lrem(-5, b"nonexistent"), 0);
  assert_eq!(l_none.lrem(5, b"nonexistent"), 0);
  assert_eq!(l_none.len(), 7);

  info!("lrem_all_modes_differential 语义通过：各删除模式结果互洽");
  OK
}

/// lrem_reverse_bitmap_last_slot：负计数 u64 位图槽位边界 (转写新增)
///
/// 位图 `1 << local` 在 local == 页容量-1 时触及 u64 最高位；
/// 整页同值时 64 位位图全置位。差分对拍为统计覆盖，此处定点回归页尾槽位。
#[test]
fn lrem_reverse_bitmap_last_slot() -> Void {
  // 恰好 2 满页 (128)：第 2 页页尾 local=63 即位图最高位
  let vals: Vec<Vec<u8>> = (0..128).map(|i| format!("e{i:03}").into_bytes()).collect();
  let mut list = ListObject::new();
  list.rpush(vals.iter().map(Vec::as_slice));

  // count=-1: 删除全局最后一个 (第 2 页 local=63, 位图 bit63)
  assert_eq!(list.lrem(-1, b"e127"), 1);
  assert_eq!(list.len(), 127);
  assert_eq!(list.lindex(-1), Some(b"e126".as_slice()));

  // 自首页页尾 (bit63) 向页首 (bit0) 逐位逆向删除，位图逐位回退
  for i in (0..64).rev() {
    assert_eq!(
      list.lrem(-1, format!("e{i:03}").as_bytes()),
      1,
      "逆向删除 e{i:03}"
    );
  }
  assert_eq!(list.len(), 63);
  assert_eq!(list.lindex(0), Some(b"e064".as_slice()));
  assert_eq!(list.lindex(-1), Some(b"e126".as_slice()));

  // 整页 64 位位图全置位：-64 一次清空满页并触发页回收，尾页保留
  let mut full: Vec<Vec<u8>> = (0..64).map(|_| b"y".to_vec()).collect();
  full.push(b"z".to_vec());
  let mut l2 = ListObject::new();
  l2.rpush(full.iter().map(Vec::as_slice));
  assert_eq!(l2.lrem(-64, b"y"), 64);
  assert_eq!(l2.lrange(0, -1), vec![b"z".to_vec()]);

  // 全删后页目录彻底释放
  assert_eq!(l2.lrem(-1, b"z"), 1);
  assert!(l2.is_empty());
  assert_eq!(l2.capacity(), 0);

  info!("lrem_reverse_bitmap_last_slot 语义通过：位图最高位与整页 64 位删除");
  OK
}
