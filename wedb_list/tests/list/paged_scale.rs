// 分页结构大规模回归（转写新增，无 C# 一一对应的测试文件；
// 对标 C# libs/server/Objects/List/ListObjectImpl.cs 的 ListObject 行为语义）
// 覆盖：跨页索引与区间、满页分裂插入、跨页 LTRIM/LREM、稀疏页回收合并、
// 大规模旋转与批量转移、迭代器与内存生命周期。

use std::mem::size_of;

use aok::{OK, Void};
use log::info;
use wedb_list::{InsertPosition, ListObject};

/// 构造 [lo, hi) 的 e{i:03} 元素列表
fn tags_range(lo: usize, hi: usize) -> Vec<Vec<u8>> {
  (lo..hi).map(|i| format!("e{i:03}").into_bytes()).collect()
}

/// 跨页索引与区间：页边界 (63/64, 127/128, ...) 处 lindex/lrange/iter 一致性
#[test]
fn page_boundary_indexing_and_range() -> Void {
  let expect = tags_range(0, 300);
  let mut list = ListObject::new();
  list.rpush(expect.iter().map(Vec::as_slice));

  // 跨页索引（页容量 64：边界 63/64, 127/128, 191/192, 255/256）
  for i in [0, 1, 63, 64, 65, 127, 128, 129, 191, 192, 255, 256, 299] {
    assert_eq!(
      list.lindex(i as isize),
      Some(expect[i].as_slice()),
      "lindex({i})"
    );
  }
  assert_eq!(list.lindex(300), None);
  assert_eq!(list.lindex(-300), Some(expect[0].as_slice()));
  assert_eq!(list.lindex(-301), None);

  // 全量与分段 range 一致性（闭区间）
  assert_eq!(list.lrange(0, -1), expect);
  for (s, e) in [(0usize, 63usize), (64, 128), (129, 200), (250, 299)] {
    assert_eq!(
      list.lrange(s as isize, e as isize),
      expect[s..=e].to_vec(),
      "lrange({s},{e})"
    );
  }

  // iter / iter_range / 双向迭代
  assert_eq!(
    list.iter().collect::<Vec<_>>(),
    expect.iter().map(Vec::as_slice).collect::<Vec<_>>()
  );
  assert_eq!(
    list.iter_range(64, 65).collect::<Vec<_>>(),
    vec![expect[64].as_slice(), expect[65].as_slice()]
  );
  assert_eq!(
    list.iter().rev().take(3).collect::<Vec<_>>(),
    expect[297..]
      .iter()
      .rev()
      .map(Vec::as_slice)
      .collect::<Vec<_>>()
  );

  // 头部跨页推入：10 个元素整体前插（逆序推入保证最终顺序）
  let heads = tags_range(300, 310);
  list.lpush(heads.iter().rev().map(Vec::as_slice));
  let mut new_expect = heads;
  new_expect.extend(expect);
  assert_eq!(list.lrange(0, -1), new_expect);
  assert_eq!(list.len(), 310);

  info!("page_boundary_indexing_and_range 语义通过：跨页索引与区间一致");
  OK
}

/// 满页插入分裂：恰好填满 1/2 页后中段与端点插入触发页分裂
#[test]
fn page_split_on_full_insert() -> Void {
  // 恰好填满 1 页 (64) 后中段插入
  let mut list = ListObject::new();
  let mut model = tags_range(0, 64);
  list.rpush(model.iter().map(Vec::as_slice));

  assert_eq!(list.linsert(b"e032", b"NEW", InsertPosition::Before), 65);
  model.insert(32, b"NEW".to_vec());
  assert_eq!(list.lrange(0, -1), model);

  // 2 满页 (128) 后在第二页中段之后插入
  let mut list2 = ListObject::new();
  let mut model2 = tags_range(0, 128);
  list2.rpush(model2.iter().map(Vec::as_slice));

  assert_eq!(list2.linsert(b"e100", b"NEW", InsertPosition::After), 129);
  model2.insert(101, b"NEW".to_vec());
  assert_eq!(list2.lrange(0, -1), model2);

  // 头页满时在头部插入 -> 开新头页
  assert_eq!(list2.linsert(b"e000", b"HEAD", InsertPosition::Before), 130);
  model2.insert(0, b"HEAD".to_vec());
  assert_eq!(list2.lrange(0, -1), model2);

  // 尾页满时在尾部之后插入 -> 开新尾页
  assert_eq!(list2.linsert(b"e127", b"TAIL", InsertPosition::After), 131);
  model2.push(b"TAIL".to_vec());
  assert_eq!(list2.lrange(0, -1), model2);

  info!("page_split_on_full_insert 语义通过：满页分裂与端点开页");
  OK
}

