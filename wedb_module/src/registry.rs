//! 模块注册表：登记已加载模块与自定义命令，供宿主在未知命令时路由。

use std::{collections::hash_map::Entry, sync::Arc};

use gxhash::{HashMap, HashSet};
use parking_lot::RwLock;

use crate::{
  error::{Error, Result},
  module::{CustomCommandInfo, CustomProcedure, Module, ModuleActionStatus, ModuleLoadContext},
};

/// 已加载模块元信息
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModuleEntry {
  /// 模块名
  pub name: String,
  /// 模块版本
  pub version: u32,
}

/// 注册表内部状态；写路径仅发生在模块加载期，读路径为命令路由热路径
#[derive(Default)]
pub struct RegistryInner {
  /// 模块名 -> 元信息，`order` 保持加载顺序
  pub modules: HashMap<String, ModuleEntry>,
  /// 加载顺序
  pub order: Vec<String>,
  /// 模块名 -> 其注册的命令名（用于卸载时成组移除）
  pub module_commands: HashMap<String, Vec<String>>,
  /// 命令名 -> 过程
  pub commands: HashMap<String, Arc<dyn CustomProcedure>>,
  /// 命令名 -> 元信息
  pub command_infos: HashMap<String, CustomCommandInfo>,
}

/// 模块注册表 (对标 Garnet CustomCommandManager 的模块部分)
///
/// 锁协议：[`load`](Self::load)/[`unload`](Self::unload) 取写锁（仅模块管理期
/// 低频发生），`lookup`/`command_info`/`list` 取读锁（命令路由热路径）；
/// parking_lot RwLock 公平调度，持续读流量下写者不会饥饿。写锁临界区覆盖
/// [`Module::on_load`] 用户代码，非重入锁——on_load 内不得再进入本注册表。
///
/// 生命周期：`lookup` 返回的 [`Arc`] 句柄在模块卸载后仍可继续执行（过程对象
/// 由句柄保持存活），卸载仅解除路由，宿主应停止向已卸载命令分发。
#[derive(Default)]
pub struct ModuleRegistry {
  inner: RwLock<RegistryInner>,
}

impl ModuleRegistry {
  /// 加载模块：以 `args` 回调 [`Module::on_load`]，返回模块名
  pub fn load(&self, module: &dyn Module, args: &[&[u8]]) -> Result<String> {
    let mut inner = self.inner.write();
    let mut ctx = ModuleLoadContext {
      registry: &mut inner,
      name: None,
      init_status: None,
      dup_name: None,
      initialized: false,
    };
    module.on_load(&mut ctx, args);
    match ctx.name {
      Some(name) => Ok(name),
      // 上抛 initialize 的失败原因
      None => match ctx.init_status {
        Some(ModuleActionStatus::AlreadyExists) => {
          Err(Error::AlreadyExists(ctx.dup_name.unwrap_or_default()))
        }
        _ => Err(Error::InvalidRegistrationInfo),
      },
    }
  }

  /// 按命令名查找过程；返回的句柄在卸载后仍可执行
  pub fn lookup(&self, name: &str) -> Option<Arc<dyn CustomProcedure>> {
    self.inner.read().commands.get(name).cloned()
  }

  /// 按命令名查找命令元信息
  pub fn command_info(&self, name: &str) -> Option<CustomCommandInfo> {
    self.inner.read().command_infos.get(name).cloned()
  }

  /// 按加载顺序列出已加载模块 (对标 MODULE LIST)
  pub fn list(&self) -> Vec<ModuleEntry> {
    let inner = self.inner.read();
    inner
      .order
      .iter()
      .filter_map(|name| inner.modules.get(name).cloned())
      .collect()
  }

  /// 已加载模块数
  pub fn len(&self) -> usize {
    self.inner.read().modules.len()
  }

  /// 是否没有任何已加载模块
  pub fn is_empty(&self) -> bool {
    self.inner.read().modules.is_empty()
  }

  /// 卸载指定模块并成组移除其注册的全部命令，返回是否确实移除了模块；
  /// 已发出的 [`Arc`] 句柄保持有效，但注册表不再路由对应命令
  pub fn unload(&self, name: &str) -> bool {
    let mut inner = self.inner.write();
    let Some(cmd_names) = inner.module_commands.remove(name) else {
      return false;
    };
    inner.modules.remove(name);
    inner.order.retain(|n| n != name);
    let cmd_names: HashSet<&str> = cmd_names.iter().map(String::as_str).collect();
    inner
      .commands
      .retain(|k, _| !cmd_names.contains(k.as_str()));
    inner
      .command_infos
      .retain(|k, _| !cmd_names.contains(k.as_str()));
    true
  }
}

impl RegistryInner {
  /// 登记模块元信息；同名已存在时拒绝
  pub fn try_add_module(&mut self, name: &str, version: u32) -> bool {
    match self.modules.entry(name.into()) {
      Entry::Occupied(_) => false,
      Entry::Vacant(v) => {
        v.insert(ModuleEntry {
          name: name.into(),
          version,
        });
        self.order.push(name.into());
        self.module_commands.insert(name.into(), Vec::new());
        true
      }
    }
  }

  /// 在当前模块名下登记命令
  pub fn try_add_command(
    &mut self,
    module: &str,
    name: &str,
    proc_: Arc<dyn CustomProcedure>,
    info: Option<CustomCommandInfo>,
  ) -> bool {
    if self.commands.contains_key(name) {
      return false;
    }
    self.commands.insert(name.into(), proc_);
    if let Some(info) = info {
      self.command_infos.insert(name.into(), info);
    }
    if let Some(list) = self.module_commands.get_mut(module) {
      list.push(name.into());
    }
    true
  }
}
