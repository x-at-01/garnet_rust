//! 模块扩展命令接线 (MODULE LOAD / UNLOAD / LIST)
//!
//! 对标 C# Garnet `AdminCommands.NetworkModuleLoad`（MODULE LOADCS 装配体加载）：
//! C# 经 .NET 装配体反射加载模块二进制；Rust 端无动态库加载通道，模块统一经
//! [`wedb_module::ModuleRegistry::load`] 以编译期内建模块形式注册，`LOAD` 规范首词即
//! 内建模块名，规范解析（parse_module_spec → registry.load）与 C# TryParseModuleSpec 同构。
//!
//! 未知命令路由：[`try_route_module_command`] 在协议解析器报未知命令时
//! 优先查模块注册表，命中即执行自定义过程，未命中才回退标准 unknown command 错误。

use std::{result, str::from_utf8, sync::Arc};

use log::{info, warn};
use wedb_module::{
  CustomCommandInfo, Module, ModuleActionStatus, ModuleApi, ModuleLoadContext, parse_module_spec,
};
use wedb_net::SendBuffer;
use wedb_resp::{RespReadUtils, SessionParseState};

use crate::{
  context::ServerContext, error::Result, scripts::StoreScriptApi, session::ServerSession,
};

/// 内建示例模块：注册只读命令 `example.ping`（对标 Garnet TestModule 示例模块）
///
/// 经 [`ModuleApi::call`] 透传内建 PING，打通「模块过程 → 生产 ModuleApi → 存储会话」全链路
struct ExampleModule;

impl Module for ExampleModule {
  fn on_load(&self, ctx: &mut ModuleLoadContext<'_>, _args: &[&[u8]]) {
    if ctx.initialize("example", 1) != ModuleActionStatus::Success {
      return;
    }
    ctx.register_procedure(
      "example.ping",
      |api: &dyn ModuleApi, _args: &[&[u8]], out: &mut Vec<u8>| {
        api.call("PING", &[])?.write_resp(out);
        Ok(())
      },
      Some(CustomCommandInfo {
        arity: 1,
        read_only: true,
        first_key: 0,
        last_key: 0,
        key_step: 0,
      }),
    );
  }
}

/// 按模块名构造内建模块实例（新增内建模块在此登记）
fn builtin_module(name: &str) -> Option<Arc<dyn Module>> {
  match name {
    "example" => Some(Arc::new(ExampleModule)),
    _ => None,
  }
}

/// 解析并分发 MODULE 命令（调用方已剥离 MODULE 首词）
pub fn dispatch_modules(
  ctx: &Arc<ServerContext>,
  _session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  let args = args.as_slice();
  let Some(sub) = args.first() else {
    buf.write_error(b"ERR wrong number of arguments for 'module' command");
    return Ok(());
  };
  let rest = &args[1..];
  if sub.eq_ignore_ascii_case(b"LOAD") {
    handle_module_load(ctx, rest, buf);
  } else if sub.eq_ignore_ascii_case(b"UNLOAD") {
    handle_module_unload(ctx, rest, buf);
  } else if sub.eq_ignore_ascii_case(b"LIST") {
    let entries = ctx.modules.list();
    buf.write_array_header(entries.len());
    for e in &entries {
      buf.write_array_header(2);
      buf.write_bulk_string(e.name.as_bytes());
      buf.write_integer(e.version as i64);
    }
  } else {
    buf.write_error_fmt(format_args!(
      "ERR Unknown subcommand or wrong number of arguments for '{}'. Try MODULE HELP.",
      from_utf8(sub).unwrap_or("<binary>")
    ));
  }
  Ok(())
}

/// MODULE LOAD <spec>：解析模块规范并经 registry.load 完成加载
fn handle_module_load(ctx: &Arc<ServerContext>, args: &[&[u8]], buf: &mut SendBuffer) {
  let Some(raw) = args.first() else {
    buf.write_error(b"ERR wrong number of arguments for 'module|load' command");
    return;
  };
  // 规范形如 `<模块名/路径> [arg0 arg1 ...]`，含空白路径以双引号包裹
  let (name, load_args) = match parse_module_spec_checked(raw) {
    Ok(parsed) => parsed,
    Err(msg) => {
      buf.write_error(msg.as_bytes());
      return;
    }
  };
  // 路径主干即模块名（兼容 "path/to/xxx.so" 形式的 C# 风格规范）
  let stem = name
    .rsplit(['/', '\\'])
    .next()
    .and_then(|s| s.split('.').next())
    .unwrap_or(name.as_str());
  let Some(module) = builtin_module(stem) else {
    buf.write_error_fmt(format_args!(
      "ERR Error loading module: '{name}' 不是已注册的内建模块"
    ));
    return;
  };
  // 加载参数转字节切片视图：模块加载参数二进制安全透传
  let arg_refs: Vec<&[u8]> = load_args.iter().map(|v| v.as_slice()).collect();
  match ctx.modules.load(module.as_ref(), &arg_refs) {
    Ok(loaded) => {
      info!(target: "wedb::module", "模块加载成功: module='{loaded}', args={load_args:?}");
      buf.write_ok();
    }
    Err(e) => {
      warn!(target: "wedb::module", "模块加载失败: name='{name}', err={e:?}");
      buf.write_error(e.to_string().as_bytes());
    }
  }
}

