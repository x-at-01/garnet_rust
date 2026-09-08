// 对标 C#: test/standalone/Garnet.test.collections/RespListTests.cs (#region LPOS)
// LPOS 语义：无选项首现查找、RANK/COUNT/MAXLEN 全参数矩阵与空键行为。
// 注：C# 侧 LPOSWithInvalidOpt 校验协议层非法参数报错（rank=0、负 COUNT、负 MAXLEN、
// 未知选项），对象层以强类型参数承接，非法值不可表达，仅保留 rank=0 退化为空结果的行为。

use aok::{OK, Void};
use log::info;
use wedb_list::ListObject;

/// LPOSWithoutOpt：默认 rank=1 返回首个匹配下标，未命中返回空
#[test]
fn lpos_without_options() -> Void {
  let push = |items: &[&[u8]]| {
    let mut list = ListObject::new();
    list.rpush(items.iter().copied());
    list
  };

  // ("a,c,b,c,d", "a") -> 下标 1 之前的 0
  let list = push(&[b"a", b"c", b"b", b"c", b"d"]);
  assert_eq!(list.lpos(b"a", 1, None, 0), vec![0]);

  // ("a,c,b,c,adc", "adc") -> 下标 4
  let list = push(&[b"a", b"c", b"b", b"c", b"adc"]);
  assert_eq!(list.lpos(b"adc", 1, None, 0), vec![4]);

  // ("a,c,b,c,d", "c") -> 首个 "c" 在下标 1
  let list = push(&[b"a", b"c", b"b", b"c", b"d"]);
  assert_eq!(list.lpos(b"c", 1, None, 0), vec![1]);

  // ("av,123,bs,c,d", "e") -> 未命中
  let list = push(&[b"av", b"123", b"bs", b"c", b"d"]);
  assert_eq!(list.lpos(b"e", 1, None, 0), Vec::<usize>::new());

  OK
}

/// LPOSWithInvalidKey：空列表上 LPOS 单值与 COUNT 形态均返回空
#[test]
fn lpos_with_invalid_key() -> Void {
  let list = ListObject::new();

  // 无 COUNT 形态：返回 nil -> 对象层为空
  assert!(list.lpos(b"nx", 1, None, 0).is_empty());
  // COUNT 3 形态：返回空数组
  assert!(list.lpos(b"nx", 1, Some(3), 0).is_empty());

  // 推入一个不含目标元素的列表后同样为空
  let mut list = ListObject::new();
  list.lpush([b"e"]);
  assert!(list.lpos(b"nx", 1, None, 0).is_empty());
  assert!(list.lpos(b"nx", 1, Some(3), 0).is_empty());
  OK
}

/// LPOSWithOpt：RANK/COUNT/MAXLEN 25 组全参数矩阵（含缓冲区拷贝回归长列表）
#[test]
fn lpos_with_options() -> Void {
  // 基准列表 [a, c, b, c, d]，"c" 首现于 1、次现于 3
  let mut list = ListObject::new();
  list.rpush([b"a", b"c", b"b", b"c", b"d"]);
  let none: Vec<usize> = Vec::new();

  // RANK 正向
  assert_eq!(list.lpos(b"c", 1, None, 0), vec![1]);
  assert_eq!(list.lpos(b"c", 2, None, 0), vec![3]);
  assert_eq!(list.lpos(b"c", 3, None, 0), none);
  // RANK 逆向
  assert_eq!(list.lpos(b"c", -1, None, 0), vec![3]);
  assert_eq!(list.lpos(b"c", -2, None, 0), vec![1]);
  assert_eq!(list.lpos(b"c", -3, None, 0), none);
  // 第二次出现不存在的元素
  assert_eq!(list.lpos(b"a", 2, None, 0), none);

  // COUNT
  assert_eq!(list.lpos(b"b", 1, Some(2), 0), vec![2]);
  assert_eq!(list.lpos(b"c", 1, Some(1), 0), vec![1]);
  assert_eq!(list.lpos(b"c", 1, Some(2), 0), vec![1, 3]);
  assert_eq!(list.lpos(b"c", 1, Some(3), 0), vec![1, 3]);
  assert_eq!(list.lpos(b"c", 1, Some(0), 0), vec![1, 3]);

  // MAXLEN
  assert_eq!(list.lpos(b"c", 1, None, 0), vec![1]);
  assert_eq!(list.lpos(b"c", 1, None, 1), none);
  assert_eq!(list.lpos(b"c", 1, None, 2), vec![1]);

  // RANK + MAXLEN 组合
  assert_eq!(list.lpos(b"c", -1, None, 1), none);
  assert_eq!(list.lpos(b"c", -1, None, 2), vec![3]);
  assert_eq!(list.lpos(b"c", -2, None, 2), none);
  assert_eq!(list.lpos(b"c", 1, None, 1), none);
  assert_eq!(list.lpos(b"c", 1, None, 2), vec![1]);
  assert_eq!(list.lpos(b"c", 2, None, 2), none);

  // RANK + MAXLEN + COUNT 组合
  assert_eq!(list.lpos(b"c", -1, Some(0), 0), vec![3, 1]);
  assert_eq!(list.lpos(b"c", -1, Some(1), 0), vec![3]);
  assert_eq!(list.lpos(b"c", 1, Some(0), 0), vec![1, 3]);
  assert_eq!(list.lpos(b"c", 1, Some(1), 0), vec![1]);

  // 25 元素长列表 COUNT 0（对应 C# 缓冲区拷贝回归用例）
  let mut long = ListObject::new();
  long.rpush([
    b"z", b"b", b"z", b"d", b"e", b"a", b"b", b"c", b"d", b"e", b"a", b"b", b"c", b"d", b"e", b"a",
    b"b", b"c", b"d", b"e", b"a", b"b", b"c", b"z", b"z",
  ]);
  assert_eq!(long.lpos(b"z", 1, Some(0), 0), vec![0, 2, 23, 24]);

  // rank = 0 在协议层为非法参数，对象层退化为空结果
  assert_eq!(list.lpos(b"c", 0, None, 0), none);

  info!("LPOSWithOpt 语义通过：RANK/COUNT/MAXLEN 全矩阵");
  OK
}

/// LPOSWithListPosition：客户端 API 形态的 rank/count/maxlen 组合矩阵
#[test]
fn lpos_with_list_position() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"a", b"c", b"b", b"c", b"d"]);

  // (find "c", count=None, rank=1, maxlen=0) -> 1
  assert_eq!(list.lpos(b"c", 1, None, 0), vec![1]);
  // (find "c", count=None, rank=-1, maxlen=0) -> 3
  assert_eq!(list.lpos(b"c", -1, None, 0), vec![3]);
  // (find "c", count=2, rank=1, maxlen=0) -> [1, 3]
  assert_eq!(list.lpos(b"c", 1, Some(2), 0), vec![1, 3]);
  // (find "c", count=2, rank=-1, maxlen=0) -> [3, 1]
  assert_eq!(list.lpos(b"c", -1, Some(2), 0), vec![3, 1]);
  // (find "c", count=2, rank=1, maxlen=3) -> 前 3 个元素中仅 1 处匹配
  assert_eq!(list.lpos(b"c", 1, Some(2), 3), vec![1]);

  info!("LPOSWithListPosition 语义通过：客户端形态参数组合");
  OK
}
