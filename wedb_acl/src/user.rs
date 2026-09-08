use core::str::from_utf8_unchecked;
use std::sync::Arc;

use parking_lot::{RwLock, RwLockReadGuard, RwLockWriteGuard};
use subtle::{Choice, ConstantTimeEq};
use wedb_resp::RespCommand;
use whasher::HashSet;

use crate::{
  command_set::{CAT_ALL, CommandPermissionSet, command_name, lookup_category},
  error::{Error, Result},
  glob::glob_match,
  key_pattern::KeyPattern,
  parser::is_valid_custom_command_name,
  password::AclPassword,
};

/// 默认系统保留用户名
pub const DEFAULT_USER_NAME: &str = "default";

/// 用户实体，包含用户身份、状态、密码哈希、键/频道模式与命令权限集
///
/// 对标 C# Garnet `User`（libs/server/ACL/User.cs），额外扩展多租户名字空间绑定
#[derive(Clone, Debug, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub struct User {
  /// 用户名
  pub name: String,
  /// 绑定的多租户命名空间 ID（对标 doc/zh/ns.md 六.1 User 实体定义）
  ///
  /// - `None`：系统管理员全局视界（仅 default 或显式声明 `ns none`）
  /// - `Some(n)`：锁定绑定的租户沙箱（n ≥ 1），同名用户跨空间互不可见
  pub namespace: Option<u64>,
  /// 账户是否启用（on / off）
  pub enabled: bool,
  /// 是否处于免密码登录模式（nopass）
  pub nopass: bool,
  /// 允许的密码 SHA-256 哈希列表
  pub passwords: Vec<AclPassword>,
  /// 绑定的命令权限集合
  pub commands: CommandPermissionSet,
  /// 是否拥有全部键访问权限（~* 或 allkeys）
  pub allkeys: bool,
  /// 键访问模式列表（支持按 glob 匹配与读写分离控制）
  pub key_patterns: Vec<KeyPattern>,
  /// 是否拥有全部发布订阅频道权限（&* 或 allchannels）
  pub allchannels: bool,
  /// 发布订阅频道模式列表（Glob 匹配）
  pub channel_patterns: Vec<String>,
}

impl User {
  /// 创建新用户（默认处于禁用 off、无密码、无命令权限、无键与频道权限状态）
  ///
  /// 名字空间默认为超管全局视界 `None`，由上层按会话作用域强制绑定租户沙箱
  pub fn new(name: impl Into<String>) -> Self {
    Self {
      name: name.into(),
      namespace: None,
      enabled: false,
      nopass: false,
      passwords: Vec::new(),
      commands: CommandPermissionSet::new(),
      allkeys: false,
      key_patterns: Vec::new(),
      allchannels: false,
      channel_patterns: Vec::new(),
    }
  }

  /// 创建 default 默认用户（enabled=true, ~*, &*, commands=+@all）
  pub fn default_user(password: &str) -> Self {
    let mut u = Self::new(DEFAULT_USER_NAME);
    u.enabled = true;
    u.set_all_keys();
    u.set_all_channels();
    u.commands = CommandPermissionSet::all();
    if password.is_empty() {
      u.nopass = true;
    } else {
      u.nopass = false;
      u.add_password_hash(AclPassword::from_cleartext(password));
    }
    u
  }

  /// 使用 bitcode 高效序列化 User
  #[inline]
  pub fn to_bitcode(&self) -> Vec<u8> {
    bitcode::encode(self)
  }

  /// 从 bitcode 二进制数据反序列化恢复 User
  #[inline]
  pub fn from_bitcode(bytes: &[u8]) -> Result<Self> {
    bitcode::decode(bytes).map_err(Error::from)
  }

  /// 获取用户名
  #[inline]
  pub fn name(&self) -> &str {
    &self.name
  }

  /// 获取用户是否处于启用状态
  #[inline]
  pub fn is_enabled(&self) -> bool {
    self.enabled
  }

  /// 设置用户启用状态
  #[inline]
  pub fn set_enabled(&mut self, enabled: bool) {
    self.enabled = enabled;
  }

  /// 获取是否处于免密模式
  #[inline]
  pub fn is_passwordless(&self) -> bool {
    self.nopass
  }

  /// 设置是否处于免密模式
  #[inline]
  pub fn set_passwordless(&mut self, nopass: bool) {
    self.nopass = nopass;
  }