/// 解析模块规范字节：UTF-8 校验 + parse_module_spec（错误回复自带 ERR 前缀）
///
/// parse_module_spec 返回借用自 spec 的切片，此处转为 owned 以便跨函数返回
fn parse_module_spec_checked(raw: &[u8]) -> result::Result<(String, Vec<Vec<u8>>), String> {
  let spec = from_utf8(raw).map_err(|_| "ERR module spec must be valid UTF-8".to_string())?;
  let (name, args) = parse_module_spec(spec).map_err(|e| e.to_string())?;
  Ok((
    name.to_string(),
    args.iter().map(|a| a.as_bytes().to_vec()).collect(),
  ))
}

/// MODULE UNLOAD <name>：卸载模块并成组移除其注册命令
fn handle_module_unload(ctx: &Arc<ServerContext>, args: &[&[u8]], buf: &mut SendBuffer) {
  let Some(name) = args.first().and_then(|a| from_utf8(a).ok()) else {
    buf.write_error(b"ERR wrong number of arguments for 'module|unload' command");
    return;
  };
  if ctx.modules.unload(name) {
    info!(target: "wedb::module", "模块卸载成功: module='{name}'");
    buf.write_ok();
  } else {
    buf.write_error_fmt(format_args!(
      "ERR Error unloading module: no such module '{name}'"
    ));
  }
}

/// 查询模块自定义命令元信息并按 Redis COMMAND INFO 形状写出，命中返回 true
///
/// 输出形状：`[name, arity, flags[], first_key, last_key, step]`
pub fn try_write_module_command_info(
  ctx: &Arc<ServerContext>,
  name: &[u8],
  buf: &mut SendBuffer,
) -> bool {
  let Ok(cmd_name) = from_utf8(name) else {
    return false;
  };
  let Some(info) = ctx.modules.command_info(cmd_name) else {
    return false;
  };
  buf.write_array_header(6);
  buf.write_bulk_string(name);
  buf.write_integer(info.arity as i64);
  // flags 数组：只读命令标注 readonly，可写命令标注 write
  if info.read_only {
    buf.write_array_header(1);
    buf.write_bulk_string(b"readonly");
  } else {
    buf.write_array_header(1);
    buf.write_bulk_string(b"write");
  }
  buf.write_integer(info.first_key as i64);
  buf.write_integer(info.last_key as i64);
  buf.write_integer(info.key_step as i64);
  true
}

/// 未知命令的模块路由：命中自定义过程则就地执行并返回 true
///
/// `name` 为协议解析失败的命令名；`raw` 为完整原始报文（含命令名），
/// `consumed` 为该报文完整字节长度（由调用方推进接收游标）
pub fn try_route_module_command(
  ctx: &Arc<ServerContext>,
  session: &mut ServerSession,
  name: &[u8],
  raw: &[u8],
  buf: &mut SendBuffer,
) -> Result<bool> {
  let Ok(cmd_name) = from_utf8(name) else {
    return Ok(false);
  };
  let Some(proc_) = ctx.modules.lookup(cmd_name) else {
    return Ok(false);
  };

  // 解析 RESP 数组报文参数切片（与主解析器同一读工具，二进制安全）
  let Ok((Some(_), header_len)) = RespReadUtils::try_read_signed_array_len(raw) else {
    return Ok(false);
  };
  let mut rest = &raw[header_len..];
  let mut parsed: Vec<&[u8]> = Vec::new();
  while !rest.is_empty()
    && let Ok((arg, used)) = RespReadUtils::try_slice_with_length_header(rest)
  {
    parsed.push(arg);
    rest = &rest[used..];
  }
  // 参数未完整解出（坏包）：回退标准错误路径，绝不以残缺参数执行过程
  if !rest.is_empty() {
    return Ok(false);
  }
  // 自定义过程约定入参不含命令名（对标 Garnet CustomProcedure）
  let Some((_cmd, args)) = parsed.split_first() else {
    return Ok(false);
  };

  // 执行自定义过程：复用脚本宿主的生产 ModuleApi（纯内存同步存储访问）
  // 注：ACL 定制命令级权限校验（can_execute_custom）待会话面接入后在此补齐
  let api = StoreScriptApi::new(Arc::clone(&session.store_session));
  let mut out = Vec::new();
  match proc_.execute(&api, args, &mut out) {
    Ok(()) => buf.write_raw(&out),
    // wedb_module 过程错误的 Display 自带 ERR/WRONGTYPE 前缀，原样透传
    Err(e) => buf.write_error(e.to_string().as_bytes()),
  }
  Ok(true)
}
