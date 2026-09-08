//! `whasher` 硬件加速哈希演示
//!
//! gxhash（AES 指令）后端：单次键哈希 ~0.31ns、流式校验和严格分块一致，
//! 并重导出抗哈希洪泛的 `HashMap` / `HashSet` 标准容器。

use whasher::{
  HashMap, HashMapExt, HashSet, HashSetExt, StreamHasher, compute_checksum, fast_hash,
};

fn main() {
  // 单次键哈希：固定种子，跨进程确定性，派生索引可安全落盘
  let key_hash = fast_hash(b"session:42");
  println!("fast_hash(\"session:42\") = {key_hash:016x}");

  // 流式校验和：任意分块追加与整块单次计算恒等 (Chunk-Invariance)
  let data: Vec<u8> = (0..1000u32).map(|i| i as u8).collect();
  let whole = compute_checksum(&data);
  let mut streamed = StreamHasher::new();
  streamed.write(&data[..333]);
  streamed.write(&data[333..]);
  let chunked = streamed.finish();
  println!("整块校验和   = {whole:016x}");
  println!("分块流校验和 = {chunked:016x}（{}）", chunked == whole);

  // gxhash 后端标准容器：默认实例随机种子，抗哈希洪泛
  let mut scores = HashMap::new();
  scores.insert("alice", 92.5_f64);
  let mut seen = HashSet::new();
  seen.insert(key_hash);
  println!(
    "HashMap 命中 = {:?}，HashSet 含 1 项 = {}",
    scores.get("alice"),
    seen.len() == 1
  );
}
