use std::result;

use thiserror::Error;

/// 模块错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 同名模块已注册（携带注册表中已记录的模块名）
  #[error("ERR module '{0}' already registered")]
  AlreadyExists(String),

  /// 注册信息无效（名称为空等）
  #[error("ERR invalid module registration info")]
  InvalidRegistrationInfo,

  /// 模块规范解析失败（路径为空、引号未闭合等）
  #[error("ERR invalid module specification")]
  InvalidModuleSpec,

  /// 模块过程执行期错误
  #[error("{0}")]
  Proc(String),
}

pub type Result<T> = result::Result<T, Error>;
