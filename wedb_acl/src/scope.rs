//! 名字空间作用域视图：将 ACL 的全部读写严格限定在单一名字空间内
//!
//! 对标 doc/zh/ns.md 多租户隔离规范（C# Garnet 无此概念，为 WeDB 刻意改造）：
//! - `Some(n)` 租户沙箱：点查/增删/鉴权仅作用于本空间，`ns` 规则越权立即拦截；
//! - `None` 超管视界：缺省操作超管全局桶，可经 `ns <n>` 规则跨空间管理任意租户用户。
//!
//! 隔离保证：用户身份 = (名字空间, 用户名) 复合主键（见 [`ns`]），
//! 同名用户在不同名字空间是完全独立的实体，互不可见、互不影响。

use wedb_resp::RespCommand;

use super::AccessControlList;
use crate::{
  error::{Error, Result},
  ns::{self, parse_ns},
  parser::{AclParser, scan_ns_bind},
  password::AclPassword,
  user::{DEFAULT_USER_NAME, User, UserHandle},
};

/// 名字空间作用域句柄（零成本借用视图，按会话持有）
pub struct NamespaceScope<'a> {
  pub(crate) acl: &'a AccessControlList,
  /// 作用域名字空间：None 为超管视界，Some(n) 为租户沙箱
  pub(crate) ns: Option<u64>,
}

impl<'a> NamespaceScope<'a> {
  /// 创建作用域（由 [`AccessControlList::scope`] 构造）
  #[inline]
  pub(crate) fn new(acl: &'a AccessControlList, ns: Option<u64>) -> Self {
    Self { acl, ns }
  }

  /// 获取作用域名字空间
  #[inline]
  pub fn ns(&self) -> Option<u64> {
    self.ns
  }

  /// 查询本作用域内指定用户名的用户句柄（同步快查：命中内存字典）
  pub fn get_user(&self, username: &str) -> Option<UserHandle> {
    if self.ns.is_none() && username == DEFAULT_USER_NAME {
      return Some(self.acl.default_user.clone());
    }
    let pin = self.acl.users.pin();
    ns::with_user_key(self.ns, username, |k| pin.get(k).cloned())
  }

  /// 查找指定用户句柄，不存在则返回 UserNotFound 错误
  pub fn find_user(&self, username: &str) -> Result<UserHandle> {
    self
      .get_user(username)
      .ok_or_else(|| Error::UserNotFound(username.to_string()))
  }

  /// 鉴权并返回认证成功的用户句柄（未命中用户时消耗恒定 SHA-256 时间，防用户名枚举侧信道）
  pub fn authenticate(&self, username: &str, password: &str) -> Option<UserHandle> {
    let handle = match self.get_user(username) {
      Some(h) => h,
      None => {
        AclPassword::dummy(password);
        return None;
      }
    };
    if handle.read().authenticate(password) {
      Some(handle)
    } else {
      None
    }
  }

  /// 校验用户登录口令凭据是否有效（同步快速通道，优先内存字典）
  #[inline]
  pub fn auth(&self, username: &str, password: &str) -> bool {
    self.authenticate(username, password).is_some()
  }

  /// 异步查询用户句柄：优先内存字典，未命中则从底层数据库点查懒加载并回填缓存
  ///
  /// 存储读取错误原样上抛（fail-fast），由调用方决定降级语义
  pub async fn get_user_async(&self, username: &str) -> Result<Option<UserHandle>> {
    if let Some(handle) = self.get_user(username) {
      return Ok(Some(handle));
    }
    // 持久化互斥锁内双检：等锁期间可能已被并发懒加载回填，避免重复点查
    let _persist = self.acl.persist_mutex.lock().await;
    if let Some(handle) = self.get_user(username) {
      return Ok(Some(handle));
    }
    self.load_from_storage_unlocked(self.ns, username).await
  }

