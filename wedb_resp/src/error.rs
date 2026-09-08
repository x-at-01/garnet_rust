use core::{result, str::Utf8Error};

use hipstr::HipStr;
use thiserror::Error;

/// RESP 协议解析与编码错误枚举
#[derive(Error, Debug, PartialEq, Eq, Clone)]
pub enum Error {
  /// 整数溢出错误
  #[error("整数溢出")]
  IntegerOverflow,

  /// 非法数字格式
  #[error("不是有效数字")]
  NotANumber,

  /// 无效长度头
  #[error("无效长度: {0}")]
  InvalidLength(i64),

  /// 遇到意外的协议标记字节
  #[error("意外的协议标记: {0}")]
  UnexpectedToken(u8),

  /// 协议数据不完整，需要等待更多输入
  #[error("协议数据不完整")]
  Incomplete,

  /// 目标缓冲区空间不足
  #[error("目标缓冲区空间不足")]
  BufferTooSmall,

  /// 未知或非法的 RESP 命令
  #[error("未知命令: {0}")]
  UnknownCommand(HipStr<'static>),

  /// 缺少必要的参数
  #[error("缺少命令参数")]
  MissingArgument,

  /// 命令参数数量超过协议上限（对齐 C# `RespParsingException.ThrowExcessiveArgumentCount`）
  #[error("参数数量过多: {0} (上限 {1})")]
  ExcessiveArgs(usize, usize),

  /// 协议格式错误
  #[error("协议格式错误: {0}")]
  Protocol(&'static str),

  /// UTF-8 编码错误
  #[error(transparent)]
  Utf8(#[from] Utf8Error),

  /// 非法的 SHA-1 脚本哈希（长度必须为 40 且为十六进制字符）
  #[error("非法的 SHA-1 脚本哈希")]
  InvalidScriptHash,
}

/// RESP 操作 Result 别名
pub type Result<T> = result::Result<T, Error>;
