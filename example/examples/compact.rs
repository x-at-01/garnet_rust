//! 紧凑集合形态演示：`CompactSet` 与 `CompactZSet`
//!
//! 单块连续内存的排序编码，零指针、CPU 缓存友好，成员数少时
//! 比标准哈希形态省一个数量级内存；`into_vec` 产出的字节流可直接落盘，
//! 回读后支持就地二分查找、插入与删除，无需重建索引。

use std::str::from_utf8;

use wedb_set::CompactSet;
use wedb_zset::{CompactZSet, CompactZSetExt, ZAddOpt};

fn main() {
  // ---- CompactSet：排序去重紧凑集合 ----
  let mut tags = CompactSet::new();
  for tag in ["db", "cache", "mq", "cache"] {
    tags.insert(tag.as_bytes()).expect("成员长度合法");
  }
  println!("---- CompactSet ----");
  println!("CARD = {}（去重后）", tags.len());
  let members: Vec<&str> = tags
    .iter_members()
    .map(|m| from_utf8(m).unwrap_or("?"))
    .collect();
  println!("有序成员 = {members:?}");
  println!("CONTAINS cache = {}", tags.contains(b"cache"));
  println!("REMOVE mq     = {}", tags.remove(b"mq").expect("编码合法"));

  // 编码落盘 → 回读（零重建索引）
  let raw = tags.into_vec();
  println!("紧凑编码 = {} 字节", raw.len());
  let restored = CompactSet::from_vec(raw).expect("编码合法");
  let back: Vec<&str> = restored
    .iter_members()
    .map(|m| from_utf8(m).unwrap_or("?"))
    .collect();
  println!("落盘回读 = {back:?}");
  println!(
    "回读 CONTAINS db = {}（就地二分，无需重建索引）",
    restored.contains(b"db")
  );

  // ---- CompactZSet：member+score 交错紧凑编码 ----
  let mut prices = CompactZSet::new();
  for (member, score) in [("iphone", 5999.0), ("pixel", 3999.0), ("mate", 4999.0)] {
    prices
      .zadd(score, member.as_bytes(), ZAddOpt::default())
      .expect("分数合法");
  }
  println!("---- CompactZSet ----");
  println!(
    "ZRANGE 升序 = {:?}",
    prices
      .zrange(0, -1, false)
      .into_iter()
      .map(|(m, s)| (String::from_utf8_lossy(&m).into_owned(), s))
      .collect::<Vec<_>>()
  );
  println!("ZRANK pixel = {:?}", prices.zrank(b"pixel"));
  println!("ZSCORE mate = {:?}", prices.zscore(b"mate"));

  // 与标准跳表版 SortedSetObject 无损互转（小集合紧凑存储、大集合跳表加速）
  let standard = prices.to_sorted_set().expect("编码合法");
  let back_z = CompactZSet::from_sorted_set(&standard).expect("编码合法");
  println!("互转后成员数 = {}", back_z.len());
}
