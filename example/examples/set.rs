//! `SetObject`（Redis Set）嵌入式使用演示
//!
//! gxhash 哈希集合，支持去重添加、交并差运算、随机采样、
//! 游标扫描（glob 模式）与成员搬运；`CompactSet` 提供紧凑二进制形态。

use std::str::from_utf8;

use wedb_set::SetObject;

/// 便于展示：字节成员集合转 UTF-8 字符串集合
fn show(members: &[Vec<u8>]) -> Vec<&str> {
  members
    .iter()
    .map(|m| from_utf8(m).unwrap_or("?"))
    .collect()
}

fn main() {
  let mut backend = SetObject::new();

  // 去重添加 (SADD)
  let added = backend.sadd(["rust", "go", "python", "rust"]);
  println!("SADD 新增   = {added}（重复成员自动去重）");
  println!("SCARD       = {}", backend.len());
  println!("SISMEMBER go = {}", backend.sismember(b"go"));
  println!("SMEMBERS    = {:?}", show(&backend.smembers()));

  let mut frontend = SetObject::new();
  frontend.sadd(["go", "typescript", "rust"]);

  // 集合运算 (SINTER / SUNION / SDIFF / SINTERCARD 限量)
  println!(
    "SINTER      = {:?}",
    show(&backend.inter(&[&frontend]).smembers())
  );
  println!(
    "SUNION      = {:?}",
    show(&backend.union(&[&frontend]).smembers())
  );
  println!(
    "SDIFF       = {:?}",
    show(&backend.diff(&[&frontend]).smembers())
  );
  println!("SINTERCARD(2) = {}", backend.intercard(&[&frontend], 2));

  // 跨集合搬运 (SMOVE)
  println!("SMOVE python = {}", backend.smove(&mut frontend, b"python"));

  // 随机采样 (SRANDMEMBER) 与游标扫描 (SSCAN，零拷贝借用 + glob)
  println!(
    "SRANDMEMBER = {:?}",
    backend
      .srandmember(2)
      .iter()
      .map(|m| String::from_utf8_lossy(m).into_owned())
      .collect::<Vec<_>>()
  );
  let (next, hit) = backend.sscan_ref(0, 10, Some(b"r*"));
  println!(
    "SSCAN r*    = {:?}，next_cursor = {next}",
    hit
      .iter()
      .map(|m| String::from_utf8_lossy(m).into_owned())
      .collect::<Vec<_>>()
  );

  // 随机弹出 (SPOP)
  println!("SPOP        = {:?}", show(&backend.spop(1)));
}
