// 对标 C#: test/standalone/Garnet.test.collections/RespListTests.cs
// 基础推入弹出语义：LPUSH/RPUSH/LPOP/RPOP/LPUSHX/RPUSHX/LLEN 与空键清理。
// 说明：C# 侧的 MEMORY USAGE 精确字节数 (224/944 等) 属 C# 分配器实现细节，
// 此处以 byte_size()/capacity() 的相对收缩与彻底释放语义等价对标。

use std::mem::size_of;

use aok::{OK, Void};
use log::info;
use wedb_list::ListObject;

/// BasicLPUSHAndLPOP：单元素左推左弹，弹空后键被移除、内存彻底释放
#[test]
fn basic_lpush_and_lpop() -> Void {
  let mut list = ListObject::new();
  let val = b"Value-0";

  assert_eq!(list.lpush([val]), 1);
  assert_eq!(list.len(), 1);

  let popped = list.lpop_one();
  assert_eq!(popped.as_deref(), Some(val.as_slice()));
  assert!(list.is_empty());

  // 对应 C# 断言 KeyExists 为假 / MEMORY USAGE 返回 nil：列表变空即彻底释放
  assert_eq!(list.capacity(), 0);
  assert_eq!(list.byte_size(), size_of::<ListObject>());

  info!("BasicLPUSHAndLPOP 语义通过：左推左弹后空列表彻底释放内存");
  OK
}

/// BasicRPUSHAndRPOP：单元素右推
#[test]
fn basic_rpush_and_rpop() -> Void {
  let mut list = ListObject::new();
  assert_eq!(list.rpush([b"Value-0"]), 1);
  assert_eq!(list.len(), 1);
  assert_eq!(list.rpop_one().as_deref(), Some(b"Value-0".as_slice()));
  OK
}

/// MultiLPUSHAndLLENWithPendingStatus：25 个键各左推 100 个元素，LLEN 保持 100
#[test]
fn multi_lpush_and_llen_with_pending_status() -> Void {
  let n_vals = 100;
  let vals: Vec<Vec<u8>> = (0..n_vals)
    .map(|i| format!("val-{}", i + 1).into_bytes())
    .collect();

  let mut tenth = None;
  for j in 0..25 {
    let mut list = ListObject::new();
    assert_eq!(list.lpush(vals.iter().map(Vec::as_slice)), n_vals);
    if j == 9 {
      tenth = Some(list);
    }
  }
  // 对应 C# 断言 ListLength("List_Test-10") == 100
  assert_eq!(tenth.unwrap().len(), 100);

  info!("MultiLPUSHAndLLENWithPendingStatus 语义通过：25 键 × 100 元素长度一致");
  OK
}

/// BasicLPUSHAndLTRIM：单元素列表 LTRIM 0 5 与 LTRIM 0 -1 均完整保留
#[test]
fn basic_lpush_and_ltrim() -> Void {
  let mut list = ListObject::new();
  assert_eq!(list.lpush([b"Value-0"]), 1);

  list.ltrim(0, 5);
  assert_eq!(list.len(), 1);

  list.ltrim(0, -1);
  assert_eq!(list.len(), 1);
  OK
}

/// MultiLPUSHAndLPOPV1：左推 10 元素后逐个左弹，顺序为后推先出，弹空返回 None
#[test]
fn multi_lpush_and_lpop_v1() -> Void {
  let mut list = ListObject::new();
  let vals: Vec<Vec<u8>> = (0..10).map(|i| format!("val_{}", i).into_bytes()).collect();
  assert_eq!(list.lpush(vals.iter().map(Vec::as_slice)), 10);

  // 左推后列表为 [val_9, val_8, ..., val_0]，逐个左弹与 values[n-1] 逐次吻合
  for i in (0..10).rev() {
    assert_eq!(list.lpop_one().as_deref(), Some(vals[i].as_slice()));
  }

  // 列表已空，再弹返回 None，键被移除且内存彻底释放
  assert_eq!(list.lpop_one(), None);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);

  info!("MultiLPUSHAndLPOPV1 语义通过：后进先出与空列表回收");
  OK
}

/// MultiLPUSHAndLPOPV2：LPOP 带数量参数一次弹出 2 个元素
#[test]
fn multi_lpush_and_lpop_v2() -> Void {
  let mut list = ListObject::new();
  let vals: Vec<Vec<u8>> = (0..10).map(|i| format!("val_{}", i).into_bytes()).collect();
  assert_eq!(list.lpush(vals.iter().map(Vec::as_slice)), 10);

  // LPOP key 2 -> 剩余 8 个
  assert_eq!(list.lpop(2), vec![vals[9].clone(), vals[8].clone()]);
  assert_eq!(list.len(), 8);
  OK
}

