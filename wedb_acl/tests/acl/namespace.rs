//! 名字空间隔离与防提权测试（对标 doc/zh/ns.md 多租户隔离规范）

use aok::{OK, Void};
use wedb_acl::{AccessControlList, Error, MAX_TENANT_NAMESPACE, NamespaceScope, User, parse_ns};
use wedb_resp::RespCommand;

/// 同名用户在不同名字空间互不可见、互不影响（隔离性核心校验）
#[test]
fn test_same_name_users_isolated_across_namespaces() -> Void {
  let acl = AccessControlList::new("");
  acl
    .scope(Some(1))
    .set_user("alice", &["on", ">pwd1", "+get"])?;
  acl
    .scope(Some(2))
    .set_user("alice", &["on", ">pwd2", "+set"])?;

  // 互不可见：跨空间点查返回 None
  assert!(acl.scope(Some(1)).get_user("alice").is_some());
  assert!(acl.scope(Some(2)).get_user("alice").is_some());
  // 超管全局桶没有该用户
  assert!(acl.get_user("alice").is_none());

  // 凭据互不通用
  assert!(!acl.scope(Some(1)).auth("alice", "pwd2"));
  assert!(!acl.scope(Some(2)).auth("alice", "pwd1"));

  // 命令权限互不影响
  assert!(acl.scope(Some(1)).can_execute("alice", RespCommand::GET));
  assert!(!acl.scope(Some(1)).can_execute("alice", RespCommand::SET));
  assert!(acl.scope(Some(2)).can_execute("alice", RespCommand::SET));
  assert!(!acl.scope(Some(2)).can_execute("alice", RespCommand::GET));

  // 键与频道访问互不影响
  acl.scope(Some(1)).set_user("alice", &["~ns1:*"])?;
  acl.scope(Some(2)).set_user("alice", &["~ns2:*"])?;
  assert!(acl.scope(Some(1)).can_access_key("alice", b"ns1:k", false));
  assert!(!acl.scope(Some(1)).can_access_key("alice", b"ns2:k", false));
  assert!(acl.scope(Some(2)).can_access_key("alice", b"ns2:k", false));
  assert!(!acl.scope(Some(2)).can_access_key("alice", b"ns1:k", false));

  // 删除互不影响
  assert!(acl.scope(Some(1)).del_user("alice")?);
  assert!(acl.scope(Some(1)).get_user("alice").is_none());
  assert!(acl.scope(Some(2)).get_user("alice").is_some());

  // 名字空间列表互相隔离
  assert_eq!(acl.scope(Some(1)).user_names(), Vec::<String>::new());
  assert_eq!(acl.scope(Some(2)).user_names(), vec!["alice"]);

  OK
}

/// 超管视界：缺省操作全局桶，`ns` 规则可跨空间管理租户用户
#[test]
fn test_admin_scope_and_cross_namespace_rules() -> Void {
  let acl = AccessControlList::new("");

  // 超管缺省创建：落入全局桶（namespace = None）
  acl.set_user("admin_ops", &["on", ">pwd", "+get"])?;
  let handle = acl
    .get_user("admin_ops")
    .expect("admin_ops 必须存在于全局桶");
  assert_eq!(
    handle.read().namespace,
    None,
    "未声明 ns 时默认必须是超级用户 (namespace = None)"
  );
  assert!(acl.scope(None).find_user("admin_ops").is_ok());
  assert!(acl.scope(None).auth("admin_ops", "pwd"));
  assert_eq!(
    acl
      .scope(None)
      .authenticate("admin_ops", "pwd")
      .unwrap()
      .read()
      .namespace,
    None
  );

  // 超管显式创建租户用户
  acl.set_user("tenant_user", &["on", ">pwd", "ns", "5"])?;
  // 全局桶不可见，租户桶可见
  assert!(acl.get_user("tenant_user").is_none());
  assert!(acl.scope(Some(5)).auth("tenant_user", "pwd"));

  // 超管在租户桶内增量修改（描述与状态正确保持）
  acl.scope(Some(5)).set_user("tenant_user", &["+get"])?;
  assert!(
    acl
      .scope(Some(5))
      .can_execute("tenant_user", RespCommand::GET)
  );

  // 超管可管理超管级用户（ns none）
  acl.set_user("super_auditor", &["on", ">pwd", "ns", "none"])?;
  assert_eq!(
    acl.scope(None).find_user("super_auditor")?.read().namespace,
    None
  );

  // default 用户固定为超管级
  assert_eq!(acl.default_user().read().namespace, None);

  OK
}

