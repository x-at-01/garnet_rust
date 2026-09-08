use aok::{OK, Void};
use wedb_acl::{
  AccessControlList, AclParser, AclPassword, CAT_ALL, CAT_NONE, CommandPermissionSet,
  DEFAULT_USER_NAME, Error, User, UserHandle,
};
use wedb_resp::RespCommand;

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_acl_password() -> Void {
  // 从明文生成哈希
  let p1 = AclPassword::from_cleartext("secret123");
  let hex_str = p1.to_hex();
  assert_eq!(hex_str.len(), 64);

  // 从十六进制串解析
  let p2 = AclPassword::from_hash_hex(&hex_str)?;
  assert_eq!(p1, p2);
  assert!(p1.ct_eq(&p2));

  // 错误长度测试
  assert!(AclPassword::from_hash_hex("too_short").is_err());
  assert!(AclPassword::from_hash_hex(&format!("{}00", hex_str)).is_err());

  // 错误字符测试
  let mut invalid_hex = hex_str.clone();
  invalid_hex.replace_range(0..1, "z");
  assert!(AclPassword::from_hash_hex(&invalid_hex).is_err());

  // 不同密码不匹配（验证 PartialEq 与 ct_eq 均为恒定时间比较）
  let p3 = AclPassword::from_cleartext("other_password");
  assert!(!p1.ct_eq(&p3));
  assert_ne!(p1, p3);
  assert_eq!(p1, p2);

  // Display 格式化
  assert_eq!(p1.to_string(), hex_str);

  OK
}

#[test]
fn test_command_permission_set() -> Void {
  // 1. 空权限集
  let mut perms = CommandPermissionSet::new();
  assert!(perms.is_empty());
  assert!(!perms.allow(RespCommand::GET));
  assert!(!perms.allow(RespCommand::SET));

  // 2. 授权单个命令与 ACL 展开
  perms.set(RespCommand::SET);
  assert!(perms.allow(RespCommand::SET));
  // SET 衍生命令也应被允许
  assert!(perms.allow(RespCommand::SETEXNX));
  assert!(perms.allow(RespCommand::SETEXXX));
  assert!(perms.allow(RespCommand::SETKEEPTTL));
  assert!(!perms.allow(RespCommand::GET));

  // 3. 取消授权
  perms.clear(RespCommand::SET);
  assert!(!perms.allow(RespCommand::SET));
  assert!(!perms.allow(RespCommand::SETEXNX));

  // 4. 免认证命令保护不可被 clear 取消
  perms.set_all();
  assert!(perms.allow(RespCommand::AUTH));
  assert!(perms.allow(RespCommand::QUIT));
  perms.clear(RespCommand::AUTH);
  assert!(perms.allow(RespCommand::AUTH));
  perms.clear(RespCommand::QUIT);
  assert!(perms.allow(RespCommand::QUIT));

  // 5. 分类授权与收回
  let mut cat_perms = CommandPermissionSet::new();
  cat_perms.set_category("keyspace", true)?;
  assert!(cat_perms.allow(RespCommand::DEL));
  assert!(cat_perms.allow(RespCommand::EXISTS));
  assert!(!cat_perms.allow(RespCommand::GET)); // GET 属于 string/read

  cat_perms.set_category("keyspace", false)?;
  assert!(!cat_perms.allow(RespCommand::DEL));

  // 6. 全量分类 (+@all / -@all)
  cat_perms.set_category("all", true)?;
  assert!(cat_perms.is_all());
  assert!(cat_perms.allow(RespCommand::GET));
  assert!(cat_perms.allow(RespCommand::SET));
  assert!(cat_perms.allow(RespCommand::DEL));

  cat_perms.set_category("all", false)?;
  assert!(cat_perms.is_empty());
  assert!(!cat_perms.allow(RespCommand::GET));

  // 7. 自定义命令授权
  let mut custom_perms = CommandPermissionSet::new();
  custom_perms.add_custom_command("JSON.SET");
  assert!(custom_perms.allow_custom(RespCommand::CustomRawStringCmd, "JSON.SET"));
  assert!(custom_perms.allow_custom(RespCommand::CustomRawStringCmd, "json.set"));
  assert!(!custom_perms.allow_custom(RespCommand::CustomRawStringCmd, "JSON.GET"));

  // 拒绝优先规则：显式 -NAME 覆盖分类
  custom_perms.set_all();
  custom_perms.remove_custom_command("BLOCKED_CMD");
  assert!(!custom_perms.allow_custom(RespCommand::CustomRawStringCmd, "BLOCKED_CMD"));
  assert!(custom_perms.allow_custom(RespCommand::CustomRawStringCmd, "OTHER_CMD"));

  OK
}