  /// 懒加载内核（调用方须已持有 [`AccessControlList::persist_mutex`]）：
  /// 存储点查成功即回填缓存与租户索引，存储错误原样上抛
  async fn load_from_storage_unlocked(
    &self,
    ns: Option<u64>,
    username: &str,
  ) -> Result<Option<UserHandle>> {
    let Some(storage) = self.acl.storage.read().clone() else {
      return Ok(None);
    };
    let key = ns::user_key(ns, username);
    match storage.get_user(&key).await {
      Ok(Some(user)) => {
        let handle = self
          .acl
          .users
          .pin()
          .get_or_insert_with(key, || UserHandle::new(user))
          .clone();
        Ok(Some(handle))
      }
      Ok(None) => Ok(None),
      Err(e) => Err(e),
    }
  }

  /// 设置或修改本作用域内的用户规则（具有原子性保证，含防越权提权校验）
  ///
  /// 绑定语义（对标 doc/zh/ns.md 六.2）：
  /// - 租户沙箱 `Some(n)`：`ns` 规则仅允许绑定自身沙箱，越权立即返回 [`Error::NamespaceDenied`]；
  /// - 超管视界 `None`：`ns` 规则可绑定任意空间（含 `ns none`），缺省作用于超管全局桶；
  /// - 用户名字空间绑定创建后不可变，跨空间迁移必须删除重建。
  pub fn set_user(&self, username: &str, rules: &[&str]) -> Result<()> {
    // 用户名禁含 `#`（AUTH 凭据「用户名#空间」保留分隔符），空名同样拒绝
    ns::validate_username(username)?;
    let lookup_ns = self.resolve_lookup_ns(rules)?;
    let _lock = self.acl.file_mutex.lock();
    let pin = self.acl.users.pin();
    match ns::with_user_key(lookup_ns, username, |k| pin.get(k).cloned()) {
      Some(handle) => {
        // 写路径采用「克隆-应用-整体替换」，任一规则非法时原状态完好无损
        let mut guard = handle.write();
        let mut clone = guard.clone();
        AclParser::apply_rules(&mut clone, rules.iter().copied())?;
        if clone.namespace != guard.namespace {
          return Err(Error::NamespaceDenied);
        }
        *guard = clone;
      }
      None => {
        let mut user = User::new(username);
        AclParser::apply_rules(&mut user, rules.iter().copied())?;
        user.namespace = lookup_ns;
        pin.insert(ns::user_key(lookup_ns, username), UserHandle::new(user));
      }
    }
    Ok(())
  }

  /// 异步设置或修改用户规则，并自动单点保存至数据库存储
  ///
  /// 全程持有持久化互斥锁：串行化「预载-修改-落库」完整生命线，
  /// 杜绝与并发异步删除/写入交错导致的用户复活或过期快照覆盖
  pub async fn set_user_async(&self, username: &str, rules: &[&str]) -> Result<()> {
    let lookup_ns = self.resolve_lookup_ns(rules)?;
    let _persist = self.acl.persist_mutex.lock().await;
    // 预载与落库必须以规则解析出的目标桶 lookup_ns 为准（而非会话视界 self.ns）：
    // 超管经 `ns <n>` 规则跨空间管理租户用户时，两者不同，按会话视界预载会漏载租户存量用户
    // 导致增量修改被全新空白用户覆盖，按会话视界落库则会漏写甚至串写他桶记录；
    // 若目标桶内存未命中，先尝试从底层存储加载存量用户，存储错误直接中止写路径，
    // 杜绝在加载失败时以空白用户误覆盖数据库存量状态
    let key = ns::user_key(lookup_ns, username);
    if self.acl.users.pin().get(&key).is_none() {
      self.load_from_storage_unlocked(lookup_ns, username).await?;
    }
    self.set_user(username, rules)?;
    // 单点落库：快照目标桶当前状态写入持久化存储（与 set_user 的插入键同源）
    let storage = self.acl.storage.read().clone();
    if let Some(storage) = storage {
      let snapshot = {
        let pin = self.acl.users.pin();
        pin.get(&key).map(|h| h.snapshot())
      };
      if let Some(snapshot) = snapshot {
        storage.put_user(&key, &snapshot).await?;
      }
    }
    Ok(())
  }

