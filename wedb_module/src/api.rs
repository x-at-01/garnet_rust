//! 模块运行时 API：宿主（wedb_server）注入的命令执行接口。
//!
//! 过程与脚本通过该接口访问存储。宿主实现走同步快速路径（内存命中），
//! 语义上等价于 Redis 嵌入式调用模型。

use crate::{error::Result, reply::RespValue};

/// 模块/脚本可见的命令执行接口 (对标 Garnet CustomProcedure 内部的 garnet API 调用)
pub trait ModuleApi {
  /// 执行一条内建命令，返回回复值（错误回复不转为 Err，调用方可用
  /// [`RespValue::into_result`] 自行判定）
  fn call(&self, cmd: &str, args: &[&[u8]]) -> Result<RespValue>;
}
