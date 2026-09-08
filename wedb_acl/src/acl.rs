use std::{mem::take, sync::Arc};

use futures_util::lock::Mutex as AsyncMutex;
use parking_lot::{Mutex, RwLock};
use wedb_resp::RespCommand;
use whasher::{GxPapayaMap, new_papaya_map};

use crate::{
  error::{Error, Result},
  ns,
  parser::split_rule_line,
  scope::NamespaceScope,
  storage::AclStorage,
  user::{DEFAULT_USER_NAME, User, UserHandle},
};

/// 访问控制列表（Access Control List）主管理器
///
/// 基于 `papaya::HashMap` 与硬件向量加速 `gxhash` 实现高并发安全的无锁读、分段写用户缓存字典，
/// 默认内置保护 `default` 用户（nopass, allcommands, ~*, &*, enabled）。
///
/// 与 C# Garnet `AccessControlList`（libs/server/ACL/AccessControlList.cs）的刻意差异：
/// 1. 不使用配置文件，用户记录持久化到数据库（[`AclStorage`]，bitcode 紧凑编码），
///    支持千万级用户按需懒加载点查，冷启动内存只驻留 default 与活跃用户；
/// 2. 用户身份为「名字空间 × 用户名」复合主键（见 [`ns`]），同名用户跨空间互不可见、互不影响；
/// 3. 原生支持 Redis 6+ 键/频道 glob 模式与读写分离（C# 仅支持 `~*` 占位）。
///
/// 并发契约：挂载存储后端后，跨 await 的增删改一律走 `*_async` 方法（经 [`Self::persist_mutex`]
/// 串行化，杜绝「删除后被并发写复活」「过期快照覆盖新状态」）；同步方法仅操作内存，
/// 供无存储模式使用。
pub struct AccessControlList {
  /// 复合主键（ns × 用户名）到用户句柄的并发字典（高频/活跃用户内存缓存）
  pub(crate) users: GxPapayaMap<Vec<u8>, UserHandle>,
  /// 默认用户句柄缓存（加速高频访问，100% 常驻内存）
  pub(crate) default_user: UserHandle,
  /// 用户增删改的排他互斥锁（对标 C# Save 的 lock(this) 串行化语义，仅内存态）
  pub(crate) file_mutex: Mutex<()>,
  /// 持久化操作异步互斥锁：异步读写删除全程持有跨 await，
  /// 串行化「预载-修改-落库」与「删除-落库」的完整生命线
  pub(crate) persist_mutex: AsyncMutex<()>,
  /// 数据库持久化存储后端（可选）
  pub(crate) storage: RwLock<Option<Arc<dyn AclStorage>>>,
}

impl Default for AccessControlList {
  fn default() -> Self {
    Self::new("")
  }
}

impl AccessControlList {
  /// 创建新的 ACL 管理器，并初始化 default 用户
  ///
  /// 若 `default_password` 为空，则 default 用户处于 nopass 免密状态；
  /// 否则将配置该密码作为 default 用户的唯一登录凭证。
  pub fn new(default_password: &str) -> Self {
    let users = new_papaya_map();
    let default_user = UserHandle::new(User::default_user(default_password));
    users
      .pin()
      .insert(ns::user_key(None, DEFAULT_USER_NAME), default_user.clone());

    Self {
      users,
      default_user,
      file_mutex: Mutex::new(()),
      persist_mutex: AsyncMutex::new(()),
      storage: RwLock::new(None),
    }
  }

