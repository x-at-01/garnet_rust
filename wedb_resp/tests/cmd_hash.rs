use std::time::Instant;

use aok::{OK, Void};
use log::info;
use wedb_resp::{
  RespCommand,
  cmd_hash::{lookup_primary, lookup_subcommand},
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_cmd_hash_primary_commands() -> Void {
  // 1. 验证常见主命令
  let cases = [
    ("GET", RespCommand::Get, false),
    ("SET", RespCommand::Set, false),
    ("DEL", RespCommand::Del, false),
    ("PING", RespCommand::Ping, false),
    ("INCR", RespCommand::Incr, false),
    ("EXISTS", RespCommand::Exists, false),
    ("ZADD", RespCommand::Zadd, false),
    ("ZRANGE", RespCommand::Zrange, false),
    ("HSET", RespCommand::Hset, false),
    ("HGET", RespCommand::Hget, false),
    ("LPUSH", RespCommand::Lpush, false),
    ("RPOP", RespCommand::Rpop, false),
    ("GEOADD", RespCommand::Geoadd, false),
    ("GEODIST", RespCommand::Geodist, false),
    // 包含子命令的主命令 ("CLUSTER", RespCommand::Cluster, true),
    ("CLIENT", RespCommand::Client, true),
    ("ACL", RespCommand::Acl, true),
    ("COMMAND", RespCommand::Command, true),
    ("CONFIG", RespCommand::Config, true),
    ("SCRIPT", RespCommand::Script, true),
    ("LATENCY", RespCommand::Latency, true),
    ("SLOWLOG", RespCommand::Slowlog, true),
    ("BITOP", RespCommand::Bitop, true),
  ];

  for (name, expected_cmd, expected_has_sub) in cases {
    let (cmd, has_sub) = lookup_primary(name.as_bytes());
    assert_eq!(cmd, expected_cmd, "命令 {} 查找结果不匹配", name);
    assert_eq!(
      has_sub, expected_has_sub,
      "命令 {} has_sub 标志不匹配",
      name
    );
  }

  info!("主命令硬件 CRC32 哈希查找验证通过");
  OK
}

#[test]
fn test_cmd_hash_subcommands() -> Void {
  // 验证子命令查找
  assert_eq!(
    lookup_subcommand(RespCommand::Cluster, b"NODES"),
    RespCommand::ClusterNodes
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Cluster, b"MEET"),
    RespCommand::ClusterMeet
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Client, b"LIST"),
    RespCommand::ClientList
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Client, b"ID"),
    RespCommand::ClientId
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Acl, b"WHOAMI"),
    RespCommand::AclWhoami
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Config, b"GET"),
    RespCommand::ConfigGet
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Config, b"SET"),
    RespCommand::ConfigSet
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Script, b"LOAD"),
    RespCommand::ScriptLoad
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Script, b"FLUSH"),
    RespCommand::ScriptFlush
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Bitop, b"AND"),
    RespCommand::BitopAnd
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Bitop, b"XOR"),
    RespCommand::BitopXor
  );

  // 不存在的子命令
  assert_eq!(
    lookup_subcommand(RespCommand::Cluster, b"NON_EXIST"),
    RespCommand::None
  );
  assert_eq!(
    lookup_subcommand(RespCommand::Get, b"FOO"),
    RespCommand::None
  );

  info!("子命令硬件 CRC32 哈希查找验证通过");
  OK
}

#[test]
fn test_cmd_hash_case_insensitivity() -> Void {
  // 验证通过 RespCommand::lookup 支持大小写混合
  let cases: &[(&[u8], RespCommand)] = &[
    (b"get", RespCommand::Get),
    (b"Set", RespCommand::Set),
    (b"pInG", RespCommand::Ping),
    (b"zAdD", RespCommand::Zadd),
    (b"clUster", RespCommand::Cluster),
  ];

  for &(name, expected) in cases {
    let res = RespCommand::lookup(name);
    assert!(res.is_some(), "大小写混合 {:?} 应当能被查到", name);
    assert_eq!(res.unwrap().0, expected);
  }

  // 验证子命令大小写
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::Cluster, b"nodes"),
    Some(RespCommand::ClusterNodes)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::Client, b"List"),
    Some(RespCommand::ClientList)
  );

  info!("大小写不敏感与小写命令兼容性验证通过");
  OK
}

#[test]
fn test_cmd_hash_perf_benchmark() -> Void {
  // 吞吐量测试：1,000,000 次查询常用命令
  let cmd_names: [&[u8]; 8] = [
    b"GET", b"SET", b"DEL", b"PING", b"INCR", b"EXISTS", b"ZADD", b"HSET",
  ];
  let iterations = 1_000_000;

  let start = Instant::now();
  let mut dummy = 0usize;
  for i in 0..iterations {
    let name = cmd_names[i & 7];
    let (cmd, _) = lookup_primary(name);
    if cmd != RespCommand::None {
      dummy += 1;
    }
  }
  let duration = start.elapsed();
  assert_eq!(dummy, iterations);

  let nanos_per_op = duration.as_nanos() as f64 / iterations as f64;
  let m_ops = (iterations as f64) / duration.as_secs_f64() / 1_000_000.0;
  info!(
    "硬件 CRC32 L1 Cache 命令查找性能: {} 次查询，总耗时 {:?}，单次耗时: {:.2} ns (吞吐量: {:.2} M ops/sec)",
    iterations, duration, nanos_per_op, m_ops
  );

  OK
}
