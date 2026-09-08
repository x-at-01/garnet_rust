//! `ListObject`（Redis List）嵌入式使用演示
//!
//! 分页链式双端队列（`LinkedPage` 串联），头尾操作 O(1)，
//! 支持按索引访问、定点插入、定位、裁剪、跨列表搬运与 Bitcode 快照序列化。

use std::str::from_utf8;

use wedb_list::{InsertPosition, ListObject};

/// 便于展示：字节成员集合转 UTF-8 字符串集合
fn show(vals: &[Vec<u8>]) -> Vec<&str> {
  vals.iter().map(|v| from_utf8(v).unwrap_or("?")).collect()
}

fn main() {
  let mut queue = ListObject::new();

  // 双端推入 (RPUSH / LPUSH)
  queue.rpush(["task_c", "task_d"]);
  queue.lpush(["task_b", "task_a"]);
  println!("LRANGE 全表 = {:?}", show(&queue.lrange(0, -1)));
  println!("LLEN        = {}", queue.len());

  // 按索引访问与改写 (LINDEX / LSET)
  println!(
    "LINDEX 0    = {:?}",
    queue.lindex(0).map(String::from_utf8_lossy).as_deref()
  );
  queue.lset(0, "task_a0").expect("索引 0 存在");

  // 定点插入 (LINSERT BEFORE|AFTER)
  let new_len = queue.linsert(b"task_c", "task_c_plus", InsertPosition::Before);
  println!("LINSERT     = {new_len} (-1 未命中 pivot，否则为插入后长度)");

  // 定位与删除 (LPOS / LREM / LTRIM)
  println!("LPOS task_d = {:?}", queue.lpos(b"task_d", 1, None, 0));
  println!("LREM task_c_plus 删除 {} 个", queue.lrem(0, b"task_c_plus"));
  queue.ltrim(0, 1);
  println!("LTRIM 后    = {:?}", show(&queue.lrange(0, -1)));

  // 头部弹出 (LPOP)
  println!("LPOP        = {:?}", show(&queue.lpop(1)));

  // 跨列表原子搬运 (RPOPLPUSH)：工作队列的消费侧典型用法
  let mut done = ListObject::new();
  if let Some(v) = ListObject::rpoplpush(&mut queue, &mut done) {
    println!("RPOPLPUSH   = {:?}", String::from_utf8_lossy(&v));
  }

  // Bitcode 快照：可直接写入存储层，重启后原样恢复
  let snapshot = done.to_bitcode();
  let restored = ListObject::from_bitcode(&snapshot).expect("快照格式合法");
  println!(
    "快照 {} 字节，回读 = {:?}",
    snapshot.len(),
    show(&restored.lrange(0, -1))
  );
}