/// 租户沙箱防越权提权：ns 规则仅允许绑定自身沙箱
#[test]
fn test_tenant_privilege_escalation_blocked() -> Void {
  let acl = AccessControlList::new("");

  // 绑定其他租户 → 拒绝
  assert!(matches!(
    acl.scope(Some(2)).set_user("evil", &["on", "ns", "3"]),
    Err(Error::NamespaceDenied)
  ));
  // 绑定超管全局视界 → 拒绝
  assert!(matches!(
    acl.scope(Some(2)).set_user("evil", &["on", "ns", "none"]),
    Err(Error::NamespaceDenied)
  ));
  assert!(matches!(
    acl.scope(Some(2)).set_user("evil", &["on", "ns", "all"]),
    Err(Error::NamespaceDenied)
  ));
  // 越权用户绝不能被创建
  assert!(acl.get_user("evil").is_none());
  assert!(acl.scope(Some(2)).get_user("evil").is_none());
  assert!(acl.scope(Some(3)).get_user("evil").is_none());

  // 绑定自身沙箱 → 允许（含省略 ns 的缺省绑定）
  acl
    .scope(Some(2))
    .set_user("worker", &["on", ">pwd", "ns", "2"])?;
  assert_eq!(
    acl.scope(Some(2)).find_user("worker")?.read().namespace,
    Some(2)
  );
  acl.scope(Some(2)).set_user("worker2", &["on", ">pwd"])?;
  assert_eq!(
    acl.scope(Some(2)).find_user("worker2")?.read().namespace,
    Some(2)
  );

  // 名字空间绑定创建后不可变：重绑异值被拒
  assert!(matches!(
    acl.scope(Some(2)).set_user("worker", &["ns", "none"]),
    Err(Error::NamespaceDenied)
  ));

  OK
}

/// 名字空间绑定值解析边界
#[test]
fn test_ns_binding_value_boundaries() -> Void {
  let acl = AccessControlList::new("");

  // 0 为控制面自动分配保留值 → 拒绝
  assert!(matches!(
    acl.set_user("u0", &["ns", "0"]),
    Err(Error::InvalidNamespace(_))
  ));
  // 超过租户上限 → 拒绝
  assert!(matches!(
    acl.set_user("u1", &["ns", "18446744073709551615"]),
    Err(Error::InvalidNamespace(_))
  ));
  // 非法字面量 → 拒绝
  assert!(matches!(
    acl.set_user("u2", &["ns", "-3"]),
    Err(Error::InvalidNamespace(_))
  ));
  assert!(matches!(
    acl.set_user("u3", &["ns", "abc"]),
    Err(Error::InvalidNamespace(_))
  ));
  // 缺失绑定值 → Malformed
  assert!(matches!(
    acl.set_user("u4", &["on", "ns"]),
    Err(Error::InvalidRule(_))
  ));

  // 上限值本身合法
  acl.set_user("u5", &["ns", "18446744073709550591"])?;
  assert_eq!(
    acl
      .scope(Some(MAX_TENANT_NAMESPACE))
      .find_user("u5")?
      .read()
      .namespace,
    Some(MAX_TENANT_NAMESPACE)
  );

  // parse_ns 直测
  assert_eq!(parse_ns("None").unwrap(), None);
  assert_eq!(parse_ns("1").unwrap(), Some(1));
  assert!(parse_ns("1.5").is_err());

  OK
}

