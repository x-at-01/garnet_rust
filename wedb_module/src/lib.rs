//! wedb_module: 模块扩展 API 与注册表 (1:1 对标 Garnet libs/server/Module)
//!
//! 提供 [`Module`] 加载入口（加载参数二进制安全透传至 [`Module::on_load`]）、
//! [`ModuleLoadContext`] 注册上下文与 [`ModuleRegistry`] 注册表；模块过程通过
//! [`ModuleApi`] 访问宿主存储，回复以 [`RespValue`] 统一表示（wedb_lua 复用
//! 同一模型）。加载规范解析见 [`parse_module_spec`]。

#![cfg_attr(docsrs, feature(doc_cfg))]

mod api;
mod error;
mod module;
mod registry;
mod reply;
mod utils;

pub use api::ModuleApi;
pub use error::{Error, Result};
pub use module::{
  CustomCommandInfo, CustomProcedure, Module, ModuleActionStatus, ModuleLoadContext,
};
pub use registry::{ModuleEntry, ModuleRegistry};
pub use reply::RespValue;
pub use utils::parse_module_spec;
