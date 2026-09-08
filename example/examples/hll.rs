//! `HyperLogLog` 基数估算演示
//!
//! 14-bit 寄存器，标准误约 0.81%：新建为 Sparse 编码（初始 146 字节），
//! 元素增多自动升级 Dense（全长 12304 字节）。字节布局对齐 Redis/Garnet
//! HYLL 规范，可直接落盘与跨实例合并；寄存器索引由 gxhash 计算，
//! 同一集合的字节内容与 Redis 不一致，估算值不可跨哈希复现。

use wedb_hll::HyperLogLog;

fn main() {
  // 单实例估算 (PFADD / PFCOUNT)
  let mut visits = HyperLogLog::new();
  for i in 0..50_000 {
    visits.add(format!("user_{i}").as_bytes());
  }
  println!("PFCOUNT     = {}（真实 50000）", visits.count());

  // 字节级持久化：升级 Dense 后固定 12304 字节，可直接写入存储层
  let bytes = visits.as_bytes();
  println!("序列化      = {} 字节", bytes.len());
  let restored = HyperLogLog::from_bytes(bytes).expect("HLL 格式合法");
  println!("回读基数    = {}", restored.count_readonly());

  // 跨实例合并 (PFMERGE) 与联合估算 (零堆分配)
  let mut ios = HyperLogLog::new();
  for i in 25_000..75_000 {
    ios.add(format!("user_{i}").as_bytes());
  }
  println!(
    "联合估算    = {}（理论约 75000）",
    HyperLogLog::count_multiple(&[&visits, &ios])
  );
  visits.merge(&ios);
  println!("merge 后    = {}", visits.count());
}
