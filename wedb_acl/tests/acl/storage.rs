use std::sync::Arc;

use aok::{OK, Void};
use log::info;
use wedb_acl::{AccessControlList, AclStorage, MemAclStorage, User, user_key};
use wedb_resp::RespCommand;

/// 验证单用户 bitcode 点查、点写、删除
#[compio::test]
async fn test_storage_point_read_write_and_del() -> Void {
  let storage = Arc::new(MemAclStorage::new());

  let mut user = User::new("alice");
  user.add_password("secret_pass");
  user.enabled = true;

  let key = user_key(None, "alice");
  storage.put_user(&key, &user).await?;

  let loaded = storage.get_user(&key).await?.expect("user must exist");
  assert_eq!(loaded.name, "alice");
  assert!(loaded.authenticate("secret_pass"));
  assert!(!loaded.authenticate("wrong_pass"));

  assert_eq!(storage.user_count().await?, 1);

  assert!(storage.del_user(&key).await?);
  assert!(storage.get_user(&key).await?.is_none());
  assert_eq!(storage.user_count().await?, 0);

  info!("AclStorage: PointReadWriteAndDel 通过");
  OK
}

/// 验证千万级用户模拟分页拉取与计数 (避免一次性拉取导致 OOM)
#[compio::test]
async fn test_storage_pagination_and_count() -> Void {
  let storage = Arc::new(MemAclStorage::new());

  // 批量写入测试数据
  for i in 0..500 {
    let mut u = User::new(format!("user_{i:04}"));
    u.enabled = true;
    storage
      .put_user(&user_key(None, &format!("user_{i:04}")), &u)
      .await?;
  }

  assert_eq!(storage.user_count().await?, 500);

  // 分页拉取
  let page1 = storage.list_users_after(None, 10).await?;
  assert_eq!(page1.len(), 10);
  assert_eq!(page1[0], user_key(None, "user_0000"));
  assert_eq!(page1[9], user_key(None, "user_0009"));

  let page2 = storage
    .list_users_after(page1.last().map(|k| k.as_slice()), 10)
    .await?;
  assert_eq!(page2.len(), 10);
  assert_eq!(page2[0], user_key(None, "user_0010"));
  assert_eq!(page2[9], user_key(None, "user_0019"));

  info!("AclStorage: PaginationAndCount 通过");
  OK
}

/// 验证 ACL 管理器在存储模式下的按需懒加载 (对标千万级用户不驻留全部内存)
#[compio::test]
async fn test_acl_manager_lazy_loading() -> Void {
  let storage = Arc::new(MemAclStorage::new());
  let acl1 = AccessControlList::with_storage(storage.clone(), "rootpass");

  // 写入新用户
  acl1
    .set_user_async("bob", &["on", ">bob123", "+get", "+set"])
    .await?;
  assert!(acl1.auth("bob", "bob123"));

  // 创建全新 ACL 实例挂载相同存储 (模拟重启或多节点，冷启动内存为空)
  let acl2 = AccessControlList::with_storage(storage.clone(), "rootpass");
  acl2.init_from_storage(Some("rootpass")).await?;

  // 刚启动时，非 default 用户未在内存中
  assert!(acl2.get_user("bob").is_none());

  // 异步点查触发数据库懒加载并回填内存缓存（get_user_async 未命中即点查）
  let bob = acl2
    .get_user_async("bob")
    .await?
    .expect("bob 应经懒加载回填");
  assert!(bob.authenticate("bob123"));
  assert!(!bob.authenticate("wrongpwd"));

  // 懒加载后，内存字典中已存在用户句柄，后续同步鉴权直接命中
  let bob_handle = acl2.get_user("bob").expect("bob should be cached now");
  assert!(bob_handle.read().can_execute(RespCommand::GET));
  assert!(!bob_handle.read().can_execute(RespCommand::DEL));
  assert!(acl2.auth("bob", "bob123"));

  // 异步删除用户
  assert!(acl2.del_user_async("bob").await?);
  assert!(acl2.get_user_async("bob").await?.is_none());
  assert!(storage.get_user(&user_key(None, "bob")).await?.is_none());

  info!("AclStorage: LazyLoading 通过");
  OK
}

