use std::result;

use thiserror::Error;

/// Lua/Luau 脚本引擎错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 脚本抛出的错误，内容即 RESP 错误回复原文（含 "ERR " 前缀或 Lua 运行时错误详情）
  #[error("{0}")]
  Reply(String),

  /// EVALSHA 找不到对应脚本
  #[error("NOSCRIPT No matching script. Please use EVAL.")]
  ScriptNotFound,

  /// Luau 虚拟机错误（编译/类型等）
  #[error(transparent)]
  Lua(#[from] luau::Error),

  /// Luau 编译器错误
  #[error(transparent)]
  Compiler(#[from] luau::CompilerError),

  /// 宿主命令执行错误
  #[error(transparent)]
  Module(#[from] wedb_module::Error),
}

pub type Result<T> = result::Result<T, Error>;
