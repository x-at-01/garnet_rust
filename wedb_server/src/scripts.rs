//! Lua 脚本命令接线 (EVAL / EVALSHA / SCRIPT)
//!
//! 脚本引擎为全局共享实例（互斥锁串行运行）；`redis.call` 经
//! [`StoreScriptApi`] 走存储会话纯内存同步快速路径，冷数据（需异步落盘读）
//! 或同步写失败时返回明确错误，绝不阻塞异步运行时。
//!
//! 全局二级脚本缓存可行性结论（wedb_lua 审查遗留项）：
//! [`ServerContext::scripts`] 以 server 生命周期持有唯一 `ScriptEngine`，
//! 编译产物与 SHA1 → 源码映射天然成为全局二级缓存——EVALSHA 跨会话/跨连接命中，
//! 且多线程每核模型下互斥锁串行执行保证了 Lua 语义与 Redis 单线程一致。
//! 可行且已落地；代价是全部脚本执行在单把锁上串行（跨核不并行），
//! 若后续成为瓶颈，可按 (ns) 分片为多个引擎实例，无需改动本接线层。

use std::{str::from_utf8, sync::Arc};

use wdev::SegmentedDevice;
use wedb_lua::Error as LuaError;
use wedb_module::{ModuleApi, RespValue};
use wedb_net::SendBuffer;
use wedb_redis::prelude::*;
use wedb_resp::{RespCommand, SessionParseState, consts};
use wkv::StoreSession;

use crate::{context::ServerContext, error::Result, session::ServerSession};