/// 跨页 LTRIM 与 LREM：页对齐裁剪、正/负向限额删除与流式 drain
#[test]
fn page_trim_and_rem_cross_pages() -> Void {
  let expect = tags_range(0, 300);

  // LTRIM 中段跨页：保留 [70, 129]
  let mut t = ListObject::new();
  t.rpush(expect.iter().map(Vec::as_slice));
  t.ltrim(70, 129);
  assert_eq!(t.lrange(0, -1), expect[70..=129].to_vec());
  assert_eq!(t.len(), 60);

  // LTRIM 边界恰好对齐页边界：保留 [64, 191]（整两页）
  let mut t2 = ListObject::new();
  t2.rpush(expect.iter().map(Vec::as_slice));
  t2.ltrim(64, 191);
  assert_eq!(t2.lrange(0, -1), expect[64..=191].to_vec());
  assert_eq!(t2.capacity(), 128);

  // LREM 正向跨页限额删除
  let mut r = ListObject::new();
  let mut r_model: Vec<Vec<u8>> = Vec::new();
  for i in 0..200 {
    let v = if i % 3 == 0 {
      b"x".to_vec()
    } else {
      format!("e{i:03}").into_bytes()
    };
    r_model.push(v.clone());
    r.rpush([v]);
  }
  assert_eq!(r.lrem(5, b"x"), 5);
  let mut removed = 0;
  r_model.retain(|v| {
    if removed < 5 && v.as_slice() == b"x" {
      removed += 1;
      false
    } else {
      true
    }
  });
  assert_eq!(r.lrange(0, -1), r_model);
  assert_eq!(r.len(), r_model.len());

  // LREM 反向跨页限额删除（删除全局最后 5 个匹配）
  assert_eq!(r.lrem(-5, b"x"), 5);
  let mut tail_hits = 0;
  for i in (0..r_model.len()).rev() {
    if tail_hits < 5 && r_model[i].as_slice() == b"x" {
      r_model.remove(i);
      tail_hits += 1;
    }
  }
  assert_eq!(tail_hits, 5);
  assert_eq!(r.lrange(0, -1), r_model);
  assert_eq!(r.len(), r_model.len());

  // drain_left / drain_right 跨页流式弹出
  let mut d = ListObject::new();
  d.rpush(expect.iter().map(Vec::as_slice));
  assert_eq!(
    d.drain_left(100).collect::<Vec<_>>(),
    expect[..100].to_vec()
  );
  assert_eq!(
    d.drain_right(70).collect::<Vec<_>>(),
    expect[230..300].to_vec()
  );
  assert_eq!(d.len(), 130);
  assert_eq!(d.drain_left(usize::MAX).count(), 130);
  assert!(d.is_empty());
  assert_eq!(d.capacity(), 0);

  info!("page_trim_and_rem_cross_pages 语义通过：跨页裁剪删除与流式弹出");
  OK
}

/// 稀疏页回收合并：大删除后相邻稀疏页两两合并，全删后页目录彻底释放
#[test]
fn paged_sparse_reclaim_merge() -> Void {
  // 10 满页，每页 48 个 "x" + 16 个 "y"
  let mut list = ListObject::new();
  for i in 0..640 {
    let v = if i % 64 < 48 { b"x" } else { b"y" };
    list.rpush([v]);
  }
  assert_eq!(list.len(), 640);
  assert_eq!(list.capacity(), 640);
  let before = list.byte_size();

  // 全量删除 "x" 后每页剩 16 个，相邻 16+16=32 <= 半容量 -> 两两合并为 5 页
  assert_eq!(list.lrem(0, b"x"), 480);
  assert_eq!(list.len(), 160);
  assert_eq!(list.capacity(), 320);
  assert!(list.byte_size() < before);

  // 内容校验：剩余全为 "y"
  assert!(list.iter().all(|v| v == b"y"));

  // 全删后页目录彻底释放
  assert_eq!(list.lrem(0, b"y"), 160);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);
  assert_eq!(list.byte_size(), size_of::<ListObject>());

  info!("paged_sparse_reclaim_merge 语义通过：稀疏页合并与彻底释放");
  OK
}

