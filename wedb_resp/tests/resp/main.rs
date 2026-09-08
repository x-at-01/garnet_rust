//! Garnet C# 兼容性测试入口：日志初始化与模块声明

/// 测试二进制全局日志初始化（整个测试进程仅需一次）
#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

mod command_parse;
mod limits;
mod read_header;
mod record_span;