/// 分割后的 KEYS 和 ARGV 切片别名
type KeyArgvSplit<'a> = (&'a [&'a [u8]], &'a [&'a [u8]]);

/// 同步冷数据不可达错误提示
const ERR_NOT_RESIDENT: &str = "ERR script attempted to access a key not resident in memory";

/// 非整数参数错误提示
const ERR_NOT_INT: &str = "ERR value is not an integer or out of range";

/// 存储会话脚本执行 API：`redis.call` 的宿主实现
///
/// 同时作为模块自定义命令（MODULE 注册的 procedure）的生产 [`ModuleApi`] 实现复用
pub(crate) struct StoreScriptApi {
  store_session: Arc<StoreSession<SegmentedDevice>>,
}

impl StoreScriptApi {
  /// 绑定当前会话的存储句柄构造宿主 API
  #[inline]
  pub(crate) fn new(store_session: Arc<StoreSession<SegmentedDevice>>) -> Self {
    Self { store_session }
  }
}

impl ModuleApi for StoreScriptApi {
  fn call(&self, cmd: &str, args: &[&[u8]]) -> wedb_module::Result<RespValue> {
    let up = cmd.to_ascii_uppercase();
    let ss = &self.store_session;
    Ok(match up.as_str() {
      "PING" => RespValue::Status(b"PONG".to_vec()),
      "ECHO" => RespValue::Bulk(Some(args.first().copied().unwrap_or(b"").to_vec())),
      "GET" => {
        let Some(key) = args.first() else {
          return Err(wedb_module::Error::Proc(
            "ERR wrong number of arguments for 'get' command".into(),
          ));
        };
        match ss.try_read_string_in_memory(key, |v| v.to_vec()) {
          Ok(Some(Some(v))) => RespValue::Bulk(Some(v)),
          Ok(Some(None)) => RespValue::Bulk(None),
          // 冷数据不在内存，需异步读取
          _ => RespValue::err(ERR_NOT_RESIDENT),
        }
      }
      "SET" => {
        let (Some(key), Some(val)) = (args.first(), args.get(1)) else {
          return Err(wedb_module::Error::Proc(
            "ERR wrong number of arguments for 'set' command".into(),
          ));
        };
        match ss.check_object_meta_fast(key) {
          Ok(Some(false)) => {}
          // 对象类型键返回 WRONGTYPE，未知元数据按冷数据处理
          Ok(Some(true)) => {
            return Err(wedb_module::Error::Proc(
              String::from_utf8_lossy(consts::err::WRONG_TYPE).into_owned(),
            ));
          }
          _ => return Err(wedb_module::Error::Proc(ERR_NOT_RESIDENT.into())),
        }
        match ss.try_upsert_sync(key, val) {
          Ok(Ok(_)) => RespValue::ok(),
          _ => RespValue::err(ERR_NOT_RESIDENT),
        }
      }
      "DEL" => {
        let Some(key) = args.first() else {
          return Err(wedb_module::Error::Proc(
            "ERR wrong number of arguments for 'del' command".into(),
          ));
        };
        match ss.try_delete_sync(key) {
          Ok(Ok(deleted)) => RespValue::Integer(deleted as i64),
          _ => RespValue::err(ERR_NOT_RESIDENT),
        }
      }
      "EXISTS" => {
        let Some(key) = args.first() else {
          return Err(wedb_module::Error::Proc(
            "ERR wrong number of arguments for 'exists' command".into(),
          ));
        };
        match ss.try_read_in_memory(key, |_| ()) {
          Ok(Some(Some(()))) => RespValue::Integer(1),
          Ok(Some(None)) => RespValue::Integer(0),
          _ => RespValue::err(ERR_NOT_RESIDENT),
        }
      }
      "INCR" | "DECR" | "INCRBY" | "DECRBY" => self.incr_decr(&up, args)?,
      _ => RespValue::err(["ERR unknown command '", cmd, "'"].concat()),
    })
  }
}

impl StoreScriptApi {
  /// INCR/DECR/INCRBY/DECRBY：内存读-改-写（脚本互斥运行，无并发竞争）
  fn incr_decr(&self, up: &str, args: &[&[u8]]) -> wedb_module::Result<RespValue> {
    let (Some(key), delta_arg) = (args.first(), args.get(1)) else {
      return Err(wedb_module::Error::Proc(
        "ERR wrong number of arguments".into(),
      ));
    };
    let delta: i64 = match up {
      "INCR" => 1,
      "DECR" => -1,
      _ => {
        let Some(raw) = delta_arg.and_then(|a| from_utf8(a).ok()) else {
          return Err(wedb_module::Error::Proc(ERR_NOT_INT.into()));
        };
        let signed: i64 = raw
          .parse()
          .map_err(|_| wedb_module::Error::Proc(ERR_NOT_INT.into()))?;
        if up == "DECRBY" { -signed } else { signed }
      }
    };
    let cur: i64 = match self
      .store_session
      .try_read_string_in_memory(key, |v| from_utf8(v).ok().and_then(|s| s.parse().ok()))
    {
      Ok(Some(Some(v))) => v.ok_or_else(|| wedb_module::Error::Proc(ERR_NOT_INT.into()))?,
      Ok(Some(None)) => 0,
      _ => return Err(wedb_module::Error::Proc(ERR_NOT_RESIDENT.into())),
    };
    let new_val = cur.checked_add(delta).ok_or_else(|| {
      wedb_module::Error::Proc("ERR increment or decrement would overflow".into())
    })?;
    match self
      .store_session
      .try_upsert_sync(key, new_val.to_string().as_bytes())
    {
      Ok(Ok(_)) => Ok(RespValue::Integer(new_val)),
      _ => Err(wedb_module::Error::Proc(ERR_NOT_RESIDENT.into())),
    }
  }
}

/// RESP 值写入发送缓冲
fn write_resp_value(buf: &mut SendBuffer, v: &RespValue) {
  match v {
    RespValue::Status(s) => buf.write_simple_string(s),
    RespValue::Error(e) => buf.write_error(e),
    RespValue::Integer(n) => buf.write_integer(*n),
    RespValue::Bulk(Some(d)) => buf.write_bulk_string(d),
    RespValue::Bulk(None) => buf.write_null(),
    RespValue::Array(items) => {
      buf.write_array_header(items.len());
      for item in items {
        write_resp_value(buf, item);
      }
    }
  }
}

/// 解析 numkeys 并切分 KEYS/ARGV
fn split_keys_argv<'a>(
  args: &'a [&'a [u8]],
  numkeys_ix: usize,
) -> wedb_module::Result<KeyArgvSplit<'a>> {
  let Some(raw) = args.get(numkeys_ix) else {
    return Err(wedb_module::Error::Proc(
      "ERR wrong number of arguments for 'eval' command".into(),
    ));
  };
  let numkeys: i64 = from_utf8(raw)
    .map_err(|_| wedb_module::Error::Proc(ERR_NOT_INT.into()))?
    .parse()
    .map_err(|_| wedb_module::Error::Proc(ERR_NOT_INT.into()))?;
  if numkeys < 0 || args.len() < numkeys_ix + 1 + numkeys as usize {
    return Err(wedb_module::Error::Proc(
      "ERR Number of keys can't be greater than number of args".into(),
    ));
  }
  let split = numkeys_ix + 1 + numkeys as usize;
  Ok((&args[numkeys_ix + 1..split], &args[split..]))
}