/// 验证冷启动时增量修改存量用户与冷启动直接删除
#[compio::test]
async fn test_cold_start_incremental_update_and_deletion() -> Void {
  let storage = Arc::new(MemAclStorage::new());
  let acl1 = AccessControlList::with_storage(storage.clone(), "rootpass");

  // 1. 创建用户 charlie 并赋予密码与 +get
  acl1
    .set_user_async("charlie", &["on", ">charliepass", "+get"])
    .await?;
  assert!(acl1.auth("charlie", "charliepass"));

  // 2. 模拟重启：创建全新 acl2 挂载同一 storage
  let acl2 = AccessControlList::with_storage(storage.clone(), "rootpass");
  acl2.init_from_storage(Some("rootpass")).await?;
  assert!(acl2.get_user("charlie").is_none());

  // 3. 在冷启动未载入内存的情况下，对 charlie 执行增量规则变更 (+set)
  acl2.set_user_async("charlie", &["+set"]).await?;

  // 校验 charlie 的原有状态并未被空白用户覆盖：密码与原有权限依旧有效，新权限也生效
  let charlie = acl2.get_user("charlie").expect("charlie must exist");
  assert!(charlie.read().is_enabled());
  assert!(charlie.read().authenticate("charliepass"));
  assert!(charlie.read().can_execute(RespCommand::GET));
  assert!(charlie.read().can_execute(RespCommand::SET));

  // 4. 模拟再次重启：创建 acl3 测试冷启动直接 del_user_async
  let acl3 = AccessControlList::with_storage(storage.clone(), "rootpass");
  acl3.init_from_storage(Some("rootpass")).await?;
  assert!(acl3.get_user("charlie").is_none());

  // 直接对未在内存中的用户执行 del_user_async，应成功返回 true
  assert!(acl3.del_user_async("charlie").await?);
  assert!(
    storage
      .get_user(&user_key(None, "charlie"))
      .await?
      .is_none()
  );
  // 再次删除应返回 false
  assert!(!acl3.del_user_async("charlie").await?);

  info!("AclStorage: ColdStartIncrementalUpdateAndDeletion 通过");
  OK
}

/// 验证名字空间用户的持久化与冷启动懒加载互不串扰（租户隔离 × 数据库持久化）
#[compio::test]
async fn test_storage_namespace_isolation_and_persistence() -> Void {
  let storage = Arc::new(MemAclStorage::new());
  let acl1 = AccessControlList::with_storage(storage.clone(), "rootpass");

  // 不同名字空间创建同名用户，互不覆盖
  acl1
    .scope(Some(1))
    .set_user_async("worker", &["on", ">pwd_ns1", "+get"])
    .await?;
  acl1
    .scope(Some(2))
    .set_user_async("worker", &["on", ">pwd_ns2", "+set"])
    .await?;

  // 双方各自的密码与权限严格隔离
  assert!(acl1.scope(Some(1)).auth("worker", "pwd_ns1"));
  assert!(!acl1.scope(Some(1)).auth("worker", "pwd_ns2"));
  assert!(acl1.scope(Some(2)).auth("worker", "pwd_ns2"));
  assert!(!acl1.scope(Some(2)).auth("worker", "pwd_ns1"));
  assert!(acl1.scope(Some(1)).can_execute("worker", RespCommand::GET));
  assert!(!acl1.scope(Some(1)).can_execute("worker", RespCommand::SET));
  assert!(acl1.scope(Some(2)).can_execute("worker", RespCommand::SET));
  assert!(!acl1.scope(Some(2)).can_execute("worker", RespCommand::GET));

  // 模拟重启：冷启动后按各自名字空间懒加载，互不串扰
  let acl2 = AccessControlList::with_storage(storage.clone(), "rootpass");
  acl2.init_from_storage(Some("rootpass")).await?;
  assert!(acl2.get_user("worker").is_none());
  assert!(
    acl2
      .scope(Some(1))
      .get_user_async("worker")
      .await?
      .unwrap()
      .authenticate("pwd_ns1")
  );
  assert!(
    !acl2
      .scope(Some(1))
      .get_user_async("worker")
      .await?
      .unwrap()
      .authenticate("pwd_ns2")
  );
  assert!(
    !acl2
      .scope(Some(2))
      .get_user_async("worker")
      .await?
      .unwrap()
      .authenticate("pwd_ns1")
  );

  // 名字空间内删除不影响另一空间的同名用户
  assert!(acl2.scope(Some(1)).del_user_async("worker").await?);
  assert!(
    acl2
      .scope(Some(1))
      .get_user_async("worker")
      .await?
      .is_none()
  );
  assert!(
    acl2
      .scope(Some(2))
      .get_user_async("worker")
      .await?
      .unwrap()
      .authenticate("pwd_ns2")
  );

  // 全量枚举合并内存缓存与持久化分页，跨空间键完整无遗漏
  let mut keys = Vec::new();
  acl2
    .for_each_user_key(|k| {
      keys.push(k);
      true
    })
    .await?;
  assert!(keys.contains(&user_key(None, "default")));
  assert!(keys.contains(&user_key(Some(2), "worker")));
  assert!(!keys.contains(&user_key(Some(1), "worker")));

  info!("AclStorage: NamespaceIsolationAndPersistence 通过");
  OK
}

