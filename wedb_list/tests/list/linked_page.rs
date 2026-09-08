// LinkedPage 分页节点回归（转写新增结构，无 C# 一一对应的测试文件；
// 对标 C# 内部 LinkedListNode 分页 redesign，见 libs/server/Objects/List/ListObjectImpl.cs）
// 覆盖：容量契约、头尾推弹、局部索引、双向迭代、分裂、合并与 bitcode 编解码。

use aok::{OK, Void};
use log::info;
use wedb_list::LinkedPage;

/// LinkedPage 基础契约：推入弹出、局部索引、迭代器、分裂合并与编解码
#[test]
fn linked_page_basic_contracts() -> Void {
  let mut page: LinkedPage<i32, 4> = LinkedPage::new();
  assert_eq!(page.capacity(), 4);
  assert_eq!(page.len(), 0);
  assert!(page.is_empty());
  assert!(!page.is_full());
  assert_eq!(page.remaining_capacity(), 4);

  // 尾部推入 10, 20，头部推入 5, 1 -> [1, 5, 10, 20]
  assert_eq!(page.push_back(10), Ok(()));
  assert_eq!(page.push_back(20), Ok(()));
  assert_eq!(page.push_front(5), Ok(()));
  assert_eq!(page.push_front(1), Ok(()));
  assert_eq!(page.len(), 4);
  assert!(page.is_full());
  assert_eq!(page.remaining_capacity(), 0);

  // 满页推入失败并归还原值
  assert_eq!(page.push_front(99), Err(99));
  assert_eq!(page.push_back(100), Err(100));

  // 局部索引访问与修改
  assert_eq!(page.get(0), Some(&1));
  assert_eq!(page.get(3), Some(&20));
  assert_eq!(page.get(4), None);
  if let Some(val) = page.get_mut(1) {
    *val = 6;
  }
  assert_eq!(page.get(1), Some(&6));

  // 双向迭代器与 ExactSizeIterator
  assert_eq!(page.iter().copied().collect::<Vec<_>>(), vec![1, 6, 10, 20]);
  assert_eq!(
    page.iter().rev().copied().collect::<Vec<_>>(),
    vec![20, 10, 6, 1]
  );

  // IntoIterator 消费
  let into_iter_res: Vec<_> = page.clone().into_iter().collect();
  assert_eq!(into_iter_res, vec![1, 6, 10, 20]);

  // 分裂：[1, 6, 10, 20] -> 左半 [1, 6]，右半 [10, 20]
  let right_page = page.split();
  assert_eq!(page.len(), 2);
  assert_eq!(right_page.len(), 2);
  assert_eq!(page.iter().copied().collect::<Vec<_>>(), vec![1, 6]);
  assert_eq!(right_page.iter().copied().collect::<Vec<_>>(), vec![10, 20]);

  // 合并：2 + 2 <= 4 成功还原
  assert_eq!(page.try_merge(right_page), Ok(()));
  assert_eq!(page.iter().copied().collect::<Vec<_>>(), vec![1, 6, 10, 20]);

  // 合并超限：4 + 1 > 4 失败并原样归还对方
  let mut other_page: LinkedPage<i32, 4> = LinkedPage::new();
  other_page.push_back(99).unwrap();
  let merge_res = page.try_merge(other_page);
  assert!(merge_res.is_err());
  let returned_other = merge_res.unwrap_err();
  assert_eq!(returned_other.len(), 1);

  // 头尾弹出
  assert_eq!(page.pop_front(), Some(1));
  assert_eq!(page.pop_back(), Some(20));
  assert_eq!(page.len(), 2);

  // bitcode 编解码
  let encoded = bitcode::encode(&page);
  let decoded: LinkedPage<i32, 4> = bitcode::decode(&encoded).unwrap();
  assert_eq!(decoded, page);

  // 清空后弹出为 None
  page.clear();
  assert!(page.is_empty());
  assert_eq!(page.pop_front(), None);
  assert_eq!(page.pop_back(), None);

  info!("linked_page_basic_contracts 语义通过");
  OK
}

/// LinkedPage 极端契约：空页分裂、单元素分裂、空页合并与超限合并双方数据完整
#[test]
fn linked_page_extreme_contracts() -> Void {
  // 空分页分裂：前后分页均为空
  let mut empty_page: LinkedPage<u8, 4> = LinkedPage::new();
  let right = empty_page.split();
  assert!(empty_page.is_empty());
  assert!(right.is_empty());

  // 单元素分页分裂：左半 0 元素，右半 1 元素
  let mut single_page: LinkedPage<u8, 4> = LinkedPage::new();
  single_page.push_back(42).unwrap();
  let right_single = single_page.split();
  assert_eq!(single_page.len(), 0);
  assert_eq!(right_single.len(), 1);
  assert_eq!(right_single.get(0), Some(&42));

  // 满分页与空分页合并：4 + 0 <= 4 成功
  let mut full_page: LinkedPage<u8, 4> = LinkedPage::new();
  for i in 0..4 {
    full_page.push_back(i).unwrap();
  }
  assert_eq!(full_page.try_merge(empty_page), Ok(()));
  assert_eq!(full_page.len(), 4);

  // 超容量合并失败且双方数据完整未丢失
  let mut p1: LinkedPage<u8, 4> = LinkedPage::new();
  p1.push_back(1).unwrap();
  p1.push_back(2).unwrap();
  p1.push_back(3).unwrap();

  let mut p2: LinkedPage<u8, 4> = LinkedPage::new();
  p2.push_back(10).unwrap();
  p2.push_back(20).unwrap();

  let merge_res = p1.try_merge(p2);
  assert!(merge_res.is_err());
  let restored_p2 = merge_res.unwrap_err();
  assert_eq!(p1.len(), 3);
  assert_eq!(restored_p2.len(), 2);
  assert_eq!(restored_p2.get(0), Some(&10));
  assert_eq!(restored_p2.get(1), Some(&20));

  info!("linked_page_extreme_contracts 语义通过");
  OK
}
