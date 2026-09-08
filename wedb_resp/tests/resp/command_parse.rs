//! RespCommandTests：RespCommand 查找与解析语义测试

use aok::{OK, Void};
use log::info;
use wedb_resp::RespCommand;

/// 对标 Garnet RespCommandTests.cs —— 命令大小写不敏感查找、子命令匹配与命令属性语义验证
#[test]
fn test_resp_command_parsing_and_lookup() -> Void {
  // 1. 大小写不敏感命令查找
  assert_eq!(RespCommand::lookup(b"get"), Some((RespCommand::GET, false)));
  assert_eq!(RespCommand::lookup(b"GET"), Some((RespCommand::GET, false)));
  assert_eq!(RespCommand::lookup(b"GeT"), Some((RespCommand::GET, false)));
  assert_eq!(RespCommand::lookup(b"set"), Some((RespCommand::SET, false)));
  assert_eq!(RespCommand::lookup(b"SET"), Some((RespCommand::SET, false)));
  assert_eq!(
    RespCommand::lookup(b"hset"),
    Some((RespCommand::HSET, false))
  );

  // 2. 具备子命令的命令查找
  assert_eq!(
    RespCommand::lookup(b"client"),
    Some((RespCommand::CLIENT, true))
  );
  assert_eq!(
    RespCommand::lookup(b"cluster"),
    Some((RespCommand::CLUSTER, true))
  );
  assert_eq!(
    RespCommand::lookup(b"config"),
    Some((RespCommand::CONFIG, true))
  );

  // 3. 子命令匹配
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::CLIENT, b"list"),
    Some(RespCommand::CLIENT_LIST)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::CONFIG, b"get"),
    Some(RespCommand::CONFIG_GET)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::CLUSTER, b"nodes"),
    Some(RespCommand::CLUSTER_NODES)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::BITOP, b"and"),
    Some(RespCommand::BITOP_AND)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::BITOP, b"xor"),
    Some(RespCommand::BITOP_XOR)
  );
  assert_eq!(
    RespCommand::lookup_subcommand(RespCommand::CLIENT, b"unknown_sub"),
    None
  );

  // 4. 命令属性验证
  assert!(RespCommand::SET.is_write());
  assert!(RespCommand::GET.is_readonly());
  assert!(RespCommand::AUTH.is_no_auth());
  assert!(!RespCommand::CLIENT_LIST.is_cluster_subcommand());
  assert!(RespCommand::CLUSTER_NODES.is_cluster_subcommand());

  // 5. ACL 规范化
  assert_eq!(RespCommand::SETEXNX.normalize_for_acls(), RespCommand::SET);
  assert_eq!(
    RespCommand::BITOP_XOR.normalize_for_acls(),
    RespCommand::BITOP
  );

  info!("C# 兼容性测试：RespCommand 解析匹配通过");
  OK
}