#[test]
fn test_user_authentication_and_authorization() -> Void {
  let mut user = User::new("alice");
  assert!(!user.enabled);
  assert!(!user.nopass);

  // 未启用用户认证与执行均失败
  user.add_password_hash(AclPassword::from_cleartext("mypassword"));
  assert!(!user.authenticate("mypassword"));
  assert!(!user.can_execute(RespCommand::GET));

  // 启用用户
  user.enabled = true;
  assert!(user.authenticate("mypassword"));
  assert!(!user.authenticate("wrongpassword"));

  // 免认证命令在启用状态下始终允许
  assert!(user.can_execute(RespCommand::AUTH));
  assert!(user.can_execute(RespCommand::QUIT));

  // 未授权业务命令不可执行
  assert!(!user.can_execute(RespCommand::GET));
  user.add_command(RespCommand::GET);
  assert!(user.can_execute(RespCommand::GET));
  assert!(!user.can_execute(RespCommand::SET));

  // nopass 模式
  user.nopass = true;
  assert!(user.authenticate("any_random_pwd"));

  // describe DSL 导出测试
  let desc = user.describe();
  assert!(desc.starts_with("user alice on nopass"));
  assert!(desc.contains("+get"));

  // reset 重置
  user.reset();
  assert!(!user.enabled);
  assert!(!user.nopass);
  assert!(user.passwords.is_empty());
  assert!(user.commands.is_empty());

  OK
}

#[test]
fn test_acl_parser_ops() -> Void {
  let mut user = User::new("bob");

  // on / off
  AclParser::apply_op(&mut user, "on")?;
  assert!(user.enabled);
  AclParser::apply_op(&mut user, "off")?;
  assert!(!user.enabled);

  // nopass / resetpass
  AclParser::apply_op(&mut user, "nopass")?;
  assert!(user.nopass);
  AclParser::apply_op(&mut user, "resetpass")?;
  assert!(!user.nopass);

  // >cleartext / <cleartext
  AclParser::apply_op(&mut user, ">foo123")?;
  assert_eq!(user.passwords.len(), 1);
  assert_eq!(user.passwords[0], AclPassword::from_cleartext("foo123"));
  AclParser::apply_op(&mut user, "<foo123")?;
  assert!(user.passwords.is_empty());

  // #hash / !hash
  let hash_str = AclPassword::from_cleartext("bar456").to_hex();
  AclParser::apply_op(&mut user, &format!("#{}", hash_str))?;
  assert_eq!(user.passwords.len(), 1);
  AclParser::apply_op(&mut user, &format!("!{}", hash_str))?;
  assert!(user.passwords.is_empty());

  // +@category / -@category
  AclParser::apply_op(&mut user, "+@string")?;
  assert!(user.commands.allow(RespCommand::GET));
  assert!(user.commands.allow(RespCommand::SET));
  AclParser::apply_op(&mut user, "-@string")?;
  assert!(!user.commands.allow(RespCommand::GET));

  // +cmd / -cmd（含子命令）
  AclParser::apply_op(&mut user, "+client|id")?;
  assert!(user.commands.allow(RespCommand::CLIENT_ID));
  AclParser::apply_op(&mut user, "-client|id")?;
  assert!(!user.commands.allow(RespCommand::CLIENT_ID));

  // 兼容性 token (allkeys, ~*, resetkeys)
  AclParser::apply_op(&mut user, "~*")?;
  AclParser::apply_op(&mut user, "allkeys")?;
  AclParser::apply_op(&mut user, "resetkeys")?;

  // 错误输入
  assert!(matches!(
    AclParser::apply_op(&mut user, "unknown_token"),
    Err(Error::UnknownOperation(_))
  ));
  assert!(matches!(
    AclParser::apply_op(&mut user, "+@nonexistent_cat"),
    Err(Error::CategoryDoesNotExist(_))
  ));

  OK
}

