// 对标 C#: test/standalone/Garnet.test.collections/RespListTests.cs (CanDoRPopLPush /
// CanDoRPopLPushGC / CanDoBasicLMove / CanUseLMoveGC) 与 RespListGarnetClientTests.cs
// LMOVE / RPOPLPUSH 语义：跨列表方向组合、同键旋转 peek、批量转移与 drain。
// 说明：C# 侧 LMOVE 同键走 ListMove sameKey 分支，对象层对应 rotate；方向大小写
// 不敏感与语法错误校验属协议层，对象层以 bool 参数承接不适用。

use aok::{OK, Void};
use log::info;
use wedb_list::ListObject;

/// CanDoRPopLPush：跨列表右弹左推与同键旋转
#[test]
fn can_do_rpop_lpush() -> Void {
  let mut mylist = ListObject::new();
  let mut myotherlist = ListObject::new();
  mylist.rpush(["Value-one", "Value-two", "Value-three"]);

  // RPOPLPUSH mylist myotherlist -> "Value-three"
  assert_eq!(
    ListObject::rpoplpush(&mut mylist, &mut myotherlist),
    Some(b"Value-three".to_vec())
  );
  assert_eq!(
    mylist.lrange(0, -1),
    vec![b"Value-one".to_vec(), b"Value-two".to_vec()]
  );
  assert_eq!(myotherlist.lrange(0, -1), vec![b"Value-three".to_vec()]);

  // 源与目标相同：等价于列表旋转（右出左入）
  assert_eq!(
    mylist.rotate(false, true).map(<[u8]>::to_vec),
    Some(b"Value-two".to_vec())
  );
  assert_eq!(
    mylist.lrange(0, -1),
    vec![b"Value-two".to_vec(), b"Value-one".to_vec()]
  );

  info!("CanDoRPopLPush 语义通过：跨列表转移与同键旋转");
  OK
}

/// CanDoRPopLPushGC：源不存在返回 None 与旋转语义
#[test]
fn can_do_rpop_lpush_gc() -> Void {
  let mut mylist = ListObject::new();
  let mut myotherlist = ListObject::new();

  // 源不存在：返回 None 且不产生任何操作
  assert_eq!(ListObject::rpoplpush(&mut mylist, &mut myotherlist), None);

  mylist.rpush(["one", "two", "three"]);
  assert_eq!(
    ListObject::rpoplpush(&mut mylist, &mut myotherlist),
    Some(b"three".to_vec())
  );
  assert_eq!(mylist.lrange(0, -1), vec![b"one".to_vec(), b"two".to_vec()]);
  assert_eq!(myotherlist.lrange(0, -1), vec![b"three".to_vec()]);

  // 源与目标相同：旋转
  assert_eq!(
    mylist.rotate(false, true).map(<[u8]>::to_vec),
    Some(b"two".to_vec())
  );
  assert_eq!(mylist.lrange(0, -1), vec![b"two".to_vec(), b"one".to_vec()]);
  OK
}

/// CanDoBasicLMove：LMOVE 右出左入逐个搬空源列表，同键同侧为 peek 语义
#[test]
fn can_do_basic_lmove() -> Void {
  let mut key1 = ListObject::new();
  let key1_vals = [b"myval1".to_vec(), b"myval2".to_vec(), b"myval3".to_vec()];
  let mut key2 = ListObject::new();
  let key2_vals = [b"myval4".to_vec()];
  key1.rpush(key1_vals.iter().map(Vec::as_slice));
  key2.rpush(key2_vals.iter().map(Vec::as_slice));

  // LMOVE key1 key2 RIGHT LEFT ×3：自尾逐个搬移
  for i in (0..3).rev() {
    assert_eq!(
      ListObject::lmove(&mut key1, &mut key2, false, true),
      Some(key1_vals[i].clone())
    );
  }
  // key2 = key1Vals ∪ key2Vals 按序
  let mut expect: Vec<Vec<u8>> = key1_vals.to_vec();
  expect.extend(key2_vals.iter().cloned());
  assert_eq!(key2.lrange(0, -1), expect);
  assert!(key1.is_empty());

  // LMOVE key2 key2 RIGHT RIGHT：同键同侧为 peek，列表不变
  assert_eq!(
    key2.rotate(false, false).map(<[u8]>::to_vec),
    Some(b"myval4".to_vec())
  );
  assert_eq!(key2.lrange(0, -1), expect);

  // LMOVE key2 key2 LEFT LEFT：同为 peek，取头部元素
  assert_eq!(
    key2.rotate(true, true).map(<[u8]>::to_vec),
    Some(b"myval1".to_vec())
  );
  assert_eq!(key2.lrange(0, -1), expect);
  assert!(key1.is_empty());

  info!("CanDoBasicLMove 语义通过：跨列表搬移与同键 peek");
  OK
}

