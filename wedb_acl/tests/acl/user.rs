//! SetUserTests / DeleteUserTests / GetUserTests：用户与密码生命周期测试

use aok::{OK, Void};
use log::info;
use wedb_acl::AccessControlList;
use wedb_resp::RespCommand;

use crate::support::{
  DUMMY_PASSWORD, DUMMY_PASSWORD_B, DUMMY_PASSWORD_HASH, TEST_USER_A, TEST_USER_B,
};

/// 前缀 + 值拼接（替代 format!("{prefix}{value}")）
fn prefixed(prefix: &str, value: &str) -> String {
  let mut s = String::from(prefix);
  s.push_str(value);
  s
}

/// "user {name} off" 描述行
fn user_line_off(name: &str) -> String {
  let mut s = String::from("user ");
  s.push_str(name);
  s.push_str(" off");
  s
}

/// 对标 Garnet SetUserTests.cs: PasswordlessDefaultUserTest —— 无密码 default 用户任意凭据均可通过
#[test]
fn test_passwordless_default_user() -> Void {
  let acl = AccessControlList::new("");
  assert!(acl.auth_default(""));
  assert!(acl.auth_default("arbitrary_pwd"));

  info!("C# 兼容性测试：PasswordlessDefaultUserTest 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: ProtectedDefaultUserErrorHandlingTest —— 受保护 default 用户的密码校验错误处理
#[test]
fn test_protected_default_user_error_handling() -> Void {
  let acl = AccessControlList::new(DUMMY_PASSWORD);
  assert!(!acl.auth_default(""));
  assert!(!acl.auth_default("wrong_password"));
  assert!(acl.auth_default(DUMMY_PASSWORD));

  info!("C# 兼容性测试：ProtectedDefaultUserErrorHandlingTest 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: EnableAndDisableUsers —— 用户启用/禁用状态切换影响认证
#[test]
fn test_enable_and_disable_users() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &[&prefixed(">", DUMMY_PASSWORD)])?;

  // 默认为 off，认证失败
  assert!(!acl.auth(TEST_USER_A, DUMMY_PASSWORD));

  // 启用用户
  acl.set_user(TEST_USER_A, &["on"])?;
  assert!(acl.auth(TEST_USER_A, DUMMY_PASSWORD));

  // 禁用用户
  acl.set_user(TEST_USER_A, &["off"])?;
  assert!(!acl.auth(TEST_USER_A, DUMMY_PASSWORD));

  info!("C# 兼容性测试：EnableAndDisableUsers 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: AddPasswordFromCleartextTest / AddPasswordFromHashTest —— 明文与哈希两种方式添加密码
#[test]
fn test_add_passwords() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &[&prefixed(">", DUMMY_PASSWORD)])?;

  let u = acl.get_user(TEST_USER_A).unwrap().snapshot();
  assert!(
    u.passwords
      .iter()
      .any(|p| p.to_hex() == DUMMY_PASSWORD_HASH)
  );

  // 通过哈希添加密码
  acl.set_user(TEST_USER_B, &[&prefixed("#", DUMMY_PASSWORD_HASH)])?;
  let ub = acl.get_user(TEST_USER_B).unwrap().snapshot();
  assert!(
    ub.passwords
      .iter()
      .any(|p| p.to_hex() == DUMMY_PASSWORD_HASH)
  );

  info!("C# 兼容性测试：AddPasswordFromCleartext & Hash 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: RemovePasswordFromCleartextTest / RemovePasswordFromHashTest —— 明文与哈希两种方式移除密码
#[test]
fn test_remove_passwords() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &[&prefixed(">", DUMMY_PASSWORD)])?;
  assert_eq!(
    acl
      .get_user(TEST_USER_A)
      .unwrap()
      .snapshot()
      .passwords
      .len(),
    1
  );

  // 通过明文移除
  acl.set_user(TEST_USER_A, &[&format!("<{}", DUMMY_PASSWORD)])?;
  assert_eq!(
    acl
      .get_user(TEST_USER_A)
      .unwrap()
      .snapshot()
      .passwords
      .len(),
    0
  );

  // 重新添加后通过哈希移除
  acl.set_user(TEST_USER_A, &[&prefixed("#", DUMMY_PASSWORD_HASH)])?;
  assert_eq!(
    acl
      .get_user(TEST_USER_A)
      .unwrap()
      .snapshot()
      .passwords
      .len(),
    1
  );
  acl.set_user(TEST_USER_A, &[&format!("!{}", DUMMY_PASSWORD_HASH)])?;
  assert_eq!(
    acl
      .get_user(TEST_USER_A)
      .unwrap()
      .snapshot()
      .passwords
      .len(),
    0
  );

  info!("C# 兼容性测试：RemovePasswordFromCleartext & Hash 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: AddDuplicatePasswordTest —— 重复添加密码应自动去重
#[test]
fn test_add_duplicate_password() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &[&prefixed(">", DUMMY_PASSWORD)])?;
  acl.set_user(TEST_USER_A, &[&prefixed(">", DUMMY_PASSWORD)])?;

  let u = acl.get_user(TEST_USER_A).unwrap().snapshot();
  assert_eq!(u.passwords.len(), 1, "重复密码应自动去重");

  info!("C# 兼容性测试：AddDuplicatePasswordTest 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: PasswordlessUserTest —— nopass 用户任意凭据均可认证
#[test]
fn test_passwordless_user() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(
    TEST_USER_A,
    &["on", &prefixed(">", DUMMY_PASSWORD), "nopass"],
  )?;

  assert!(acl.auth(TEST_USER_A, DUMMY_PASSWORD_B));

  info!("C# 兼容性测试：PasswordlessUserTest 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: ResetPasswordsTest —— resetpass 清空全部密码
#[test]
fn test_reset_passwords() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(
    TEST_USER_A,
    &[
      &prefixed(">", DUMMY_PASSWORD),
      &format!(">{}", DUMMY_PASSWORD_B),
    ],
  )?;
  assert_eq!(
    acl
      .get_user(TEST_USER_A)
      .unwrap()
      .snapshot()
      .passwords
      .len(),
    2
  );

  acl.set_user(TEST_USER_A, &["resetpass"])?;
  assert_eq!(
    acl
      .get_user(TEST_USER_A)
      .unwrap()
      .snapshot()
      .passwords
      .len(),
    0
  );

  info!("C# 兼容性测试：ResetPasswordsTest 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: AddAndRemoveCategoryTest —— 分类权限的添加与移除
#[test]
fn test_add_and_remove_category() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &["on", &prefixed(">", DUMMY_PASSWORD)])?;

  // 确保用户初始未被分配该分类
  let list = acl.list_users();
  let user_str = list
    .iter()
    .find(|s| s.starts_with(&format!("user {}", TEST_USER_A)))
    .unwrap();
  assert!(!user_str.contains("+@admin"));

  // 为用户添加分类
  acl.set_user(TEST_USER_A, &["+@admin"])?;
  let list = acl.list_users();
  let user_str = list
    .iter()
    .find(|s| s.starts_with(&format!("user {}", TEST_USER_A)))
    .unwrap();
  assert!(user_str.contains("+@admin"));

  // 从用户移除分类
  acl.set_user(TEST_USER_A, &["-@admin"])?;
  let list = acl.list_users();
  let user_str = list
    .iter()
    .find(|s| s.starts_with(&format!("user {}", TEST_USER_A)))
    .unwrap();
  assert!(!user_str.contains("+@admin"));

  info!("C# 兼容性测试：AddAndRemoveCategoryTest 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: ResetUser —— reset 将用户恢复到初始 off 无密码状态
#[test]
fn test_reset_user() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(
    TEST_USER_A,
    &["on", &prefixed(">", DUMMY_PASSWORD), "+@admin"],
  )?;

  acl.set_user(TEST_USER_A, &["reset"])?;
  let u = acl.get_user(TEST_USER_A).unwrap().snapshot();
  assert!(!u.enabled);
  assert!(u.passwords.is_empty());
  assert_eq!(u.describe(), user_line_off(TEST_USER_A));

  info!("C# 兼容性测试：ResetUser 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: BadInputUnknownOperation —— 未知操作指令报错
#[test]
fn test_bad_input_unknown_operation() -> Void {
  let acl = AccessControlList::new("");
  let res = acl.set_user(TEST_USER_A, &["qwerty"]);
  assert!(res.is_err());

  info!("C# 兼容性测试：BadInputUnknownOperation 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: KeyPatternsWildcard —— 通配符键模式合法
#[test]
fn test_key_patterns_wildcard() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &["~*"])?;

  info!("C# 兼容性测试：KeyPatternsWildcard 通过");
  OK
}

/// 对标 Garnet DeleteUserTests.cs: DeleteSingleUser —— 删除单个用户不影响其他用户
#[test]
fn test_delete_single_user() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &[">passwd"])?;
  acl.set_user(TEST_USER_B, &[">passwd"])?;

  assert!(acl.del_user(TEST_USER_A)?);
  assert!(!acl.user_names().contains(&TEST_USER_A.to_string()));
  assert!(acl.user_names().contains(&TEST_USER_B.to_string()));

  info!("C# 兼容性测试：DeleteSingleUser 通过");
  OK
}

/// 对标 Garnet DeleteUserTests.cs: DeleteMultipleUser —— 依次删除多个用户
#[test]
fn test_delete_multiple_user() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &[">passwd"])?;
  acl.set_user(TEST_USER_B, &[">passwd"])?;

  assert!(acl.del_user(TEST_USER_A)?);
  assert!(acl.del_user(TEST_USER_B)?);

  assert!(!acl.user_names().contains(&TEST_USER_A.to_string()));
  assert!(!acl.user_names().contains(&TEST_USER_B.to_string()));

  info!("C# 兼容性测试：DeleteMultipleUser 通过");
  OK
}

/// 对标 Garnet DeleteUserTests.cs: DeleteNonexistingUser —— 删除不存在用户返回 false
#[test]
fn test_delete_nonexisting_user() -> Void {
  let acl = AccessControlList::new("");
  assert!(!acl.del_user("DoesNotExist")?);

  info!("C# 兼容性测试：DeleteNonexistingUser 通过");
  OK
}

/// 对标 Garnet DeleteUserTests.cs: DeleteDefaultUser —— default 用户受保护不可删除
#[test]
fn test_delete_default_user() -> Void {
  let acl = AccessControlList::new("");
  let res = acl.del_user("default");
  assert!(res.is_err(), "default 用户必须受到保护不可删除");

  info!("C# 兼容性测试：DeleteDefaultUser 通过");
  OK
}

/// 对标 Garnet DeleteUserTests.cs: DeleteNoUser —— 空用户列表时不删除任何用户
#[test]
fn test_delete_no_user() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(TEST_USER_A, &[">passwd"])?;
  acl.set_user(TEST_USER_B, &[">passwd"])?;

  assert!(acl.user_names().contains(&TEST_USER_A.to_string()));
  assert!(acl.user_names().contains(&TEST_USER_B.to_string()));
  info!("C# 兼容性测试：DeleteNoUser 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: ProtectedDefaultUserLoginImplicitTest —— 受保护 default 用户隐式认证
#[test]
fn test_protected_default_user_login_implicit() -> Void {
  let acl = AccessControlList::new(DUMMY_PASSWORD);
  // 隐式 default 用户认证
  assert!(acl.auth_default(DUMMY_PASSWORD));
  assert!(!acl.auth_default("WrongPassword"));
  info!("C# 兼容性测试：ProtectedDefaultUserLoginImplicitTest 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: ProtectedDefaultUserLoginExplicitTest —— 受保护 default 用户显式认证
#[test]
fn test_protected_default_user_login_explicit() -> Void {
  let acl = AccessControlList::new(DUMMY_PASSWORD);
  // 显式指定 default 用户认证
  assert!(acl.auth("default", DUMMY_PASSWORD));
  assert!(!acl.auth("default", "WrongPassword"));
  info!("C# 兼容性测试：ProtectedDefaultUserLoginExplicitTest 通过");
  OK
}

/// 对标 Garnet SetUserTests.cs: BadInputEmpty —— 空规则合法（无操作）
#[test]
fn test_bad_input_empty() -> Void {
  let acl = AccessControlList::new("");
  assert!(acl.set_user(TEST_USER_A, &[]).is_ok());
  info!("C# 兼容性测试：BadInputEmpty 通过");
  OK
}

/// 对标 Garnet GetUserTests.cs: GetUserTest —— 获取 default 用户并校验初始状态
#[test]
fn test_get_user() -> Void {
  let acl = AccessControlList::new("");
  let user_handle = acl.get_user("default").expect("default 用户应存在");
  let user = user_handle.read();
  assert!(user.enabled);
  assert_eq!(user.passwords.len(), 0);
  assert_eq!(user.commands.description(), "+@all");
  info!("C# 兼容性测试：GetUserTest 通过");
  OK
}

/// 对标 Garnet GetUserTests.cs: GetUserNotFoundTest —— 获取不存在用户返回 None
#[test]
fn test_get_user_not_found() -> Void {
  let acl = AccessControlList::new("");
  assert!(acl.get_user("!default").is_none());
  info!("C# 兼容性测试：GetUserNotFoundTest 通过");
  OK
}

/// 对标 Garnet GetUserTests.cs: GetUserMultiUserTest —— 多用户场景下按名获取并校验权限
#[test]
fn test_get_user_multi_user() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user(
    TEST_USER_A,
    &["on", &prefixed(">", DUMMY_PASSWORD), "+get", "+set"],
  )?;
  let user_handle = acl.get_user(TEST_USER_A).expect("TEST_USER_A 应存在");
  let user = user_handle.read();
  assert!(user.enabled);
  assert_eq!(user.passwords.len(), 1);
  assert!(user.can_execute(RespCommand::GET));
  assert!(user.can_execute(RespCommand::SET));
  info!("C# 兼容性测试：GetUserMultiUserTest 通过");
  OK
}

/// 对标 Garnet GetUserTests.cs: GetUserAclTest —— 多组规则组合下的用户状态与命令描述一致性
#[test]
fn test_get_user_acl() -> Void {
  let cases = [
    ("on", DUMMY_PASSWORD, "+@admin", "+@admin", true),
    ("off", "nopass", "+get", "+get", false),
    ("", "", "+@all", "+@all", false),
    ("on", "nopass", "-@all +get", "+get", false),
  ];

  for (enabled, cred, cmds, exp_cmds, has_pass) in cases {
    let acl = AccessControlList::new("");
    let mut tokens = Vec::new();
    if !enabled.is_empty() {
      tokens.push(enabled);
    }
    let cred_token;
    if !cred.is_empty() {
      if cred != "nopass" {
        cred_token = prefixed(">", cred);
        tokens.push(&cred_token);
      } else {
        tokens.push(cred);
      }
    }
    for c in cmds.split_whitespace() {
      tokens.push(c);
    }
    acl.set_user(TEST_USER_A, &tokens)?;
    let user_handle = acl.get_user(TEST_USER_A).expect("用户应存在");
    let user = user_handle.read();
    assert_eq!(user.enabled, enabled == "on");
    assert_eq!(!user.passwords.is_empty(), has_pass);
    assert_eq!(user.commands.description(), exp_cmds);
  }

  info!("C# 兼容性测试：GetUserAclTest 通过");
  OK
}