/// 规则消解用例（对照 C# Garnet: AclParserTests.cs）
#[test]
fn test_garnet_rule_reductions() -> Void {
  // AclParserTests.cs 中的全部规则消解用例
  let cases = [
    ("user 1-command on +set", "+set"),
    ("user 2-command on +set +get", "+set +get"),
    ("user 3-command-duplicates-reduce on +set +set", "+set"),
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
    ("user 18-category-command-reduces on -@all +set", "+set"),
    (
      "user 19-category-command-reduces on -@all +set +get",
      "+set +get",
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
  ];

  for (rule_line, expected_desc) in cases {
    let user = AclParser::parse_rule_line(rule_line)?;
    assert_eq!(
      user.enabled_commands_description(),
      expected_desc,
      "Failed on rule: {rule_line}"
    );
  }

  OK
}

#[test]
fn test_access_control_list_manager() -> Void {
  // 1. 初始化，校验默认 default 用户
  let acl = AccessControlList::new("");
  let def_user = acl.default_user();
  assert_eq!(def_user.read().name, "default");
  assert!(def_user.read().enabled);
  assert!(def_user.read().nopass);
  assert!(def_user.read().commands.is_all());

  // 免密 default 认证
  assert!(acl.auth("default", "any_pwd"));
  assert!(acl.auth_default("any_pwd"));

  // 默认命令全部可执行
  assert!(acl.can_execute("default", RespCommand::SET));
  assert!(acl.can_execute("default", RespCommand::GET));

  // 2. default 用户保护测试（不可删除）
  assert!(matches!(
    acl.del_user("default"),
    Err(Error::DefaultUserProtected)
  ));

  // 3. set_user 创建与配置用户
  acl.set_user("alice", &["on", ">alice123", "+@string"])?;
  assert!(acl.auth("alice", "alice123"));
  assert!(!acl.auth("alice", "wrongpwd"));
  assert!(acl.can_execute("alice", RespCommand::GET));
  assert!(!acl.can_execute("alice", RespCommand::DEL));

  // 4. set_user 更新已有用户
  acl.set_user("alice", &["+@keyspace"])?;
  assert!(acl.can_execute("alice", RespCommand::DEL));

  // 5. set_user_from_line
  acl.set_user_from_line("user bob on >bob123 +ping +info")?;
  assert!(acl.auth("bob", "bob123"));
  assert!(acl.can_execute("bob", RespCommand::PING));
  assert!(acl.can_execute("bob", RespCommand::INFO));
  assert!(!acl.can_execute("bob", RespCommand::GET));

  // 6. 用户列表与用户名列表查询
  let names = acl.user_names();
  assert!(names.contains(&"default".to_string()));
  assert!(names.contains(&"alice".to_string()));
  assert!(names.contains(&"bob".to_string()));

  let user_list = acl.list_users();
  assert!(user_list.iter().any(|u| u.starts_with("user default")));
  assert!(user_list.iter().any(|u| u.starts_with("user alice")));
  assert!(user_list.iter().any(|u| u.starts_with("user bob")));

  // 7. 删除用户
  assert!(acl.del_user("bob")?);
  assert!(!acl.del_user("bob")?); // 再次删除返回 false
  assert!(acl.get_user("bob").is_none());

  // 8. bitcode 二进制持久化快照与恢复
  let acl_bytes = acl.to_bitcode();
  let reloaded_acl = AccessControlList::new("");
  reloaded_acl.from_bitcode_bytes(&acl_bytes)?;
  assert!(reloaded_acl.auth("alice", "alice123"));
  assert!(reloaded_acl.can_execute("alice", RespCommand::GET));

  // 校验恢复原子性：若数据损坏，已有 ACL 状态不被破坏
  let corrupt_bytes = [0xFF, 0xFE, 0xFD, 0xFC];
  assert!(reloaded_acl.from_bitcode_bytes(&corrupt_bytes).is_err());
  // 确保 alice 仍然完好无损
  assert!(reloaded_acl.auth("alice", "alice123"));

  OK
}

#[test]
fn test_acl_password_extended() -> Void {
  let hash_bytes = [7u8; 32];
  let p = AclPassword::new(hash_bytes);
  assert_eq!(p.hash(), &hash_bytes);

  // 验证 bitcode 序列化与反序列化
  let encoded = bitcode::encode(&p);
  let decoded: AclPassword = bitcode::decode(&encoded)?;
  assert_eq!(p, decoded);

  // 验证 dummy 不会 panic
  AclPassword::dummy("test_pwd_for_dummy");

  // 验证 PartialEq 与 ct_eq
  let p_same = AclPassword::new(hash_bytes);
  assert_eq!(p, p_same);
  assert!(p.ct_eq(&p_same));

  let mut different_bytes = hash_bytes;
  different_bytes[0] = 8;
  let p_diff = AclPassword::new(different_bytes);
  assert_ne!(p, p_diff);
  assert!(!p.ct_eq(&p_diff));

  OK
}

#[test]
fn test_command_permission_set_extended() -> Void {
  let p_desc = CommandPermissionSet::with_description("+@admin");
  assert_eq!(p_desc.description(), "+@admin");
  assert!(p_desc.is_empty());

  let p_bitmap = CommandPermissionSet::from_bitmap(CAT_ALL, "+@all");
  assert!(p_bitmap.is_all());
  assert_eq!(p_bitmap.bits.len() * 64, 1024);

  let p_copy = p_bitmap.clone();
  assert!(p_copy.is_all());
  assert_eq!(p_copy, p_bitmap);

  // 验证 bitcode 序列化与反序列化
  let encoded = bitcode::encode(&p_bitmap);
  let decoded: CommandPermissionSet = bitcode::decode(&encoded)?;
  assert_eq!(p_bitmap, decoded);

  let mut p_empty = CommandPermissionSet::new();
  assert_eq!(p_empty.bits, CAT_NONE);
  assert!(p_empty.custom_allowed().is_empty());
  assert!(p_empty.custom_denied().is_empty());

  // 边界检查：NONE (0) 与越界命令都不应抛异常
  assert!(!p_empty.allow(RespCommand::NONE));
  assert!(!p_empty.allow(RespCommand::INVALID));
  p_empty.set(RespCommand::NONE);
  assert!(!p_empty.allow(RespCommand::NONE));
  p_empty.clear(RespCommand::NONE);

  OK
}

#[test]
fn test_user_and_handle_extended() -> Void {
  let mut user = User::new("test_user");
  assert_eq!(user.name(), "test_user");
  assert!(!user.is_enabled());

  user.set_enabled(true);
  assert!(user.is_enabled());

  assert!(!user.is_passwordless());
  user.set_passwordless(true);
  assert!(user.is_passwordless());

  // 校验添加密码会自动清除 nopass
  AclParser::apply_op(&mut user, ">pwd123")?;
  assert!(!user.is_passwordless());
  assert_eq!(user.passwords().len(), 1);

  // 校验自定义命令集合读取
  user.add_custom_command("JSON.SET")?;
  assert!(user.custom_commands_allowed().contains("JSON.SET"));
  user.remove_custom_command("JSON.SET")?;
  assert!(user.custom_commands_denied().contains("JSON.SET"));

  // 拷贝辅助方法测试
  let copied_cmds = user.copy_command_permission_set();
  assert_eq!(copied_cmds, user.commands);
  let copied_passwords = user.copy_password_hashes();
  assert_eq!(copied_passwords, user.passwords);

  // 验证 User bitcode 序列化与反序列化
  let user_bytes = user.to_bitcode();
  let decoded_user = User::from_bitcode(&user_bytes)?;
  assert_eq!(user, decoded_user);

  // UserHandle 与 CAS 原子更新
  let handle = UserHandle::new(user.clone());
  let handle_clone = handle.clone();
  assert!(handle.ptr_eq(&handle_clone));
  assert_eq!(handle.with_name(|n| n.to_string()), "test_user");
  assert!(handle.with_user(|u| u.is_enabled()));

  let mut updated_user = user.clone();
  updated_user.name = "renamed_user".to_string();

  // CAS 成功分支
  assert!(handle.try_set_user(&user, updated_user.clone()));
  assert_eq!(handle.read().name(), "renamed_user");

  // CAS 失败分支（expected 不匹配）
  assert!(!handle.try_set_user(&user, user.clone()));
  assert_eq!(handle.read().name(), "renamed_user");

  OK
}

#[test]
fn test_acl_manager_sync_and_import() -> Void {
  let acl = AccessControlList::new("initial_default_pass");

  // 测试 create_default_user_handle
  let def_handle = AccessControlList::create_default_user_handle("initial_default_pass");
  assert!(def_handle.read().authenticate("initial_default_pass"));

  // 测试 add_user_handle 与 get_user_handles
  let extra_user = UserHandle::new(User::new("charlie"));
  acl.add_user_handle(extra_user.clone())?;
  // 重复添加同一用户名应报错 UserAlreadyExists
  assert!(matches!(
    acl.add_user_handle(extra_user),
    Err(Error::UserAlreadyExists(_))
  ));
  let handles = acl.get_user_handles();
  assert!(handles.iter().any(|h| h.read().name() == "charlie"));
  assert!(handles.iter().any(|h| h.read().name() == DEFAULT_USER_NAME));

  // 测试 AccessControlList to_bitcode / from_bitcode_bytes 全量快照序列化
  let acl_bytes = acl.to_bitcode();
  let restored_acl = AccessControlList::new("");
  restored_acl.from_bitcode_bytes(&acl_bytes)?;
  assert!(restored_acl.auth("default", "initial_default_pass"));
  assert!(restored_acl.get_user("charlie").is_some());

  // 配置新规则与添加 eve 用户
  acl.set_user("default", &["on", ">reloaded_pwd", "+@all"])?;
  acl.set_user("eve", &["on", "nopass", "+get", "+set"])?;

  // 验证 default 用户在主字典与 default_user 引用间 100% 保持同步（关键修复验证）
  assert!(acl.auth("default", "reloaded_pwd"));
  assert!(acl.auth_default("reloaded_pwd"));

  // 修改 default 用户，确保 default_user 同步反映修改
  acl.set_user("default", &["off"])?;
  assert!(!acl.default_user().read().is_enabled());
  assert!(!acl.auth_default("reloaded_pwd"));

  // 验证 eve 用户导入成功
  assert!(acl.auth("eve", ""));
  assert!(acl.can_execute("eve", RespCommand::GET));
  assert!(!acl.can_execute("eve", RespCommand::DEL));

  OK
}

#[test]
fn test_genpass_extended() -> Void {
  // 默认 256 位，输出 64 个十六进制小写字符
  let p1 = AclPassword::genpass(None)?;
  assert_eq!(p1.len(), 64);
  assert!(p1.chars().all(|c| c.is_ascii_hexdigit()));

  // 128 位 -> 32 字符
  let p2 = AclPassword::genpass(Some(128))?;
  assert_eq!(p2.len(), 32);

  // 64 位 -> 16 字符
  let p3 = AclPassword::genpass(Some(64))?;
  assert_eq!(p3.len(), 16);

  // 1 位 -> 1 字符
  let p4 = AclPassword::genpass(Some(1))?;
  assert_eq!(p4.len(), 1);

  // 4096 位 -> 1024 字符
  let p5 = AclPassword::genpass(Some(4096))?;
  assert_eq!(p5.len(), 1024);

  // 非法位数测试（0 位或超过 4096 位）
  assert!(AclPassword::genpass(Some(0)).is_err());
  assert!(AclPassword::genpass(Some(4097)).is_err());

  // 两次生成的密码随机互异
  let p6 = AclPassword::genpass(None)?;
  assert_ne!(p1, p6);

  OK
}

#[test]
fn test_key_patterns_and_read_write_separation() -> Void {
  let mut user = User::new("test_keys_user");
  user.set_enabled(true);

  // 初始无权限
  assert!(!user.can_access_key("any_key", false));
  assert!(!user.can_access_key("any_key", true));

  // 读写分离授权
  // 1. %R~data:* 仅允许读
  AclParser::apply_op(&mut user, "%R~data:*")?;
  assert!(user.can_access_key("data:123", false));
  assert!(!user.can_access_key("data:123", true)); // 写被拒绝
  assert!(!user.can_access_key("other:123", false));

  // 2. %W~log:* 仅允许写
  AclParser::apply_op(&mut user, "%W~log:*")?;
  assert!(!user.can_access_key("log:app", false)); // 读被拒绝
  assert!(user.can_access_key("log:app", true)); // 写被允许

  // 3. %RW~cache:* 允许读写
  AclParser::apply_op(&mut user, "%RW~cache:*")?;
  assert!(user.can_access_key("cache:user:1", false));
  assert!(user.can_access_key("cache:user:1", true));

  // 4. ~temp:* 等价于读写
  AclParser::apply_op(&mut user, "~temp:*")?;
  assert!(user.can_access_key("temp:file", false));
  assert!(user.can_access_key("temp:file", true));

  // 5. 中括号与问号 Glob 模式
  AclParser::apply_op(&mut user, "~item:[0-9]?")?;
  assert!(user.can_access_key("item:3a", false));
  assert!(user.can_access_key("item:3a", true));
  assert!(!user.can_access_key("item:xx", false));

  // 校验 describe 导出的键模式格式
  let desc = user.describe();
  assert!(desc.contains("%R~data:*"));
  assert!(desc.contains("%W~log:*"));
  assert!(desc.contains("~cache:*"));
  assert!(desc.contains("~temp:*"));

  // 6. resetkeys 重置全部键模式
  AclParser::apply_op(&mut user, "resetkeys")?;
  assert!(!user.can_access_key("data:123", false));
  assert!(!user.can_access_key("cache:user:1", false));
  assert!(user.key_patterns().is_empty());

  // 7. allkeys / ~* 允许全部键
  AclParser::apply_op(&mut user, "allkeys")?;
  assert!(user.can_access_key("any:key:read", false));
  assert!(user.can_access_key("any:key:write", true));
  let all_desc = user.describe();
  assert!(all_desc.contains("~*"));

  OK
}

#[test]
fn test_channel_patterns_and_pubsub() -> Void {
  let mut user = User::new("test_channel_user");
  user.set_enabled(true);

  // 初始无频道权限
  assert!(!user.can_access_channel("news.sport"));

  // 1. 频道模式授权
  AclParser::apply_op(&mut user, "&news.*")?;
  assert!(user.can_access_channel("news.sport"));
  assert!(user.can_access_channel("news.finance"));
  assert!(!user.can_access_channel("chat.general"));

  // 2. 追加频道模式并去重
  AclParser::apply_op(&mut user, "&chat.*")?;
  AclParser::apply_op(&mut user, "&chat.*")?; // 重复添加自动去重
  assert_eq!(user.channel_patterns().len(), 2);
  assert!(user.can_access_channel("chat.room1"));

  // 校验 describe 导出格式
  let desc = user.describe();
  assert!(desc.contains("&news.*"));
  assert!(desc.contains("&chat.*"));

  // 3. resetchannels 重置全部频道
  AclParser::apply_op(&mut user, "resetchannels")?;
  assert!(!user.can_access_channel("news.sport"));
  assert!(!user.can_access_channel("chat.room1"));
  assert!(user.channel_patterns().is_empty());

  // 4. allchannels / &* 允许全部频道
  AclParser::apply_op(&mut user, "allchannels")?;
  assert!(user.can_access_channel("any.channel.at.all"));
  assert!(user.describe().contains("&*"));

  OK
}

#[test]
fn test_batch_del_users_and_protection() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user("user_a", &["on"])?;
  acl.set_user("user_b", &["on"])?;
  acl.set_user("user_c", &["on"])?;

  // 批量删除 user_a 和 user_b（以及不存在的用户）
  let deleted_count = acl.del_users(&["user_a", "user_b", "nonexistent"])?;
  assert_eq!(deleted_count, 2);
  assert!(acl.get_user("user_a").is_none());
  assert!(acl.get_user("user_b").is_none());
  assert!(acl.get_user("user_c").is_some());

  // 包含 default 用户时必须拦截报错（与 C# 一致：用户名精确匹配，大小写敏感）
  let res = acl.del_users(&["user_c", "default", "user_d"]);
  assert!(matches!(res, Err(Error::DefaultUserProtected)));
  // default 必须依然存在
  assert!(acl.get_user("default").is_some());
  // 原子性保障：排在 default 前面的 user_c 绝不能被部分删除！
  assert!(acl.get_user("user_c").is_some());

  // 大小写变体不是系统保留用户：可正常创建与删除（用户名二进制安全、精确匹配）
  acl.set_user("DeFaUlT", &["on"])?;
  assert!(acl.del_user("DeFaUlT")?);

  OK
}

