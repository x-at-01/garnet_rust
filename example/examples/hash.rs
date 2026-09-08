//! `HashObject`（Redis Hash）嵌入式使用演示
//!
//! 纯内存哈希表，`new()` 即用：支持字段读写、批量操作、数值增量、
//! 字段级过期（Redis 7.4 HEXPIRE 语义）、游标扫描与 glob 模式匹配。

use wedb_hash::{ExpireOpt, HashObject};

/// 便于展示：字节切片转 UTF-8，空值显示为 nil
fn txt(v: Option<&[u8]>) -> String {
  v.map_or_else(|| "nil".into(), |b| String::from_utf8_lossy(b).into_owned())
}

fn main() {
  let mut user = HashObject::new();

  // 单字段读写 (HSET / HGET)
  user.hset("name", "张三");
  user.hset("age", "28");
  user.hset("city", "北京");
  println!("HGET name  = {}", txt(user.hget(b"name")));
  println!("HLEN       = {}", user.len());

  // 批量写入与读取 (HMSET / HMGET)
  user.hmset(
    [("lang", "rust"), ("os", "linux")]
      .into_iter()
      .map(|(f, v)| (f.as_bytes().to_vec(), v.as_bytes().to_vec())),
  );
  let vals = user.hmget(&[b"lang", b"os", b"missing"]);
  println!(
    "HMGET      = {:?}",
    vals.iter().map(|v| txt(v.as_deref())).collect::<Vec<_>>()
  );

  // 数值增量 (HINCRBY / HINCRBYFLOAT)
  user.hset("score", "10");
  println!(
    "HINCRBY +5 = {}",
    user.hincrby(b"score", 5).expect("既有字段为合法整数")
  );
  println!(
    "HINCRBYFLOAT +0.5 = {}",
    user
      .hincrbyfloat(b"score", 0.5)
      .expect("既有字段为合法浮点数")
  );

  // 字段级过期 (HEXPIRE / HPTTL / HPERSIST，毫秒绝对时间戳)
  let now_ms = coarsetime::Clock::now_since_epoch().as_millis();
  println!(
    "HEXPIRE city（1 小时后）= {:?}",
    user.hexpire(b"city", now_ms + 3_600_000, ExpireOpt::NONE)
  );
  println!("HPTTL city = {} ms", user.httl(b"city"));
  user.hpersist(b"city");
  println!("HPERSIST 后 = {} (-1 表示永不过期)", user.httl(b"city"));
  // 过期时间戳早于当前时刻：校验通过后立即清除字段，返回 KeyAlreadyExpired
  println!(
    "HEXPIRE age（过去时刻）= {:?}",
    user.hexpire(b"age", now_ms.saturating_sub(1), ExpireOpt::NONE)
  );
  println!("过期后 HEXISTS age = {}", user.hexists(b"age"));

  // 游标扫描 + glob 模式匹配 (HSCAN，只读零拷贝借用版本 hscan_borrowed)
  let (next, hit) = user.hscan_borrowed(0, 10, Some(b"c*"));
  println!(
    "HSCAN c*   = {:?}，next_cursor = {next}",
    hit
      .iter()
      .map(|(f, _)| String::from_utf8_lossy(f).into_owned())
      .collect::<Vec<_>>()
  );

  // 零拷贝遍历与删除
  for (f, v) in user.iter_valid() {
    println!(
      "  {} = {}",
      String::from_utf8_lossy(f),
      String::from_utf8_lossy(v)
    );
  }
  println!(
    "HDEL name+age = {}（age 已过期消失，仅存活字段计入）",
    user.hdel(&[b"name", b"age"])
  );
}
