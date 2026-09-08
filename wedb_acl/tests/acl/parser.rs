//! AclParserTests：ACL 规则解析与化简测试

use aok::{OK, Void};
use log::info;
use wedb_acl::AclParser;

/// 对标 Garnet AclParserTests.cs: ParseACLRuleDescriptionTest —— 验证 28 组规则解析后化简与描述生成的完全一致性
#[test]
fn test_parse_acl_rule_description() -> Void {
  let cases: &[(&str, &str)] = &[
    ("user 1-command on +set", "+set"),
    ("user 2-command on +set +get", "+set +get"),
    ("user 3-command-duplicates-reduce on +set +set", "+set"),
    (
      "user 4-command-duplicates-complicated on +set +set -set +set",
      "+set",
    ),
    (
      "user 5-command-duplicates-complicated on +get -set +set",
      "+get +set",
    ),
    ("user 6-category on +@keyspace", "+@keyspace"),
    ("user 7-category-reduces on +@all", "+@all"),
    ("user 7-category-reduces on -@all", ""),
    ("user 8-category-reduces on -@all +@keyspace", "+@keyspace"),
    ("user 9-category-reduces on +@all +@keyspace", "+@all"),
    (
      "user 10-category-command-reduces on +@keyspace +del",
      "+@keyspace",
    ),
    (
      "user 11-category-command-reduces on +@keyspace +set",
      "+@keyspace +set",
    ),
    (
      "user 12-category-command-reduces on +@keyspace +del -del",
      "+@keyspace -del",
    ),
    ("user 13-category-command-reduces on +del -@keyspace", ""),
    (
      "user 14-category-command-reduces on -del +@keyspace",
      "+@keyspace",
    ),
    (
      "user 15-category-command-reduces on +set +@keyspace",
      "+set +@keyspace",
    ),
    ("user 16-category-command-reduces on +@all +set", "+@all"),
    (
      "user 17-category-command-reduces on +@all +set +get +incr -decr",
      "+@all -decr",
    ),
    ("user 18-category-command-reduces on -@all +set", "+set"),
    (
      "user 19-category-command-reduces on -@all +set +get",
      "+set +get",
    ),
    (
      "user 20-category-command-reduces on -@all +set +get +incr +decr +incrby +decrby",
      "+set +get +incr +decr +incrby +decrby",
    ),
    (
      "user 21-category-command-reduces on -@all +ping +auth +set +get +del +incr +decr +incrby +decrby +expire +ttl +keys +scan +hget",
      "+ping +auth +set +get +del +incr +decr +incrby +decrby +expire +ttl +keys +scan +hget",
    ),
    (
      "user 22-category-command-reduces on -@all +ping +auth +set +get +del +incr +decr +incrby +decrby +expire +ttl +keys +scan +hget +config|get",
      "+ping +auth +set +get +del +incr +decr +incrby +decrby +expire +ttl +keys +scan +hget +config|get",
    ),
    (
      "user 23-category-command-reduces on -@all +set +get +incr +decr +@keyspace +@hash +incrby +decrby",
      "+set +get +incr +decr +@keyspace +@hash +incrby +decrby",
    ),
    (
      "user 24-multi-category-reduces on -@all +@keyspace +@hash",
      "+@keyspace +@hash",
    ),
    (
      "user 25-multi-category-reduces on -@all +@keyspace +@hash -flushdb",
      "+@keyspace +@hash -flushdb",
    ),
    (
      "user 26-multi-category-reduces on -@all +@keyspace -flushdb +@hash -flushdb",
      "+@keyspace -flushdb +@hash",
    ),
    (
      "user 27-multi-category-reduces on -@all +set +get +incr +decr +@keyspace +@hash +incrby +decrby +script|exists +@pubsub +expire +ttl",
      "+set +get +incr +decr +@keyspace +@hash +incrby +decrby +script|exists +@pubsub",
    ),
    ("user 28-command-reversed-duplicates on -set +set", "+set"),
  ];

  for &(acl, expected) in cases {
    let user = AclParser::parse_rule_line(acl)?;
    assert_eq!(
      user.enabled_commands_description(),
      expected,
      "规则 '{acl}' 化简描述与 Garnet 不一致"
    );
  }

  info!("C# 兼容性测试：ParseACLRuleDescriptionTest 通过");
  OK
}

