//! `SortedSetObject`（Redis ZSet）嵌入式使用演示
//!
//! 跳表 + 哈希字典双结构：O(log N) 排名与范围操作 + O(1) 分数反查，
//! 支持同分字典序、加权聚合运算与成员级过期（Redis 7.4 ZEXPIRE 语义）。

use wedb_zset::{ExpireOpt, LexBound, ScoreRange, SortedSetAggregate, SortedSetObject, ZAddOpt};

/// 便于展示：(member, score) 集合转 UTF-8 可读形式
fn show(pairs: Vec<(Vec<u8>, f64)>) -> Vec<(String, f64)> {
  pairs
    .into_iter()
    .map(|(m, s)| (String::from_utf8_lossy(&m).into_owned(), s))
    .collect()
}

fn main() {
  let mut board = SortedSetObject::new();

  // 添加成员 (ZADD)
  for (name, score) in [
    ("alice", 92.5),
    ("bob", 87.0),
    ("carol", 95.0),
    ("dave", 87.0),
  ] {
    board
      .zadd(score, name, ZAddOpt::default())
      .expect("分数合法");
  }
  println!("ZCARD       = {}", board.len());

  // 分数与排名（同分按成员字典序排列）
  println!("ZSCORE bob  = {:?}", board.zscore(b"bob"));
  println!("ZRANK bob   = {:?}（0-based 升序）", board.zrank(b"bob"));
  println!("ZREVRANK carol = {:?}", board.zrevrank(b"carol"));

  // 原子加分 (ZINCRBY)
  println!(
    "ZINCRBY alice +1.5 = {}",
    board.zincrby("alice", 1.5).expect("合法增量")
  );

  // 范围查询 (ZRANGE / ZRANGEBYSCORE / ZRANGEBYLEX / ZCOUNT)
  println!("ZRANGE      = {:?}", show(board.zrange(0, -1, false)));
  println!(
    "ZRANGEBYSCORE [87, 93) = {:?}",
    show(board.zrangebyscore(ScoreRange::new(87.0, true, 93.0, false), false, 0, 10))
  );
  // 字典序边界过滤：输出保持 (score, member) 序（Redis 语义：仅同分成员间按字典序）
  println!(
    "ZRANGEBYLEX [alice, dave] = {:?}",
    show(board.zrangebylex(
      &LexBound::Included(b"alice".to_vec()),
      &LexBound::Included(b"dave".to_vec()),
      false,
      0,
      10
    ))
  );
  println!(
    "ZCOUNT (80, 90] = {}",
    board.zcount(ScoreRange::new(80.0, false, 90.0, true))
  );

  // 加权聚合运算 (ZUNIONSTORE 语义)
  let mut bonus = SortedSetObject::new();
  for (name, score) in [("alice", 10.0), ("eve", 5.0)] {
    bonus
      .zadd(score, name, ZAddOpt::default())
      .expect("分数合法");
  }
  let mut total = SortedSetObject::union(&[(&board, 1.0), (&bonus, 0.5)], SortedSetAggregate::Sum);
  println!("加权 ZUNION alice = {:?}", total.zscore(b"alice"));
  println!("加权 ZUNION eve   = {:?}", total.zscore(b"eve"));

  // 成员级过期 (ZEXPIRE / ZTTL / ZPERSIST)
  let now_ms = coarsetime::Clock::now_since_epoch().as_millis();
  board.zexpire(b"bob", now_ms + 3_600_000, ExpireOpt::default());
  println!("ZTTL bob    = {} ms", board.zttl(b"bob"));
  board.zpersist(b"bob");

  // 弹出最低分 (ZPOPMIN)
  println!("ZPOPMIN     = {:?}", show(board.zpopmin(1)));
}