/// 验证 for_each_user_key 双流归并全量无遗漏：
/// 存储独有键（含排序在内存键之前的）与仅存内存的键（同步写入不落库）都必须输出
#[compio::test]
async fn test_for_each_user_key_merges_memory_and_storage() -> Void {
  let storage = Arc::new(MemAclStorage::new());
  let acl1 = AccessControlList::with_storage(storage.clone(), "rootpass");

  // 存储中落三个用户：alice / default / mall（字节序 alice < default < mall）
  for (name, pwd) in [("mall", ">mallpw"), ("alice", ">alicepw")] {
    acl1.set_user_async(name, &["on", pwd, "+get"]).await?;
  }

  // 模拟重启（内存为空），再以同步 set_user 创建仅存内存、不落库的用户 zed（字节序最大）
  let acl2 = AccessControlList::with_storage(storage.clone(), "rootpass");
  acl2.init_from_storage(Some("rootpass")).await?;
  acl2.set_user("zed", &["on", ">zedpw", "+get"])?;
  // 同步再建一个排序落在存储键之前的内存独有用户 aaron（字节序最小）
  acl2.set_user("aaron", &["on", ">aaronpw", "+get"])?;

  let mut keys = Vec::new();
  acl2
    .for_each_user_key(|k| {
      keys.push(k);
      true
    })
    .await?;

  let expect: Vec<Vec<u8>> = [
    (None, "aaron"),
    (None, "alice"),
    (None, "default"),
    (None, "mall"),
    (None, "zed"),
  ]
  .iter()
  .map(|(ns, n)| user_key(*ns, n))
  .collect();
  assert_eq!(keys, expect, "归并必须严格升序且双流键全量无遗漏");
  // 严格升序即隐含无重复
  for w in keys.windows(2) {
    assert!(w[0] < w[1], "输出必须严格升序: {:?}", w);
  }

  info!("AclStorage: ForEachUserKeyMerge 通过");
  OK
}

/// 验证超管经 `ns <n>` 规则跨空间 set_user_async 的预载与落库严格按目标桶执行
#[compio::test]
async fn test_cross_namespace_set_user_async_targets_lookup_bucket() -> Void {
  let storage = Arc::new(MemAclStorage::new());
  let acl = AccessControlList::with_storage(storage.clone(), "rootpass");

  // 1. 超管视界下以 `ns 9` 规则创建新租户用户：必须按租户键单点落库（而非静默漏写）
  acl
    .set_user_async("mall", &["on", ">mall_pwd", "+get", "ns", "9"])
    .await?;
  let stored = storage
    .get_user(&user_key(Some(9), "mall"))
    .await?
    .expect("跨 ns 新建用户必须按目标桶落库");
  assert!(stored.authenticate("mall_pwd"));
  assert_eq!(stored.namespace, Some(9));

  // 2. 冷启动后内存为空，超管经 `ns 9` 增量修改存量租户用户：
  //    必须先按租户桶懒加载再叠加规则，绝不能以空白用户覆盖丢失原有密码与权限
  let acl2 = AccessControlList::with_storage(storage.clone(), "rootpass");
  acl2.init_from_storage(Some("rootpass")).await?;
  acl2.set_user_async("mall", &["+set", "ns", "9"]).await?;
  let mall = acl2.scope(Some(9)).find_user("mall")?;
  assert!(
    mall.read().authenticate("mall_pwd"),
    "存量密码不得被空白覆盖"
  );
  assert!(mall.read().can_execute(RespCommand::GET));
  assert!(mall.read().can_execute(RespCommand::SET));

  // 3. 用户名 default 携带 ns 规则：租户桶写入的是租户自身 default，
  //    系统 default 的快照绝不能串写到租户键下
  acl2
    .set_user_async("default", &["on", ">tenant_def_pwd", "ns", "9"])
    .await?;
  let tenant_default = storage
    .get_user(&user_key(Some(9), "default"))
    .await?
    .expect("租户 default 必须按租户键落库");
  assert!(tenant_default.authenticate("tenant_def_pwd"));
  assert!(!tenant_default.authenticate("rootpass"));
  // 系统 default 依然完好且归属全局桶
  let global_default = storage
    .get_user(&user_key(None, "default"))
    .await?
    .expect("系统 default 必须保留在全局桶");
  assert_eq!(global_default.namespace, None);
  assert!(acl2.auth_default("rootpass"));

  // 4. 全局桶绝不能被跨 ns 写入污染
  assert!(storage.get_user(&user_key(None, "mall")).await?.is_none());

  info!("AclStorage: CrossNamespaceSetUserAsync 通过");
  OK
}

/// 验证命令分类查找与命令列表
#[test]
fn test_acl_category_commands() -> Void {
  use wedb_acl::{ALL_CATEGORIES, category_commands, lookup_category};

  assert_eq!(ALL_CATEGORIES.len(), 25);
  assert!(lookup_category("string").is_some());
  assert!(lookup_category("nonexistent_cat").is_none());

  let string_cmds = category_commands("string").expect("string category must exist");
  assert!(string_cmds.contains(&"get"));
  assert!(string_cmds.contains(&"set"));
  assert!(string_cmds.contains(&"mget"));
  assert!(!string_cmds.contains(&"hget"));

  let hash_cmds = category_commands("hash").expect("hash category must exist");
  assert!(hash_cmds.contains(&"hget"));
  assert!(hash_cmds.contains(&"hset"));
  assert!(!hash_cmds.contains(&"get"));

  info!("AclCategoryCommands 通过");
  OK
}
