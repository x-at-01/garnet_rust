//! 全部嵌入式数据结构的冒烟测试：逐一实例化并验证核心读写路径

use aok::{OK, Void};
use log::info;
use wedb_hash::HashObject;
use wedb_hll::HyperLogLog;
use wedb_list::ListObject;
use wedb_set::SetObject;
use wedb_zset::{SortedSetObject, ZAddOpt};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn smoke_all_structures() -> Void {
  // Hash：字段读写与删除
  let mut h = HashObject::new();
  assert!(h.hset("name", "wedb"));
  assert_eq!(h.hget(b"name"), Some(b"wedb".as_slice()));
  assert_eq!(h.hdel(&[b"name"]), 1);

  // List：双端队列
  let mut l = ListObject::new();
  assert_eq!(l.rpush(["a", "b", "c"]), 3);
  assert_eq!(l.lpop_one().as_deref(), Some(b"a".as_slice()));

  // Set：去重集合
  let mut s = SetObject::new();
  assert_eq!(s.sadd(["x", "x", "y"]), 2);
  assert!(s.sismember(b"y"));

  // ZSet：跳表有序集合
  let mut z = SortedSetObject::new();
  assert_eq!(
    z.zadd(1.5, "m1", ZAddOpt::default()).expect("合法分数"),
    (1, 1.5)
  );
  assert_eq!(z.zscore_ref(b"m1"), Some(1.5));

  // HLL：基数估算
  let mut hl = HyperLogLog::new();
  hl.add(b"uv_1");
  hl.add(b"uv_2");
  assert!(hl.count() >= 1);

  info!("> 全部嵌入式数据结构冒烟通过");
  OK
}
