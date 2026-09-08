//! wedb_set 集成测试入口（对标 Microsoft Garnet RespSetTest.cs 与 GlobUtils 套件）

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

mod algebra_and_store;
mod codec_and_extensible;
mod glob_differential;
mod resp_commands;
mod support;
