//! 模块定义与加载上下文 (1:1 对标 Garnet ModuleBase / ModuleLoadContext / ModuleActionStatus)

use std::sync::Arc;

use crate::{api::ModuleApi, error::Result, registry::RegistryInner};

/// 模块动作状态 (1:1 对标 Garnet ModuleActionStatus)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModuleActionStatus {
  /// 成功
  Success,
  /// 失败
  Failure,
  /// 已加载
  AlreadyLoaded,
  /// 已存在
  AlreadyExists,
  /// 注册信息无效
  InvalidRegistrationInfo,
}

/// 模块自定义命令元信息 (对标 RespCommandsInfo 的最小路由所需子集)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomCommandInfo {
  /// 参数个数，负数表示不少于其绝对值（语义同 Redis arity）
  pub arity: i32,
  /// 是否只读命令
  pub read_only: bool,
  /// 首个键参数位置（1-based，0 表示无键参数）
  pub first_key: usize,
  /// 末个键参数位置（0 表示无键，负数表示相对倒数）
  pub last_key: i32,
  /// 键参数步长
  pub key_step: usize,
}

impl CustomCommandInfo {
  /// 构造无键参数、参数个数自由的只读命令元信息
  pub fn arity(arity: i32) -> Self {
    Self {
      arity,
      read_only: false,
      first_key: 0,
      last_key: 0,
      key_step: 0,
    }
  }
}

/// 模块自定义过程：以非事务方式执行一组操作 (对标 Garnet CustomProcedure)
pub trait CustomProcedure: Send + Sync {
  /// 执行过程：`args` 为去掉命令名后的参数切片，回复字节直接写入 `out`
  fn execute(&self, api: &dyn ModuleApi, args: &[&[u8]], out: &mut Vec<u8>) -> Result<()>;
}

impl<F> CustomProcedure for F
where
  F: Fn(&dyn ModuleApi, &[&[u8]], &mut Vec<u8>) -> Result<()> + Send + Sync,
{
  fn execute(&self, api: &dyn ModuleApi, args: &[&[u8]], out: &mut Vec<u8>) -> Result<()> {
    self(api, args, out)
  }
}

/// 所有模块必须实现的加载入口 (对标 Garnet ModuleBase::OnLoad)
pub trait Module: Send + Sync {
  /// 模块加载时调用，在 `ctx` 上完成初始化与注册；`args` 为加载参数
  /// （MODULE LOAD 规范中路径之后的部分，二进制安全透传）
  fn on_load(&self, ctx: &mut ModuleLoadContext<'_>, args: &[&[u8]]);
}

/// 模块加载上下文：向注册表登记模块元信息与命令
pub struct ModuleLoadContext<'a> {
  pub(crate) registry: &'a mut RegistryInner,
  /// 已初始化的模块名，与注册表键一致
  pub(crate) name: Option<String>,
  /// initialize 失败的原因，供注册表上抛
  pub(crate) init_status: Option<ModuleActionStatus>,
  /// 重名冲突的模块名，供注册表构造错误信息
  pub(crate) dup_name: Option<String>,
  pub(crate) initialized: bool,
}

impl ModuleLoadContext<'_> {
  /// 初始化模块名与版本，成功后才能注册命令 (对标 ModuleLoadContext.Initialize)
  pub fn initialize(&mut self, name: &str, version: u32) -> ModuleActionStatus {
    let status = if name.is_empty() {
      ModuleActionStatus::InvalidRegistrationInfo
    } else if self.initialized {
      ModuleActionStatus::AlreadyLoaded
    } else if !self.registry.try_add_module(name, version) {
      self.dup_name = Some(name.into());
      ModuleActionStatus::AlreadyExists
    } else {
      self.name = Some(name.into());
      self.initialized = true;
      ModuleActionStatus::Success
    };
    if status != ModuleActionStatus::Success {
      self.init_status = Some(status);
    }
    status
  }

  /// 注册自定义过程；`info` 提供路由所需的命令元信息
  pub fn register_procedure(
    &mut self,
    name: &str,
    proc_: impl CustomProcedure + 'static,
    info: Option<CustomCommandInfo>,
  ) -> ModuleActionStatus {
    if name.is_empty() {
      return ModuleActionStatus::InvalidRegistrationInfo;
    }
    let Some(module) = self.name.as_deref() else {
      // initialize 未成功调用
      return ModuleActionStatus::Failure;
    };
    if !self
      .registry
      .try_add_command(module, name, Arc::new(proc_), info)
    {
      return ModuleActionStatus::AlreadyExists;
    }
    ModuleActionStatus::Success
  }
}