/// describe / set_user_from_line / bitcode 三方往返：ns 绑定无损
#[test]
fn test_namespace_describe_roundtrip() -> Void {
  let acl = AccessControlList::new("");
  acl.set_user("mall", &["on", ">pw1", "~data:*", "+@string", "ns", "42"])?;

  // ACL LIST 单行携带 ns 标记且可无损重放
  let line = acl
    .list_users()
    .into_iter()
    .find(|l| l.contains("user mall"))
    .expect("mall 必须在列表中");
  assert!(line.contains("ns 42"), "describe 缺少 ns 标记: {line}");

  let replayed = AccessControlList::new("");
  replayed.set_user_from_line(&line)?;
  let mall = replayed.scope(Some(42)).find_user("mall")?;
  let snapshot = mall.snapshot();
  assert_eq!(snapshot.namespace, Some(42));
  assert!(snapshot.authenticate("pw1"));
  assert!(snapshot.can_execute(RespCommand::GET));
  assert!(snapshot.can_access_key(b"data:1", false));

  // describe_rules 携带 ns 标记（GETUSER 规则导出）
  let rules = snapshot.describe_rules();
  assert!(rules.iter().any(|r| r == "ns 42"));

  // 超管级用户 describe 省略 ns 标记
  acl.set_user("plain", &["on"])?;
  let plain_line = acl
    .list_users()
    .into_iter()
    .find(|l| l.contains("user plain"))
    .unwrap();
  assert_eq!(plain_line, "user plain on");

  // bitcode 序列化保留名字空间
  let bytes = snapshot.to_bitcode();
  let decoded = User::from_bitcode(&bytes)?;
  assert_eq!(decoded.namespace, Some(42));

  OK
}

/// ACL USERS / LIST 全量视图包含租户用户，作用域视图只含本桶
#[test]
fn test_global_listing_includes_all_namespaces() -> Void {
  let acl = AccessControlList::new("");
  acl.scope(Some(1)).set_user("w1", &["on"])?;
  acl.scope(Some(2)).set_user("w2", &["on", "ns", "2"])?;

  // 全局 user_names 包含所有租户用户
  let names = acl.user_names();
  assert!(names.contains(&"w1".to_string()));
  assert!(names.contains(&"w2".to_string()));

  // 全局 LIST 行包含 ns 标记
  let list = acl.list_users();
  assert!(
    list
      .iter()
      .any(|l| l.starts_with("user w2 ") && l.contains("ns 2"))
  );

  // 作用域列表互相隔离
  assert!(
    acl
      .scope(Some(1))
      .list_users()
      .iter()
      .all(|l| !l.contains("ns 2"))
  );
  assert_eq!(acl.scope(Some(1)).user_names(), vec!["w1"]);
  assert_eq!(acl.scope(Some(2)).user_names(), vec!["w2"]);

  // 作用域句柄类型可直接构造引用比较
  let s1: NamespaceScope<'_> = acl.scope(Some(1));
  assert_eq!(s1.ns(), Some(1));

  OK
}

/// 租户桶内的 default 用户与系统 default 完全隔离
#[test]
fn test_tenant_default_user_is_independent() -> Void {
  let acl = AccessControlList::new("");
  // 租户可创建自己沙箱内的 "default" 用户（与系统保留 default 无关）
  acl
    .scope(Some(7))
    .set_user("default", &["on", ">tenant_pwd"])?;
  assert!(acl.scope(Some(7)).auth("default", "tenant_pwd"));

  // 系统 default 依然完好：免密、全权限
  assert!(acl.auth_default("anything"));
  assert!(acl.scope(Some(7)).del_user("default")?);
  assert!(acl.scope(Some(7)).get_user("default").is_none());
  assert!(acl.get_user("default").is_some());
  assert!(matches!(
    acl.del_user("default"),
    Err(Error::DefaultUserProtected)
  ));

  OK
}

