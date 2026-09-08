// wedb_list 集成测试唯一入口：所有测试模块统一挂载于此（见 list/mod.rs），
// 编译为单一测试二进制；模块内不得重复定义 ctor 日志初始化。

mod list;

/// 全 crate 集成测试唯一的日志初始化入口
#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}