  /// 获取指定名字空间的作用域视图（零成本借用；None 为超管视界）
  ///
  /// 租户会话持有 `scope(Some(n))` 后所有操作被严格限定在自身沙箱内
  #[inline]
  pub fn scope(&self, ns: Option<u64>) -> NamespaceScope<'_> {
    NamespaceScope::new(self, ns)
  }

  /// 创建初始默认用户句柄（对标 C# CreateDefaultUserHandle）
  #[inline]
  pub fn create_default_user_handle(default_password: &str) -> UserHandle {
    UserHandle::new(User::default_user(default_password))
  }

  /// 设置底层持久化存储后端
  pub fn set_storage(&self, storage: Arc<dyn AclStorage>) {
    *self.storage.write() = Some(storage);
  }

  /// 获取当前配置的底层存储后端引用
  pub fn storage(&self) -> Option<Arc<dyn AclStorage>> {
    self.storage.read().clone()
  }

  /// 使用指定存储后端构建 ACL 管理器
  pub fn with_storage(storage: Arc<dyn AclStorage>, default_password: &str) -> Self {
    let acl = Self::new(default_password);
    acl.set_storage(storage);
    acl
  }

  /// 从底层存储加载初始化（仅同步 default 用户状态，避免千万级用户全量载入内存）
  ///
  /// 全程持有持久化互斥锁，与并发的异步读写删除串行化，杜绝加载与删除/写入交错
  pub async fn init_from_storage(&self, default_password: Option<&str>) -> Result<()> {
    let storage = self.storage.read().clone();
    let Some(s) = storage else {
      return Ok(());
    };
    let _persist = self.persist_mutex.lock().await;
    let key = ns::user_key(None, DEFAULT_USER_NAME);
    if let Some(db_default) = s.get_user(&key).await? {
      *self.default_user.write() = db_default;
    } else {
      // 若数据库尚无 default 用户记录，则写入当前默认用户
      if let Some(pwd) = default_password
        && !pwd.is_empty()
      {
        self.default_user.write().add_password(pwd);
      }
      let u = self.default_user.snapshot();
      s.put_user(&key, &u).await?;
    }
    Ok(())
  }

  /// 全量快照内存用户桶逐个幂等落库（对标 Redis ACL SAVE 语义）
  ///
  /// 全程持有持久化互斥锁：与并发异步读写删除串行化，杜绝已删除用户的过期
  /// 快照在保存途中被重新落库（复活）；任一写入失败即中止上抛，调用方据此
  /// 回传错误，杜绝半保存状态被误认为已持久化
  pub async fn save_all_to_storage(&self) -> Result<()> {
    let Some(s) = self.storage.read().clone() else {
      return Ok(());
    };
    let _persist = self.persist_mutex.lock().await;
    for h in self.get_user_handles() {
      let u = h.snapshot();
      s.put_user(&ns::user_key(u.namespace, &u.name), &u).await?;
    }
    Ok(())
  }

  /// 获取 default 默认用户句柄
  #[inline]
  pub fn default_user(&self) -> UserHandle {
    self.default_user.clone()
  }

  /// 查询超管视界（全局桶）内指定用户名的用户句柄（同步快查：命中内存字典）
  pub fn get_user(&self, username: &str) -> Option<UserHandle> {
    self.scope(None).get_user(username)
  }

  /// 查找指定用户句柄，不存在则返回 UserNotFound 错误
  pub fn find_user(&self, username: &str) -> Result<UserHandle> {
    self.scope(None).find_user(username)
  }

  /// 直接添加已构建的用户句柄（按用户自带名字空间绑定归位）
  pub fn add_user_handle(&self, handle: UserHandle) -> Result<()> {
    let (name, ns_bind) = handle.with_user(|u| (u.name.clone(), u.namespace));
    // 用户名禁含 `#`（AUTH 凭据「用户名#空间」保留分隔符），与 set_user 同一口径
    ns::validate_username(&name)?;
    let _lock = self.file_mutex.lock();
    let inserted = ns::with_user_key(ns_bind, &name, |k| {
      self.users.pin().try_insert(k.to_vec(), handle).is_ok()
    });
    if inserted {
      Ok(())
    } else {
      Err(Error::UserAlreadyExists(name))
    }
  }

  /// 获取当前所有用户句柄集合（按复合主键字节序稳定排序，单趟加锁）
  pub fn get_user_handles(&self) -> Vec<UserHandle> {
    let pin = self.users.pin();
    let mut pairs: Vec<(&[u8], &UserHandle)> = pin.iter().map(|(k, v)| (k.as_slice(), v)).collect();
    pairs.sort_unstable_by_key(|(k, _)| *k);
    pairs.into_iter().map(|(_, v)| v.clone()).collect()
  }

  /// 鉴权并返回认证成功的用户句柄（超管视界；`None` 用户名走 default 快通道）
  pub fn authenticate(&self, username: Option<&str>, password: &str) -> Option<UserHandle> {
    match username {
      Some(u) => self.scope(None).authenticate(u, password),
      None => self
        .default_user
        .read()
        .authenticate(password)
        .then(|| self.default_user.clone()),
    }
  }

  /// 异步查询用户句柄：优先内存字典，未命中则从底层数据库点查懒加载（支持千万级用户按需读取）
  ///
  /// 存储读取错误原样上抛（fail-fast），由调用方决定降级语义
  pub async fn get_user_async(&self, username: &str) -> Result<Option<UserHandle>> {
    self.scope(None).get_user_async(username).await
  }

  /// 校验用户登录口令凭据是否有效（同步快速通道，优先内存字典）
  pub fn auth(&self, username: &str, password: &str) -> bool {
    self.scope(None).auth(username, password)
  }

  /// 校验 default 用户密码是否正确
  #[inline]
  pub fn auth_default(&self, password: &str) -> bool {
    self.default_user.read().authenticate(password)
  }

  /// 校验超管视界内指定用户是否有权执行指定命令
  pub fn can_execute(&self, username: &str, cmd: RespCommand) -> bool {
    self.scope(None).can_execute(username, cmd)
  }

  /// 校验超管视界内指定用户是否有权访问指定 Key
  pub fn can_access_key(&self, username: &str, key: &[u8], is_write: bool) -> bool {
    self.scope(None).can_access_key(username, key, is_write)
  }

  /// 校验超管视界内指定用户是否有权访问指定 Pub/Sub 频道
  pub fn can_access_channel(&self, username: &str, channel: &[u8]) -> bool {
    self.scope(None).can_access_channel(username, channel)
  }

  /// 设置或修改超管视界内用户规则（具有原子性保证；`ns` 规则可绑定任意空间）
  pub fn set_user(&self, username: &str, rules: &[&str]) -> Result<()> {
    self.scope(None).set_user(username, rules)
  }

  /// 异步设置或修改超管视界内用户规则，并自动单点保存至数据库存储
  pub async fn set_user_async(&self, username: &str, rules: &[&str]) -> Result<()> {
    self.scope(None).set_user_async(username, rules).await
  }

  /// 删除超管视界内指定用户（default 用户禁止删除，受到永久核心保护）
  pub fn del_user(&self, username: &str) -> Result<bool> {
    self.scope(None).del_user(username)
  }

  /// 异步删除超管视界内指定用户，并自动从数据库存储中同步删除
  pub async fn del_user_async(&self, username: &str) -> Result<bool> {
    self.scope(None).del_user_async(username).await
  }

  /// 批量删除超管视界内用户（原子性保障，任一为 default 则整体报错且不删除）
  pub fn del_users(&self, usernames: &[&str]) -> Result<usize> {
    self.scope(None).del_users(usernames)
  }

  /// 从单行规则文本中解析并应用配置（超管视界）
  pub fn set_user_from_line(&self, line: &str) -> Result<()> {
    let (username, ops) = split_rule_line(line)?;
    let rules: Vec<&str> = ops.collect();
    self.set_user(username, &rules)
  }

  /// 导出当前内存缓存的所有用户列表（对标 ACL LIST，输出兼容 Redis 6+ DSL 描述文本，
  /// 租户用户额外携带 `ns <n>` 绑定标记，可直接经 [`Self::set_user_from_line`] 无损重放）
  pub fn list_users(&self) -> Vec<String> {
    let pin = self.users.pin();
    let mut list: Vec<String> = pin.iter().map(|(_, h)| h.read().describe()).collect();
    list.sort_unstable();
    list
  }

  /// 获取当前内存缓存中全部用户名列表（排序稳定输出；跨空间同名可能出现重复）
  pub fn user_names(&self) -> Vec<String> {
    let pin = self.users.pin();
    let mut names: Vec<String> = pin
      .iter()
      .filter_map(|(k, _)| ns::decode_user_key(k).map(|(_, name)| name.to_string()))
      .collect();
    names.sort_unstable();
    names
  }

  /// 流式枚举用户复合主键：合并持久化存储分页与内存缓存（千万级用户防 OOM）
  ///
  /// 双有序流（内存键升序 × 存储游标页升序）单趟归并后逐键回调，全量键完整无遗漏：
  /// 同键并存时（懒加载回填）内存流胜出仅输出一次。除内存缓存自身键表外零额外物化，
  /// 存储侧每页仅驻留一个游标页。回调返回 `false` 可提前终止。
  pub async fn for_each_user_key(&self, mut f: impl FnMut(Vec<u8>) -> bool) -> Result<()> {
    let mut mem_keys: Vec<Vec<u8>> = self.users.pin().iter().map(|(k, _)| k.clone()).collect();
    mem_keys.sort_unstable();

    let storage = self.storage.read().clone();
    let Some(s) = storage else {
      for k in mem_keys {
        if !f(k) {
          return Ok(());
        }
      }
      return Ok(());
    };

    const PAGE: usize = 4096;
    let mut db_page: Vec<Vec<u8>> = Vec::new();
    let mut db_ix = 0usize;
    let mut db_cursor: Option<Vec<u8>> = None;
    let mut mem_ix = 0usize;

    loop {
      // 存储流优先补页：保证归并全程双流各自升序
      if db_ix >= db_page.len() {
        match s.list_users_after(db_cursor.as_deref(), PAGE).await? {
          page if page.is_empty() => break,
          page => {
            db_cursor = page.last().cloned();
            db_page = page;
            db_ix = 0;
          }
        }
      }

      match mem_keys.get(mem_ix) {
        // 存储键较小：仅存于存储的键（未回填内存缓存）也必须输出
        Some(m) if db_page[db_ix] < *m => {
          if !f(take(&mut db_page[db_ix])) {
            return Ok(());
          }
          db_ix += 1;
        }
        // 内存键较小或与存储键相等：相等时仅走内存流输出一次，存储流同键跳过
        Some(m) => {
          if *m == db_page[db_ix] {
            db_ix += 1;
          }
          mem_ix += 1;
          if !f(mem_keys[mem_ix - 1].clone()) {
            return Ok(());
          }
        }
        None => {
          if !f(take(&mut db_page[db_ix])) {
            return Ok(());
          }
          db_ix += 1;
        }
      }
    }
    // 存储流耗尽：冲刷仅存于内存的尾部键（同步写入不落库的内存独有用户）
    for m in &mem_keys[mem_ix..] {
      if !f(m.clone()) {
        return Ok(());
      }
    }
    Ok(())
  }

  /// 将所有用户快照序列化为 bitcode 二进制数据
  pub fn to_bitcode(&self) -> Vec<u8> {
    let pin = self.users.pin();
    let users: Vec<User> = pin.iter().map(|(_, h)| h.snapshot()).collect();
    bitcode::encode(&users)
  }

  /// 从 bitcode 二进制数据全量恢复用户列表（解码先行，失败则原状态完好无损）
  ///
  /// 用于启动期全量恢复；运行期非并发原子替换，仅应在初始化路径调用
  pub fn from_bitcode_bytes(&self, bytes: &[u8]) -> Result<()> {
    let users: Vec<User> = bitcode::decode(bytes).map_err(Error::from)?;
    let tmp_map: GxPapayaMap<Vec<u8>, UserHandle> = new_papaya_map();
    let tmp_pin = tmp_map.pin();
    for u in users {
      let key = ns::user_key(u.namespace, &u.name);
      tmp_pin.insert(key, UserHandle::new(u));
    }
    self.install_users(&tmp_map, None);
    Ok(())
  }

  /// 用临时用户字典替换当前全部用户，并同步 default 句柄缓存
  fn install_users(
    &self,
    tmp_map: &GxPapayaMap<Vec<u8>, UserHandle>,
    default_password: Option<&str>,
  ) {
    let default_key = ns::user_key(None, DEFAULT_USER_NAME);
    let tmp_pin = tmp_map.pin();
    let new_default = if let Some(h) = tmp_pin.get(&default_key) {
      h.snapshot()
    } else {
      User::default_user(default_password.unwrap_or(""))
    };

    let main_pin = self.users.pin();
    main_pin.clear();
    *self.default_user.write() = new_default;
    main_pin.insert(default_key.clone(), self.default_user.clone());

    for (key, handle) in tmp_pin.iter() {
      if key.as_slice() != default_key.as_slice() {
        main_pin.insert(key.clone(), handle.clone());
      }
    }
  }
}