#[test]
fn test_authenticate_flow_and_session() -> Void {
  let acl = AccessControlList::new("def_pass_999");
  acl.set_user("operator", &["on", ">oper_pass", "+@read"])?;
  acl.set_user("nopass_user", &["on", "nopass", "+@read"])?;
  acl.set_user("disabled_user", &["off", ">pwd", "+@all"])?;

  // 1. 无用户名单密码模式（默认验证 default 用户）
  let def_sess = acl.authenticate(None, "def_pass_999");
  assert!(def_sess.is_some());
  assert_eq!(def_sess.unwrap().name(), "default");

  let wrong_def = acl.authenticate(None, "bad_pass");
  assert!(wrong_def.is_none());

  // 2. 指定用户名与密码模式
  let oper_sess = acl.authenticate(Some("operator"), "oper_pass");
  assert!(oper_sess.is_some());
  assert_eq!(oper_sess.unwrap().name(), "operator");

  let wrong_oper = acl.authenticate(Some("operator"), "wrong_pass");
  assert!(wrong_oper.is_none());

  // 3. nopass 用户使用任意密码认证通过
  let nopass_sess = acl.authenticate(Some("nopass_user"), "any_arbitrary_secret");
  assert!(nopass_sess.is_some());

  // 4. disabled 用户认证拒绝
  let dis_sess = acl.authenticate(Some("disabled_user"), "pwd");
  assert!(dis_sess.is_none());

  // 5. 不存在用户认证拒绝（包含 dummy 耗时防时序枚举）
  let ghost_sess = acl.authenticate(Some("ghost_user"), "pwd");
  assert!(ghost_sess.is_none());

  OK
}