/// 已有用户名字空间绝对不可变，跨空间必须删除重建；未声明 ns 新建用户继承当前 scope
#[test]
fn test_user_namespace_immutability_and_scope_inheritance() -> Void {
  let acl = AccessControlList::new("");

  // 1. 未声明 ns 新建用户继承当前 scope
  acl.scope(Some(2)).set_user("u2_inherit", &["on", ">pwd"])?;
  let u2 = acl.scope(Some(2)).find_user("u2_inherit")?;
  assert_eq!(u2.read().namespace, Some(2));

  acl.scope(Some(5)).set_user("u5_inherit", &["on", ">pwd"])?;
  let u5 = acl.scope(Some(5)).find_user("u5_inherit")?;
  assert_eq!(u5.read().namespace, Some(5));

  // 2. 已有用户在相同名字空间下增量修改其他规则 → 成功
  acl.scope(Some(2)).set_user("u2_inherit", &["+get"])?;
  assert!(
    acl
      .scope(Some(2))
      .can_execute("u2_inherit", RespCommand::GET)
  );
  assert_eq!(
    acl.scope(Some(2)).find_user("u2_inherit")?.read().namespace,
    Some(2)
  );

  // 显式带与自身相同的 ns 规则修改 → 成功
  acl
    .scope(Some(2))
    .set_user("u2_inherit", &["ns", "2", "+set"])?;
  assert!(
    acl
      .scope(Some(2))
      .can_execute("u2_inherit", RespCommand::SET)
  );
  assert_eq!(
    acl.scope(Some(2)).find_user("u2_inherit")?.read().namespace,
    Some(2)
  );

  // 3. 已有用户尝试篡改 ns 规则 → 失败（NamespaceDenied）
  assert!(matches!(
    acl.scope(Some(2)).set_user("u2_inherit", &["ns", "3"]),
    Err(Error::NamespaceDenied)
  ));
  assert!(matches!(
    acl.scope(Some(2)).set_user("u2_inherit", &["ns", "none"]),
    Err(Error::NamespaceDenied)
  ));
  assert!(matches!(
    acl.scope(Some(2)).set_user("u2_inherit", &["ns", "all"]),
    Err(Error::NamespaceDenied)
  ));

  // 验证用户状态完好未被破坏
  assert_eq!(
    acl.scope(Some(2)).find_user("u2_inherit")?.read().namespace,
    Some(2)
  );

  // 4. 超管全局视界下修改已有租户用户：使用相同 ns 2 增量更新合法
  acl
    .scope(None)
    .set_user("u2_inherit", &["ns", "2", "+del"])?;
  assert!(
    acl
      .scope(Some(2))
      .can_execute("u2_inherit", RespCommand::DEL)
  );

  OK
}

/// 超长用户名（> 64 字节栈缓冲容量）在作用域视图下的生命周期与隔离性
#[test]
fn test_long_username_stack_fallback_and_isolation() -> Void {
  let acl = AccessControlList::new("");
  let long_name = "user_".to_string() + &"a".repeat(100);

  // 租户沙箱创建超长用户
  acl
    .scope(Some(9))
    .set_user(&long_name, &["on", ">pwd_long", "+get"])?;

  assert!(acl.scope(Some(9)).auth(&long_name, "pwd_long"));
  assert!(!acl.scope(Some(9)).auth(&long_name, "wrong_pwd"));
  assert!(acl.scope(Some(10)).get_user(&long_name).is_none());
  assert!(acl.get_user(&long_name).is_none());

  let names = acl.scope(Some(9)).user_names();
  assert_eq!(names, vec![long_name.clone()]);

  let list = acl.scope(Some(9)).list_users();
  assert_eq!(list.len(), 1);
  assert!(list[0].contains(&long_name));
  assert!(list[0].contains("ns 9"));

  assert!(acl.scope(Some(9)).del_user(&long_name)?);
  assert!(acl.scope(Some(9)).get_user(&long_name).is_none());

  OK
}
