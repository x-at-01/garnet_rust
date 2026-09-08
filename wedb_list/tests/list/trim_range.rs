// 对标 C#: test/standalone/Garnet.test.collections/RespListTests.cs
// LTRIM / LRANGE 语义：正负索引切片矩阵、负数越界钳制与空区间清空。

use aok::{OK, Void};
use log::info;
use wedb_list::ListObject;

/// 构造 [lo, hi) 的 val_i 元素列表
fn vals_range(lo: usize, hi: usize) -> Vec<Vec<u8>> {
  (lo..hi)
    .map(|i| format!("val_{}", i).into_bytes())
    .collect()
}

/// MultiLPUSHAndLTRIMWithMemoryCheck：LTRIM 各阶段长度递减，内存占用同步收缩
#[test]
fn multi_lpush_and_ltrim_with_memory_check() -> Void {
  let mut list = ListObject::new();
  let vals = vals_range(0, 10);
  assert_eq!(list.lpush(vals.iter().map(Vec::as_slice)), 10);
  let size_before = list.byte_size();

  // LTRIM 1 5 -> 保留 5 个
  list.ltrim(1, 5);
  assert_eq!(list.len(), 5);
  assert!(list.byte_size() < size_before, "裁剪后内存占用必须收缩");

  // 全部保留 LTRIM 0 -1 -> 仍为 5 个
  list.ltrim(0, -1);
  assert_eq!(list.len(), 5);

  // LTRIM 0 -3 -> 保留前 3 个：左推后列表为 [val_9..val_0]，裁剪后为 [val_8, val_7, val_6]
  list.ltrim(0, -3);
  assert_eq!(list.len(), 3);
  assert_eq!(
    list.lrange(0, -1),
    vec![b"val_8".to_vec(), b"val_7".to_vec(), b"val_6".to_vec()]
  );

  // LTRIM -4 -4：负 start 钳制到 0，负 stop 越界保持负值 -> 空区间清空列表（键被移除）
  list.ltrim(-4, -4);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);

  info!("MultiLPUSHAndLTRIMWithMemoryCheck 语义通过：裁剪收缩内存与负越界清空");
  OK
}

/// MultiRPUSHAndLTRIM：14 组 (start, stop) 切片矩阵，逐一校验保留元素下标
#[test]
fn multi_rpush_and_ltrim() -> Void {
  let vals = vals_range(0, 10);
  let cases: [(isize, isize, &[usize]); 14] = [
    (0, 0, &[0]),
    (-2, -1, &[8, 9]),
    (-2, -2, &[8]),
    (3, 5, &[3, 4, 5]),
    (-12, 0, &[0]),
    (-12, 2, &[0, 1, 2]),
    (-12, -7, &[0, 1, 2, 3]),
    (-15, -11, &[]),
    (8, 8, &[8]),
    (8, 12, &[8, 9]),
    (9, 12, &[9]),
    (10, 12, &[]),
    (5, 3, &[]),
    (-3, -5, &[]),
  ];

  for (start, stop, expected_idx) in cases {
    let mut list = ListObject::new();
    assert_eq!(list.rpush(vals.iter().map(Vec::as_slice)), 10);
    list.ltrim(start, stop);

    let expected: Vec<Vec<u8>> = expected_idx.iter().map(|&i| vals[i].clone()).collect();
    assert_eq!(list.len(), expected.len(), "LTRIM {start} {stop} 长度不符");
    assert_eq!(
      list.lrange(0, -1),
      expected,
      "LTRIM {start} {stop} 内容不符"
    );
  }

  info!("MultiRPUSHAndLTRIM 语义通过：14 组正负索引切片矩阵全吻合");
  OK
}

/// BasicLPUSHAndLRANGE：左推 3 元素后各 LRANGE 区间长度校验
#[test]
fn basic_lpush_and_lrange() -> Void {
  let mut list = ListObject::new();
  let vals = vals_range(0, 3);
  assert_eq!(list.lpush(vals.iter().map(Vec::as_slice)), 3);
  assert_eq!(list.len(), 3);

  let rev_vals: Vec<Vec<u8>> = vals.into_iter().rev().collect();
  assert_eq!(list.lrange(0, 0), vec![b"val_2".to_vec()]);
  assert_eq!(list.lrange(-3, 2), rev_vals);
  assert_eq!(list.lrange(-100, 100), rev_vals);
  assert!(list.lrange(5, 10).is_empty());
  OK
}

/// CanDoLRANGEbasic：3 元素列表的基础区间读取
#[test]
fn can_do_lrange_basic() -> Void {
  let mut list = ListObject::new();
  let vals = [b"one".to_vec(), b"two".to_vec(), b"three".to_vec()];
  list.rpush(vals.iter().map(Vec::as_slice));

  assert_eq!(list.lrange(0, 0), vec![b"one".to_vec()]);
  assert_eq!(list.lrange(-3, 2), vals);
  assert_eq!(list.lrange(-100, 100), vals);
  assert!(list.lrange(5, 100).is_empty());
  OK
}

/// CanDoLRANGEcorrect：7 元素列表的完整区间矩阵，含负 stop 越界返回空数组语义
#[test]
fn can_do_lrange_correct() -> Void {
  let mut list = ListObject::new();
  let vals = [
    b"a".to_vec(),
    b"b".to_vec(),
    b"c".to_vec(),
    b"d".to_vec(),
    b"e".to_vec(),
    b"f".to_vec(),
    b"g".to_vec(),
  ];
  list.rpush(vals.iter().map(Vec::as_slice));

  assert_eq!(list.lrange(-10, -7), vec![b"a".to_vec()]);
  assert_eq!(
    list.lrange(-4, -2),
    vec![b"d".to_vec(), b"e".to_vec(), b"f".to_vec()]
  );
  assert_eq!(list.lrange(-1, -1), vec![b"g".to_vec()]);
  // start = 7-3 = 4 > stop = 3：负 start 越界钳 0 前提下仍为空区间
  assert!(list.lrange(-3, 3).is_empty());
  assert_eq!(list.lrange(-3, 4), vec![b"e".to_vec()]);
  assert_eq!(list.lrange(-4, 4), vec![b"d".to_vec(), b"e".to_vec()]);
  assert_eq!(list.lrange(0, 0), vec![b"a".to_vec()]);
  assert_eq!(list.lrange(1, 2), vec![b"b".to_vec(), b"c".to_vec()]);
  assert_eq!(list.lrange(3, 3), vec![b"d".to_vec()]);
  assert_eq!(list.lrange(4, 4), vec![b"e".to_vec()]);
  assert_eq!(list.lrange(5, 10), vec![b"f".to_vec(), b"g".to_vec()]);

  // 3 元素列表：补上长度后仍为负的 stop 寻址为空，绝不允许回绕到列表头部
  let mut list3 = ListObject::new();
  list3.rpush([b"a", b"b", b"c"]);
  assert_eq!(list3.lrange(0, -3), vec![b"a".to_vec()]);
  assert!(list3.lrange(0, -4).is_empty());
  assert!(list3.lrange(0, -5).is_empty());
  assert!(list3.lrange(-5, -4).is_empty());
  assert!(list3.lrange(1, -10).is_empty());

  // 5 元素列表：stop 越界量进一步扩大时同样为空
  let mut list5 = ListObject::new();
  list5.rpush([b"a", b"b", b"c", b"d", b"e"]);
  assert!(list5.lrange(0, -6).is_empty());
  assert!(list5.lrange(0, -7).is_empty());

  info!("CanDoLRANGEcorrect 语义通过：区间矩阵与负 stop 越界空数组语义");
  OK
}
