// 对标 C#: test/standalone/Garnet.test.collections/RespListTests.cs
// LINDEX / LSET 语义：正负索引读取、原地覆写、越界报错与 isize 极值防护。
// 注：lindex_mut 原地覆写为 Rust 转写特有能力（C# 侧经 Span 覆写），一并覆盖。

use aok::{OK, Void};
use log::info;
use wedb_list::{Error, ListObject};

/// BasicRPUSHAndLINDEX：正负索引读取与越界返回 None
#[test]
fn basic_rpush_and_lindex() -> Void {
  let mut list = ListObject::new();
  let vals: Vec<Vec<u8>> = (0..3).map(|i| format!("val_{}", i).into_bytes()).collect();
  assert_eq!(list.rpush(vals.iter().map(Vec::as_slice)), 3);

  // LINDEX -1 与 LINDEX 2 均取最后一个元素
  assert_eq!(list.lindex(-1), Some(vals[2].as_slice()));
  assert_eq!(list.lindex(2), Some(vals[2].as_slice()));
  // 越界索引返回空
  assert_eq!(list.lindex(3), None);
  OK
}

/// lindex_mut 原地覆写：可变引用直接扩展元素内容（转写新增能力）
#[test]
fn lindex_mut_in_place_overwrite() -> Void {
  let mut list = ListObject::new();
  let vals: Vec<Vec<u8>> = (0..3).map(|i| format!("val_{}", i).into_bytes()).collect();
  list.rpush(vals.iter().map(Vec::as_slice));

  if let Some(item) = list.lindex_mut(0) {
    item.extend_from_slice(b"_mut");
  }
  assert_eq!(list.lindex(0), Some(b"val_0_mut".as_slice()));
  assert_eq!(list.lindex(1), Some(b"val_1".as_slice()));

  // 负索引与越界路径
  assert!(list.lindex_mut(-3).is_some());
  assert!(list.lindex_mut(3).is_none());

  info!("lindex_mut 原地覆写语义通过");
  OK
}

/// CanDoLSETbasicLC：LSET 正索引与负索引覆写
#[test]
fn can_do_lset_basic_lc() -> Void {
  let mut list = ListObject::new();
  list.rpush([&b"one"[..], &b"two"[..], &b"three"[..]]);

  // LSET mylist 0 four / LSET mylist -2 five -> [four, five, three]
  list.lset(0, b"four")?;
  list.lset(-2, b"five")?;

  assert_eq!(
    list.lrange(0, -1),
    vec![b"four".to_vec(), b"five".to_vec(), b"three".to_vec()]
  );

  info!("CanDoLSETbasicLC 语义通过：正负索引覆写");
  OK
}

/// CanReturnErrorLSETWhenIndexOutRange：越界索引与空键 LSET 均返回索引越界错误
#[test]
fn can_return_error_lset_when_index_out_range() -> Void {
  let mut list = ListObject::new();
  list.rpush([&b"one"[..], &b"two"[..], &b"three"[..]]);

  assert_eq!(list.lset(10, b"four"), Err(Error::IndexOutOfRange));
  assert_eq!(list.lset(-100, b"four"), Err(Error::IndexOutOfRange));
  // 覆写失败不得修改列表
  assert_eq!(
    list.lrange(0, -1),
    vec![b"one".to_vec(), b"two".to_vec(), b"three".to_vec()]
  );

  // 空键（不存在键）上 LSET 同样越界报错
  let mut missing = ListObject::new();
  assert_eq!(missing.lset(0, b"four"), Err(Error::IndexOutOfRange));

  info!("CanReturnErrorLSETWhenIndexOutRange 语义通过：越界报错且无副作用");
  OK
}

/// index_extreme_isize_boundary：isize::MIN/MAX 极值输入下各命令的溢出防护（转写新增）
#[test]
fn index_extreme_isize_boundary() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"elem0", b"elem1", b"elem2"]);

  // 极限负数索引读取与设置安全防护
  assert_eq!(list.lindex(isize::MIN), None);
  assert_eq!(list.lset(isize::MIN, b"val"), Err(Error::IndexOutOfRange));

  // 极限正数索引读取与设置安全防护
  assert_eq!(list.lindex(isize::MAX), None);
  assert_eq!(list.lset(isize::MAX, b"val"), Err(Error::IndexOutOfRange));

  // 极限范围切片 LRANGE [isize::MIN, isize::MAX] 返回全量
  assert_eq!(
    list.lrange(isize::MIN, isize::MAX),
    vec![b"elem0".to_vec(), b"elem1".to_vec(), b"elem2".to_vec()]
  );

  // 极限修剪 LTRIM [isize::MIN, -1]：负 start 钳 0，stop = len-1，全量保留
  list.ltrim(isize::MIN, -1);
  assert_eq!(list.len(), 3);

  // 倒置极限范围为空区间
  assert!(list.lrange(isize::MAX, isize::MIN).is_empty());

  // LPOS 与 LREM 在极端负数输入下绝不溢出
  assert!(list.lpos(b"elem1", isize::MIN, None, 0).is_empty());
  assert_eq!(list.lrem(isize::MIN, b"nonexistent"), 0);

  info!("index_extreme_isize_boundary 语义通过：isize 极值全命令防护");
  OK
}