/// MultiRPUSHAndRPOP：右推 10 元素后逐个右弹直至取空，空推入不建列表
#[test]
fn multi_rpush_and_rpop() -> Void {
  let mut list = ListObject::new();
  let vals: Vec<Vec<u8>> = (0..10).map(|i| format!("val_{}", i).into_bytes()).collect();
  assert_eq!(list.rpush(vals.iter().map(Vec::as_slice)), 10);

  for i in (0..10).rev() {
    assert_eq!(list.rpop_one().as_deref(), Some(vals[i].as_slice()));
  }

  // 列表已空，左弹返回 None，键被移除且内存彻底释放
  assert_eq!(list.lpop_one(), None);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);

  // 空数组推入：返回 0 且不创建列表
  assert_eq!(list.rpush(Vec::<Vec<u8>>::new()), 0);
  assert_eq!(list.lpush(Vec::<Vec<u8>>::new()), 0);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);

  info!("MultiRPUSHAndRPOP 语义通过：右推右弹取空与空推入无副作用");
  OK
}

/// CanDoLPopMultipleValues：RPOP 带数量参数一次弹出 5 个元素，弹出顺序自尾向头
#[test]
fn can_do_lpop_multiple_values() -> Void {
  let mut list = ListObject::new();
  let vals: Vec<Vec<u8>> = (1..=10)
    .map(|i| format!("valkey-{}", i).into_bytes())
    .collect();
  assert_eq!(list.lpush(vals.iter().map(Vec::as_slice)), 10);

  // 左推后列表为 [valkey-10, ..., valkey-1]，右弹 5 个按弹出顺序返回
  let popped = list.rpop(5);
  assert_eq!(popped.len(), 5);
  assert_eq!(popped[0], vals[0]);
  assert_eq!(popped[4], vals[4]);
  assert_eq!(list.len(), 5);

  info!("CanDoLPopMultipleValues 语义通过：批量右弹顺序自尾向头");
  OK
}

/// CanDoLPushXRpushX：LPUSHX/RPUSHX 对空列表不生效，列表存在后正常追加
#[test]
fn can_do_lpush_x_rpush_x() -> Void {
  let mut list = ListObject::new();
  let vals: Vec<Vec<u8>> = (1..=10)
    .map(|i| format!("valkey-{}", i).into_bytes())
    .collect();

  // When.Exists 语义：列表不存在时不创建、不推入
  assert_eq!(list.lpushx(vals.iter().map(Vec::as_slice)), 0);
  assert_eq!(list.rpushx(vals.iter().map(Vec::as_slice)), 0);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);

  // 正常创建列表
  assert_eq!(list.rpush(vals.iter().map(Vec::as_slice)), 10);
  assert_eq!(list.lrange(0, -1), vals);

  // 列表非空后 LPUSHX/RPUSHX 正常生效
  assert_eq!(list.lpushx([b"head"]), 11);
  assert_eq!(list.rpushx([b"tail"]), 12);
  let mut expect = vec![b"head".to_vec()];
  expect.extend(vals.iter().cloned());
  expect.push(b"tail".to_vec());
  assert_eq!(list.lrange(0, -1), expect);

  info!("CanDoLPushXRpushX 语义通过：空列表不创建、非空列表正常追加");
  OK
}

/// CheckEmptyListKeyRemoved：全部弹出后列表变空，键被移除且内存彻底释放
#[test]
fn check_empty_list_key_removed() -> Void {
  let mut list = ListObject::new();
  let vals = [b"Hello".to_vec(), b"World".to_vec()];
  assert_eq!(list.rpush(vals.iter().map(Vec::as_slice)), 2);

  assert_eq!(list.rpop(2).len(), 2);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);
  OK
}

/// CanHandleNoPrexistentKey：对不存在的键执行全部列表命令均为安全空结果且不建键
#[test]
fn can_handle_no_prexistent_key() -> Void {
  for _ in 0..100 {
    let mut list = ListObject::new();

    // LLEN
    assert_eq!(list.len(), 0);
    assert!(list.is_empty());

    // LPOP / RPOP 单元素
    assert_eq!(list.lpop_one(), None);
    assert_eq!(list.rpop_one(), None);

    // LPOP / RPOP 带数量
    assert!(list.lpop(100).is_empty());
    assert!(list.rpop(100).is_empty());

    // LRANGE
    assert!(list.lrange(0, -1).is_empty());

    // LINDEX
    assert_eq!(list.lindex(15), None);

    // LTRIM
    list.ltrim(0, 15);
    assert!(list.is_empty());

    // LREM
    assert_eq!(list.lrem(100, b"hello"), 0);

    assert_eq!(list.capacity(), 0);
  }

  info!("CanHandleNoPrexistentKey 语义通过：空键上全部命令无副作用");
  OK
}

/// LPOPAndRPOPWithZeroCountReturnEmptyArray：显式 count=0 返回空数组且不修改列表
#[test]
fn lpop_and_rpop_with_zero_count_return_empty_array() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"a", b"b"]);

  assert!(list.lpop(0).is_empty());
  assert!(list.rpop(0).is_empty());
  assert_eq!(list.len(), 2);

  // 计数为 0 不破坏后续操作配对
  assert_eq!(list.lpop_one().as_deref(), Some(b"a".as_slice()));
  assert_eq!(list.len(), 1);

  info!("LPOPAndRPOPWithZeroCountReturnEmptyArray 语义通过：count=0 空数组且零修改");
  OK
}