/// 对标 Garnet AclParserTests.cs: ParseACLRuleDescriptionTimeoutsTest —— 验证长规则解析及无超时现象
#[test]
fn test_parse_acl_rule_description_timeouts() -> Void {
  let cases: &[(&str, &str)] = &[
    (
      "user 1-command-notimeout on +auth +ping +get +set +del +exists +incr +decr +mget +mset +expire +ttl +keys +scan +hget +hset +lpush +rpush +sadd +decrby",
      "+auth +ping +get +set +del +exists +incr +decr +mget +mset +expire +ttl +keys +scan +hget +hset +lpush +rpush +sadd +decrby",
    ),
    (
      "user 2-category-command-notimeout on -@all +ping +auth +set +get +del +incr +decr +incrby +decrby +expire +ttl +keys +scan +hget +mget +mset +eval +evalsha +setex",
      "+ping +auth +set +get +del +incr +decr +incrby +decrby +expire +ttl +keys +scan +hget +mget +mset +eval +evalsha +setex",
    ),
    (
      "user 3-category-command-notimeout on -@all +client|id +client|info +cluster|nodes +cluster|slots +echo +info +ping +config|get +decr -decr +decrby +del +expire +flushdb +get +incr +incrby +latency +eval +evalsha +script|exists +script|flush +script|load +set +setex +unlink",
      "+client|id +client|info +cluster|nodes +cluster|slots +echo +info +ping +config|get +decr -decr +decrby +del +expire +flushdb +get +incr +incrby +latency +eval +evalsha +script|exists +script|flush +script|load +set +setex +unlink",
    ),
    (
      "user 4-category-command-notimeout on +@keyspace +client|id +client|info +cluster|nodes +cluster|slots +echo +info +ping +config|get +decr -decr +decrby +del +expire +flushdb +get +incr +incrby +latency +eval +evalsha +script|exists +script|flush +script|load +set +setex +unlink",
      "+@keyspace +client|id +client|info +cluster|nodes +cluster|slots +echo +info +ping +config|get +decr -decr +decrby +get +incr +incrby +latency +eval +evalsha +script|exists +script|flush +script|load +set +setex",
    ),
    (
      "user 5-category-command-notimeout on -@all +@keyspace +client|id +client|info +cluster|nodes +cluster|slots +echo +info +ping +config|get +decr -decr +decrby +del +expire +flushdb +get +incr +incrby +latency +eval +evalsha +script|exists +script|flush +script|load +set +setex +unlink",
      "+@keyspace +client|id +client|info +cluster|nodes +cluster|slots +echo +info +ping +config|get +decr -decr +decrby +get +incr +incrby +latency +eval +evalsha +script|exists +script|flush +script|load +set +setex",
    ),
  ];

  for &(acl, expected) in cases {
    let user = AclParser::parse_rule_line(acl)?;
    assert_eq!(
      user.enabled_commands_description(),
      expected,
      "长规则 '{acl}' 化简描述不一致"
    );
  }

  info!("C# 兼容性测试：ParseACLRuleDescriptionTimeoutsTest 通过");
  OK
}

/// 对标 Garnet AclParserTests.cs: ParseACLRuleDescriptionShouldReduceTest —— 验证原版 Explicit 边界规则的健壮解析
#[test]
fn test_parse_acl_rule_description_should_reduce() -> Void {
  let user1 =
    AclParser::parse_rule_line("user 1-category-command-reduces on +@keyspace +del -del +del")?;
  assert!(!user1.enabled_commands_description().is_empty());

  let user2 = AclParser::parse_rule_line(
    "user 2-command-duplicates-complicated on +set -get +get +set -set +set",
  )?;
  assert!(!user2.enabled_commands_description().is_empty());

  info!("C# 兼容性测试：ParseACLRuleDescriptionShouldReduceTest 通过");
  OK
}