  /// 获取密码哈希切片引用
  #[inline]
  pub fn passwords(&self) -> &[AclPassword] {
    &self.passwords
  }

  /// 获取键模式列表切片引用
  #[inline]
  pub fn key_patterns(&self) -> &[KeyPattern] {
    &self.key_patterns
  }

  /// 获取频道模式列表切片引用
  #[inline]
  pub fn channel_patterns(&self) -> &[String] {
    &self.channel_patterns
  }

  /// 获取允许的自定义命令集合引用
  #[inline]
  pub fn custom_commands_allowed(&self) -> &HashSet<String> {
    &self.commands.custom_allowed
  }

  /// 获取拒绝的自定义命令集合引用
  #[inline]
  pub fn custom_commands_denied(&self) -> &HashSet<String> {
    &self.commands.custom_denied
  }

  /// 拷贝绑定的命令权限集合
  #[inline]
  pub fn copy_command_permission_set(&self) -> CommandPermissionSet {
    self.commands.clone()
  }

  /// 拷贝当前密码哈希列表
  #[inline]
  pub fn copy_password_hashes(&self) -> Vec<AclPassword> {
    self.passwords.clone()
  }

  /// 验证输入明文密码是否有效
  ///
  /// 始终先计算 SHA-256 并采用 subtle::Choice 恒定时间无分支聚合比对，杜绝早期分支中断与时序泄露
  pub fn authenticate(&self, password: &str) -> bool {
    let target = AclPassword::from_cleartext(password);
    if !self.enabled {
      return false;
    }
    if self.nopass {
      return true;
    }
    if self.passwords.is_empty() {
      return false;
    }
    let mut matched = Choice::from(0);
    for p in &self.passwords {
      matched |= p.hash.ct_eq(&target.hash);
    }
    matched.into()
  }

  /// 检查用户是否允许执行指定命令
  pub fn can_execute(&self, cmd: RespCommand) -> bool {
    if !self.enabled {
      return false;
    }
    if cmd.is_no_auth() {
      return true;
    }
    self.commands.allow(cmd)
  }

  /// 检查用户是否允许执行自定义（扩展）命令
  pub fn can_execute_custom(&self, generic_cmd: RespCommand, custom_name: &str) -> bool {
    if !self.enabled {
      return false;
    }
    self.commands.allow_custom(generic_cmd, custom_name)
  }

  /// 授予所有键的完全读写访问权限（~* 或 allkeys）
  #[inline]
  pub fn set_all_keys(&mut self) {
    self.allkeys = true;
    self.key_patterns.clear();
    self.key_patterns.push(KeyPattern::all());
  }

  /// 重置键权限（清空所有键访问规则）
  #[inline]
  pub fn reset_keys(&mut self) {
    self.allkeys = false;
    self.key_patterns.clear();
  }

  /// 添加键访问模式规则，支持自动合并与去重
  pub fn add_key_pattern(&mut self, pattern: &str, read: bool, write: bool) {
    if pattern == "*" && read && write {
      self.set_all_keys();
      return;
    }
    // 全键权限已覆盖任何模式（含 `*` 的读写子集），无需重复登记
    if self.allkeys {
      return;
    }
    match self
      .key_patterns
      .iter_mut()
      .find(|kp| kp.pattern == pattern)
    {
      Some(existing) => {
        existing.read |= read;
        existing.write |= write;
        // 合并升级为完全读写 `~*` 时，规范化为 allkeys 标记
        if pattern == "*" && existing.read && existing.write {
          self.set_all_keys();
        }
      }
      None => self
        .key_patterns
        .push(KeyPattern::new(pattern, read, write)),
    }
  }

  /// 检查用户是否允许访问指定键
  #[inline]
  pub fn can_access_key(&self, key: impl AsRef<[u8]>, is_write: bool) -> bool {
    if !self.enabled {
      return false;
    }
    if self.allkeys {
      return true;
    }
    let key_bytes = key.as_ref();
    self
      .key_patterns
      .iter()
      .any(|kp| kp.matches(key_bytes, is_write))
  }

  /// 授予所有发布订阅频道的完全访问权限（&* 或 allchannels）
  #[inline]
  pub fn set_all_channels(&mut self) {
    self.allchannels = true;
    self.channel_patterns.clear();
    self.channel_patterns.push("*".to_string());
  }