  /// 解析规则目标查找域：显式 `ns` 绑定 > 租户自身沙箱 > 超管全局桶（含防提权拦截）
  fn resolve_lookup_ns(&self, rules: &[&str]) -> Result<Option<u64>> {
    let bind = scan_ns_bind(rules)?
      .map(|(_, raw)| parse_ns(raw))
      .transpose()?;
    if let Some(n) = self.ns
      && bind.is_some_and(|b| b != Some(n))
    {
      return Err(Error::NamespaceDenied);
    }
    Ok(bind.unwrap_or(self.ns))
  }

  /// 删除本作用域内指定用户（default 用户禁止删除，受到永久核心保护）
  pub fn del_user(&self, username: &str) -> Result<bool> {
    if self.ns.is_none() && username == DEFAULT_USER_NAME {
      return Err(Error::DefaultUserProtected);
    }
    let _lock = self.acl.file_mutex.lock();
    let pin = self.acl.users.pin();
    Ok(ns::with_user_key(self.ns, username, |k| {
      pin.remove(k).is_some()
    }))
  }

  /// 异步删除指定用户，并自动从数据库存储中同步删除
  ///
  /// 全程持有持久化互斥锁：内存删除与存储删除作为单一原子生命线，
  /// 杜绝并发异步写「先复活内存再落库」造成的删除丢失
  pub async fn del_user_async(&self, username: &str) -> Result<bool> {
    let _persist = self.acl.persist_mutex.lock().await;
    let mem_deleted = self.del_user(username)?;
    let key = ns::user_key(self.ns, username);
    let storage = self.acl.storage.read().clone();
    let db_deleted = if let Some(s) = storage {
      s.del_user(&key).await?
    } else {
      false
    };
    Ok(mem_deleted || db_deleted)
  }

  /// 批量删除用户（原子性保障：任一为 default 则整体报错且不删除任何用户）
  pub fn del_users(&self, usernames: &[&str]) -> Result<usize> {
    if self.ns.is_none() && usernames.contains(&DEFAULT_USER_NAME) {
      return Err(Error::DefaultUserProtected);
    }
    let _lock = self.acl.file_mutex.lock();
    let pin = self.acl.users.pin();
    Ok(
      usernames
        .iter()
        .filter(|name| ns::with_user_key(self.ns, name, |k| pin.remove(k).is_some()))
        .count(),
    )
  }

  /// 校验本作用域内指定用户是否有权执行指定命令
  pub fn can_execute(&self, username: &str, cmd: RespCommand) -> bool {
    self
      .get_user(username)
      .is_some_and(|h| h.read().can_execute(cmd))
  }

  /// 校验本作用域内指定用户是否有权访问指定 Key
  pub fn can_access_key(&self, username: &str, key: &[u8], is_write: bool) -> bool {
    self
      .get_user(username)
      .is_some_and(|h| h.read().can_access_key(key, is_write))
  }

  /// 校验本作用域内指定用户是否有权访问指定 Pub/Sub 频道
  pub fn can_access_channel(&self, username: &str, channel: &[u8]) -> bool {
    self
      .get_user(username)
      .is_some_and(|h| h.read().can_access_channel(channel))
  }

  /// 获取本作用域内全部用户名列表（排序稳定输出；超管视界仅含全局桶用户）
  pub fn user_names(&self) -> Vec<String> {
    let pin = self.acl.users.pin();
    let mut names: Vec<String> = pin
      .iter()
      .filter_map(|(k, _)| {
        if !ns::matches_ns(k, self.ns) {
          return None;
        }
        let (_, name) = ns::decode_user_key(k)?;
        Some(name.to_string())
      })
      .collect();
    names.sort_unstable();
    names
  }

  /// 导出本作用域内全部用户的 ACL DSL 描述行（对标 ACL LIST 的作用域过滤版）
  pub fn list_users(&self) -> Vec<String> {
    let pin = self.acl.users.pin();
    let mut list: Vec<String> = pin
      .iter()
      .filter(|(k, _)| ns::matches_ns(k, self.ns))
      .map(|(_, h)| h.read().describe())
      .collect();
    list.sort_unstable();
    list
  }
}