/// CanUseLMoveGC：LMOVE 四方向组合与同键旋转
#[test]
fn can_use_lmove() -> Void {
  let mut mylist = ListObject::new();
  let mut myotherlist = ListObject::new();

  // 源不存在：返回 None 且无操作
  assert_eq!(
    ListObject::lmove(&mut mylist, &mut myotherlist, false, true),
    None
  );

  mylist.rpush(["one", "two", "three"]);

  // RIGHT -> LEFT：尾出头入
  assert_eq!(
    ListObject::lmove(&mut mylist, &mut myotherlist, false, true),
    Some(b"three".to_vec())
  );
  assert_eq!(mylist.lrange(0, -1), vec![b"one".to_vec(), b"two".to_vec()]);
  assert_eq!(myotherlist.lrange(0, -1), vec![b"three".to_vec()]);

  // LEFT -> RIGHT：头出尾入
  assert_eq!(
    ListObject::lmove(&mut mylist, &mut myotherlist, true, false),
    Some(b"one".to_vec())
  );
  assert_eq!(mylist.lrange(0, -1), vec![b"two".to_vec()]);
  assert_eq!(
    myotherlist.lrange(0, -1),
    vec![b"three".to_vec(), b"one".to_vec()]
  );

  // 同键 LEFT -> RIGHT：旋转
  assert_eq!(
    mylist.rotate(true, false).map(<[u8]>::to_vec),
    Some(b"two".to_vec())
  );
  assert_eq!(mylist.lrange(0, -1), vec![b"two".to_vec()]);

  assert_eq!(
    myotherlist.rotate(true, false).map(<[u8]>::to_vec),
    Some(b"three".to_vec())
  );
  assert_eq!(
    myotherlist.lrange(0, -1),
    vec![b"one".to_vec(), b"three".to_vec()]
  );
  OK
}

/// transfer_to_all_directions：批量转移的 4 种方向组合与逐次 LMOVE 完全等价（转写新增）
#[test]
fn transfer_to_all_directions() -> Void {
  for (from_left, to_left, expect_dst) in [
    (
      true,
      true,
      vec![b"3".to_vec(), b"2".to_vec(), b"1".to_vec()],
    ),
    (
      true,
      false,
      vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()],
    ),
    (
      false,
      true,
      vec![b"2".to_vec(), b"3".to_vec(), b"4".to_vec()],
    ),
    (
      false,
      false,
      vec![b"4".to_vec(), b"3".to_vec(), b"2".to_vec()],
    ),
  ] {
    let mut s1 = ListObject::new();
    let mut d1 = ListObject::new();
    s1.rpush([b"1", b"2", b"3", b"4"]);
    assert_eq!(s1.transfer_to(&mut d1, 3, from_left, to_left), 3);

    // 逐次 lmove 对照组
    let mut s2 = ListObject::new();
    let mut d2 = ListObject::new();
    s2.rpush([b"1", b"2", b"3", b"4"]);
    for _ in 0..3 {
      ListObject::lmove(&mut s2, &mut d2, from_left, to_left);
    }
    assert_eq!(s1, s2, "from_left={from_left}, to_left={to_left}");
    assert_eq!(d1, d2);
    assert_eq!(d1.lrange(0, -1), expect_dst);
  }

  info!("transfer_to_all_directions 语义通过：批量转移与逐次 LMOVE 等价");
  OK
}