  /// 重置发布订阅频道权限（清空所有频道规则）
  #[inline]
  pub fn reset_channels(&mut self) {
    self.allchannels = false;
    self.channel_patterns.clear();
  }

  /// 添加频道访问模式规则，支持自动去重
  pub fn add_channel_pattern(&mut self, pattern: &str) {
    if pattern == "*" {
      self.set_all_channels();
      return;
    }
    if self.allchannels {
      return;
    }
    if !self.channel_patterns.iter().any(|p| p == pattern) {
      self.channel_patterns.push(pattern.to_string());
    }
  }

  /// 检查用户是否允许访问指定发布订阅频道
  #[inline]
  pub fn can_access_channel(&self, channel: impl AsRef<[u8]>) -> bool {
    if !self.enabled {
      return false;
    }
    if self.allchannels {
      return true;
    }
    let ch_bytes = channel.as_ref();
    self
      .channel_patterns
      .iter()
      .any(|pat| glob_match(pat.as_bytes(), ch_bytes))
  }

  /// 添加明文密码（自动计算 SHA-256 并去重存储）
  #[inline]
  pub fn add_password(&mut self, cleartext: &str) {
    self.add_password_hash(AclPassword::from_cleartext(cleartext));
  }

  /// 添加密码哈希（自动去重）
  pub fn add_password_hash(&mut self, password: AclPassword) {
    if !self.passwords.iter().any(|p| p.ct_eq(&password)) {
      self.passwords.push(password);
    }
  }

  /// 移除指定密码哈希
  pub fn remove_password_hash(&mut self, password: &AclPassword) {
    self.passwords.retain(|p| !p.ct_eq(password));
  }

  /// 清空当前全部密码
  pub fn clear_passwords(&mut self) {
    self.passwords.clear();
  }

  /// 重置用户状态为初始未配置状态并禁用
  pub fn reset(&mut self) {
    self.clear_passwords();
    self.nopass = false;
    self.enabled = false;
    self.commands = CommandPermissionSet::new();
    self.reset_keys();
    self.reset_channels();
  }

  /// 添加命令授权（对标 C# User.AddCommand：判定范围覆盖命令及其全部 ACL 展开子命令）
  pub fn add_command(&mut self, cmd: RespCommand) {
    let norm = cmd.normalize_for_acls();
    let name = command_name(norm);
    let all_allowed = self.commands.allow(norm)
      && norm
        .expand_for_acls()
        .iter()
        .all(|&e| self.commands.allow(e));
    // 全部展开命令均已授权则 no-op，避免描述文本冗余追加
    if all_allowed {
      return;
    }
    self.commands.set(norm);
    if !name.is_empty() {
      self.commands.append_op('+', name);
    }
  }

  /// 移除命令授权（对标 C# User.RemoveCommand：任一展开命令仍被授权即需整体收回）
  pub fn remove_command(&mut self, cmd: RespCommand) {
    if cmd.is_no_auth() {
      return;
    }
    let norm = cmd.normalize_for_acls();
    let any_allowed = self.commands.allow(norm)
      || norm
        .expand_for_acls()
        .iter()
        .any(|&e| self.commands.allow(e));
    // 全部展开命令均未被授权则 no-op（如仅 `+config|get` 后执行 `-config` 也需收回 CONFIG_GET）
    if !any_allowed {
      return;
    }
    self.commands.clear(norm);
    if self.commands.is_empty() {
      self.commands.set_description("");
    } else {
      self.commands.append_op('-', command_name(norm));
    }
  }

  /// 授予指定分类的命令权限（单次查表 + 逐字 OR，零堆分配；对标 C# User.AddCategory）
  pub fn add_category(&mut self, cat: &str) -> Result<()> {
    let (name, bits) =
      lookup_category(cat).ok_or_else(|| Error::CategoryDoesNotExist(cat.to_string()))?;
    if bits == CAT_ALL {
      self.commands.set_all();
      self.commands.set_description("+@all");
      return Ok(());
    }
    // 已全量授权（含空位图分类 @stream，空集全称量化恒真）则 no-op，与 C# 一致不改动描述
    if self.commands.is_category_all_allowed(&bits) {
      return Ok(());
    }
    self.commands.or_bits(&bits);
    self.commands.append_category_op('+', name);
    Ok(())
  }

