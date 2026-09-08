//! `SkipList`（跳表）独立使用演示
//!
//! wedb_zset 的底层有序结构可单独取用：O(log N) 插入/删除/排名，
//! 同分按成员字典序稳定排列，支持分数区间与弹出最值。

use wedb_zset::{ScoreRange, SkipList};

/// 便于展示：(member, score) 集合转 UTF-8 可读形式
fn show(pairs: Vec<(Vec<u8>, f64)>) -> Vec<(String, f64)> {
  pairs
    .into_iter()
    .map(|(m, s)| (String::from_utf8_lossy(&m).into_owned(), s))
    .collect()
}

fn main() {
  let mut sl = SkipList::new();

  // 插入 (score, member)
  for (score, member) in [
    (1.0, "redis"),
    (3.0, "mysql"),
    (2.0, "kafka"),
    (2.0, "etcd"),
  ] {
    sl.insert(score, member.as_bytes().to_vec());
  }
  println!("ZCARD       = {}", sl.len());
  let ordered: Vec<(String, f64)> = sl
    .iter()
    .map(|(m, s)| (String::from_utf8_lossy(m).into_owned(), s))
    .collect();
  println!("顺序迭代    = {ordered:?}");

  // 排名 (0-based，ZRANK 语义)
  println!("RANK (2.0, etcd) = {:?}", sl.get_rank(2.0, b"etcd"));
  println!(
    "BY RANK [0, 1]   = {:?}",
    show(sl.range_by_rank(0, 1, false))
  );

  // 分数区间查询与计数
  let range = ScoreRange::new(2.0, true, f64::INFINITY, true);
  println!(
    "RANGEBYSCORE [2, +inf) = {:?}",
    show(sl.range_by_score(range, false, 0, 10))
  );
  println!("COUNT [2, +inf) = {}", sl.count_by_score(range));

  // O(log N) 弹出最值
  println!(
    "POP FIRST   = {:?}",
    sl.pop_first()
      .map(|(m, s)| (String::from_utf8_lossy(&m).into_owned(), s))
  );
  println!(
    "POP LAST    = {:?}",
    sl.pop_last()
      .map(|(m, s)| (String::from_utf8_lossy(&m).into_owned(), s))
  );
}