/// 大规模旋转与批量转移：rotate 70 次、跨列表批量转移与逐次 LMOVE 等价、自转 100 个
#[test]
fn paged_rotate_transfer_large() -> Void {
  let expect = tags_range(0, 200);
  let mut model = expect.clone();

  // 自旋 rotate (LEFT, RIGHT)：头出尾入
  let mut list = ListObject::new();
  list.rpush(expect.iter().map(Vec::as_slice));
  for i in 0..70 {
    let head = model.remove(0);
    model.push(head.clone());
    assert_eq!(
      list.rotate(true, false),
      Some(head.as_slice()),
      "rotate {i}"
    );
    if i % 10 == 0 {
      assert_eq!(list.lrange(0, -1), model);
    }
  }

  // 跨列表批量转移与逐次 LMOVE 等价（跨页规模）
  let mut s1 = ListObject::new();
  s1.rpush(expect.iter().map(Vec::as_slice));
  let mut s2 = ListObject::new();
  s2.rpush(expect.iter().map(Vec::as_slice));
  let mut d1 = ListObject::new();
  let mut d2 = ListObject::new();

  assert_eq!(s1.transfer_to(&mut d1, 95, false, true), 95);
  for _ in 0..95 {
    ListObject::lmove(&mut s2, &mut d2, false, true);
  }
  assert_eq!(s1, s2);
  assert_eq!(d1, d2);
  assert_eq!(d1.len(), 95);
  assert_eq!(s1.len(), 105);

  // 自转批量：头 100 个整体移到尾部
  let mut l = ListObject::new();
  l.rpush(expect.iter().map(Vec::as_slice));
  let l_ptr = &mut l as *mut ListObject;
  // SAFETY: 别名引用仅用于命中 ptr::eq 自旋分支，调用期间无并发访问
  let alias = unsafe { &mut *l_ptr };
  assert_eq!(l.transfer_to(alias, 100, true, false), 100);
  assert_eq!(
    l.lrange(0, -1),
    expect[100..]
      .iter()
      .chain(expect[..100].iter())
      .cloned()
      .collect::<Vec<_>>()
  );

  info!("paged_rotate_transfer_large 语义通过：大规模旋转与批量转移");
  OK
}

/// 迭代器与内存生命周期：iter/iter_range、byte_size、shrink 与 clear
#[test]
fn iter_and_memory_lifecycle() -> Void {
  let mut list = ListObject::new();
  list.rpush([b"x", b"y", b"z"]);

  // 零拷贝 iter
  assert_eq!(
    list.iter().collect::<Vec<_>>(),
    vec![b"x".as_slice(), b"y".as_slice(), b"z".as_slice()]
  );

  // 零拷贝 iter_range（闭区间 [1, 2]）
  assert_eq!(
    list.iter_range(1, 2).collect::<Vec<_>>(),
    vec![b"y".as_slice(), b"z".as_slice()]
  );

  // byte_size 估算大于结构体本体
  assert!(list.byte_size() > size_of::<ListObject>());

  // 压缩与彻底清空
  list.shrink_to_fit();
  list.clear();
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);
  assert_eq!(list.byte_size(), size_of::<ListObject>());
  OK
}

/// 各弹出与清理路径的物理内存彻底释放（对标 C# 空键移除后 MEMORY USAGE 为 nil）
#[test]
fn clear_physical_and_memory_lifecycle() -> Void {
  // 1. lpop_one
  let mut l1 = ListObject::new();
  l1.rpush([b"val"]);
  assert!(l1.capacity() > 0);
  assert_eq!(l1.lpop_one(), Some(b"val".to_vec()));
  assert_eq!(l1.capacity(), 0);

  // 2. rpop_one
  let mut l2 = ListObject::new();
  l2.rpush([b"val"]);
  assert_eq!(l2.rpop_one(), Some(b"val".to_vec()));
  assert_eq!(l2.capacity(), 0);

  // 3. lpop(k)
  let mut l3 = ListObject::new();
  l3.rpush([b"1", b"2"]);
  assert_eq!(l3.lpop(5).len(), 2);
  assert_eq!(l3.capacity(), 0);

  // 4. rpop(k)
  let mut l4 = ListObject::new();
  l4.rpush([b"1", b"2"]);
  assert_eq!(l4.rpop(5).len(), 2);
  assert_eq!(l4.capacity(), 0);

  // 5. lrem 全量
  let mut l5 = ListObject::new();
  l5.rpush([b"same", b"same"]);
  assert_eq!(l5.lrem(0, b"same"), 2);
  assert_eq!(l5.capacity(), 0);

  // 6. ltrim (start > stop 清空)
  let mut l6 = ListObject::new();
  l6.rpush([b"a", b"b"]);
  l6.ltrim(2, 1);
  assert_eq!(l6.capacity(), 0);

  // 7. byte_size 精确性：空列表尺寸必须等于结构体本体尺寸
  let empty = ListObject::new();
  assert_eq!(empty.byte_size(), size_of::<ListObject>());

  info!("clear_physical_and_memory_lifecycle 语义通过：全路径内存彻底释放");
  OK
}
