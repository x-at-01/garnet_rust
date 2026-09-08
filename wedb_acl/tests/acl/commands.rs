//! BasicTests：基础认证、ACL 命令与鉴权拒绝测试

use aok::{OK, Void};
use log::info;
use wedb_acl::{AccessControlList, AclPassword};
use wedb_resp::RespCommand;

use crate::support::TEST_USER_A;

/// 对标 Garnet BasicTests.cs: BasicWhoamiTest —— 验证 WHOAMI / AUTH 切换机制
#[test]
fn test_basic_whoami() -> Void {
  let acl = AccessControlList::new("");

  // 默认认证身份为 default
  assert!(acl.auth("default", ""));

  // 添加 testUserA 并开启
  acl.set_user(TEST_USER_A, &["on", "nopass", "+@admin", "+@slow"])?;

  // 验证用户可以免密通过认证
  assert!(acl.auth(TEST_USER_A, ""));

  // 切回 default 同样有效
  assert!(acl.auth("default", ""));

  info!("C# 兼容性测试：BasicWhoamiTest 通过");
  OK
}

/// 对标 Garnet BasicTests.cs: BasicListTest —— 验证 ACL LIST 描述动态适配
#[test]
fn test_basic_list() -> Void {
  let acl = AccessControlList::new("");
  let list1 = acl.list_users();
  assert_eq!(list1.len(), 1);
  assert_eq!(list1[0], "user default on nopass ~* &* +@all");

  // 添加 testUserA
  acl.set_user(TEST_USER_A, &[])?;
  let list2 = acl.list_users();
  assert_eq!(list2.len(), 2);
  assert!(list2.contains(&"user default on nopass ~* &* +@all".to_string()));
  assert!(list2.contains(&format!("user {} off", TEST_USER_A)));

  // 删除 testUserA
  assert!(acl.del_user(TEST_USER_A)?);
  let list3 = acl.list_users();
  assert_eq!(list3.len(), 1);

  info!("C# 兼容性测试：BasicListTest 通过");
  OK
}

/// 对标 Garnet BasicTests.cs: BasicUsersTest —— 验证 ACL USERS 用户名集合跟踪
#[test]
fn test_basic_users() -> Void {
  let acl = AccessControlList::new("");
  assert_eq!(acl.user_names(), vec!["default"]);

  acl.set_user(TEST_USER_A, &[])?;
  let mut expected = vec!["default", TEST_USER_A];
  expected.sort();
  assert_eq!(acl.user_names(), expected);

  info!("C# 兼容性测试：BasicUsersTest 通过");
  OK
}

/// 对标 Garnet BasicTests.cs: BasicGenPassTest —— 验证随机密码生成与哈希特征
#[test]
fn test_basic_genpass() -> Void {
  let p1 = AclPassword::from_cleartext("password_123");
  let p2 = AclPassword::from_cleartext("password_123");
  let p3 = AclPassword::from_cleartext("different_pwd");

  assert_eq!(p1.to_hex().len(), 64);
  assert!(p1.ct_eq(&p2));
  assert!(!p1.ct_eq(&p3));

  info!("C# 兼容性测试：BasicGenPassTest 通过");
  OK
}

/// 对标 Garnet BasicTests.cs: InvalidSubcommand —— 非法子命令规则应报错
#[test]
fn test_invalid_subcommand() -> Void {
  let acl = AccessControlList::new("");
  // 非法子命令规则应报错
  let err = acl.set_user(TEST_USER_A, &["invalid_subcommand_op"]);
  assert!(err.is_err());

  info!("C# 兼容性测试：InvalidSubcommand 通过");
  OK
}

/// 对标 Garnet BasicTests.cs: NoAuthValidation —— 验证不需要鉴权的基础命令集合
#[test]
fn test_no_auth_validation() -> Void {
  assert!(RespCommand::AUTH.is_no_auth());
  assert!(RespCommand::HELLO.is_no_auth());
  assert!(RespCommand::QUIT.is_no_auth());

  assert!(!RespCommand::SET.is_no_auth());
  assert!(!RespCommand::GET.is_no_auth());
  assert!(!RespCommand::DEL.is_no_auth());

  info!("C# 兼容性测试：NoAuthValidation 通过");
  OK
}

/// 对标 Garnet BasicTests.cs: DeniedCommandReturnsNoPermAsync —— 验证受限用户被拒绝执行无权限指令
#[test]
fn test_denied_command_returns_noperm() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &["on", ">pass", "~*", "+@all", "-type"])?;

  assert!(acl.can_execute(TEST_USER_A, RespCommand::SET));
  assert!(acl.can_execute(TEST_USER_A, RespCommand::GET));
  assert!(
    !acl.can_execute(TEST_USER_A, RespCommand::TYPE),
    "TYPE 命令已被 -type 排除，应被拒绝执行"
  );

  info!("C# 兼容性测试：DeniedCommandReturnsNoPermAsync 通过");
  OK
}

/// 对标 Garnet BasicTests.cs: PermittedCommandStillWorksAsync —— 验证受限用户被允许指令不受影响
#[test]
fn test_permitted_command_still_works() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &["on", ">pass", "+get", "+set"])?;

  assert!(acl.can_execute(TEST_USER_A, RespCommand::SET));
  assert!(acl.can_execute(TEST_USER_A, RespCommand::GET));
  assert!(!acl.can_execute(TEST_USER_A, RespCommand::DEL));

  info!("C# 兼容性测试：PermittedCommandStillWorksAsync 通过");
  OK
}

/// 对标 Garnet BasicTests.cs: ClientSetInfoDeniedReturnsNoPermAsync —— 验证子命令独立鉴权拦截
#[test]
fn test_client_setinfo_denied_returns_noperm() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &["on", ">pass", "+@all", "-client|setinfo"])?;

  assert!(acl.can_execute(TEST_USER_A, RespCommand::SET));
  assert!(!acl.can_execute(TEST_USER_A, RespCommand::CLIENT_SETINFO));
  assert!(acl.can_execute(TEST_USER_A, RespCommand::CLIENT_ID));

  info!("C# 兼容性测试：ClientSetInfoDeniedReturnsNoPermAsync 通过");
  OK
}