/// transfer_to_self_alias_and_edge_cases：同列表自转批量移动与跨列表边界（转写新增）
#[test]
fn transfer_to_self_alias_and_edge_cases() -> Void {
  // 同键自转 transfer_to (std::ptr::eq 命中自旋路径)
  let mut list = ListObject::new();
  list.rpush([b"1", b"2", b"3", b"4"]);
  let list_ptr = &mut list as *mut ListObject;
  // SAFETY: 别名引用仅用于命中 ptr::eq 自旋分支，调用期间无并发访问
  let self_ref = unsafe { &mut *list_ptr };

  // 左旋 2 步 -> [3, 4, 1, 2]
  assert_eq!(list.transfer_to(self_ref, 2, true, false), 2);
  assert_eq!(
    list.lrange(0, -1),
    vec![b"3".to_vec(), b"4".to_vec(), b"1".to_vec(), b"2".to_vec()]
  );

  // 右旋 2 步 -> 回到 [1, 2, 3, 4]
  assert_eq!(list.transfer_to(self_ref, 2, false, true), 2);
  assert_eq!(
    list.lrange(0, -1),
    vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec(), b"4".to_vec()]
  );

  // 同向自转为无操作
  assert_eq!(list.transfer_to(self_ref, 3, true, true), 3);
  assert_eq!(
    list.lrange(0, -1),
    vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec(), b"4".to_vec()]
  );
  assert_eq!(list.transfer_to(self_ref, 3, false, false), 3);
  assert_eq!(
    list.lrange(0, -1),
    vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec(), b"4".to_vec()]
  );

  // count == 0 自转
  assert_eq!(list.transfer_to(self_ref, 0, true, false), 0);

  // count > len 自转：转移量钳制为 len
  assert_eq!(list.transfer_to(self_ref, 100, true, false), 4);
  assert_eq!(list.len(), 4);

  // 空列表自转
  let mut empty = ListObject::new();
  let empty_ptr = &mut empty as *mut ListObject;
  // SAFETY: 同上，仅构造自转别名
  let empty_ref = unsafe { &mut *empty_ptr };
  assert_eq!(empty.transfer_to(empty_ref, 5, true, false), 0);

  // 跨列表转移边界：源为空、count == 0、count > len
  let mut src = ListObject::new();
  let mut dst = ListObject::new();
  assert_eq!(src.transfer_to(&mut dst, 5, true, false), 0);

  src.rpush([b"x", b"y"]);
  assert_eq!(src.transfer_to(&mut dst, 0, true, false), 0);
  assert_eq!(src.len(), 2);
  assert_eq!(dst.len(), 0);

  // 转移量大于源长度：全量转移且源彻底释放
  assert_eq!(src.transfer_to(&mut dst, 10, true, false), 2);
  assert!(src.is_empty());
  assert_eq!(src.capacity(), 0);
  assert_eq!(dst.lrange(0, -1), vec![b"x".to_vec(), b"y".to_vec()]);

  info!("transfer_to_self_alias_and_edge_cases 语义通过：自转与跨列表边界");
  OK
}

/// rotate_same_direction_and_empty：同向 LMOVE 为纯 peek 且空列表旋转返回 None（转写新增）
#[test]
fn rotate_same_direction_and_empty() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"1", b"2", b"3"]);

  // 同向 (LEFT, LEFT)：仅窥视头部
  assert_eq!(list.rotate(true, true), Some(b"1".as_slice()));
  assert_eq!(
    list.lrange(0, -1),
    vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]
  );

  // 同向 (RIGHT, RIGHT)：仅窥视尾部
  assert_eq!(list.rotate(false, false), Some(b"3".as_slice()));
  assert_eq!(
    list.lrange(0, -1),
    vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]
  );

  // 空列表旋转返回 None
  let mut empty = ListObject::new();
  assert_eq!(empty.rotate(true, false), None);
  assert_eq!(empty.rotate(false, true), None);
  OK
}

/// transfer_and_drain：批量转移、流式 drain 与物理内存释放（转写新增）
#[test]
fn transfer_and_drain() -> Void {
  let mut src = ListObject::new();
  let mut dst = ListObject::new();

  src.rpush([b"a", b"b", b"c", b"d"]);
  // 头部批量转移 2 个到 dst 尾部
  assert_eq!(src.transfer_to(&mut dst, 2, true, false), 2);
  assert_eq!(src.lrange(0, -1), vec![b"c".to_vec(), b"d".to_vec()]);
  assert_eq!(dst.lrange(0, -1), vec![b"a".to_vec(), b"b".to_vec()]);

  // 流式头部弹出
  assert_eq!(src.drain_left(1).collect::<Vec<_>>(), vec![b"c".to_vec()]);
  assert_eq!(src.len(), 1);

  // 流式尾部弹出
  assert_eq!(src.drain_right(1).collect::<Vec<_>>(), vec![b"d".to_vec()]);
  assert!(src.is_empty());

  // 空列表主动物理清理
  src.clear_physical();
  assert_eq!(src.capacity(), 0);

  info!("transfer_and_drain 语义通过：流式转移弹出与内存释放");
  OK
}