  /// 移除指定分类的命令权限（NO_AUTH 位受保护；对标 C# User.RemoveCategory）
  pub fn remove_category(&mut self, cat: &str) -> Result<()> {
    let (name, bits) =
      lookup_category(cat).ok_or_else(|| Error::CategoryDoesNotExist(cat.to_string()))?;
    if bits == CAT_ALL {
      self.commands.clear_all();
      self.commands.set_description("");
      return Ok(());
    }
    // 分类下无任何已授权命令则 no-op
    if !self.commands.is_category_any_allowed(&bits) {
      return Ok(());
    }
    self.commands.andnot_bits_protect_noauth(&bits);
    if self.commands.is_empty() {
      self.commands.set_description("");
    } else {
      self.commands.append_category_op('-', name);
    }
    Ok(())
  }

  /// 授权自定义命令
  pub fn add_custom_command(&mut self, name: &str) -> Result<()> {
    if !is_valid_custom_command_name(name) {
      return Err(Error::InvalidCustomCommandName(name.to_string()));
    }
    let norm = name.to_ascii_uppercase();
    if self.commands.is_all()
      || (self.commands.custom_allowed.contains(&norm)
        && !self.commands.custom_denied.contains(&norm))
    {
      return Ok(());
    }
    self.commands.add_custom_command(name);
    let name_lower = name.to_ascii_lowercase();
    self.commands.append_op('+', &name_lower);
    Ok(())
  }

  /// 拒绝自定义命令
  pub fn remove_custom_command(&mut self, name: &str) -> Result<()> {
    if !is_valid_custom_command_name(name) {
      return Err(Error::InvalidCustomCommandName(name.to_string()));
    }
    let norm = name.to_ascii_uppercase();
    if !self.commands.is_all()
      && self.commands.custom_denied.contains(&norm)
      && !self.commands.custom_allowed.contains(&norm)
    {
      return Ok(());
    }
    self.commands.remove_custom_command(name);
    // 对标 C# User.RemoveCustomCommand：拒绝标记无条件保留在描述中（即便权限集已空），
    // 否则 describe → 规则重放往返会丢失 `-name` 的显式拒绝语义
    let name_lower = name.to_ascii_lowercase();
    self.commands.append_op('-', &name_lower);
    Ok(())
  }

  /// 遍历导出用户全部权限规则（[`Self::describe`] 与 [`Self::describe_rules`] 的单一事实来源）
  ///
  /// 复合规则（`ns <n>`、`#<hash>`、键/频道模式）经 scratch 缓冲拼装后整条回调
  fn for_each_rule(&self, mut f: impl FnMut(&str)) {
    f(if self.enabled { "on" } else { "off" });

    if self.nopass {
      f("nopass");
    }

    // 名字空间绑定（超管全局视界 None 缺省省略，保证 ACL LIST 单行可无损重放）
    let mut scratch = String::new();
    if let Some(ns) = self.namespace {
      scratch.clear();
      scratch.push_str("ns ");
      let mut buf = itoa::Buffer::new();
      scratch.push_str(buf.format(ns));
      f(&scratch);
    }

    for p in &self.passwords {
      scratch.clear();
      scratch.push('#');
      let mut buf = [0u8; AclPassword::HEX_LEN];
      let _ = hex::encode_to_slice(p.hash, &mut buf);
      // SAFETY: hex::encode_to_slice 输出必然是合法 ASCII 十六进制小写字符
      scratch.push_str(unsafe { from_utf8_unchecked(&buf) });
      f(&scratch);
    }

    // 键模式导出
    if self.allkeys {
      f("~*");
    } else {
      for kp in &self.key_patterns {
        scratch.clear();
        if kp.read && kp.write {
          scratch.push('~');
        } else if kp.read {
          scratch.push_str("%R~");
        } else if kp.write {
          scratch.push_str("%W~");
        }
        scratch.push_str(&kp.pattern);
        f(&scratch);
      }
    }

    // 频道模式导出
    if self.allchannels {
      f("&*");
    } else {
      for pat in &self.channel_patterns {
        scratch.clear();
        scratch.push('&');
        scratch.push_str(pat);
        f(&scratch);
      }
    }

    let desc = self.commands.description();
    if !desc.is_empty() {
      for op in desc.split_whitespace() {
        f(op);
      }
    }
  }