#[test]
fn test_concurrent_user_creation_no_lost_updates() -> Void {
  use std::{sync::Arc, thread};

  let acl = Arc::new(AccessControlList::new(""));

  // 多线程并发对同一新用户 set_user，验证规则原子合并且无更新丢失
  let t1 = {
    let acl = acl.clone();
    thread::spawn(move || acl.set_user("shared_user", &["on", ">pwd_from_t1", "+get"]))
  };
  let t2 = {
    let acl = acl.clone();
    thread::spawn(move || acl.set_user("shared_user", &["on", ">pwd_from_t2", "+set"]))
  };

  t1.join().unwrap()?;
  t2.join().unwrap()?;

  let handle = acl.get_user("shared_user").unwrap();
  let user = handle.snapshot();
  assert!(user.is_enabled());
  // 验证两个线程添加的密码均成功生效
  assert!(user.authenticate("pwd_from_t1"));
  assert!(user.authenticate("pwd_from_t2"));
  // 验证两个线程添加的命令权限均生效
  assert!(user.can_execute(RespCommand::GET));
  assert!(user.can_execute(RespCommand::SET));

  OK
}

#[test]
fn test_round2_boundary_and_corner_cases() -> Void {
  use wedb_acl::glob_match;

  // 1. Glob 极端模式匹配测试（对齐 Garnet GlobUtils 规范：空目标非空模式返回 false；仅 ^ 为取反）
  assert!(!glob_match(b"****", b""));
  assert!(glob_match(b"****", b"hello_world"));
  assert!(glob_match(b"*a*b*c*", b"111a222b333c444"));
  assert!(!glob_match(b"*a*b*c*", b"111a222c333b444"));
  assert!(glob_match(b"[!0-9]", b"!"));
  assert!(!glob_match(b"[!0-9]", b"a"));
  assert!(glob_match(b"[^a-z]", b"A"));
  assert!(!glob_match(b"[^a-z]", b"x"));
  assert!(glob_match(b"[\\]]", b"]"));
  assert!(glob_match(b"[\\\\]", b"\\"));
  assert!(glob_match(b"\\*hello", b"*hello"));
  assert!(!glob_match(b"\\*hello", b"foo_hello"));

  // 2. KeyPattern 自动合并去重逻辑校验
  let mut user = User::new("merger");
  user.set_enabled(true);
  AclParser::apply_op(&mut user, "%R~db:shared:*")?;
  assert!(user.can_access_key("db:shared:1", false));
  assert!(!user.can_access_key("db:shared:1", true));

  // 追加只写权限后，自动合成为读写模式 ~db:shared:*
  AclParser::apply_op(&mut user, "%W~db:shared:*")?;
  assert!(user.can_access_key("db:shared:1", false));
  assert!(user.can_access_key("db:shared:1", true));
  let desc = user.describe();
  assert!(desc.contains("~db:shared:*"));
  assert!(!desc.contains("%R~db:shared:*"));
  assert!(!desc.contains("%W~db:shared:*"));

  // 3. 自定义超长命令名（>64 字符）与大小写不敏感
  let long_cmd = "CUSTOM.EXTREMELY.LONG.MODULE.OPERATION.COMMAND.NAME.FOR.STRESS.TESTING";
  user.add_custom_command(long_cmd)?;
  assert!(user.can_execute_custom(
    RespCommand::CustomRawStringCmd,
    &long_cmd.to_ascii_lowercase()
  ));
  user.remove_custom_command(long_cmd)?;
  assert!(!user.can_execute_custom(RespCommand::CustomRawStringCmd, long_cmd));

  // 4. GENPASS 各奇偶位数精确长度验证
  for bits in [1, 2, 3, 4, 7, 8, 9, 15, 16, 17, 63, 64, 255, 256, 4096] {
    let pass = AclPassword::genpass(Some(bits))?;
    let expected_len = bits.div_ceil(4);
    assert_eq!(pass.len(), expected_len, "bits={bits} 产生长度不匹配");
    assert!(pass.chars().all(|c| c.is_ascii_hexdigit()));
  }

  OK
}