/// 脚本命令族入口
pub fn dispatch_scripts(
  ctx: &Arc<ServerContext>,
  session: &mut ServerSession,
  cmd: RespCommand,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  let args = args.as_slice();
  match cmd {
    RespCommand::Eval => {
      if args.len() < 2 {
        buf.write_error(b"ERR wrong number of arguments for 'eval' command");
        return Ok(());
      }
      let (keys, argv) = match split_keys_argv(args, 1) {
        Ok(kv) => kv,
        Err(e) => {
          buf.write_error_fmt(format_args!("{e}"));
          return Ok(());
        }
      };
      let script = String::from_utf8_lossy(args[0]).into_owned();
      let api = Arc::new(StoreScriptApi::new(Arc::clone(&session.store_session)));
      match ctx.scripts.lock().eval(&script, keys, argv, api) {
        Ok(reply) => write_resp_value(buf, &reply),
        Err(e) => write_lua_error(buf, e),
      }
    }
    RespCommand::Evalsha => {
      if args.len() < 2 {
        buf.write_error(b"ERR wrong number of arguments for 'evalsha' command");
        return Ok(());
      }
      let sha = String::from_utf8_lossy(args[0]).into_owned();
      let (keys, argv) = match split_keys_argv(args, 1) {
        Ok(kv) => kv,
        Err(e) => {
          buf.write_error_fmt(format_args!("{e}"));
          return Ok(());
        }
      };
      let api = Arc::new(StoreScriptApi::new(Arc::clone(&session.store_session)));
      match ctx.scripts.lock().eval_sha(&sha, keys, argv, api) {
        Ok(reply) => write_resp_value(buf, &reply),
        Err(e) => write_lua_error(buf, e),
      }
    }
    RespCommand::Script => dispatch_script_sub(ctx, args, buf),
    RespCommand::ScriptLoad => {
      let Some(script) = args.first() else {
        buf.write_error(b"ERR wrong number of arguments for 'script load' command");
        return Ok(());
      };
      match ctx
        .scripts
        .lock()
        .script_load(&String::from_utf8_lossy(script))
      {
        Ok(sha) => buf.write_bulk_string(sha.as_bytes()),
        Err(e) => write_lua_error(buf, e),
      }
    }
    RespCommand::ScriptExists => {
      let shas: Vec<String> = args
        .iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
      let refs: Vec<&str> = shas.iter().map(String::as_str).collect();
      buf.write_array_header(refs.len());
      for ok in ctx.scripts.lock().script_exists(&refs) {
        buf.write_integer(ok as i64);
      }
    }
    RespCommand::ScriptFlush => {
      ctx.scripts.lock().script_flush();
      buf.write_ok();
    }
    _ => buf.write_error(b"ERR unknown command"),
  }
  Ok(())
}

/// SCRIPT LOAD / EXISTS / FLUSH 子命令
fn dispatch_script_sub(ctx: &Arc<ServerContext>, args: &[&[u8]], buf: &mut SendBuffer) {
  let Some(sub) = args.first().map(|a| a.to_ascii_uppercase()) else {
    buf.write_error(b"ERR wrong number of arguments for 'script' command");
    return;
  };
  match sub.as_slice() {
    b"LOAD" => {
      let Some(script) = args.get(1) else {
        buf.write_error(b"ERR wrong number of arguments for 'script load' command");
        return;
      };
      match ctx
        .scripts
        .lock()
        .script_load(&String::from_utf8_lossy(script))
      {
        Ok(sha) => buf.write_bulk_string(sha.as_bytes()),
        Err(e) => write_lua_error(buf, e),
      }
    }
    b"EXISTS" => {
      let shas: Vec<String> = args[1..]
        .iter()
        .map(|a| String::from_utf8_lossy(a).into_owned())
        .collect();
      let refs: Vec<&str> = shas.iter().map(String::as_str).collect();
      buf.write_array_header(refs.len());
      for ok in ctx.scripts.lock().script_exists(&refs) {
        buf.write_integer(ok as i64);
      }
    }
    b"FLUSH" => {
      ctx.scripts.lock().script_flush();
      buf.write_ok();
    }
    _ => buf.write_error(b"ERR Unknown SCRIPT subcommand or wrong # of args."),
  }
}

/// Lua 错误转 RESP 错误回复
fn write_lua_error(buf: &mut SendBuffer, e: LuaError) {
  match e {
    LuaError::Reply(msg) => buf.write_error(msg.as_bytes()),
    LuaError::ScriptNotFound => buf.write_error(b"NOSCRIPT No matching script. Please use EVAL."),
    e => buf.write_error_fmt(format_args!("ERR {e}")),
  }
}
