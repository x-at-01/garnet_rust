// 对标 C#: test/standalone/Garnet.test.collections/RespListGarnetClientTests.cs
// GarnetClient 层用例矩阵：批量/逐元素推入、LRANGE 区间矩阵与 LLEN。
// 说明：C# 侧 WithCallback / WithAsync / InBulk 三种传输形态在对象层语义一致，
// 合并为同一矩阵断言；键在用例间累积复用的行为按原矩阵 1:1 复刻。

use aok::{OK, Void};
use log::info;
use wedb_list::ListObject;

/// AddElementsToTheListHead(InBulk/WithCallback/WithAsync)：LeftPushTestCases 矩阵
#[test]
fn add_elements_to_the_list_head() -> Void {
  let mut list1 = ListObject::new();
  let mut list2 = ListObject::new();
  let mut list3 = ListObject::new();

  // list1 <- [foo] -> [foo]
  assert_eq!(list1.lpush(["foo"]), 1);
  assert_eq!(list1.lrange(0, -1), vec![b"foo".to_vec()]);

  // list2 <- [foo, baz] -> [baz, foo]
  assert_eq!(list2.lpush(["foo", "baz"]), 2);
  assert_eq!(list2.lrange(0, -1), vec![b"baz".to_vec(), b"foo".to_vec()]);

  // list1 <- [bar, baz] -> [baz, bar, foo]
  assert_eq!(list1.lpush(["bar", "baz"]), 3);
  assert_eq!(
    list1.lrange(0, -1),
    vec![b"baz".to_vec(), b"bar".to_vec(), b"foo".to_vec()]
  );

  // list3 <- [foo, bar, baz] -> [baz, bar, foo]
  assert_eq!(list3.lpush(["foo", "bar", "baz"]), 3);
  assert_eq!(
    list3.lrange(0, -1),
    vec![b"baz".to_vec(), b"bar".to_vec(), b"foo".to_vec()]
  );

  // list2 <- [foo, bar, baz] -> [baz, bar, foo, baz, foo]
  assert_eq!(list2.lpush(["foo", "bar", "baz"]), 5);
  assert_eq!(
    list2.lrange(0, -1),
    vec![
      b"baz".to_vec(),
      b"bar".to_vec(),
      b"foo".to_vec(),
      b"baz".to_vec(),
      b"foo".to_vec()
    ]
  );

  info!("LeftPushTestCases 矩阵语义通过：批量左推累积顺序与 C# 一致");
  OK
}

/// AddElementsToListTail(InBulk/WithCallback/WithAsync)：RightPushTestCases 矩阵
#[test]
fn add_elements_to_the_list_tail() -> Void {
  let mut list1 = ListObject::new();
  let mut list2 = ListObject::new();
  let mut list3 = ListObject::new();

  // list1 <- [foo] -> [foo]
  assert_eq!(list1.rpush(["foo"]), 1);
  assert_eq!(list1.lrange(0, -1), vec![b"foo".to_vec()]);

  // list2 <- [foo, baz] -> [foo, baz]
  assert_eq!(list2.rpush(["foo", "baz"]), 2);
  assert_eq!(list2.lrange(0, -1), vec![b"foo".to_vec(), b"baz".to_vec()]);

  // list1 <- [bar, baz] -> [foo, bar, baz]
  assert_eq!(list1.rpush(["bar", "baz"]), 3);
  assert_eq!(
    list1.lrange(0, -1),
    vec![b"foo".to_vec(), b"bar".to_vec(), b"baz".to_vec()]
  );

  // list3 <- [foo, bar, baz] -> [foo, bar, baz]
  assert_eq!(list3.rpush(["foo", "bar", "baz"]), 3);
  assert_eq!(
    list3.lrange(0, -1),
    vec![b"foo".to_vec(), b"bar".to_vec(), b"baz".to_vec()]
  );

  // list2 <- [foo, bar, baz] -> [foo, baz, foo, bar, baz]
  assert_eq!(list2.rpush(["foo", "bar", "baz"]), 5);
  assert_eq!(
    list2.lrange(0, -1),
    vec![
      b"foo".to_vec(),
      b"baz".to_vec(),
      b"foo".to_vec(),
      b"bar".to_vec(),
      b"baz".to_vec()
    ]
  );

  info!("RightPushTestCases 矩阵语义通过：批量右推累积顺序与 C# 一致");
  OK
}

/// GetListElements：ListRangeTestCases 区间矩阵（每个用例基于全新 3 元素列表）
#[test]
fn get_list_elements() -> Void {
  let expect = vec![b"foo".to_vec(), b"bar".to_vec(), b"baz".to_vec()];
  let cases: [(isize, isize, Vec<Vec<u8>>); 6] = [
    (0, -1, expect.clone()),
    (0, 0, vec![b"foo".to_vec()]),
    (1, 2, vec![b"bar".to_vec(), b"baz".to_vec()]),
    (-3, 1, vec![b"foo".to_vec(), b"bar".to_vec()]),
    (-3, 2, expect.clone()),
    (-100, 100, expect),
  ];

  for (start, stop, expected) in cases {
    // C# 每个用例先 KeyDelete 再 RPUSH，等价于全新列表
    let mut list = ListObject::new();
    list.rpush([b"foo", b"bar", b"baz"]);
    assert_eq!(list.lrange(start, stop), expected, "LRANGE {start} {stop}");
  }

  info!("ListRangeTestCases 区间矩阵语义通过");
  OK
}

/// GetListLength：右推 3 元素后 LLEN 为 3
#[test]
fn get_list_length() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"foo", b"bar", b"baz"]);
  assert_eq!(list.len(), 3);
  OK
}