  /// 导出用户的单行 ACL DSL 描述格式（如 `user default on nopass ~* &* +@all`）
  pub fn describe(&self) -> String {
    let desc = self.commands.description();
    // 预分配容量，避免重复重分配（含 ns 绑定标记的最长约 29 字节）
    let mut s = String::with_capacity(
      32 + self.name.len()
        + self.passwords.len() * 66
        + self.key_patterns.len() * 16
        + self.channel_patterns.len() * 16
        + desc.len()
        + 29,
    );
    s.push_str("user ");
    s.push_str(&self.name);
    self.for_each_rule(|rule| {
      s.push(' ');
      s.push_str(rule);
    });
    s
  }

  /// 将用户全部权限规则导出为操作标记列表 (用于 AclConfig 与 NestedText 序列化)
  pub fn describe_rules(&self) -> Vec<String> {
    let mut rules = Vec::new();
    self.for_each_rule(|rule| rules.push(rule.to_string()));
    rules
  }

  /// 获取当前已启用的命令描述文本
  #[inline]
  pub fn enabled_commands_description(&self) -> &str {
    self.commands.description()
  }
}

/// 用户并发访问句柄，基于 Arc<RwLock<User>> 包装
#[derive(Clone, Debug)]
pub struct UserHandle {
  inner: Arc<RwLock<User>>,
}

impl UserHandle {
  /// 包装 User 为并发安全句柄
  pub fn new(user: User) -> Self {
    Self {
      inner: Arc::new(RwLock::new(user)),
    }
  }

  /// 获取共享读锁
  #[inline]
  pub fn read(&self) -> RwLockReadGuard<'_, User> {
    self.inner.read()
  }

  /// 获取独占写锁
  #[inline]
  pub fn write(&self) -> RwLockWriteGuard<'_, User> {
    self.inner.write()
  }

  /// 零拷贝读取用户引用闭包执行
  #[inline]
  pub fn with_user<R>(&self, f: impl FnOnce(&User) -> R) -> R {
    f(&self.inner.read())
  }

  /// 零拷贝获取用户名执行闭包
  #[inline]
  pub fn with_name<R>(&self, f: impl FnOnce(&str) -> R) -> R {
    f(&self.inner.read().name)
  }

  /// 获取当前 User 的克隆快照
  #[inline]
  pub fn snapshot(&self) -> User {
    self.inner.read().clone()
  }

  /// 获取用户名
  #[inline]
  pub fn name(&self) -> String {
    self.read().name.clone()
  }

  /// 用户是否启用
  #[inline]
  pub fn is_enabled(&self) -> bool {
    self.read().is_enabled()
  }

  /// 是否免密
  #[inline]
  pub fn is_passwordless(&self) -> bool {
    self.read().is_passwordless()
  }

  /// 验证密码
  #[inline]
  pub fn authenticate(&self, password: &str) -> bool {
    self.read().authenticate(password)
  }

  /// 检查是否允许执行指定命令
  #[inline]
  pub fn can_execute(&self, cmd: RespCommand) -> bool {
    self.read().can_execute(cmd)
  }

  /// 检查是否允许执行指定自定义命令
  #[inline]
  pub fn can_execute_custom(&self, generic_cmd: RespCommand, custom_name: &str) -> bool {
    self.read().can_execute_custom(generic_cmd, custom_name)
  }

  /// 检查是否允许访问指定键
  #[inline]
  pub fn can_access_key(&self, key: impl AsRef<[u8]>, is_write: bool) -> bool {
    self.read().can_access_key(key, is_write)
  }

  /// 检查是否允许访问指定发布订阅频道
  #[inline]
  pub fn can_access_channel(&self, channel: impl AsRef<[u8]>) -> bool {
    self.read().can_access_channel(channel)
  }

  /// CAS 原子更新用户状态（若当前状态与 expected 相等则更新为 new_user，成功返回 true）
  pub fn try_set_user(&self, expected: &User, new_user: User) -> bool {
    let mut guard = self.inner.write();
    if *guard == *expected {
      *guard = new_user;
      true
    } else {
      false
    }
  }

  /// 检查两个句柄是否指向同一并发用户实例
  #[inline]
  pub fn ptr_eq(&self, other: &Self) -> bool {
    Arc::ptr_eq(&self.inner, &other.inner)
  }
}
