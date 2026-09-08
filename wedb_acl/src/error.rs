use std::{io, result};

use thiserror::Error;

/// ACL 模块统一错误枚举
#[derive(Error, Debug)]
pub enum Error {
  /// 用户不存在
  #[error("User '{0}' not found")]
  UserNotFound(String),

  /// 用户已存在
  #[error("User '{0}' already exists")]
  UserAlreadyExists(String),

  /// 保护 default 用户不可删除
  #[error("The special 'default' user cannot be removed from the system")]
  DefaultUserProtected,

  /// 非法 ACL 规则格式
  #[error("Malformed ACL rule: {0}")]
  InvalidRule(String),

  /// 非法名字空间绑定值（0 为控制面自动分配保留值，租户上限见 MAX_TENANT_NAMESPACE）
  #[error("ERR invalid namespace '{0}'")]
  InvalidNamespace(String),

  /// 用户名非法（空或含 AUTH 凭据保留分隔符 `#`，见 `parse_user_token`）
  #[error("ERR invalid username '{0}'")]
  InvalidUsername(String),

  /// 跨名字空间提权拦截（租户仅可绑定自身沙箱）
  #[error("ERR Permission denied: cannot grant namespace outside of the current tenant scope")]
  NamespaceDenied,

  /// 规则必须以 USER 关键字开头
  #[error("ACL rules need to start with the USER keyword")]
  MissingUserKeyword,

  /// 未知的 ACL 操作符
  #[error("Unknown operation '{0}'")]
  UnknownOperation(String),

  /// 命令分类不存在
  #[error("ACL Category '{0}' does not exist")]
  CategoryDoesNotExist(String),

  /// 命令不存在
  #[error("Command '{0}' does not exist")]
  CommandDoesNotExist(String),

  /// 密码哈希非法
  #[error("Invalid password hash: {0}")]
  InvalidPasswordHash(String),

  /// 密码哈希长度错误（必须为 64 字符十六进制）
  #[error("Invalid password hash length: {0} (expected 64)")]
  InvalidPasswordHashLength(usize),

  /// 自定义命令名称非法
  #[error("Invalid custom command name '{0}'")]
  InvalidCustomCommandName(String),

  /// IO 错误透明转发
  #[error(transparent)]
  Io(#[from] io::Error),

  /// Bitcode 编解码错误
  #[error(transparent)]
  Bitcode(#[from] bitcode::Error),

  /// GENPASS 位数参数错误（必须在 1..=4096 范围内）
  #[error(
    "ERR ACL GENPASS argument must be the number of bits for the output password, a positive number up to 4096"
  )]
  InvalidGenpassBits(usize),
}

/// ACL 模块统一 Result 类型
pub type Result<T> = result::Result<T, Error>;
