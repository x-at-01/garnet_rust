use std::{
  fmt::Write,
  mem, result,
  str::from_utf8,
  sync::{Arc, atomic::Ordering},
  time::{Duration, Instant},
};

use coarsetime::Clock;
use compio::runtime::spawn;
use gxhash::gxhash64;
use itoa::Buffer;
use log::{debug, error, info, warn};
use wcpr::{CheckpointManager, CheckpointType};
use wdev::SegmentedDevice;
use wedb_cluster::{
  RouteResult, format_cluster_info, format_cluster_nodes, format_cluster_slots, hash_slot,
};
use wedb_list::InsertPosition;
use wedb_net::SendBuffer;
use wedb_redis::prelude::*;
use wedb_resp::{CRLF, ParseUtils, RespCommand, SessionParseState, consts};
use wedb_zset::{GeoDistanceUnit, ScoreRange, ZAddOpt};
use wedb_redis::{
  AggregateType, GeoSearchCenter, GeoSearchShape, GeoSortOrder, LexBound, RenameResult,
};
use wkv::{StoreSession, TTL_VALUE_LEN, TtlOpt};

use crate::{
  context::{ServerContext, is_myself_addr},
  error::Result,
  modules,
  range_index::dispatch_range_index,
  scripts::dispatch_scripts,
  session::{QueuedCommand, ServerSession},
  vectors::dispatch_vectors,
};

/// 键切片提取结果，零堆分配支撑单键与多键跨槽决策
enum CommandKeys<'a> {
  None,
  Single(&'a [u8]),
  Slice(&'a [&'a [u8]]),
  Small([&'a [u8]; 8], usize),
  Vec(Vec<&'a [u8]>),
}

/// 解析有序集合分值边界
fn parse_score_bound(s: &[u8]) -> Option<(f64, bool)> {
  if s.is_empty() {
    return None;
  }
  let (val_slice, inclusive) = if s[0] == b'(' {
    (&s[1..], false)
  } else {
    (s, true)
  };
  let score = ParseUtils::try_read_double(val_slice, true)?;
  Some((score, inclusive))
}

/// 解析有序集合区间 (ScoreRange)
fn parse_score_range(min_slice: &[u8], max_slice: &[u8]) -> Option<ScoreRange> {
  let (min, min_inclusive) = parse_score_bound(min_slice)?;
  let (max, max_inclusive) = parse_score_bound(max_slice)?;
  Some(ScoreRange::new(min, min_inclusive, max, max_inclusive))
}

/// 快路径同步 TTL 探针（store 侧 `probe_ttl` 为 pub(crate)，经公开 raw 接口等价实现）
///
/// 返回 `Some(false)`=无 TTL 记录或未过期，可安全同步直读；`Some(true)`=已过期；
/// `None`=TTL 记录冷在磁盘候选或内存探针异常，保守转异步路径交由 `check_expired` 裁决
pub(crate) fn sync_ttl_expired(
  store: &StoreSession<SegmentedDevice>,
  user_key: &[u8],
  now_ms: u64,
) -> Option<bool> {
  let ttl_k = store.ttl_key(user_key);
  store
    .try_read_raw_in_memory(&ttl_k, |v| {
      <[u8; TTL_VALUE_LEN]>::try_from(v)
        .ok()
        .map(u64::from_be_bytes)
    })
    .ok()
    .map(|hit| match hit {
      Some(Some(Some(exp))) => exp <= now_ms,
      // 无 TTL 记录（无候选/墓碑）或记录值非法长度（按无 TTL 容错口径）
      _ => false,
    })
}

/// SET 附加选项（EX/PX/EXAT/PXAT/KEEPTTL/NX/XX/GET，对标 C# BasicCommands.NetworkSETEXNX 选项集）
struct SetOptions {
  /// EX/PX 换算后的相对毫秒数（None = 不调整过期）
  expiry_ms: Option<u64>,
  /// EXAT/PXAT 换算后的绝对毫秒时间戳（None = 无绝对过期；过去值交由底层即时删除）
  expiry_at_ms: Option<u64>,
  /// KEEPTTL：写入后保留既有 TTL
  keepttl: bool,
  /// 仅当键不存在时写入 (NX)
  nx: bool,
  /// 仅当键已存在时写入 (XX)
  xx: bool,
  /// 返回旧值 (GET)
  get: bool,
}

/// 解析 SET 选项尾缀；错误文案直接可回传客户端（事务排队预校验共用同一实现）
fn parse_set_options(args: &[&[u8]]) -> result::Result<SetOptions, &'static str> {
  let mut o = SetOptions {
    expiry_ms: None,
    expiry_at_ms: None,
    keepttl: false,
    nx: false,
    xx: false,
    get: false,
  };
  let mut i = 0;
  while i < args.len() {
    let a = args[i];
    // EX/PX 折算相对毫秒、EXAT/PXAT 折算绝对毫秒；其余选项按令牌分发
    let rel_unit = if a.eq_ignore_ascii_case(b"EX") {
      1000
    } else if a.eq_ignore_ascii_case(b"PX") {
      1
    } else {
      0
    };
    let abs_unit = if a.eq_ignore_ascii_case(b"EXAT") {
      1000
    } else if a.eq_ignore_ascii_case(b"PXAT") {
      1
    } else {
      0
    };
    if rel_unit > 0 || abs_unit > 0 {
      if o.keepttl || o.expiry_ms.is_some() || o.expiry_at_ms.is_some() {
        return Err(consts::err::SYNTAX_STR);
      }
      let Some(v) = args.get(i + 1).and_then(|v| ParseUtils::try_read_long(v)) else {
        return Err(consts::err::INT_OUT_OF_RANGE_STR);
      };
      if abs_unit > 0 {
        // 绝对时间戳允许过去/零值：底层即时删除语义，不做正数校验（Redis 口径）
        o.expiry_at_ms = Some(if v <= 0 {
          0
        } else {
          (v as u64).saturating_mul(abs_unit)
        });
      } else {
        if v <= 0 {
          return Err("ERR invalid expire time in 'set' command");
        }
        o.expiry_ms = Some((v as u64).saturating_mul(rel_unit));
      }
      i += 2;
    } else if a.eq_ignore_ascii_case(b"NX") {
      if o.nx || o.xx {
        return Err(consts::err::SYNTAX_STR);
      }
      o.nx = true;
      i += 1;
    } else if a.eq_ignore_ascii_case(b"XX") {
      if o.nx || o.xx {
        return Err(consts::err::SYNTAX_STR);
      }
      o.xx = true;
      i += 1;
    } else if a.eq_ignore_ascii_case(b"GET") {
      o.get = true;
      i += 1;
    } else if a.eq_ignore_ascii_case(b"KEEPTTL") {
      if o.keepttl || o.expiry_ms.is_some() || o.expiry_at_ms.is_some() {
        return Err(consts::err::SYNTAX_STR);
      }
      o.keepttl = true;
      i += 1;
    } else {
      return Err(consts::err::SYNTAX_STR);
    }
  }
  Ok(o)
}

/// 解析 EXPIRE 家族第三参数（NX/XX/GT/LT，大小写不敏感）
///
/// 对齐 Redis 7：选项仅允许恰好一个令牌，缺失即无条件过期；非法或多余令牌一律 `ERR syntax error`
fn parse_expire_options(args: &[&[u8]]) -> result::Result<TtlOpt, &'static str> {
  match args {
    [] => Ok(TtlOpt::NONE),
    [a] if a.eq_ignore_ascii_case(b"NX") => Ok(TtlOpt {
      nx: true,
      ..TtlOpt::NONE
    }),
    [a] if a.eq_ignore_ascii_case(b"XX") => Ok(TtlOpt {
      xx: true,
      ..TtlOpt::NONE
    }),
    [a] if a.eq_ignore_ascii_case(b"GT") => Ok(TtlOpt {
      gt: true,
      ..TtlOpt::NONE
    }),
    [a] if a.eq_ignore_ascii_case(b"LT") => Ok(TtlOpt {
      lt: true,
      ..TtlOpt::NONE
    }),
    _ => Err(consts::err::SYNTAX_STR),
  }
}

/// 解析 ZADD 前导选项 NX/XX/GT/LT/CH/INCR（对标 C# SortedSet ZAddOptions 解析）
///
/// 返回选项结果与 score-member 键值对区起始下标（下标含 key，自 args[1] 起扫描）
/// 写出 ACL 错误行：自带 `ERR ` 前缀的错误原样输出，其余（io 透明转发等）统一补前缀
#[inline]
fn write_err_line(buf: &mut SendBuffer, msg: &str) {
  if msg.starts_with("ERR ") {
    buf.write_error_fmt(format_args!("{msg}"));
  } else {
    buf.write_error_fmt(format_args!("ERR {msg}"));
  }
}

fn parse_zadd_options(args: &[&[u8]]) -> result::Result<(ZAddOpt, usize), &'static str> {
  let mut opt = ZAddOpt::default();
  let mut i = 1;
  while i < args.len() {
    let a = args[i];
    if a.eq_ignore_ascii_case(b"NX") {
      if opt.nx || opt.xx {
        return Err(consts::err::SYNTAX_STR);
      }
      opt.nx = true;
    } else if a.eq_ignore_ascii_case(b"XX") {
      if opt.nx || opt.xx {
        return Err(consts::err::SYNTAX_STR);
      }
      opt.xx = true;
    } else if a.eq_ignore_ascii_case(b"GT") {
      if opt.gt || opt.lt || opt.nx {
        return Err(consts::err::SYNTAX_STR);
      }
      opt.gt = true;
    } else if a.eq_ignore_ascii_case(b"LT") {
      if opt.gt || opt.lt || opt.nx {
        return Err(consts::err::SYNTAX_STR);
      }
      opt.lt = true;
    } else if a.eq_ignore_ascii_case(b"CH") {
      opt.ch = true;
    } else if a.eq_ignore_ascii_case(b"INCR") {
      if opt.incr {
        return Err(consts::err::SYNTAX_STR);
      }
      opt.incr = true;
    } else {
      // 首个非选项 token 即 score，键值对区自此开始
      return Ok((opt, i));
    }
    i += 1;
  }
  Ok((opt, i))
}

/// ZSet 区间查询选项
struct ZRangeOpt {
  by_score: bool,
  by_lex: bool,
  reverse: bool,
  with_scores: bool,
  offset: usize,
  count: usize,
}

fn parse_zrange_options(args: &[&[u8]]) -> Option<ZRangeOpt> {
  let mut by_score = false;
  let mut by_lex = false;
  let mut reverse = false;
  let mut with_scores = false;
  let mut offset = 0;
  let mut count = usize::MAX;

  let mut idx = 0;
  while idx < args.len() {
    let a = args[idx];
    if a.eq_ignore_ascii_case(b"WITHSCORES") {
      with_scores = true;
      idx += 1;
    } else if a.eq_ignore_ascii_case(b"REV") {
      reverse = true;
      idx += 1;
    } else if a.eq_ignore_ascii_case(b"BYSCORE") {
      if by_lex {
        return None;
      }
      by_score = true;
      idx += 1;
    } else if a.eq_ignore_ascii_case(b"BYLEX") {
      if by_score {
        return None;
      }
      by_lex = true;
      idx += 1;
    } else if a.eq_ignore_ascii_case(b"LIMIT") {
      if idx + 2 >= args.len() {
        return None;
      }
      let off = ParseUtils::try_read_long(args[idx + 1])?;
      let cnt = ParseUtils::try_read_long(args[idx + 2])?;
      if off < 0 {
        return None;
      }
      offset = off as usize;
      count = if cnt < 0 { usize::MAX } else { cnt as usize };
      idx += 3;
    } else {
      return None;
    }
  }

  Some(ZRangeOpt {
    by_score,
    by_lex,
    reverse,
    with_scores,
    offset,
    count,
  })
}

/// ZSet 多集合运算参数
struct ZSetOpArgs<'a> {
  keys: Vec<&'a [u8]>,
  weights: Vec<f64>,
  aggregate: AggregateType,
  with_scores: bool,
}

fn parse_zset_op_args<'a>(args: &'a [&'a [u8]], allow_withscores: bool) -> Option<ZSetOpArgs<'a>> {
  if args.is_empty() {
    return None;
  }
  let numkeys = ParseUtils::try_read_long(args[0])?;
  if numkeys <= 0 {
    return None;
  }
  let numkeys = numkeys as usize;
  if args.len() < numkeys + 1 {
    return None;
  }
  let keys = args[1..=numkeys].to_vec();
  let mut weights = Vec::new();
  let mut aggregate = AggregateType::Sum;
  let mut with_scores = false;

  let mut idx = numkeys + 1;
  while idx < args.len() {
    let arg = args[idx];
    if arg.eq_ignore_ascii_case(b"WITHSCORES") && allow_withscores {
      with_scores = true;
      idx += 1;
    } else if arg.eq_ignore_ascii_case(b"WEIGHTS") {
      idx += 1;
      if idx + numkeys > args.len() {
        return None;
      }
      for _ in 0..numkeys {
        let w = ParseUtils::try_read_double(args[idx], false)?;
        weights.push(w);
        idx += 1;
      }
    } else if arg.eq_ignore_ascii_case(b"AGGREGATE") {
      idx += 1;
      if idx >= args.len() {
        return None;
      }
      let agg_str = args[idx];
      if agg_str.eq_ignore_ascii_case(b"SUM") {
        aggregate = AggregateType::Sum;
      } else if agg_str.eq_ignore_ascii_case(b"MIN") {
        aggregate = AggregateType::Min;
      } else if agg_str.eq_ignore_ascii_case(b"MAX") {
        aggregate = AggregateType::Max;
      } else {
        return None;
      }
      idx += 1;
    } else {
      return None;
    }
  }

  Some(ZSetOpArgs {
    keys,
    weights,
    aggregate,
    with_scores,
  })
}

/// 纯同步快速分发结果状态（严格对标 C# Garnet ProcessBasicCommands / TryConsumeMessages）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FastPathResult {
  /// 已在当前线程调用栈上纯同步完成处理（零 Future 构造与挂起开销）
  Handled,
  /// 需回退到通用异步调度器（冷数据磁盘 I/O、复杂集合对象、跨槽事务等）
  NeedsAsync,
}

/// 命令分发执行器
pub struct CommandDispatcher;

impl CommandDispatcher {
  /// 纯同步快速分发路由（严格对标 C# Garnet ProcessBasicCommands & BasicCommands.cs）
  ///
  /// 在非事务状态下，针对 95% 以上命中内存驻留区的热点基础指令（PING, AUTH, SELECT, QUIT, ECHO, GET, INCR, DECR, SETBIT, GETBIT, STRLEN 等），
  /// 在当前线程调用栈上直接就地同步执行并格式化输出到网络缓冲区，
  /// 彻底消除巨型异步状态机构造、I-Cache 颠簸与 Future 轮询开销！
  #[inline]
  pub fn try_dispatch_sync(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> FastPathResult {
    if cmd == RespCommand::NONE {
      return FastPathResult::Handled;
    }

    if args.len() > 65536 {
      buf.write_error(b"ERR excessive number of arguments");
      return FastPathResult::Handled;
    }

    // 认证检查
    if !session.authenticated && !matches!(cmd, RespCommand::AUTH | RespCommand::QUIT) {
      buf.write_error(b"NOAUTH Authentication required.");
      return FastPathResult::Handled;
    }

    // 动态 ACL 权限拦截（经会话名字空间作用域视图，租户用户按复合主键解析）
    let username = session.user.as_deref().unwrap_or("default");
    if !matches!(cmd, RespCommand::AUTH | RespCommand::QUIT)
      && !ctx.acl.scope(session.user_ns).can_execute(username, cmd)
    {
      warn!(target: "wedb::audit::acl", "权限拦截: user='{username}' 无权执行指令 '{cmd:?}'");
      buf.write_error_fmt(format_args!(
        "NOPERM this user has no permissions to run the '{cmd:?}' command"
      ));
      return FastPathResult::Handled;
    }

    // 事务或集群模式下，交由完整异步路由处理
    if session.in_txn || ctx.cluster.is_some() {
      return FastPathResult::NeedsAsync;
    }

    match cmd {
      RespCommand::PING => {
        if args.is_empty() {
          buf.write_pong();
        } else {
          buf.write_bulk_string(args[0]);
        }
        FastPathResult::Handled
      }
      RespCommand::AUTH => match args.len() {
        1 => {
          let pwd = from_utf8(args[0]).unwrap_or("");
          if ctx.acl.auth_default(pwd) {
            session.set_authenticated("default", ctx.acl.default_user().read().namespace);
            info!(target: "wedb::audit::auth", "默认用户认证成功");
            buf.write_ok();
          } else {
            warn!(target: "wedb::audit::auth", "默认用户密码认证失败");
            buf.write_error(b"WRONGPASS invalid username-password pair or user is disabled.");
          }
          FastPathResult::Handled
        }
        2 => {
          let token = from_utf8(args[0]).unwrap_or("");
          let pwd = from_utf8(args[1]).unwrap_or("");
          // 「用户名#空间」凭据解析失败（语法/非法空间值）直接报 ERR，与 WRONGPASS 严格区分
          match wedb_acl::parse_user_token(token) {
            Err(e) => {
              warn!(target: "wedb::audit::auth", "AUTH 凭据语法非法: token='{token}', err={e:?}");
              write_err_line(buf, &e.to_string());
              FastPathResult::Handled
            }
            Ok((username, ns)) => {
              // 单次内存点查闭环三分支：命中+口令对 / 命中+口令错 / 未命中降级异步懒加载
              match ctx.acl.scope(ns).get_user(username) {
                Some(h) => {
                  if h.read().authenticate(pwd) {
                    session.set_authenticated(username, ns);
                    info!(target: "wedb::audit::auth", "用户认证成功: user='{username}', ns={ns:?}");
                    buf.write_ok();
                  } else {
                    warn!(target: "wedb::audit::auth", "用户密码认证失败: user='{username}', ns={ns:?}");
                    buf.write_error(
                      b"WRONGPASS invalid username-password pair or user is disabled.",
                    );
                  }
                  FastPathResult::Handled
                }
                // 内存未命中：可能是持久层冷用户，交由异步存储点查懒加载认证
                None => FastPathResult::NeedsAsync,
              }
            }
          }
        }
        _ => {
          buf.write_error(b"ERR wrong number of arguments for 'auth' command");
          FastPathResult::Handled
        }
      },
      RespCommand::SELECT => {
        Self::handle_select(session, args, buf);
        FastPathResult::Handled
      }
      RespCommand::QUIT => {
        session.is_closed = true;
        buf.write_ok();
        FastPathResult::Handled
      }
      RespCommand::ECHO => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'echo' command");
        } else {
          buf.write_bulk_string(args[0]);
        }
        FastPathResult::Handled
      }
      RespCommand::GET => {
        if args.len() != 1 {
          FastPathResult::NeedsAsync
        } else {
          let key = args[0];
          // 快路径同步 TTL 探针：已过期或 TTL 记录冷在磁盘时转异步（read_with 的
          // check_expired 惰性删除裁决），杜绝同步直读脏数据
          if sync_ttl_expired(
            &session.store_session,
            key,
            Clock::now_since_epoch().as_millis(),
          ) != Some(false)
          {
            return FastPathResult::NeedsAsync;
          }
          match session.store_session.try_read_string_in_memory(key, |val| {
            buf.write_bulk_string(val);
          }) {
            Ok(Some(Some(()))) => FastPathResult::Handled,
            Ok(Some(None)) => {
              buf.write_null();
              FastPathResult::Handled
            }
            _ => FastPathResult::NeedsAsync,
          }
        }
      }
      RespCommand::SET => {
        if args.len() != 2 {
          FastPathResult::NeedsAsync
        } else {
          let key = args[0];
          let val = args[1];
          // 快速检查对象元数据防 WRONGTYPE（严格对标 Garnet NetworkSET）
          match session.store_session.check_object_meta_fast(key) {
            Ok(Some(false)) => {}
            _ => return FastPathResult::NeedsAsync,
          }
          match session.store_session.try_upsert_sync(key, val) {
            Ok(Ok(_addr)) => {
              buf.write_ok();
              FastPathResult::Handled
            }
            _ => FastPathResult::NeedsAsync,
          }
        }
      }
      RespCommand::DEL => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'del' command");
          FastPathResult::Handled
        } else if args.len() == 1 {
          // 单键删除快速同步闭环：复用 try_delete_sync，兼备元数据预检与纯内存同步脱钩/墓碑追加。
          // 注意必须走 user 键入口（内部完成方案 A 前缀编码），严禁直接调 raw 接口传裸键
          match session.store_session.try_delete_sync(args[0]) {
            Ok(Ok(deleted)) => {
              buf.write_integer(if deleted { 1 } else { 0 });
              FastPathResult::Handled
            }
            _ => FastPathResult::NeedsAsync,
          }
        } else {
          // 批量键 DEL 杜绝半提交状态撕裂：直接交由异步 dispatch 循环执行，
          // 确保删除计数绝对准确，同时内部依然享受纯内存同步删除快路径
          FastPathResult::NeedsAsync
        }
      }
      RespCommand::INCR => {
        if args.len() != 1 {
          FastPathResult::NeedsAsync
        } else {
          let key = args[0];
          let mut itoa_buf = Buffer::new();
          let try_res = session.store_session.try_modify_in_place(key, |bytes| {
            let s = from_utf8(bytes).ok()?;
            let old_val = s.trim().parse::<i64>().ok()?;
            let new_val = old_val.checked_add(1)?;
            let formatted = itoa_buf.format(new_val);
            if formatted.len() == bytes.len() {
              bytes.copy_from_slice(formatted.as_bytes());
              Some(new_val)
            } else {
              None
            }
          });
          match try_res {
            Ok(Some(new_val)) => {
              buf.write_integer(new_val);
              FastPathResult::Handled
            }
            _ => FastPathResult::NeedsAsync,
          }
        }
      }
      RespCommand::DECR => {
        if args.len() != 1 {
          FastPathResult::NeedsAsync
        } else {
          let key = args[0];
          let mut itoa_buf = Buffer::new();
          let try_res = session.store_session.try_modify_in_place(key, |bytes| {
            let s = from_utf8(bytes).ok()?;
            let old_val = s.trim().parse::<i64>().ok()?;
            let new_val = old_val.checked_add(-1)?;
            let formatted = itoa_buf.format(new_val);
            if formatted.len() == bytes.len() {
              bytes.copy_from_slice(formatted.as_bytes());
              Some(new_val)
            } else {
              None
            }
          });
          match try_res {
            Ok(Some(new_val)) => {
              buf.write_integer(new_val);
              FastPathResult::Handled
            }
            _ => FastPathResult::NeedsAsync,
          }
        }
      }
      RespCommand::SETBIT => {
        if args.len() != 3 {
          FastPathResult::NeedsAsync
        } else {
          let Some(offset) = ParseUtils::try_read_long(args[1]) else {
            buf.write_error(consts::err::BIT_OFFSET_OUT_OF_RANGE);
            return FastPathResult::Handled;
          };
          if !(0..=4_294_967_295).contains(&offset) {
            buf.write_error(consts::err::BIT_OFFSET_OUT_OF_RANGE);
            return FastPathResult::Handled;
          }
          let val_bit = match args[2] {
            b"0" => 0u8,
            b"1" => 1u8,
            _ => {
              buf.write_error(b"ERR bit is not an integer or out of range");
              return FastPathResult::Handled;
            }
          };
          let offset = offset as usize;
          let key = args[0];
          let byte_idx = offset >> 3;
          let bit_idx = 7 - (offset & 7);
          let try_res = session.store_session.try_modify_in_place(key, |bytes| {
            if bytes.len() > byte_idx {
              let old_bit = (bytes[byte_idx] >> bit_idx) & 1;
              if val_bit != 0 {
                bytes[byte_idx] |= 1 << bit_idx;
              } else {
                bytes[byte_idx] &= !(1 << bit_idx);
              }
              Some(old_bit)
            } else {
              None
            }
          });
          match try_res {
            Ok(Some(old_bit)) => {
              buf.write_integer(old_bit as i64);
              FastPathResult::Handled
            }
            _ => FastPathResult::NeedsAsync,
          }
        }
      }
      RespCommand::GETBIT => {
        if args.len() != 2 {
          FastPathResult::NeedsAsync
        } else {
          let Some(offset) = ParseUtils::try_read_long(args[1]) else {
            buf.write_error(consts::err::BIT_OFFSET_OUT_OF_RANGE);
            return FastPathResult::Handled;
          };
          if !(0..=4_294_967_295).contains(&offset) {
            buf.write_error(consts::err::BIT_OFFSET_OUT_OF_RANGE);
            return FastPathResult::Handled;
          }
          let offset = offset as usize;
          let key = args[0];
          let byte_idx = offset >> 3;
          let bit_idx = 7 - (offset & 7);
          match session
            .store_session
            .try_read_string_in_memory(key, |bytes| {
              if byte_idx < bytes.len() {
                (bytes[byte_idx] >> bit_idx) & 1
              } else {
                0
              }
            }) {
            Ok(Some(Some(bit))) => {
              buf.write_integer(bit as i64);
              FastPathResult::Handled
            }
            Ok(Some(None)) => {
              buf.write_integer(0);
              FastPathResult::Handled
            }
            _ => FastPathResult::NeedsAsync,
          }
        }
      }
      RespCommand::STRLEN => {
        if args.len() != 1 {
          FastPathResult::NeedsAsync
        } else {
          let key = args[0];
          match session
            .store_session
            .try_read_string_in_memory(key, |bytes| bytes.len())
          {
            Ok(Some(Some(len))) => {
              buf.write_integer(len as i64);
              FastPathResult::Handled
            }
            Ok(Some(None)) => {
              buf.write_integer(0);
              FastPathResult::Handled
            }
            _ => FastPathResult::NeedsAsync,
          }
        }
      }
      _ => FastPathResult::NeedsAsync,
    }
  }

  /// 主分发路由入口
  pub async fn dispatch(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    // 0. 空命令直接忽略
    if cmd == RespCommand::NONE {
      return Ok(());
    }

    // 0.1 参数超限保护（防御恶意巨量参数攻击导致 OOM）
    if args.len() > 65536 {
      buf.write_error(b"ERR excessive number of arguments");
      return Ok(());
    }

    // 1. 认证拦截：若未认证且非认证相关/QUIT命令，直接拦截
    if !session.authenticated && !matches!(cmd, RespCommand::AUTH | RespCommand::QUIT) {
      buf.write_error(b"NOAUTH Authentication required.");
      return Ok(());
    }

    // 1.1 动态 ACL 权限拦截（经会话名字空间作用域视图，租户用户按复合主键解析）
    let username = session.user.as_deref().unwrap_or("default");
    if !matches!(cmd, RespCommand::AUTH | RespCommand::QUIT)
      && !ctx.acl.scope(session.user_ns).can_execute(username, cmd)
    {
      warn!(target: "wedb::audit::acl", "权限拦截: user='{username}' 无权执行指令 '{cmd:?}'");
      buf.write_error_fmt(format_args!(
        "NOPERM this user has no permissions to run the '{cmd:?}' command"
      ));
      return Ok(());
    }

    // 2. 事务排队拦截：若处于 MULTI 事务中且不是事务控制指令与 QUIT，校验并推入队列
    if session.in_txn
      && !matches!(
        cmd,
        RespCommand::EXEC | RespCommand::DISCARD | RespCommand::MULTI | RespCommand::QUIT
      )
    {
      // WATCH 在 MULTI 事务内只报错，绝不置脏事务（Redis 原生语义）
      if cmd == RespCommand::WATCH {
        buf.write_error(b"ERR WATCH inside MULTI is not allowed");
        return Ok(());
      }
      // SELECT/SWAPDB 切换数据库会撕裂事务两阶段锁与 (ns, db) 键空间一致性，
      // 须报错并置脏事务（EXEC 时 EXECABORT）；SELECT 目标与当前库相同则视为无操作放行
      if is_db_switch(cmd, args, session.active_db) {
        session.txn_aborted = true;
        buf.write_error(b"ERR switching databases inside a transaction is not allowed");
        return Ok(());
      }
      if let Err(err_msg) = validate_command_syntax(cmd, args) {
        session.txn_aborted = true;
        buf.write_error(err_msg.as_bytes());
        return Ok(());
      }
      let queued_args = args.iter().map(|a| a.to_vec()).collect();
      session.txn_queue.push(QueuedCommand {
        cmd,
        args: queued_args,
      });
      buf.write_queued();
      return Ok(());
    }

    // 3. 集群模式路由拦截（跨槽多键检测与重定向，零额外堆分配）
    if let Some(ref cluster) = ctx.cluster {
      let keys = extract_keys(cmd, args);
      let is_read = is_read_command(cmd);
      let read_only = session.is_readonly && is_read;
      let route = match keys {
        CommandKeys::None => None,
        CommandKeys::Single(key) => {
          let key_exists = session.store_session.contains_key(key).await?;
          Some(cluster.verify_key(key, key_exists, session.asking, read_only))
        }
        CommandKeys::Slice(keys_slice) => {
          let key_exists = if let Some(&first_key) = keys_slice.first() {
            session.store_session.contains_key(first_key).await?
          } else {
            false
          };
          Some(cluster.verify_keys(keys_slice, key_exists, session.asking, read_only))
        }
        CommandKeys::Small(ref arr, len) => {
          let keys_slice = &arr[..len];
          let key_exists = if let Some(&first_key) = keys_slice.first() {
            session.store_session.contains_key(first_key).await?
          } else {
            false
          };
          Some(cluster.verify_keys(keys_slice, key_exists, session.asking, read_only))
        }
        CommandKeys::Vec(ref keys_vec) => {
          if keys_vec.is_empty() {
            None
          } else {
            let key_exists = session.store_session.contains_key(keys_vec[0]).await?;
            Some(cluster.verify_keys(keys_vec, key_exists, session.asking, read_only))
          }
        }
      };

      if let Some(route) = route {
        match route {
          RouteResult::Ok(_) => {
            // 本地负责该槽位，放行执行
          }
          RouteResult::Moved { slot, endpoint } => {
            session.clear_asking();
            let mut num_buf = Buffer::new();
            buf.write_raw(b"-MOVED ");
            buf.write_raw(num_buf.format(slot).as_bytes());
            buf.write_raw(b" ");
            buf.write_raw(endpoint.as_bytes());
            buf.write_raw(CRLF);
            return Ok(());
          }
          RouteResult::Ask { slot, endpoint } => {
            session.clear_asking();
            let mut num_buf = Buffer::new();
            buf.write_raw(b"-ASK ");
            buf.write_raw(num_buf.format(slot).as_bytes());
            buf.write_raw(b" ");
            buf.write_raw(endpoint.as_bytes());
            buf.write_raw(CRLF);
            return Ok(());
          }
          RouteResult::CrossSlot => {
            session.clear_asking();
            buf.write_error(b"CROSSSLOT Keys in request don't hash to the same slot");
            return Ok(());
          }
          RouteResult::ClusterDown => {
            session.clear_asking();
            buf.write_error(b"CLUSTERDOWN Hash slot not served");
            return Ok(());
          }
        }
      }
    }

    // 4. 执行命令
    Self::execute_command(ctx, session, cmd, args, buf).await?;

    // 5. 单次 ASKING 消耗重置
    if session.asking {
      session.clear_asking();
    }

    Ok(())
  }

  /// 顶层事务包装与分发
  async fn execute_command(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    match cmd {
      // ====== 事务命令 ======
      RespCommand::MULTI => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'multi' command");
        } else if session.in_txn {
          buf.write_error(b"ERR MULTI calls can not be nested");
        } else {
          session.in_txn = true;
          session.txn_aborted = false;
          session.txn_queue.clear();
          buf.write_ok();
        }
      }
      RespCommand::DISCARD => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'discard' command");
        } else if !session.in_txn {
          buf.write_error(b"ERR DISCARD without MULTI");
        } else {
          session.reset_txn();
          buf.write_ok();
        }
      }
      RespCommand::EXEC => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'exec' command");
          return Ok(());
        }
        if !session.in_txn {
          buf.write_error(b"ERR EXEC without MULTI");
          return Ok(());
        }
        if session.txn_aborted {
          session.reset_txn();
          buf.write_error(b"EXECABORT Transaction discarded because of previous errors.");
          return Ok(());
        }
        let queued = mem::take(&mut session.txn_queue);
        session.in_txn = false;

        // 对标 Garnet TxnKeyEntry.LockAllKeys:
        // 在执行事务前，收集事务队列所有命令涉及的键哈希与读写类型，按哈希桶两阶段锁进行原子并发加锁
        let mut lock_items = Vec::new();
        let mut temp_state = SessionParseState::with_capacity(16);
        for item in &queued {
          temp_state.clear();
          temp_state.reserve(item.args.len());
          for a in &item.args {
            temp_state.push(a);
          }
          let is_exclusive = !is_read_command(item.cmd);
          let keys = extract_keys(item.cmd, &temp_state);
          let mut add_key = |k: &[u8]| {
            if !k.is_empty() {
              lock_items.push((gxhash64(k, 0), is_exclusive));
            }
          };
          match keys {
            CommandKeys::None => {}
            CommandKeys::Single(k) => add_key(k),
            CommandKeys::Slice(ks) => {
              for &k in ks {
                add_key(k);
              }
            }
            CommandKeys::Small(ks, n) => {
              for &k in &ks[..n] {
                add_key(k);
              }
            }
            CommandKeys::Vec(ks) => {
              for k in ks {
                add_key(k);
              }
            }
          }
        }
        let _guard = if !lock_items.is_empty() {
          Some(
            ctx
              .store
              .index
              .acquire_hash_locks(&lock_items)
              .map_err(wkv::Error::from)?,
          )
        } else {
          None
        };

        buf.write_array_header(queued.len());
        let mut sub_state = SessionParseState::with_capacity(16);
        for item in &queued {
          sub_state.clear();
          sub_state.reserve(item.args.len());
          for a in &item.args {
            sub_state.push(a);
          }
          if let Err(e) =
            Self::execute_single_command(ctx, session, item.cmd, &sub_state, buf).await
          {
            if e.is_wrong_type() {
              buf.write_error(consts::err::WRONG_TYPE);
            } else {
              return Err(e);
            }
          }
        }
      }
      _ => {
        Self::execute_single_command(ctx, session, cmd, args, buf).await?;
      }
    }
    Ok(())
  }

  /// 单条具体命令执行逻辑（彻底消除递归 async 调用与 Box::pin 堆分配）
  async fn execute_single_command(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    match cmd {
      // ====== Server / 基础命令 ======
      RespCommand::PING => {
        if args.len() > 1 {
          buf.write_error(b"ERR wrong number of arguments for 'ping' command");
        } else if args.is_empty() {
          buf.write_pong();
        } else {
          buf.write_bulk_string(args[0]);
        }
      }
      RespCommand::ECHO => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'echo' command");
        } else {
          buf.write_bulk_string(args[0]);
        }
      }
      RespCommand::QUIT => {
        session.close();
        buf.write_ok();
      }
      RespCommand::INFO => {
        let section = args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("");
        Self::write_info(ctx, section, buf);
      }
      RespCommand::ROLE => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'role' command");
          return Ok(());
        }
        buf.write_array_header(3);
        buf.write_bulk_string(b"master");
        buf.write_integer(0);
        buf.write_array_header(0);
      }
      RespCommand::HELLO => {
        if args.len() > 6 {
          buf.write_error(b"ERR wrong number of arguments for 'hello' command");
          return Ok(());
        }
        let mut proto_ver = 2i64;
        if !args.is_empty() {
          let Some(ver) = ParseUtils::try_read_long(args[0]) else {
            buf.write_error(b"ERR Protocol version is not an integer or out of range");
            return Ok(());
          };
          if !(2..=3).contains(&ver) {
            buf.write_error(b"NOPROTO unsupported protocol version");
            return Ok(());
          }
          proto_ver = ver;
          let mut idx = 1;
          while idx < args.len() {
            if args[idx].eq_ignore_ascii_case(b"AUTH") {
              if args.len() - idx < 3 {
                buf.write_error(consts::err::SYNTAX);
                return Ok(());
              }
              let username = args[idx + 1];
              let password = args[idx + 2];
              let user_str = from_utf8(username).unwrap_or("");
              let pass_str = from_utf8(password).unwrap_or("");
              match Self::auth_and_bind_session(ctx, session, user_str, pass_str).await {
                Ok(true) => {}
                Ok(false) => {
                  buf.write_error(b"WRONGPASS invalid username-password pair or user is disabled.");
                  return Ok(());
                }
                Err(e) => {
                  write_err_line(buf, &e.to_string());
                  return Ok(());
                }
              }
              idx += 3;
            } else if args[idx].eq_ignore_ascii_case(b"SETNAME") {
              if args.len() - idx < 2 {
                buf.write_error(consts::err::SYNTAX);
                return Ok(());
              }
              idx += 2;
            } else {
              buf.write_error(consts::err::SYNTAX);
              return Ok(());
            }
          }
        }
        buf.write_array_header(14);
        buf.write_bulk_string(b"server");
        buf.write_bulk_string(b"redis");
        buf.write_bulk_string(b"version");
        buf.write_bulk_string(b"7.2.4");
        buf.write_bulk_string(b"proto");
        buf.write_integer(proto_ver);
        buf.write_bulk_string(b"id");
        buf.write_integer(session.id as i64);
        buf.write_bulk_string(b"mode");
        buf.write_bulk_string(b"standalone");
        buf.write_bulk_string(b"role");
        buf.write_bulk_string(b"master");
        buf.write_bulk_string(b"modules");
        buf.write_array_header(0);
      }
      RespCommand::SAVE => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'save' command");
          return Ok(());
        }
        // SAVE = 同步完整 Checkpoint (hlog + HashIndex + RangeIndex + 共享 BfTree)，
        // 快照失败向客户端暴露错误 (对标 Garnet 快照失败致命语义)
        match CheckpointManager::new()
          .create_checkpoint(&ctx.store, &ctx.checkpoint_dir, CheckpointType::FoldOver)
          .await
        {
          Ok(meta) => {
            ctx.last_save_ms.store(meta.created_at, Ordering::Release);
            buf.write_simple_string(b"OK");
          }
          Err(e) => {
            let mut err_msg = String::from("ERR save failed: ");
            err_msg.push_str(&e.to_string());
            buf.write_error(err_msg.as_bytes());
          }
        }
      }
      RespCommand::BGSAVE => {
        if args.len() > 2 {
          buf.write_error(b"ERR wrong number of arguments for 'bgsave' command");
          return Ok(());
        }
        // BGSAVE = 后台异步完整 Checkpoint，各 Token 文件集互不相交天然免锁
        let store = ctx.store.clone();
        let checkpoint_dir = ctx.checkpoint_dir.clone();
        let last_save = ctx.last_save_ms.clone();
        spawn(async move {
          match CheckpointManager::new()
            .create_checkpoint(&store, &checkpoint_dir, CheckpointType::FoldOver)
            .await
          {
            Ok(meta) => {
              last_save.store(meta.created_at, Ordering::Release);
            }
            Err(e) => error!("BGSAVE Checkpoint 失败: {e}"),
          }
        })
        .detach();
        buf.write_simple_string(b"Background saving started");
      }
      RespCommand::LASTSAVE => {
        if args.len() > 1 {
          buf.write_error(b"ERR wrong number of arguments for 'lastsave' command");
          return Ok(());
        }
        let saved_ms = ctx.last_save_ms.load(Ordering::Acquire);
        buf.write_integer((saved_ms / 1000) as i64);
      }
      RespCommand::COMMAND
      | RespCommand::COMMAND_COUNT
      | RespCommand::COMMAND_DOCS
      | RespCommand::COMMAND_INFO
      | RespCommand::COMMAND_GETKEYS
      | RespCommand::COMMAND_GETKEYSANDFLAGS => {
        Self::handle_command(ctx, session, cmd, args, buf);
      }
      RespCommand::SELECT => {
        Self::handle_select(session, args, buf);
      }
      RespCommand::CLIENT
      | RespCommand::CLIENT_ID
      | RespCommand::CLIENT_INFO
      | RespCommand::CLIENT_LIST
      | RespCommand::CLIENT_KILL
      | RespCommand::CLIENT_GETNAME
      | RespCommand::CLIENT_SETNAME
      | RespCommand::CLIENT_SETINFO
      | RespCommand::CLIENT_UNBLOCK => {
        Self::handle_client(ctx, session, cmd, args, buf);
      }
      RespCommand::CONFIG
      | RespCommand::CONFIG_GET
      | RespCommand::CONFIG_SET
      | RespCommand::CONFIG_REWRITE => {
        Self::handle_config(ctx, session, cmd, args, buf);
      }
      RespCommand::FLUSHDB | RespCommand::FLUSHALL => {
        // FLUSHALL 清空全部键空间；FLUSHDB 规范上仅清当前 (ns, db) 视界，
        // 底层存储尚未提供会话前缀级清空 API，暂统一走全量清空（跨 crate 联动项）
        ctx.store.flush_all().await?;
        buf.write_ok();
      }

      // ====== ACL / 认证 ======
      RespCommand::AUTH => {
        if args.is_empty() || args.len() > 2 {
          buf.write_error(b"ERR wrong number of arguments for 'auth' command");
        } else if args.len() == 1 {
          let pwd = from_utf8(args[0]).unwrap_or("");
          if ctx.acl.auth_default(pwd) {
            session.set_authenticated("default", ctx.acl.default_user().read().namespace);
            info!(target: "wedb::audit::auth", "默认用户认证成功");
            buf.write_ok();
          } else {
            warn!(target: "wedb::audit::auth", "默认用户密码认证失败");
            buf.write_error(b"WRONGPASS invalid username-password pair or user is disabled.");
          }
        } else {
          let token = from_utf8(args[0]).unwrap_or("");
          let pwd = from_utf8(args[1]).unwrap_or("");
          match Self::auth_and_bind_session(ctx, session, token, pwd).await {
            Ok(true) => {
              info!(target: "wedb::audit::auth", "用户异步认证成功: token='{token}'");
              buf.write_ok();
            }
            Ok(false) => {
              warn!(target: "wedb::audit::auth", "用户异步认证失败: token='{token}'");
              buf.write_error(b"WRONGPASS invalid username-password pair or user is disabled.");
            }
            Err(e) => {
              warn!(target: "wedb::audit::auth", "AUTH 凭据语法非法: token='{token}', err={e:?}");
              write_err_line(buf, &e.to_string());
            }
          }
        }
      }
      RespCommand::ACL
      | RespCommand::ACL_WHOAMI
      | RespCommand::ACL_LIST
      | RespCommand::ACL_USERS
      | RespCommand::ACL_SETUSER
      | RespCommand::ACL_DELUSER
      | RespCommand::ACL_GETUSER
      | RespCommand::ACL_GENPASS
      | RespCommand::ACL_LOAD
      | RespCommand::ACL_SAVE
      | RespCommand::ACL_CAT => {
        Self::handle_acl(ctx, session, cmd, args, buf).await;
      }

      // ====== KV 存储命令 ======
      RespCommand::GET => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'get' command");
          return Ok(());
        }
        let key = args[0];
        // 零拷贝直写响应缓冲：store 层 read_with 已将 TTL 过期裁决前移到读闭包执行前，
        // 过期键绝不会触碰响应缓冲，无需按值读规避双写
        match session
          .store_session
          .read_string_with(key, |val| buf.write_bulk_string(val))
          .await
        {
          Ok(Some(())) => {}
          Ok(None) => buf.write_null(),
          Err(err) if err.is_wrong_type() => buf.write_error(consts::err::WRONG_TYPE),
          Err(err) => return Err(err.into()),
        }
      }
      RespCommand::SET => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'set' command");
          return Ok(());
        }
        let key = args[0];
        let val = args[1];
        // 选项解析（EX/PX/EXAT/PXAT/KEEPTTL/NX/XX/GET，对标 C# BasicCommands.NetworkSETEXNX）
        let opts = match parse_set_options(&args[2..]) {
          Ok(o) => o,
          Err(msg) => {
            buf.write_error(msg.as_bytes());
            return Ok(());
          }
        };
        // GET 选项先行取旧值：对象类型键直接 WRONGTYPE 终止（对齐 Redis 语义）
        let mut old: Option<Vec<u8>> = None;
        if opts.get
          && let Err(err) = session
            .store_session
            .read_string_with(key, |v| old = Some(v.to_vec()))
            .await
        {
          if err.is_wrong_type() {
            buf.write_error(consts::err::WRONG_TYPE);
          } else {
            buf.write_error_fmt(format_args!("ERR {err}"));
          }
          return Ok(());
        }
        let exists = session.store_session.contains_key(key).await?;
        if (opts.nx && exists) || (opts.xx && !exists) {
          // 存在性条件不成立：不写入，仅按 GET 语义回旧值
          match old {
            Some(v) => buf.write_bulk_string(&v),
            None => buf.write_null(),
          }
          return Ok(());
        }
        if let Some(meta) = session.store_session.load_meta(key).await?
          && meta.size > 0
        {
          buf.write_error(consts::err::WRONG_TYPE);
          return Ok(());
        }
        // KEEPTTL：写入前快照既有 TTL，写入后原样回填（值更新不影响过期点）。
        // 注意：快照与回填非原子（并发写窗口内过期点可能漂移），系对齐 Redis 单命令
        // 语义的尽力实现——store 层 upsert 写路径会同步清除 TTL，无法原位保留
        let keep_ttl = if opts.keepttl {
          session.store_session.ttl_of(key).await?
        } else {
          None
        };
        session.store_session.upsert(key, val).await?;
        let now_ms = Clock::now_since_epoch().as_millis();
        if let Some(rel_ms) = opts.expiry_ms {
          session
            .store_session
            .expire_at(key, now_ms.saturating_add(rel_ms), TtlOpt::NONE)
            .await?;
        } else if let Some(at_ms) = opts.expiry_at_ms {
          session
            .store_session
            .expire_at(key, at_ms, TtlOpt::NONE)
            .await?;
        } else if let Some(exp) = keep_ttl {
          session
            .store_session
            .expire_at(key, exp, TtlOpt::NONE)
            .await?;
        }
        if opts.get {
          match old {
            Some(v) => buf.write_bulk_string(&v),
            None => buf.write_null(),
          }
        } else {
          buf.write_ok();
        }
      }
      RespCommand::INCR | RespCommand::DECR => {
        let is_incr = cmd == RespCommand::INCR;
        let cmd_str = if is_incr { "incr" } else { "decr" };
        if args.len() != 1 {
          buf.write_error_fmt(format_args!(
            "ERR wrong number of arguments for '{cmd_str}' command"
          ));
          return Ok(());
        }
        let delta = if is_incr { 1 } else { -1 };
        match session.store_session.incrby(args[0], delta).await {
          Ok(new_val) => buf.write_integer(new_val),
          Err(err) => {
            if err.is_wrong_type() {
              buf.write_error(consts::err::WRONG_TYPE);
            } else {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
            }
          }
        }
      }
      RespCommand::INCRBY | RespCommand::DECRBY => {
        let is_incr = cmd == RespCommand::INCRBY;
        let cmd_str = if is_incr { "incrby" } else { "decrby" };
        if args.len() != 2 {
          buf.write_error_fmt(format_args!(
            "ERR wrong number of arguments for '{cmd_str}' command"
          ));
          return Ok(());
        }
        let Some(step) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let delta = if is_incr {
          step
        } else {
          match step.checked_neg() {
            Some(neg) => neg,
            None => {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            }
          }
        };
        match session.store_session.incrby(args[0], delta).await {
          Ok(new_val) => buf.write_integer(new_val),
          Err(err) => {
            if err.is_wrong_type() {
              buf.write_error(consts::err::WRONG_TYPE);
            } else {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
            }
          }
        }
      }
      RespCommand::INCRBYFLOAT => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'incrbyfloat' command");
          return Ok(());
        }
        let Some(step) = ParseUtils::try_read_double(args[1], true) else {
          buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
          return Ok(());
        };
        if step.is_nan() || step.is_infinite() {
          buf.write_error(consts::err::NAN_OR_INFINITY);
          return Ok(());
        }
        match session.store_session.incrbyfloat(args[0], step).await {
          Ok(new_val) => buf.write_double_bulk(new_val),
          Err(wedb_redis::Error::NanOrInfinity) => {
            buf.write_error(consts::err::NAN_OR_INFINITY);
          }
          Err(err) => {
            if err.is_wrong_type() {
              buf.write_error(consts::err::WRONG_TYPE);
            } else {
              buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
            }
          }
        }
      }
      RespCommand::MGET => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'mget' command");
          return Ok(());
        }
        buf.write_array_header(args.len());
        session
          .store_session
          .mget_each(&args[..], |val_opt| match val_opt {
            Some(val) => buf.write_bulk_string(val),
            None => buf.write_null(),
          })
          .await?;
      }
      RespCommand::MSET => {
        if args.len() < 2 || !args.len().is_multiple_of(2) {
          buf.write_error(b"ERR wrong number of arguments for 'mset' command");
          return Ok(());
        }
        let (chunks, _) = args.as_chunks::<2>();
        session.store_session.mset_chunks(chunks).await?;
        buf.write_ok();
      }
      RespCommand::GETSET => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'getset' command");
          return Ok(());
        }
        match session.store_session.getset(args[0], args[1]).await {
          Ok(Some(val)) => buf.write_bulk_string(&val),
          Ok(None) => buf.write_null(),
          Err(err) => {
            if err.is_wrong_type() {
              buf.write_error(consts::err::WRONG_TYPE);
            } else {
              buf.write_null();
            }
          }
        }
      }
      RespCommand::GETDEL => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'getdel' command");
          return Ok(());
        }
        match session.store_session.getdel(args[0]).await {
          Ok(Some(val)) => buf.write_bulk_string(&val),
          Ok(None) => buf.write_null(),
          Err(err) => {
            if err.is_wrong_type() {
              buf.write_error(consts::err::WRONG_TYPE);
            } else {
              buf.write_null();
            }
          }
        }
      }
      RespCommand::SETNX => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'setnx' command");
          return Ok(());
        }
        let exists = session.store_session.contains_key(args[0]).await?;
        if exists {
          buf.write_integer(0);
        } else {
          session.store_session.upsert(args[0], args[1]).await?;
          buf.write_integer(1);
        }
      }
      RespCommand::SETEX | RespCommand::PSETEX => {
        let is_ms = cmd == RespCommand::PSETEX;
        if args.len() != 3 {
          buf.write_error_fmt(format_args!(
            "ERR wrong number of arguments for '{}' command",
            cmd.as_str().to_ascii_lowercase()
          ));
          return Ok(());
        }
        let Some(ttl) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        if ttl <= 0 {
          buf.write_error_fmt(format_args!(
            "ERR invalid expire time in '{}' command",
            cmd.as_str().to_ascii_lowercase()
          ));
          return Ok(());
        }
        // 写值后落盘绝对过期毫秒（对标 C# SETEX 单命令携带过期元数据语义）
        session.store_session.upsert(args[0], args[2]).await?;
        let rel = ttl as u64;
        let rel_ms = if is_ms { rel } else { rel.saturating_mul(1000) };
        let now_ms = Clock::now_since_epoch().as_millis();
        session
          .store_session
          .expire_at(args[0], now_ms.saturating_add(rel_ms), TtlOpt::NONE)
          .await?;
        buf.write_ok();
      }
      RespCommand::SUBSTR => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'substr' command");
          return Ok(());
        }
        let (Some(start), Some(end)) = (
          ParseUtils::try_read_long(args[1]),
          ParseUtils::try_read_long(args[2]),
        ) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        match session
          .store_session
          .getrange(args[0], start as isize, end as isize)
          .await
        {
          Ok(bytes) => buf.write_bulk_string(&bytes),
          Err(err) => {
            if err.is_wrong_type() {
              buf.write_error(consts::err::WRONG_TYPE);
            } else {
              buf.write_bulk_string(b"");
            }
          }
        }
      }
      RespCommand::DEL => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'del' command");
          return Ok(());
        }
        let mut count = 0i64;
        for key in args.iter() {
          if session.store_session.delete(key).await? {
            count += 1;
          }
        }
        buf.write_integer(count);
      }
      RespCommand::DBSIZE => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'dbsize' command");
          return Ok(());
        }
        let count = session.store_session.dbsize().await?;
        buf.write_integer(count as i64);
      }
      RespCommand::TYPE => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'type' command");
          return Ok(());
        }
        let key = args[0];
        let t = session.store_session.type_of(key).await?;
        buf.write_simple_string(t.as_bytes());
      }
      RespCommand::RENAME | RespCommand::RENAMENX => {
        let is_nx = cmd == RespCommand::RENAMENX;
        if args.len() != 2 {
          let err: &[u8] = if is_nx {
            b"ERR wrong number of arguments for 'renamenx' command"
          } else {
            b"ERR wrong number of arguments for 'rename' command"
          };
          buf.write_error(err);
          return Ok(());
        }
        match session
          .store_session
          .rename(args[0], args[1], is_nx)
          .await?
        {
          RenameResult::NoSuchKey => {
            buf.write_error(b"ERR no such key");
          }
          RenameResult::AlreadyExists => {
            buf.write_integer(0);
          }
          RenameResult::SameKey => {
            if is_nx {
              buf.write_integer(0);
            } else {
              buf.write_ok();
            }
          }
          RenameResult::Success => {
            if is_nx {
              buf.write_integer(1);
            } else {
              buf.write_ok();
            }
          }
        }
      }
      RespCommand::EXISTS => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'exists' command");
          return Ok(());
        }
        let mut count = 0i64;
        for key in args.iter() {
          if session.store_session.contains_key(key).await? {
            count += 1;
          }
        }
        buf.write_integer(count);
      }
      // ====== 键生命周期家族（对标 C# KeyAdminCommands.NetworkEXPIRE / NetworkTTL /
      // NetworkPERSIST / NetworkEXPIRETIME）：经底层 TTL 记录真实读写 ======
      RespCommand::EXPIRE
      | RespCommand::PEXPIRE
      | RespCommand::EXPIREAT
      | RespCommand::PEXPIREAT => {
        Self::handle_expire(session, cmd, args, buf).await;
      }
      RespCommand::TTL | RespCommand::PTTL => {
        if args.len() != 1 {
          buf.write_error_fmt(format_args!(
            "ERR wrong number of arguments for '{}' command",
            cmd.as_str().to_ascii_lowercase()
          ));
          return Ok(());
        }
        let ms = session.store_session.pttl_ms(args[0]).await?;
        // 秒级换算仅作用于正剩余毫秒：向上取整 (p+999)/1000，保证活键 TTL 永不为 0（Redis 口径）；
        // -1/-2 哨兵值原样透传
        let reply = if cmd == RespCommand::PTTL || ms <= 0 {
          ms
        } else {
          (ms + 999) / 1000
        };
        buf.write_integer(reply);
      }
      RespCommand::PERSIST => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'persist' command");
          return Ok(());
        }
        let removed = session.store_session.persist(args[0]).await?;
        buf.write_integer(removed as i64);
      }
      RespCommand::EXPIRETIME | RespCommand::PEXPIRETIME => {
        if args.len() != 1 {
          let cmd_name = if cmd == RespCommand::EXPIRETIME {
            "expiretime"
          } else {
            "pexpiretime"
          };
          buf.write_error_fmt(format_args!(
            "ERR wrong number of arguments for '{cmd_name}' command"
          ));
          return Ok(());
        }
        let ms = session.store_session.expiretime_ms(args[0]).await?;
        let reply = if cmd == RespCommand::PEXPIRETIME || ms <= 0 {
          ms
        } else {
          ms / 1000
        };
        buf.write_integer(reply);
      }
      RespCommand::UNLINK => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'unlink' command");
          return Ok(());
        }
        let mut count = 0i64;
        for key in args.iter() {
          if session.store_session.delete(key).await? {
            count += 1;
          }
        }
        buf.write_integer(count);
      }
      RespCommand::EXPDELSCAN => {
        if args.len() > 1 {
          buf.write_error(b"ERR wrong number of arguments for 'expdelscan' command");
          return Ok(());
        }
        let bg_scan_enabled = (ctx.args.gc_enabled && ctx.args.expired_scan_interval_ms > 0)
          || (ctx.store.config.gc.enabled && ctx.store.config.gc.scan_interval_ms > 0);
        if bg_scan_enabled {
          buf.write_error(
            b"ERR Cannot execute EXPDELSCAN with background expired key deletion scan enabled",
          );
          return Ok(());
        }
        let db_id = if args.is_empty() {
          None
        } else {
          match ParseUtils::try_read_long(args[0]) {
            Some(id) if id >= 0 => Some(id as u64),
            _ => {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            }
          }
        };
        let (records_expired, records_scanned) =
          match ctx.store.expired_key_deletion_scan(db_id).await {
            Ok(v) => v,
            Err(e) => {
              error!("EXPDELSCAN 执行失败: {e:?}");
              buf.write_error_fmt(format_args!("ERR {e}"));
              return Ok(());
            }
          };
        buf.write_array_header(2);
        buf.write_integer(records_expired as i64);
        buf.write_integer(records_scanned as i64);
      }
      RespCommand::GETEX => {
        Self::handle_getex(session, args, buf).await;
      }
      RespCommand::KEYS => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'keys' command");
          return Ok(());
        }
        let matched_keys = session.store_session.keys(args[0]).await?;
        buf.write_array_header(matched_keys.len());
        for k in matched_keys {
          buf.write_bulk_string(&k);
        }
      }
      RespCommand::Scan => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'scan' command");
          return Ok(());
        }
        if ParseUtils::try_read_ulong(args[0]).is_none() {
          buf.write_error(b"ERR invalid cursor");
          return Ok(());
        }
        let mut pattern = b"*".as_slice();
        let mut i = 1;
        while i < args.len() {
          if args[i].eq_ignore_ascii_case(b"MATCH") && i + 1 < args.len() {
            pattern = args[i + 1];
            i += 2;
          } else if args[i].eq_ignore_ascii_case(b"COUNT") && i + 1 < args.len() {
            // COUNT 仅为渐进迭代提示：底层全量匹配单轮返回，提示值校验后无需消费
            if ParseUtils::try_read_long(args[i + 1]).is_none() {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            }
            i += 2;
          } else {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        }
        // 底层 keys() 为一次性全量匹配：单轮穷尽返回全部命中并回 next=0
        // （符合 SCAN「游标 0 即迭代结束」契约；真分页游标待存储层支持后接入）
        let matched = session.store_session.keys(pattern).await?;
        buf.write_array_header(2);
        buf.write_bulk_string(b"0");
        buf.write_array_header(matched.len());
        for k in &matched {
          buf.write_bulk_string(k);
        }
      }
      RespCommand::TIME => {
        if !args.is_empty() {
          buf.write_error(consts::err::WRONG_NUM_ARGS);
          return Ok(());
        }
        let now = Clock::now_since_epoch();
        let secs = now.as_secs();
        let micros = now.subsec_nanos() / 1000;
        let mut b1 = itoa::Buffer::new();
        let s_secs = b1.format(secs);
        let mut b2 = itoa::Buffer::new();
        let s_micros = b2.format(micros);
        buf.write_array_header(2);
        buf.write_bulk_string(s_secs.as_bytes());
        buf.write_bulk_string(s_micros.as_bytes());
      }
      RespCommand::MSETNX => {
        if args.len() < 2 || !args.len().is_multiple_of(2) {
          buf.write_error(b"ERR wrong number of arguments for 'msetnx' command");
          return Ok(());
        }
        let (chunks, _) = args.as_chunks::<2>();
        let ok = session.store_session.msetnx(chunks).await?;
        buf.write_integer(if ok { 1 } else { 0 });
      }
      RespCommand::APPEND => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'append' command");
          return Ok(());
        }
        match session.store_session.append(args[0], args[1]).await {
          Ok(len) => buf.write_integer(len as i64),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE);
          }
        }
      }
      RespCommand::STRLEN => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'strlen' command");
          return Ok(());
        }
        match session.store_session.strlen(args[0]).await {
          Ok(len) => buf.write_integer(len as i64),
          Err(err) if err.is_wrong_type() => {
            buf.write_error(consts::err::WRONG_TYPE);
          }
          Err(err) => return Err(err.into()),
        }
      }
      RespCommand::GETRANGE => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'getrange' command");
          return Ok(());
        }
        let (Some(start), Some(end)) = (
          ParseUtils::try_read_long(args[1]),
          ParseUtils::try_read_long(args[2]),
        ) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        match session
          .store_session
          .getrange_with(args[0], start as isize, end as isize, |slice| {
            buf.write_bulk_string(slice);
          })
          .await
        {
          Ok(Some(())) => {}
          Ok(None) => buf.write_bulk_string(b""),
          Err(err) if err.is_wrong_type() => buf.write_error(consts::err::WRONG_TYPE),
          Err(err) => return Err(err.into()),
        }
      }
      RespCommand::SETRANGE => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'setrange' command");
          return Ok(());
        }
        let Some(offset) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        if !(0..=536_870_911).contains(&offset) {
          buf.write_error(b"ERR offset is out of range");
          return Ok(());
        }
        match session
          .store_session
          .setrange(args[0], offset as usize, args[2])
          .await
        {
          Ok(len) => buf.write_integer(len as i64),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE);
          }
        }
      }
      RespCommand::SETBIT => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'setbit' command");
          return Ok(());
        }
        let Some(offset) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(consts::err::BIT_OFFSET_OUT_OF_RANGE);
          return Ok(());
        };
        if !(0..=4_294_967_295).contains(&offset) {
          buf.write_error(consts::err::BIT_OFFSET_OUT_OF_RANGE);
          return Ok(());
        }
        let Some(val) = ParseUtils::try_read_int(args[2]) else {
          buf.write_error(b"ERR bit is not an integer or out of range");
          return Ok(());
        };
        if val != 0 && val != 1 {
          buf.write_error(b"ERR bit is not an integer or out of range");
          return Ok(());
        }
        match session
          .store_session
          .setbit(args[0], offset as usize, val as u8)
          .await
        {
          Ok(old) => buf.write_integer(old as i64),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE);
          }
        }
      }
      RespCommand::GETBIT => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'getbit' command");
          return Ok(());
        }
        let Some(offset) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(consts::err::BIT_OFFSET_OUT_OF_RANGE);
          return Ok(());
        };
        if !(0..=4_294_967_295).contains(&offset) {
          buf.write_error(consts::err::BIT_OFFSET_OUT_OF_RANGE);
          return Ok(());
        }
        match session.store_session.getbit(args[0], offset as usize).await {
          Ok(bit) => buf.write_integer(bit as i64),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE);
          }
        }
      }
      RespCommand::BITCOUNT => {
        if args.len() != 1 && args.len() != 3 {
          buf.write_error(b"ERR syntax error");
          return Ok(());
        }
        let range = if args.len() == 3 {
          let (Some(s), Some(e)) = (
            ParseUtils::try_read_long(args[1]),
            ParseUtils::try_read_long(args[2]),
          ) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return Ok(());
          };
          Some((s as isize, e as isize))
        } else {
          None
        };
        match session.store_session.bitcount(args[0], range).await {
          Ok(count) => buf.write_integer(count as i64),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE);
          }
        }
      }
      RespCommand::BITPOS => {
        if args.len() < 2 || args.len() > 5 {
          buf.write_error(b"ERR wrong number of arguments for 'bitpos' command");
          return Ok(());
        }
        let key = args[0];
        let bit_arg = args[1];
        if bit_arg.len() != 1 || (bit_arg[0] != b'0' && bit_arg[0] != b'1') {
          buf.write_error(consts::err::BIT_MUST_BE_ZERO_OR_ONE);
          return Ok(());
        }
        let search_for = bit_arg[0] - b'0';
        let mut start = None;
        let mut end = None;
        let mut is_bit_index = false;

        if args.len() > 2 {
          let Some(s) = ParseUtils::try_read_long(args[2]) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return Ok(());
          };
          start = Some(s);

          if args.len() > 3 {
            let Some(e) = ParseUtils::try_read_long(args[3]) else {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            };
            end = Some(e);

            if args.len() > 4 {
              if args[4].eq_ignore_ascii_case(b"BIT") {
                is_bit_index = true;
              } else if !args[4].eq_ignore_ascii_case(b"BYTE") {
                buf.write_error(consts::err::SYNTAX);
                return Ok(());
              }
            }
          }
        }

        match session
          .store_session
          .bitpos(key, search_for, start, end, is_bit_index)
          .await
        {
          Ok(pos) => buf.write_integer(pos),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE);
          }
        }
      }
      RespCommand::BITOP
      | RespCommand::BitopAnd
      | RespCommand::BitopOr
      | RespCommand::BitopXor
      | RespCommand::BitopNot
      | RespCommand::BitopDiff => {
        let (op, dest_key, src_keys) = match cmd {
          RespCommand::BITOP => {
            if args.len() < 3 {
              buf.write_error(b"ERR wrong number of arguments for 'bitop' command");
              return Ok(());
            }
            let op_str = args[0];
            let op = if op_str.eq_ignore_ascii_case(b"AND") {
              wedb_redis::BitmapOp::And
            } else if op_str.eq_ignore_ascii_case(b"OR") {
              wedb_redis::BitmapOp::Or
            } else if op_str.eq_ignore_ascii_case(b"XOR") {
              wedb_redis::BitmapOp::Xor
            } else if op_str.eq_ignore_ascii_case(b"NOT") {
              wedb_redis::BitmapOp::Not
            } else if op_str.eq_ignore_ascii_case(b"DIFF") {
              wedb_redis::BitmapOp::Diff
            } else {
              buf.write_error(consts::err::SYNTAX);
              return Ok(());
            };
            (op, args[1], &args[2..])
          }
          RespCommand::BitopAnd => (
            wedb_redis::BitmapOp::And,
            args.first().copied().unwrap_or_default(),
            if args.len() > 1 { &args[1..] } else { &[] },
          ),
          RespCommand::BitopOr => (
            wedb_redis::BitmapOp::Or,
            args.first().copied().unwrap_or_default(),
            if args.len() > 1 { &args[1..] } else { &[] },
          ),
          RespCommand::BitopXor => (
            wedb_redis::BitmapOp::Xor,
            args.first().copied().unwrap_or_default(),
            if args.len() > 1 { &args[1..] } else { &[] },
          ),
          RespCommand::BitopNot => (
            wedb_redis::BitmapOp::Not,
            args.first().copied().unwrap_or_default(),
            if args.len() > 1 { &args[1..] } else { &[] },
          ),
          RespCommand::BitopDiff => (
            wedb_redis::BitmapOp::Diff,
            args.first().copied().unwrap_or_default(),
            if args.len() > 1 { &args[1..] } else { &[] },
          ),
          _ => unreachable!(),
        };

        if src_keys.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'bitop' command");
          return Ok(());
        }
        if op == wedb_redis::BitmapOp::Diff && src_keys.len() < 2 {
          buf.write_error(consts::err::BITOP_DIFF_TWO_SOURCE_KEYS);
          return Ok(());
        }
        if op == wedb_redis::BitmapOp::Not && src_keys.len() > 1 {
          buf.write_error(consts::err::BITOP_NOT_SINGLE_SOURCE);
          return Ok(());
        }
        if src_keys.len() + 1 > 64 {
          buf.write_error(consts::err::BITOP_KEY_LIMIT);
          return Ok(());
        }

        match session.store_session.bitop(op, dest_key, src_keys).await {
          Ok(len) => buf.write_integer(len as i64),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE);
          }
        }
      }
      RespCommand::PFADD => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'pfadd' command");
          return Ok(());
        }
        let key = args[0];
        let elements = if args.len() > 1 { &args[1..] } else { &[] };
        match session.store_session.pfadd(key, elements).await {
          Ok(updated) => buf.write_integer(if updated { 1 } else { 0 }),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE_HLL);
          }
        }
      }
      RespCommand::PFCOUNT => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'pfcount' command");
          return Ok(());
        }
        match session.store_session.pfcount(args).await {
          Ok(count) => buf.write_integer(count as i64),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE_HLL);
          }
        }
      }
      RespCommand::PFMERGE => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'pfmerge' command");
          return Ok(());
        }
        let dest_key = args[0];
        let src_keys = if args.len() > 1 { &args[1..] } else { &[] };
        match session.store_session.pfmerge(dest_key, src_keys).await {
          Ok(()) => buf.write_simple_string(b"OK"),
          Err(_) => {
            buf.write_error(consts::err::WRONG_TYPE_HLL);
          }
        }
      }

      // ====== 富数据结构命令 ======
      // ====== 哈希命令 (HASH) ======
      RespCommand::HSET => {
        if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
          buf.write_error(b"ERR wrong number of arguments for 'hset' command");
          return Ok(());
        }
        let key = args[0];
        let pairs = args[1..]
          .as_chunks::<2>()
          .0
          .iter()
          .map(|pair| (pair[0], pair[1]));
        let count = session.store_session.hmset(key, pairs).await?;
        buf.write_integer(count as i64);
      }
      RespCommand::HGET => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'hget' command");
          return Ok(());
        }
        let key = args[0];
        let field = args[1];
        match session.store_session.hget(key, field).await? {
          Some(v) => buf.write_bulk_string(&v),
          None => buf.write_null(),
        }
      }
      RespCommand::HDEL => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'hdel' command");
          return Ok(());
        }
        let key = args[0];
        let count = session.store_session.hdel(key, &args[1..]).await?;
        buf.write_integer(count as i64);
      }
      RespCommand::HLEN => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'hlen' command");
          return Ok(());
        }
        let key = args[0];
        let len = session.store_session.hlen(key).await?;
        buf.write_integer(len as i64);
      }
      RespCommand::HGETALL => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'hgetall' command");
          return Ok(());
        }
        let key = args[0];
        let pairs = session.store_session.hgetall(key).await?;
        buf.write_array_header(pairs.len() * 2);
        for (field, val) in &pairs {
          buf.write_bulk_string(field);
          buf.write_bulk_string(val);
        }
      }
      RespCommand::HKEYS => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'hkeys' command");
          return Ok(());
        }
        let key = args[0];
        let keys = session.store_session.hkeys(key).await?;
        buf.write_array_header(keys.len());
        for k in &keys {
          buf.write_bulk_string(k);
        }
      }
      RespCommand::HVALS => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'hvals' command");
          return Ok(());
        }
        let key = args[0];
        let vals = session.store_session.hvals(key).await?;
        buf.write_array_header(vals.len());
        for v in &vals {
          buf.write_bulk_string(v);
        }
      }
      RespCommand::HEXISTS => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'hexists' command");
          return Ok(());
        }
        let key = args[0];
        let field = args[1];
        let exists = session.store_session.hexists(key, field).await?;
        buf.write_integer(if exists { 1 } else { 0 });
      }
      RespCommand::HINCRBY => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'hincrby' command");
          return Ok(());
        }
        let Some(incr) = ParseUtils::try_read_long(args[2]) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let key = args[0];
        let field = args[1];
        match session.store_session.hincrby(key, field, incr).await {
          Ok(new_val) => buf.write_integer(new_val),
          Err(_) => {
            buf.write_error(b"ERR hash value is not an integer");
          }
        }
      }
      RespCommand::HINCRBYFLOAT => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'hincrbyfloat' command");
          return Ok(());
        }
        let Some(incr) = ParseUtils::try_read_double(args[2], true) else {
          buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
          return Ok(());
        };
        if incr.is_nan() || incr.is_infinite() {
          buf.write_error(consts::err::NAN_OR_INFINITY);
          return Ok(());
        }
        let key = args[0];
        let field = args[1];
        match session.store_session.hincrbyfloat(key, field, incr).await {
          Ok(new_val) => buf.write_double_bulk(new_val),
          Err(err) => {
            if err.is_wrong_type() {
              buf.write_error(consts::err::WRONG_TYPE);
            } else {
              buf.write_error(b"ERR hash value is not a float");
            }
          }
        }
      }
      RespCommand::HMGET => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'hmget' command");
          return Ok(());
        }
        let key = args[0];
        let fields = &args[1..];
        let values = session.store_session.hmget(key, fields).await?;
        buf.write_array_header(values.len());
        for val in &values {
          if let Some(v) = val {
            buf.write_bulk_string(v);
          } else {
            buf.write_null();
          }
        }
      }
      RespCommand::HMSET => {
        if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
          buf.write_error(b"ERR wrong number of arguments for 'hmset' command");
          return Ok(());
        }
        let key = args[0];
        let pairs = args[1..]
          .as_chunks::<2>()
          .0
          .iter()
          .map(|pair| (pair[0], pair[1]));
        session.store_session.hmset(key, pairs).await?;
        buf.write_ok();
      }
      RespCommand::HSETNX => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'hsetnx' command");
          return Ok(());
        }
        let key = args[0];
        let field = args[1];
        let value = args[2];
        let is_new = session.store_session.hsetnx(key, field, value).await?;
        buf.write_integer(if is_new { 1 } else { 0 });
      }
      RespCommand::HSCAN => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'hscan' command");
          return Ok(());
        }
        let key = args[0];
        let Some(cursor) = ParseUtils::try_read_int(args[1]) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let mut pattern = None;
        let mut count = 10;
        let mut i = 2;
        while i < args.len() {
          if i + 1 >= args.len() {
            buf.write_error(b"ERR syntax error");
            return Ok(());
          }
          let opt = args[i];
          if opt.eq_ignore_ascii_case(b"MATCH") {
            pattern = Some(args[i + 1]);
            i += 2;
          } else if opt.eq_ignore_ascii_case(b"COUNT") {
            if let Some(c) = ParseUtils::try_read_int(args[i + 1]) {
              count = (c as usize).max(1);
            } else {
              buf.write_error(b"ERR value is not an integer or out of range");
              return Ok(());
            }
            i += 2;
          } else {
            buf.write_error(b"ERR syntax error");
            return Ok(());
          }
        }
        let (next_cursor, items) = session
          .store_session
          .hscan(key, cursor as usize, count, pattern)
          .await?;
        buf.write_array_header(2);
        let mut cur_buf = Buffer::new();
        buf.write_bulk_string(cur_buf.format(next_cursor).as_bytes());
        buf.write_array_header(items.len() * 2);
        for (field, val) in &items {
          buf.write_bulk_string(field);
          buf.write_bulk_string(val);
        }
      }
      RespCommand::HEXPIRE | RespCommand::HEXPIREAT => {
        let is_at = cmd == RespCommand::HEXPIREAT;
        if args.len() < 3 {
          let err: &[u8] = if is_at {
            b"ERR wrong number of arguments for 'hexpireat' command"
          } else {
            b"ERR wrong number of arguments for 'hexpire' command"
          };
          buf.write_error(err);
          return Ok(());
        }
        let key = args[0];
        let Some(time_val) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let now = coarsetime::Clock::now_since_epoch().as_millis();
        let expire_at_ms = if is_at {
          if time_val < 0 {
            0
          } else {
            (time_val as u64).saturating_mul(1000)
          }
        } else if time_val <= 0 {
          0
        } else {
          now.saturating_add((time_val as u64).saturating_mul(1000))
        };

        let mut option = wedb_hash::ExpireOpt::NONE;
        let mut idx = 2;
        while idx < args.len() {
          let a = args[idx];
          if a.eq_ignore_ascii_case(b"NX") {
            option.nx = true;
            idx += 1;
          } else if a.eq_ignore_ascii_case(b"XX") {
            option.xx = true;
            idx += 1;
          } else if a.eq_ignore_ascii_case(b"GT") {
            option.gt = true;
            idx += 1;
          } else if a.eq_ignore_ascii_case(b"LT") {
            option.lt = true;
            idx += 1;
          } else if a.eq_ignore_ascii_case(b"FIELDS") {
            idx += 1;
            if idx < args.len() && ParseUtils::try_read_int(args[idx]).is_some() {
              idx += 1;
            }
            break;
          } else {
            break;
          }
        }

        let fields = &args[idx..];
        if fields.is_empty() {
          buf.write_error(b"ERR wrong number of arguments");
          return Ok(());
        }

        if fields.len() == 1 {
          let res = session
            .store_session
            .hexpire(key, fields[0], expire_at_ms, option)
            .await?;
          buf.write_integer(res as i64);
        } else {
          buf.write_array_header(fields.len());
          for f in fields {
            let res = session
              .store_session
              .hexpire(key, f, expire_at_ms, option)
              .await?;
            buf.write_integer(res as i64);
          }
        }
      }
      RespCommand::HTTL => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'httl' command");
          return Ok(());
        }
        let key = args[0];
        let mut idx = 1;
        if args[idx].eq_ignore_ascii_case(b"FIELDS") {
          idx += 1;
          if idx < args.len() && ParseUtils::try_read_int(args[idx]).is_some() {
            idx += 1;
          }
        }
        let fields = &args[idx..];
        if fields.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'httl' command");
          return Ok(());
        }

        if fields.len() == 1 {
          let ttl = session.store_session.httl(key, fields[0]).await?;
          let ttl_sec = if ttl > 0 { ttl / 1000 } else { ttl };
          buf.write_integer(ttl_sec);
        } else {
          buf.write_array_header(fields.len());
          for f in fields {
            let ttl = session.store_session.httl(key, f).await?;
            let ttl_sec = if ttl > 0 { ttl / 1000 } else { ttl };
            buf.write_integer(ttl_sec);
          }
        }
      }
      RespCommand::HPERSIST => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'hpersist' command");
          return Ok(());
        }
        let key = args[0];
        let mut idx = 1;
        if args[idx].eq_ignore_ascii_case(b"FIELDS") {
          idx += 1;
          if idx < args.len() && ParseUtils::try_read_int(args[idx]).is_some() {
            idx += 1;
          }
        }
        let fields = &args[idx..];
        if fields.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'hpersist' command");
          return Ok(());
        }

        if fields.len() == 1 {
          let ok = session.store_session.hpersist(key, fields[0]).await?;
          buf.write_integer(if ok { 1 } else { -1 });
        } else {
          buf.write_array_header(fields.len());
          for f in fields {
            let ok = session.store_session.hpersist(key, f).await?;
            buf.write_integer(if ok { 1 } else { -1 });
          }
        }
      }

      // ====== 列表命令 (LIST) ======
      RespCommand::LPUSH => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'lpush' command");
          return Ok(());
        }
        let key = args[0];
        let new_len = session
          .store_session
          .lpush(key, args[1..].iter().copied())
          .await?;
        ctx
          .blocking
          .notify_waiters(&session.full_key(key), args.len() - 1);
        buf.write_integer(new_len as i64);
      }
      RespCommand::RPUSH => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'rpush' command");
          return Ok(());
        }
        let key = args[0];
        let new_len = session
          .store_session
          .rpush(key, args[1..].iter().copied())
          .await?;
        ctx
          .blocking
          .notify_waiters(&session.full_key(key), args.len() - 1);
        buf.write_integer(new_len as i64);
      }
      RespCommand::LPOP => {
        if args.is_empty() || args.len() > 2 {
          buf.write_error(b"ERR wrong number of arguments for 'lpop' command");
          return Ok(());
        }
        let key = args[0];
        let count = if args.len() > 1 {
          match ParseUtils::try_read_int(args[1]) {
            Some(c) if c < 0 => {
              buf.write_error(b"ERR value is out of range, must be positive");
              return Ok(());
            }
            Some(c) => (c as usize).min(65536),
            None => {
              buf.write_error(b"ERR value is not an integer or out of range");
              return Ok(());
            }
          }
        } else {
          1
        };
        let popped = session.store_session.lpop(key, count).await?;
        if args.len() > 1 {
          buf.write_array_header(popped.len());
          for item in &popped {
            buf.write_bulk_string(item);
          }
        } else if let Some(item) = popped.first() {
          buf.write_bulk_string(item);
        } else {
          buf.write_null();
        }
      }
      RespCommand::RPOP => {
        if args.is_empty() || args.len() > 2 {
          buf.write_error(b"ERR wrong number of arguments for 'rpop' command");
          return Ok(());
        }
        let key = args[0];
        let count = if args.len() > 1 {
          match ParseUtils::try_read_int(args[1]) {
            Some(c) if c < 0 => {
              buf.write_error(b"ERR value is out of range, must be positive");
              return Ok(());
            }
            Some(c) => (c as usize).min(65536),
            None => {
              buf.write_error(b"ERR value is not an integer or out of range");
              return Ok(());
            }
          }
        } else {
          1
        };
        let popped = session.store_session.rpop(key, count).await?;
        if args.len() > 1 {
          buf.write_array_header(popped.len());
          for item in &popped {
            buf.write_bulk_string(item);
          }
        } else if let Some(item) = popped.first() {
          buf.write_bulk_string(item);
        } else {
          buf.write_null();
        }
      }
      RespCommand::LLEN => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'llen' command");
          return Ok(());
        }
        let key = args[0];
        let len = session.store_session.llen(key).await?;
        buf.write_integer(len as i64);
      }
      // ====== 阻塞集合命令 (BLOCKING) ======
      RespCommand::BLPOP | RespCommand::BRPOP => {
        Self::handle_blocking_pop(ctx, session, cmd, args, buf).await?;
      }
      RespCommand::BLMOVE => {
        Self::handle_blocking_move(ctx, session, args, buf).await?;
      }
      RespCommand::BRPOPLPUSH => {
        Self::handle_brpoplpush(ctx, session, args, buf).await?;
      }
      RespCommand::LRANGE => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'lrange' command");
          return Ok(());
        }
        let (Some(start), Some(stop)) = (
          ParseUtils::try_read_long(args[1]),
          ParseUtils::try_read_long(args[2]),
        ) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let key = args[0];
        let items = session
          .store_session
          .lrange(key, start as isize, stop as isize)
          .await?;
        buf.write_array_header(items.len());
        for item in &items {
          buf.write_bulk_string(item);
        }
      }
      RespCommand::LINDEX => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'lindex' command");
          return Ok(());
        }
        let Some(index) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let key = args[0];
        match session.store_session.lindex(key, index as isize).await? {
          Some(item) => buf.write_bulk_string(&item),
          None => buf.write_null(),
        }
      }
      RespCommand::LTRIM => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'ltrim' command");
          return Ok(());
        }
        let (Some(start), Some(stop)) = (
          ParseUtils::try_read_long(args[1]),
          ParseUtils::try_read_long(args[2]),
        ) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let key = args[0];
        session
          .store_session
          .ltrim(key, start as isize, stop as isize)
          .await?;
        buf.write_ok();
      }
      RespCommand::LPUSHX | RespCommand::RPUSHX => {
        let is_left = cmd == RespCommand::LPUSHX;
        let cmd_str = if is_left { "lpushx" } else { "rpushx" };
        if args.len() < 2 {
          buf.write_error_fmt(format_args!(
            "ERR wrong number of arguments for '{cmd_str}' command"
          ));
          return Ok(());
        }
        let key = args[0];
        let count = if is_left {
          session
            .store_session
            .lpushx(key, args[1..].iter().copied())
            .await?
        } else {
          session
            .store_session
            .rpushx(key, args[1..].iter().copied())
            .await?
        };
        if count > 0 {
          ctx
            .blocking
            .notify_waiters(&session.full_key(key), args.len() - 1);
        }
        buf.write_integer(count as i64);
      }
      RespCommand::LINSERT => {
        if args.len() != 4 {
          buf.write_error(b"ERR wrong number of arguments for 'linsert' command");
          return Ok(());
        }
        let pos = if args[1].eq_ignore_ascii_case(b"BEFORE") {
          InsertPosition::Before
        } else if args[1].eq_ignore_ascii_case(b"AFTER") {
          InsertPosition::After
        } else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        };
        let res = session
          .store_session
          .linsert(args[0], args[2], args[3], pos)
          .await?;
        if res > 0 {
          ctx.blocking.notify_waiters(&session.full_key(args[0]), 1);
        }
        buf.write_integer(res as i64);
      }
      RespCommand::LREM => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'lrem' command");
          return Ok(());
        }
        let Some(count) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let removed = session
          .store_session
          .lrem(args[0], count as isize, args[2])
          .await?;
        buf.write_integer(removed as i64);
      }
      RespCommand::LSET => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'lset' command");
          return Ok(());
        }
        let Some(index) = ParseUtils::try_read_long(args[1]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let ok = session
          .store_session
          .lset(args[0], index as isize, args[2].to_vec())
          .await?;
        if ok {
          buf.write_ok();
        } else {
          buf.write_error(consts::err::INDEX_OUT_OF_RANGE);
        }
      }
      RespCommand::LPOS => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'lpos' command");
          return Ok(());
        }
        let mut rank = 1isize;
        let mut count = None;
        let mut maxlen = 0usize;
        let mut i = 2;
        while i < args.len() {
          if args[i].eq_ignore_ascii_case(b"RANK") && i + 1 < args.len() {
            if let Some(r) = ParseUtils::try_read_long(args[i + 1]) {
              rank = r as isize;
              i += 2;
              continue;
            }
          } else if args[i].eq_ignore_ascii_case(b"COUNT")
            && i + 1 < args.len()
            && let Some(c) = ParseUtils::try_read_long(args[i + 1])
          {
            count = Some(c as usize);
            i += 2;
            continue;
          } else if args[i].eq_ignore_ascii_case(b"MAXLEN")
            && i + 1 < args.len()
            && let Some(m) = ParseUtils::try_read_long(args[i + 1])
          {
            maxlen = m as usize;
            i += 2;
            continue;
          }
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        let positions = session
          .store_session
          .lpos(args[0], args[1], rank, count, maxlen)
          .await?;
        if count.is_some() {
          buf.write_array_header(positions.len());
          for p in positions {
            buf.write_integer(p as i64);
          }
        } else if let Some(p) = positions.first() {
          buf.write_integer(*p as i64);
        } else {
          buf.write_null();
        }
      }
      RespCommand::LMOVE => {
        if args.len() != 4 {
          buf.write_error(b"ERR wrong number of arguments for 'lmove' command");
          return Ok(());
        }
        let from_left = if args[2].eq_ignore_ascii_case(b"LEFT") {
          true
        } else if args[2].eq_ignore_ascii_case(b"RIGHT") {
          false
        } else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        };
        let to_left = if args[3].eq_ignore_ascii_case(b"LEFT") {
          true
        } else if args[3].eq_ignore_ascii_case(b"RIGHT") {
          false
        } else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        };
        match session
          .store_session
          .lmove(args[0], args[1], from_left, to_left)
          .await?
        {
          Some(item) => {
            ctx.blocking.notify_waiters(&session.full_key(args[1]), 1);
            buf.write_bulk_string(&item);
          }
          None => buf.write_null(),
        }
      }
      RespCommand::RPOPLPUSH => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'rpoplpush' command");
          return Ok(());
        }
        match session.store_session.rpoplpush(args[0], args[1]).await? {
          Some(item) => {
            ctx.blocking.notify_waiters(&session.full_key(args[1]), 1);
            buf.write_bulk_string(&item);
          }
          None => buf.write_null(),
        }
      }
      RespCommand::LMPOP => {
        if args.len() < 3 {
          buf.write_error(b"ERR wrong number of arguments for 'lmpop' command");
          return Ok(());
        }
        let Some(numkeys) = ParseUtils::try_read_long(args[0]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let numkeys = numkeys as usize;
        if numkeys == 0 || numkeys + 1 > args.len() {
          buf.write_error(b"ERR numkeys must be greater than 0 and match key arguments");
          return Ok(());
        }
        let keys = &args[1..=numkeys];
        let mut idx = numkeys + 1;
        let from_left;
        let mut count = 1usize;
        if idx < args.len() {
          if args[idx].eq_ignore_ascii_case(b"LEFT") {
            from_left = true;
            idx += 1;
          } else if args[idx].eq_ignore_ascii_case(b"RIGHT") {
            from_left = false;
            idx += 1;
          } else {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        } else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        if idx < args.len() {
          if args[idx].eq_ignore_ascii_case(b"COUNT") && idx + 1 < args.len() {
            if let Some(c) = ParseUtils::try_read_long(args[idx + 1]) {
              count = (c as usize).min(65536);
            } else {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            }
          } else {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        }
        match session.store_session.lmpop(keys, from_left, count).await? {
          Some((key, popped)) => {
            buf.write_array_header(2);
            buf.write_bulk_string(&key);
            buf.write_array_header(popped.len());
            for p in &popped {
              buf.write_bulk_string(p);
            }
          }
          None => buf.write_null(),
        }
      }

      // ====== 集合命令 (SET) ======
      RespCommand::SADD => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'sadd' command");
          return Ok(());
        }
        let key = args[0];
        let added = session
          .store_session
          .sadd(key, args[1..].iter().copied())
          .await?;
        buf.write_integer(added as i64);
      }
      RespCommand::SREM => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'srem' command");
          return Ok(());
        }
        let key = args[0];
        let removed = session.store_session.srem(key, &args[1..]).await?;
        buf.write_integer(removed as i64);
      }
      RespCommand::SCARD => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'scard' command");
          return Ok(());
        }
        let key = args[0];
        let card = session.store_session.scard(key).await?;
        buf.write_integer(card as i64);
      }
      RespCommand::SISMEMBER => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'sismember' command");
          return Ok(());
        }
        let key = args[0];
        let member = args[1];
        let exists = session.store_session.sismember(key, member).await?;
        buf.write_integer(if exists { 1 } else { 0 });
      }
      RespCommand::SMEMBERS => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'smembers' command");
          return Ok(());
        }
        let key = args[0];
        let members = session.store_session.smembers(key).await?;
        buf.write_array_header(members.len());
        for m in &members {
          buf.write_bulk_string(m);
        }
      }
      RespCommand::SPOP => {
        if args.is_empty() || args.len() > 2 {
          buf.write_error(b"ERR wrong number of arguments for 'spop' command");
          return Ok(());
        }
        let key = args[0];
        let count = if args.len() == 2 {
          let Some(c) = ParseUtils::try_read_long(args[1]) else {
            buf.write_error(b"ERR value is not an integer or out of range");
            return Ok(());
          };
          if c < 0 {
            buf.write_error(b"ERR value is out of range, must be positive");
            return Ok(());
          }
          c as usize
        } else {
          1
        };
        let popped = session.store_session.spop(key, count).await?;
        if args.len() == 2 {
          buf.write_array_header(popped.len());
          for p in &popped {
            buf.write_bulk_string(p);
          }
        } else if let Some(first) = popped.first() {
          buf.write_bulk_string(first);
        } else {
          buf.write_null();
        }
      }
      RespCommand::SMOVE => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'smove' command");
          return Ok(());
        }
        let source = args[0];
        let dest = args[1];
        let member = args[2];
        let moved = session.store_session.smove(source, dest, member).await?;
        buf.write_integer(if moved { 1 } else { 0 });
      }
      RespCommand::SMISMEMBER => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'smismember' command");
          return Ok(());
        }
        let key = args[0];
        let members = &args[1..];
        let res = session.store_session.smismember(key, members).await?;
        buf.write_array_header(res.len());
        for b in res {
          buf.write_integer(if b { 1 } else { 0 });
        }
      }
      RespCommand::SSCAN => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'sscan' command");
          return Ok(());
        }
        let key = args[0];
        let Some(cursor) = ParseUtils::try_read_int(args[1]) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let mut pattern = None;
        let mut count = 10;
        let mut i = 2;
        while i < args.len() {
          if i + 1 >= args.len() {
            buf.write_error(b"ERR syntax error");
            return Ok(());
          }
          let opt = args[i];
          if opt.eq_ignore_ascii_case(b"MATCH") {
            pattern = Some(args[i + 1]);
            i += 2;
          } else if opt.eq_ignore_ascii_case(b"COUNT") {
            if let Some(c) = ParseUtils::try_read_int(args[i + 1]) {
              count = (c as usize).max(1);
            } else {
              buf.write_error(b"ERR value is not an integer or out of range");
              return Ok(());
            }
            i += 2;
          } else {
            buf.write_error(b"ERR syntax error");
            return Ok(());
          }
        }
        let (next_cursor, items) = session
          .store_session
          .sscan(key, cursor as usize, count, pattern)
          .await?;
        buf.write_array_header(2);
        let mut cur_buf = Buffer::new();
        buf.write_bulk_string(cur_buf.format(next_cursor).as_bytes());
        buf.write_array_header(items.len());
        for member in &items {
          buf.write_bulk_string(member);
        }
      }
      RespCommand::SINTER => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'sinter' command");
          return Ok(());
        }
        let inter = session.store_session.sinter(args).await?;
        buf.write_array_header(inter.len());
        for m in &inter {
          buf.write_bulk_string(m);
        }
      }
      RespCommand::SINTERSTORE => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'sinterstore' command");
          return Ok(());
        }
        let count = session
          .store_session
          .sinterstore(args[0], &args[1..])
          .await?;
        buf.write_integer(count as i64);
      }
      RespCommand::SINTERCARD => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'sintercard' command");
          return Ok(());
        }
        let Some(numkeys) = ParseUtils::try_read_long(args[0]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let numkeys = numkeys as usize;
        if numkeys == 0 || numkeys + 1 > args.len() {
          buf.write_error(b"ERR numkeys must be greater than 0 and match key arguments");
          return Ok(());
        }
        let keys = &args[1..=numkeys];
        let mut limit = 0usize;
        if args.len() > numkeys + 1 {
          if args[numkeys + 1].eq_ignore_ascii_case(b"LIMIT") && args.len() == numkeys + 3 {
            let Some(l) = ParseUtils::try_read_long(args[numkeys + 2]) else {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            };
            if l < 0 {
              buf.write_error(b"ERR LIMIT can't be negative");
              return Ok(());
            }
            limit = l as usize;
          } else {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        }
        let card = session.store_session.sintercard(keys, limit).await?;
        buf.write_integer(card as i64);
      }
      RespCommand::SUNION => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'sunion' command");
          return Ok(());
        }
        let union_res = session.store_session.sunion(args).await?;
        buf.write_array_header(union_res.len());
        for m in &union_res {
          buf.write_bulk_string(m);
        }
      }
      RespCommand::SUNIONSTORE => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'sunionstore' command");
          return Ok(());
        }
        let count = session
          .store_session
          .sunionstore(args[0], &args[1..])
          .await?;
        buf.write_integer(count as i64);
      }
      RespCommand::SDIFF => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'sdiff' command");
          return Ok(());
        }
        let diff = session.store_session.sdiff(args).await?;
        buf.write_array_header(diff.len());
        for m in &diff {
          buf.write_bulk_string(m);
        }
      }
      RespCommand::SDIFFSTORE => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'sdiffstore' command");
          return Ok(());
        }
        let count = session
          .store_session
          .sdiffstore(args[0], &args[1..])
          .await?;
        buf.write_integer(count as i64);
      }
      RespCommand::SRANDMEMBER => {
        if args.is_empty() || args.len() > 2 {
          buf.write_error(b"ERR wrong number of arguments for 'srandmember' command");
          return Ok(());
        }
        let key = args[0];
        let (is_single, count) = if args.len() == 2 {
          let Some(c) = ParseUtils::try_read_long(args[1]) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return Ok(());
          };
          // 保留符号：负数 = 恰好 |count| 个可重复成员 (对标 C# SetObjectImpl.SetRandomMember)
          (false, c as isize)
        } else {
          (true, 1)
        };
        let items = session.store_session.srandmember(key, count).await?;
        if is_single {
          if let Some(first) = items.first() {
            buf.write_bulk_string(first);
          } else {
            buf.write_null();
          }
        } else {
          buf.write_array_header(items.len());
          for m in &items {
            buf.write_bulk_string(m);
          }
        }
      }

      // ====== 有序集合命令 (ZSET) ======
      RespCommand::ZADD => {
        if args.len() < 3 {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        // 前导选项解析（NX/XX/GT/LT/CH/INCR，对标 C# SortedSet ZAddOptions）
        let (zadd_opt, pairs_at) = match parse_zadd_options(args) {
          Ok(parsed) => parsed,
          Err(_) => {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        };
        let key = args[0];
        let pairs = &args[pairs_at..];
        if pairs.is_empty() || !pairs.len().is_multiple_of(2) {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        let mut items = Vec::with_capacity(pairs.len() / 2);
        for chunk in pairs.as_chunks::<2>().0 {
          let Some(score) = ParseUtils::try_read_double(chunk[0], false) else {
            buf.write_error(b"ERR value is not a valid float");
            return Ok(());
          };
          items.push((score, chunk[1]));
        }
        if zadd_opt.incr {
          // INCR 模式：仅接受单 score-member，条件不满足回 Nil（对标 Redis ZADD INCR 语义）
          if items.len() != 1 {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
          let (incr, member) = items[0];
          let old = session.store_session.zscore(key, member).await?;
          if (zadd_opt.nx && old.is_some()) || (zadd_opt.xx && old.is_none()) {
            buf.write_null();
            return Ok(());
          }
          // GT/LT 以「自增后分值」与旧分值比较；inf + -inf 产生 NaN 视为非法输入
          let base = old.unwrap_or(0.0);
          let new_score = base + incr;
          if new_score.is_nan() {
            buf.write_error(b"ERR resulting score is not a number (NaN)");
            return Ok(());
          }
          if old.is_some()
            && ((zadd_opt.gt && new_score <= base) || (zadd_opt.lt && new_score >= base))
          {
            buf.write_null();
            return Ok(());
          }
          let applied = session.store_session.zincrby(key, incr, member).await?;
          buf.write_double_bulk(applied);
          return Ok(());
        }
        let added = session.store_session.zmadd(key, items, zadd_opt).await?;
        if added > 0 {
          ctx.blocking.notify_waiters(&session.full_key(key), added);
        }
        buf.write_integer(added as i64);
      }
      RespCommand::ZRANGE => {
        if args.len() < 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zrange' command");
          return Ok(());
        }
        let key = args[0];
        let min_raw = args[1];
        let max_raw = args[2];
        let opts = match parse_zrange_options(&args[3..]) {
          Some(o) => o,
          None => {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        };

        if opts.by_score {
          let Some(range) = parse_score_range(min_raw, max_raw) else {
            buf.write_error(b"ERR min or max is not a float");
            return Ok(());
          };
          let items = session
            .store_session
            .zrangebyscore(key, range, opts.reverse, opts.offset, opts.count)
            .await?;
          if opts.with_scores {
            buf.write_array_header(items.len() * 2);
            for item in &items {
              buf.write_bulk_string(&item.0);
              buf.write_double_bulk(item.1);
            }
          } else {
            buf.write_array_header(items.len());
            for item in &items {
              buf.write_bulk_string(&item.0);
            }
          }
        } else if opts.by_lex {
          let (Some(min), Some(max)) = (LexBound::parse(min_raw), LexBound::parse(max_raw)) else {
            buf.write_error(b"ERR min or max not valid string range item");
            return Ok(());
          };
          let members = session
            .store_session
            .zrangebylex(key, min, max, opts.offset, opts.count, opts.reverse)
            .await?;
          buf.write_array_header(members.len());
          for m in &members {
            buf.write_bulk_string(m);
          }
        } else {
          let (Some(start), Some(stop)) = (
            ParseUtils::try_read_int(min_raw),
            ParseUtils::try_read_int(max_raw),
          ) else {
            buf.write_error(b"ERR value is not an integer or out of range");
            return Ok(());
          };
          let mut items = session
            .store_session
            .zrange(key, start as isize, stop as isize, opts.reverse)
            .await?;
          if opts.offset > 0 || opts.count < usize::MAX {
            items = items
              .into_iter()
              .skip(opts.offset)
              .take(opts.count)
              .collect();
          }
          if opts.with_scores {
            buf.write_array_header(items.len() * 2);
            for item in &items {
              buf.write_bulk_string(&item.0);
              buf.write_double_bulk(item.1);
            }
          } else {
            buf.write_array_header(items.len());
            for item in &items {
              buf.write_bulk_string(&item.0);
            }
          }
        }
      }
      RespCommand::ZREVRANGE => {
        if args.len() < 3 || args.len() > 4 {
          buf.write_error(b"ERR wrong number of arguments for 'zrevrange' command");
          return Ok(());
        }
        let key = args[0];
        let (Some(start), Some(stop)) = (
          ParseUtils::try_read_int(args[1]),
          ParseUtils::try_read_int(args[2]),
        ) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let with_scores = if args.len() == 4 {
          if !args[3].eq_ignore_ascii_case(b"WITHSCORES") {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
          true
        } else {
          false
        };
        let items = session
          .store_session
          .zrange(key, start as isize, stop as isize, true)
          .await?;
        if with_scores {
          buf.write_array_header(items.len() * 2);
          for item in &items {
            buf.write_bulk_string(&item.0);
            buf.write_double_bulk(item.1);
          }
        } else {
          buf.write_array_header(items.len());
          for item in &items {
            buf.write_bulk_string(&item.0);
          }
        }
      }
      RespCommand::ZRANGEBYSCORE | RespCommand::ZREVRANGEBYSCORE => {
        let is_rev = cmd == RespCommand::ZREVRANGEBYSCORE;
        if args.len() < 3 {
          let err: &[u8] = if is_rev {
            b"ERR wrong number of arguments for 'zrevrangebyscore' command"
          } else {
            b"ERR wrong number of arguments for 'zrangebyscore' command"
          };
          buf.write_error(err);
          return Ok(());
        }
        let key = args[0];
        let (min_raw, max_raw) = if is_rev {
          (args[2], args[1])
        } else {
          (args[1], args[2])
        };
        let Some(range) = parse_score_range(min_raw, max_raw) else {
          buf.write_error(b"ERR min or max is not a float");
          return Ok(());
        };
        let mut with_scores = false;
        let mut offset = 0;
        let mut count = usize::MAX;
        let mut idx = 3;
        while idx < args.len() {
          let a = args[idx];
          if a.eq_ignore_ascii_case(b"WITHSCORES") {
            with_scores = true;
            idx += 1;
          } else if a.eq_ignore_ascii_case(b"LIMIT") && idx + 2 < args.len() {
            let (Some(off), Some(cnt)) = (
              ParseUtils::try_read_long(args[idx + 1]),
              ParseUtils::try_read_long(args[idx + 2]),
            ) else {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            };
            if off < 0 {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            }
            offset = off as usize;
            count = if cnt < 0 { usize::MAX } else { cnt as usize };
            idx += 3;
          } else {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        }
        let items = session
          .store_session
          .zrangebyscore(key, range, is_rev, offset, count)
          .await?;
        if with_scores {
          buf.write_array_header(items.len() * 2);
          for item in &items {
            buf.write_bulk_string(&item.0);
            buf.write_double_bulk(item.1);
          }
        } else {
          buf.write_array_header(items.len());
          for item in &items {
            buf.write_bulk_string(&item.0);
          }
        }
      }
      RespCommand::ZRANGEBYLEX | RespCommand::ZREVRANGEBYLEX => {
        let is_rev = cmd == RespCommand::ZREVRANGEBYLEX;
        if args.len() != 3 && args.len() != 6 {
          let err: &[u8] = if is_rev {
            b"ERR wrong number of arguments for 'zrevrangebylex' command"
          } else {
            b"ERR wrong number of arguments for 'zrangebylex' command"
          };
          buf.write_error(err);
          return Ok(());
        }
        let key = args[0];
        let (min_raw, max_raw) = if is_rev {
          (args[2], args[1])
        } else {
          (args[1], args[2])
        };
        let (Some(min), Some(max)) = (LexBound::parse(min_raw), LexBound::parse(max_raw)) else {
          buf.write_error(b"ERR min or max not valid string range item");
          return Ok(());
        };
        let mut offset = 0;
        let mut count = usize::MAX;
        if args.len() == 6 {
          if !args[3].eq_ignore_ascii_case(b"LIMIT") {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
          let (Some(off), Some(cnt)) = (
            ParseUtils::try_read_long(args[4]),
            ParseUtils::try_read_long(args[5]),
          ) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return Ok(());
          };
          if off < 0 {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return Ok(());
          }
          offset = off as usize;
          count = if cnt < 0 { usize::MAX } else { cnt as usize };
        }
        let items = session
          .store_session
          .zrangebylex(key, min, max, offset, count, is_rev)
          .await?;
        buf.write_array_header(items.len());
        for m in &items {
          buf.write_bulk_string(m);
        }
      }
      RespCommand::ZRANGESTORE => {
        if args.len() < 4 {
          buf.write_error(b"ERR wrong number of arguments for 'zrangestore' command");
          return Ok(());
        }
        let dst = args[0];
        let src = args[1];
        let min_raw = args[2];
        let max_raw = args[3];
        let Some(opts) = parse_zrange_options(&args[4..]) else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        };
        let items = if opts.by_score {
          let Some(range) = parse_score_range(min_raw, max_raw) else {
            buf.write_error(b"ERR min or max is not a float");
            return Ok(());
          };
          session
            .store_session
            .zrangebyscore(src, range, opts.reverse, opts.offset, opts.count)
            .await?
        } else if opts.by_lex {
          let (Some(min), Some(max)) = (LexBound::parse(min_raw), LexBound::parse(max_raw)) else {
            buf.write_error(b"ERR min or max not valid string range item");
            return Ok(());
          };
          let members = session
            .store_session
            .zrangebylex(src, min, max, opts.offset, opts.count, opts.reverse)
            .await?;
          members.into_iter().map(|m| (m, 0.0)).collect()
        } else {
          let (Some(start), Some(stop)) = (
            ParseUtils::try_read_int(min_raw),
            ParseUtils::try_read_int(max_raw),
          ) else {
            buf.write_error(b"ERR value is not an integer or out of range");
            return Ok(());
          };
          let mut res = session
            .store_session
            .zrange(src, start as isize, stop as isize, opts.reverse)
            .await?;
          if opts.offset > 0 || opts.count < usize::MAX {
            res = res.into_iter().skip(opts.offset).take(opts.count).collect();
          }
          res
        };
        session.store_session.delete(dst).await?;
        let count = if items.is_empty() {
          0
        } else {
          session
            .store_session
            .zmadd(
              dst,
              items.into_iter().map(|(m, s)| (s, m)),
              ZAddOpt::default(),
            )
            .await?
        };
        buf.write_integer(count as i64);
      }
      RespCommand::ZCARD => {
        if args.len() != 1 {
          buf.write_error(b"ERR wrong number of arguments for 'zcard' command");
          return Ok(());
        }
        let key = args[0];
        let card = session.store_session.zcard(key).await?;
        buf.write_integer(card as i64);
      }
      RespCommand::ZSCORE => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'zscore' command");
          return Ok(());
        }
        let key = args[0];
        let member = args[1];
        match session.store_session.zscore(key, member).await? {
          Some(score) => buf.write_double_bulk(score),
          None => buf.write_null(),
        }
      }
      RespCommand::ZMSCORE => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'zmscore' command");
          return Ok(());
        }
        let key = args[0];
        let members: Vec<&[u8]> = args[1..].to_vec();
        let scores = session.store_session.zmscore(key, &members).await?;
        buf.write_array_header(scores.len());
        for opt in scores {
          match opt {
            Some(s) => buf.write_double_bulk(s),
            None => buf.write_null(),
          }
        }
      }
      RespCommand::ZINCRBY => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zincrby' command");
          return Ok(());
        }
        let key = args[0];
        let Some(incr) = ParseUtils::try_read_double(args[1], false) else {
          buf.write_error(b"ERR value is not a valid float");
          return Ok(());
        };
        let member = args[2];
        let new_score = session.store_session.zincrby(key, incr, member).await?;
        buf.write_double_bulk(new_score);
      }
      RespCommand::ZCOUNT => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zcount' command");
          return Ok(());
        }
        let key = args[0];
        let Some(range) = parse_score_range(args[1], args[2]) else {
          buf.write_error(b"ERR min or max is not a float");
          return Ok(());
        };
        let count = session.store_session.zcount(key, range).await?;
        buf.write_integer(count as i64);
      }
      RespCommand::ZREM => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'zrem' command");
          return Ok(());
        }
        let key = args[0];
        let members = &args[1..];
        let removed = session.store_session.zrem(key, members).await?;
        buf.write_integer(removed as i64);
      }
      RespCommand::ZREMRANGEBYRANK => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zremrangebyrank' command");
          return Ok(());
        }
        let key = args[0];
        let Some(start) = ParseUtils::try_read_int(args[1]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let Some(stop) = ParseUtils::try_read_int(args[2]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let count = session
          .store_session
          .zremrangebyrank(key, start as isize, stop as isize)
          .await?;
        buf.write_integer(count as i64);
      }
      RespCommand::ZREMRANGEBYSCORE => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zremrangebyscore' command");
          return Ok(());
        }
        let key = args[0];
        let Some(range) = parse_score_range(args[1], args[2]) else {
          buf.write_error(b"ERR min or max is not a float");
          return Ok(());
        };
        let count = session.store_session.zremrangebyscore(key, range).await?;
        buf.write_integer(count as i64);
      }
      RespCommand::ZREMRANGEBYLEX => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zremrangebylex' command");
          return Ok(());
        }
        let key = args[0];
        let (Some(min), Some(max)) = (LexBound::parse(args[1]), LexBound::parse(args[2])) else {
          buf.write_error(b"ERR min or max not valid string range item");
          return Ok(());
        };
        let count = session.store_session.zremrangebylex(key, min, max).await?;
        buf.write_integer(count as i64);
      }
      RespCommand::ZLEXCOUNT => {
        if args.len() != 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zlexcount' command");
          return Ok(());
        }
        let key = args[0];
        let (Some(min), Some(max)) = (LexBound::parse(args[1]), LexBound::parse(args[2])) else {
          buf.write_error(b"ERR min or max not valid string range item");
          return Ok(());
        };
        let count = session.store_session.zlexcount(key, min, max).await?;
        buf.write_integer(count as i64);
      }
      RespCommand::ZRANK | RespCommand::ZREVRANK => {
        if args.len() != 2 {
          let err: &[u8] = if cmd == RespCommand::ZRANK {
            b"ERR wrong number of arguments for 'zrank' command"
          } else {
            b"ERR wrong number of arguments for 'zrevrank' command"
          };
          buf.write_error(err);
          return Ok(());
        }
        let key = args[0];
        let member = args[1];
        let rank = if cmd == RespCommand::ZRANK {
          session.store_session.zrank(key, member).await?
        } else {
          session.store_session.zrevrank(key, member).await?
        };
        match rank {
          Some(r) => buf.write_integer(r as i64),
          None => buf.write_null(),
        }
      }
      RespCommand::ZPOPMIN | RespCommand::ZPOPMAX => {
        if args.is_empty() || args.len() > 2 {
          buf.write_error(if cmd == RespCommand::ZPOPMIN {
            b"ERR wrong number of arguments for 'zpopmin' command"
          } else {
            b"ERR wrong number of arguments for 'zpopmax' command"
          });
          return Ok(());
        }
        let key = args[0];
        let count = if args.len() == 2 {
          let Some(c) = ParseUtils::try_read_long(args[1]) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return Ok(());
          };
          if c < 0 {
            buf.write_error(b"ERR value is out of range, must be positive");
            return Ok(());
          }
          c as usize
        } else {
          1
        };
        let items = if cmd == RespCommand::ZPOPMIN {
          session.store_session.zpopmin(key, count).await?
        } else {
          session.store_session.zpopmax(key, count).await?
        };
        buf.write_array_header(items.len() * 2);
        for item in &items {
          buf.write_bulk_string(&item.0);
          buf.write_double_bulk(item.1);
        }
      }
      RespCommand::ZRANDMEMBER => {
        if args.is_empty() || args.len() > 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zrandmember' command");
          return Ok(());
        }
        let key = args[0];
        let is_single = args.len() == 1;
        let mut count = 1isize;
        let mut with_scores = false;
        if args.len() >= 2 {
          let Some(c) = ParseUtils::try_read_long(args[1]) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return Ok(());
          };
          count = c as isize;
        }
        if args.len() == 3 {
          if !args[2].eq_ignore_ascii_case(b"WITHSCORES") {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
          with_scores = true;
        }
        let items = session.store_session.zrandmember(key, count).await?;
        if is_single {
          if let Some(first) = items.first() {
            buf.write_bulk_string(&first.0);
          } else {
            buf.write_null();
          }
        } else if with_scores {
          buf.write_array_header(items.len() * 2);
          for item in &items {
            buf.write_bulk_string(&item.0);
            buf.write_double_bulk(item.1);
          }
        } else {
          buf.write_array_header(items.len());
          for item in &items {
            buf.write_bulk_string(&item.0);
          }
        }
      }
      RespCommand::ZUNION | RespCommand::ZINTER | RespCommand::ZDIFF => {
        let is_diff = cmd == RespCommand::ZDIFF;
        let Some(op_args) = parse_zset_op_args(args.as_slice(), true) else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        };
        let items = if is_diff {
          session.store_session.zdiff(&op_args.keys).await?
        } else if cmd == RespCommand::ZINTER {
          session
            .store_session
            .zinter(&op_args.keys, &op_args.weights, op_args.aggregate)
            .await?
        } else {
          session
            .store_session
            .zunion(&op_args.keys, &op_args.weights, op_args.aggregate)
            .await?
        };
        if op_args.with_scores {
          buf.write_array_header(items.len() * 2);
          for item in &items {
            buf.write_bulk_string(&item.0);
            buf.write_double_bulk(item.1);
          }
        } else {
          buf.write_array_header(items.len());
          for item in &items {
            buf.write_bulk_string(&item.0);
          }
        }
      }
      RespCommand::ZUNIONSTORE | RespCommand::ZINTERSTORE | RespCommand::ZDIFFSTORE => {
        if args.len() < 2 {
          buf.write_error(consts::err::WRONG_NUM_ARGS);
          return Ok(());
        }
        let dest = args[0];
        let is_diff = cmd == RespCommand::ZDIFFSTORE;
        let Some(op_args) = parse_zset_op_args(&args[1..], false) else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        };
        let count = if is_diff {
          session
            .store_session
            .zdiffstore(dest, &op_args.keys)
            .await?
        } else if cmd == RespCommand::ZINTERSTORE {
          session
            .store_session
            .zinterstore(dest, &op_args.keys, &op_args.weights, op_args.aggregate)
            .await?
        } else {
          session
            .store_session
            .zunionstore(dest, &op_args.keys, &op_args.weights, op_args.aggregate)
            .await?
        };
        buf.write_integer(count as i64);
      }
      RespCommand::ZINTERCARD => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'zintercard' command");
          return Ok(());
        }
        let Some(numkeys) = ParseUtils::try_read_long(args[0]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let numkeys = numkeys as usize;
        if numkeys == 0 || numkeys + 1 > args.len() {
          buf.write_error(b"ERR numkeys must be greater than 0 and match key arguments");
          return Ok(());
        }
        let keys = &args[1..=numkeys];
        let mut limit = 0usize;
        if args.len() > numkeys + 1 {
          if args[numkeys + 1].eq_ignore_ascii_case(b"LIMIT") && args.len() == numkeys + 3 {
            let Some(l) = ParseUtils::try_read_long(args[numkeys + 2]) else {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            };
            if l < 0 {
              buf.write_error(b"ERR LIMIT can't be negative");
              return Ok(());
            }
            limit = l as usize;
          } else {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        }
        let card = session.store_session.zintercard(keys, limit).await?;
        buf.write_integer(card as i64);
      }
      RespCommand::ZMPOP => {
        if args.len() < 3 {
          buf.write_error(b"ERR wrong number of arguments for 'zmpop' command");
          return Ok(());
        }
        let Some(numkeys) = ParseUtils::try_read_long(args[0]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        let numkeys = numkeys as usize;
        if numkeys == 0 || numkeys + 1 > args.len() {
          buf.write_error(b"ERR numkeys must be greater than 0 and match key arguments");
          return Ok(());
        }
        let keys = &args[1..=numkeys];
        let mut idx = numkeys + 1;
        let is_min;
        let mut count = 1usize;
        if idx < args.len() {
          if args[idx].eq_ignore_ascii_case(b"MIN") {
            is_min = true;
            idx += 1;
          } else if args[idx].eq_ignore_ascii_case(b"MAX") {
            is_min = false;
            idx += 1;
          } else {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        } else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        if idx < args.len() {
          if args[idx].eq_ignore_ascii_case(b"COUNT") && idx + 1 < args.len() {
            if let Some(c) = ParseUtils::try_read_long(args[idx + 1]) {
              count = (c as usize).min(65536);
            } else {
              buf.write_error(consts::err::INT_OUT_OF_RANGE);
              return Ok(());
            }
          } else {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
        }
        let mut popped = None;
        for &k in keys {
          let items = if is_min {
            session.store_session.zpopmin(k, count).await?
          } else {
            session.store_session.zpopmax(k, count).await?
          };
          if !items.is_empty() {
            popped = Some((k, items));
            break;
          }
        }
        match popped {
          Some((key, items)) => {
            buf.write_array_header(2);
            buf.write_bulk_string(key);
            buf.write_array_header(items.len() * 2);
            for item in &items {
              buf.write_bulk_string(&item.0);
              buf.write_double_bulk(item.1);
            }
          }
          None => buf.write_null(),
        }
      }
      RespCommand::BZPOPMIN | RespCommand::BZPOPMAX => {
        Self::handle_blocking_zpop(ctx, session, cmd, args, buf).await?;
      }
      RespCommand::BZMPOP => {
        Self::handle_blocking_zmpop(ctx, session, args, buf).await?;
      }
      RespCommand::ZSCAN => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'zscan' command");
          return Ok(());
        }
        let key = args[0];
        let Some(cursor) = ParseUtils::try_read_int(args[1]) else {
          buf.write_error(b"ERR value is not an integer or out of range");
          return Ok(());
        };
        let mut pattern = None;
        let mut count = 10;
        let mut i = 2;
        while i < args.len() {
          if args[i].eq_ignore_ascii_case(b"MATCH") && i + 1 < args.len() {
            pattern = Some(args[i + 1]);
            i += 2;
          } else if args[i].eq_ignore_ascii_case(b"COUNT") && i + 1 < args.len() {
            if let Some(c) = ParseUtils::try_read_int(args[i + 1]) {
              count = c as usize;
            }
            i += 2;
          } else {
            buf.write_error(b"ERR syntax error");
            return Ok(());
          }
        }
        let (next_cursor, items) = session
          .store_session
          .zscan(key, cursor as usize, count, pattern)
          .await?;
        buf.write_array_header(2);
        let mut cur_buf = Buffer::new();
        buf.write_bulk_string(cur_buf.format(next_cursor).as_bytes());
        buf.write_array_header(items.len() * 2);
        for (member, score) in &items {
          buf.write_bulk_string(member);
          buf.write_double_bulk(*score);
        }
      }

      // ====== 地理位置 (Geo) 命令 ======
      RespCommand::GEOADD => {
        if args.len() < 4 {
          buf.write_error(b"ERR wrong number of arguments for 'geoadd' command");
          return Ok(());
        }
        let key = args[0];
        let mut curr = 1;
        let mut nx = false;
        let mut xx = false;
        let mut ch = false;
        while curr < args.len() {
          if args[curr].eq_ignore_ascii_case(b"NX") {
            nx = true;
            curr += 1;
          } else if args[curr].eq_ignore_ascii_case(b"XX") {
            xx = true;
            curr += 1;
          } else if args[curr].eq_ignore_ascii_case(b"CH") {
            ch = true;
            curr += 1;
          } else {
            break;
          }
        }
        if nx && xx {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        let rem = args.len() - curr;
        if rem < 3 || !rem.is_multiple_of(3) {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        let mut items = Vec::with_capacity(rem / 3);
        while curr < args.len() {
          let Some(lon) = ParseUtils::try_read_double(args[curr], false) else {
            buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
            return Ok(());
          };
          let Some(lat) = ParseUtils::try_read_double(args[curr + 1], false) else {
            buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
            return Ok(());
          };
          if !(wedb_zset::LONGITUDE_MIN..=wedb_zset::LONGITUDE_MAX).contains(&lon)
            || !(wedb_zset::LATITUDE_MIN..=wedb_zset::LATITUDE_MAX).contains(&lat)
          {
            buf.write_error(b"ERR -180.0 <= longitude <= 180.0 and -90.0 <= latitude <= 90.0");
            return Ok(());
          }
          items.push((lon, lat, args[curr + 2]));
          curr += 3;
        }
        let added = session
          .store_session
          .geoadd_multi(key, &items, nx, xx, ch)
          .await?;
        if added > 0 {
          ctx.blocking.notify_waiters(&session.full_key(key), added);
        }
        buf.write_integer(added as i64);
      }
      RespCommand::GEODIST => {
        if args.len() < 3 || args.len() > 4 {
          buf.write_error(b"ERR wrong number of arguments for 'geodist' command");
          return Ok(());
        }
        let key = args[0];
        let member1 = args[1];
        let member2 = args[2];
        let unit = if args.len() == 4 {
          let Some(u) = GeoDistanceUnit::from_bytes(args[3]) else {
            buf.write_error(consts::err::NOT_VALID_GEO_DISTANCE_UNIT);
            return Ok(());
          };
          u
        } else {
          GeoDistanceUnit::M
        };
        match session.store_session.geodist(key, member1, member2).await? {
          Some(meters) => {
            let converted = unit.from_meters(meters);
            buf.write_double_bulk(converted);
          }
          None => buf.write_null(),
        }
      }
      RespCommand::GEOPOS => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'geopos' command");
          return Ok(());
        }
        let key = args[0];
        let members = &args[1..];
        let positions = session.store_session.geopos_multi(key, members).await?;
        buf.write_array_header(positions.len());
        for pos in positions {
          match pos {
            Some((lon, lat)) => {
              buf.write_array_header(2);
              buf.write_double_bulk(lon);
              buf.write_double_bulk(lat);
            }
            None => buf.write_null_array(),
          }
        }
      }
      RespCommand::GEOHASH => {
        if args.len() < 2 {
          buf.write_error(b"ERR wrong number of arguments for 'geohash' command");
          return Ok(());
        }
        let key = args[0];
        let members = &args[1..];
        let hashes = session.store_session.geohash(key, members).await?;
        buf.write_array_header(hashes.len());
        for hash in hashes {
          match hash {
            Some(h) => buf.write_bulk_string(h.as_bytes()),
            None => buf.write_null(),
          }
        }
      }
      RespCommand::GEORADIUS
      | RespCommand::GEORADIUS_RO
      | RespCommand::GEORADIUSBYMEMBER
      | RespCommand::GEORADIUSBYMEMBER_RO
      | RespCommand::GEOSEARCH
      | RespCommand::GEOSEARCHSTORE => {
        Self::handle_geosearch(cmd, args, session, buf).await?;
      }

      // ====== 集群协议 ======
      RespCommand::CLUSTER
      | RespCommand::CLUSTER_NODES
      | RespCommand::CLUSTER_SLOTS
      | RespCommand::CLUSTER_INFO
      | RespCommand::CLUSTER_KEYSLOT
      | RespCommand::CLUSTER_MEET
      | RespCommand::CLUSTER_FORGET
      | RespCommand::CLUSTER_RESET
      | RespCommand::CLUSTER_MYID
      | RespCommand::CLUSTER_ADDSLOTS
      | RespCommand::CLUSTER_DELSLOTS
      | RespCommand::CLUSTER_BUMPEPOCH
      | RespCommand::CLUSTER_SETSLOT
      | RespCommand::CLUSTER_MIGRATE => {
        Self::handle_cluster(ctx, session, cmd, args, buf);
      }
      RespCommand::MIGRATE => {
        if args.len() < 5 {
          buf.write_error(b"ERR wrong number of arguments for 'migrate' command");
          return Ok(());
        }
        buf.write_ok();
      }
      RespCommand::ASKING => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'asking' command");
          return Ok(());
        }
        session.asking = true;
        buf.write_ok();
      }
      RespCommand::READONLY => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'readonly' command");
          return Ok(());
        }
        session.is_readonly = true;
        buf.write_ok();
      }
      RespCommand::READWRITE => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'readwrite' command");
          return Ok(());
        }
        session.is_readonly = false;
        buf.write_ok();
      }
      RespCommand::Swapdb => {
        // 库交换需要跨 (ns, db) 键空间整体重定位，底层存储引擎尚未支持；
        // 事务内已由排队拦截报错并置脏（is_db_switch），此处兜底非事务路径
        buf.write_error(b"ERR SWAPDB is not supported");
      }

      // ====== 复制协议 ======
      RespCommand::REPLICAOF => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'replicaof' command");
          return Ok(());
        }
        buf.write_ok();
      }

      // ====== 事务辅助指令 ======
      // WATCH 接入设计（对标 Garnet TxnManager WatchVersionMap）：
      // 会话侧挂载 wedb_txn::TransactionManager + 共享 WatchVersionMap 后，
      // 此处应将各键经 session.full_key 全名注册 watched keys；所有非事务写路径
      // （含后台过期删除与内存逐出）写后必须调用 WatchVersionMap::bump_version_key
      // 使其他会话的 WATCH 失效。server 侧尚未挂载事务管理器，WATCH 暂为语义占位。
      RespCommand::WATCH => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'watch' command");
          return Ok(());
        }
        buf.write_ok();
      }
      RespCommand::UNWATCH => {
        if !args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'unwatch' command");
          return Ok(());
        }
        buf.write_ok();
      }

      // ====== 发布订阅 ======
      RespCommand::SUBSCRIBE => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'subscribe' command");
          return Ok(());
        }
        for ch in args.iter() {
          ctx.pubsub.subscribe(ch, &session.pubsub_session);
          let count = ctx.pubsub.session_subscription_count(session.id);
          buf.write_sub_reply(b"subscribe", Some(ch), count);
        }
        session.is_subscription_session = true;
      }
      RespCommand::UNSUBSCRIBE => {
        if args.is_empty() {
          let channels = ctx.pubsub.list_all_subscriptions(session.id);
          if channels.is_empty() {
            buf.write_sub_reply(b"unsubscribe", None, 0);
          } else {
            for ch in &channels {
              ctx.pubsub.unsubscribe(ch, &session.pubsub_session);
              let count = ctx.pubsub.session_subscription_count(session.id);
              buf.write_sub_reply(b"unsubscribe", Some(ch), count);
            }
          }
        } else {
          for ch in args.iter() {
            ctx.pubsub.unsubscribe(ch, &session.pubsub_session);
            let count = ctx.pubsub.session_subscription_count(session.id);
            buf.write_sub_reply(b"unsubscribe", Some(ch), count);
          }
        }
      }
      RespCommand::Psubscribe => {
        if args.is_empty() {
          buf.write_error(b"ERR wrong number of arguments for 'psubscribe' command");
          return Ok(());
        }
        for pat in args.iter() {
          ctx.pubsub.psubscribe(pat, &session.pubsub_session);
          let count = ctx.pubsub.session_subscription_count(session.id);
          buf.write_sub_reply(b"psubscribe", Some(pat), count);
        }
        session.is_subscription_session = true;
      }
      RespCommand::Punsubscribe => {
        if args.is_empty() {
          let patterns = ctx.pubsub.list_all_pattern_subscriptions(session.id);
          if patterns.is_empty() {
            buf.write_sub_reply(b"punsubscribe", None, 0);
          } else {
            for pat in &patterns {
              ctx.pubsub.punsubscribe(pat, &session.pubsub_session);
              let count = ctx.pubsub.session_subscription_count(session.id);
              buf.write_sub_reply(b"punsubscribe", Some(pat), count);
            }
          }
        } else {
          for pat in args.iter() {
            ctx.pubsub.punsubscribe(pat, &session.pubsub_session);
            let count = ctx.pubsub.session_subscription_count(session.id);
            buf.write_sub_reply(b"punsubscribe", Some(pat), count);
          }
        }
      }
      RespCommand::Pubsub
      | RespCommand::PubsubChannels
      | RespCommand::PubsubNumsub
      | RespCommand::PubsubNumpat => {
        Self::handle_pubsub(ctx, cmd, args, buf);
      }
      RespCommand::PUBLISH => {
        if args.len() != 2 {
          buf.write_error(b"ERR wrong number of arguments for 'publish' command");
          return Ok(());
        }
        let ch = args[0];
        let msg = args[1];
        // 底层广播同时覆盖精准频道与模式订阅者（pmessage 帧由 broker 组装）
        let receivers = ctx.pubsub.publish_now(ch, msg);
        buf.write_integer(receivers as i64);
      }

      // ====== 模块扩展命令 (MODULE LOAD / UNLOAD / LIST) ======
      RespCommand::Module | RespCommand::ModuleLoadcs => {
        modules::dispatch_modules(ctx, session, args, buf)?;
      }

      // ====== 范围索引命令 (RI.*) ======
      RespCommand::Ricreate
      | RespCommand::Riset
      | RespCommand::Riget
      | RespCommand::Ridel
      | RespCommand::Riscan
      | RespCommand::Rirange
      | RespCommand::Riexists
      | RespCommand::Riconfig
      | RespCommand::Rimetrics => {
        dispatch_range_index(ctx, session, cmd, args, buf).await?;
      }

      // ====== Lua 脚本命令 (EVAL / EVALSHA / SCRIPT) ======
      RespCommand::Eval
      | RespCommand::Evalsha
      | RespCommand::Script
      | RespCommand::ScriptLoad
      | RespCommand::ScriptFlush
      | RespCommand::ScriptExists => {
        dispatch_scripts(ctx, session, cmd, args, buf)?;
      }

      // ====== 向量集命令 (V*) ======
      RespCommand::Vadd
      | RespCommand::Vsim
      | RespCommand::Vcard
      | RespCommand::Vdim
      | RespCommand::Vemb
      | RespCommand::Vgetattr
      | RespCommand::Vsetattr
      | RespCommand::Vrem
      | RespCommand::Vismember
      | RespCommand::Vrandmember
      | RespCommand::Vlinks
      | RespCommand::Vinfo => {
        dispatch_vectors(ctx, session, cmd, args, buf)?;
      }

      _ => {
        buf.write_error_fmt(format_args!("ERR unknown command '{}'", cmd.as_str()));
      }
    }
    Ok(())
  }

  /// 「用户名#空间」AUTH 凭据点查认证：成功注入会话身份并切换数据面前缀
  ///
  /// 凭据完整携带 `(空间, 用户名)` 二元身份（语法见 [`wedb_acl::parse_user_token`]）：
  /// - `username` 纯用户名 → 仅在超管全局桶点查（超级用户专属登录方式）；
  /// - `username#<N>` → 仅在租户沙箱 Some(N) 点查（租户用户唯一登录方式）。
  ///
  /// 两种形式都是单点查 O(1)，无跨空间候选扫描；租户用户在全局桶不可见，
  /// 省略 `#N` 一律 WRONGPASS，从语法上杜绝越权登录尝试。
  ///
  /// 返回值语义：`Ok(true)` 认证成功；`Ok(false)` 口令错误/用户不存在（WRONGPASS）；
  /// `Err` 凭据语法非法或存储故障（ERR），与口令错误严格区分。
  async fn auth_and_bind_session(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    token: &str,
    password: &str,
  ) -> Result<bool> {
    let (username, ns) = wedb_acl::parse_user_token(token)?;
    let scope = ctx.acl.scope(ns);
    match scope.get_user_async(username).await {
      Ok(Some(h)) => {
        let ok = h.read().authenticate(password);
        if ok {
          session.set_authenticated(username, ns);
        }
        Ok(ok)
      }
      Ok(None) => {
        // 未命中恒时口令散列兜底，抹平「用户不存在」与「口令错误」的时间差
        wedb_acl::AclPassword::dummy(password);
        Ok(false)
      }
      Err(e) => Err(e.into()),
    }
  }

  /// 处理 ACL 相关子命令
  async fn handle_acl(
    ctx: &Arc<ServerContext>,
    session: &ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    let sub = if cmd == RespCommand::ACL {
      args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("")
    } else {
      ""
    };

    // 全部读写经会话名字空间作用域视图：租户会话被严格限定在自身沙箱内
    let scope = ctx.acl.scope(session.user_ns);

    if cmd == RespCommand::ACL_WHOAMI || sub.eq_ignore_ascii_case("WHOAMI") {
      let username = session.user.as_deref().unwrap_or("default");
      buf.write_bulk_string(username.as_bytes());
    } else if cmd == RespCommand::ACL_LIST || sub.eq_ignore_ascii_case("LIST") {
      let rules = scope.list_users();
      buf.write_array_header(rules.len());
      for r in &rules {
        buf.write_bulk_string(r.as_bytes());
      }
    } else if cmd == RespCommand::ACL_USERS || sub.eq_ignore_ascii_case("USERS") {
      let users = scope.user_names();
      buf.write_array_header(users.len());
      for u in &users {
        buf.write_bulk_string(u.as_bytes());
      }
    } else if cmd == RespCommand::ACL_SETUSER || sub.eq_ignore_ascii_case("SETUSER") {
      let (username_idx, rules_idx) = if cmd == RespCommand::ACL_SETUSER {
        (0, 1)
      } else {
        (1, 2)
      };
      if args.len() <= username_idx {
        buf.write_error(b"ERR wrong number of arguments for 'acl|setuser' command");
        return;
      }
      let username = match from_utf8(args[username_idx]) {
        Ok(u) => u,
        Err(_) => {
          buf.write_error(b"ERR invalid username");
          return;
        }
      };
      let rule_args = &args[rules_idx..];
      let mut rules: Vec<&str> = Vec::with_capacity(rule_args.len() + 2);
      for &arg in rule_args {
        match from_utf8(arg) {
          Ok(r) => rules.push(r),
          Err(_) => {
            buf.write_error(b"ERR invalid rule");
            return;
          }
        }
      }
      let mut ns_buf = itoa::Buffer::new();
      let mut alloc_buf = itoa::Buffer::new();
      // 名字空间绑定策略（doc/zh/ns.md 六.2）：
      // 区分租户沙箱会话（session.user_ns = Some(curr_ns)）与超管全局视界会话；
      // ns 绑定统一经 wedb_acl::scan_ns_bind 单趟扫描（取末次绑定，与应用语义一致）
      if let Some(curr_ns) = session.user_ns {
        match wedb_acl::scan_ns_bind(&rules) {
          Err(e) => {
            buf.write_error_fmt(format_args!("{e}"));
            return;
          }
          Ok(Some((_, raw))) => {
            // 租户显式指定 ns：仅当解析值等于 curr_ns 时允许，且绝对不推进全局水位
            if wedb_acl::parse_ns(raw).ok() != Some(Some(curr_ns)) {
              buf.write_error(
                b"ERR Permission denied: cannot grant namespace outside of the current tenant scope",
              );
              return;
            }
          }
          Ok(None) => {
            // 租户未指定 ns 规则：自动绑定当前租户名字空间（栈缓冲零堆分配）
            let ns_str = ns_buf.format(curr_ns);
            rules.push("ns");
            rules.push(ns_str);
          }
        }
      } else {
        // 超管会话：定位末次 ns 绑定值
        if let Ok(Some((vi, raw))) = wedb_acl::scan_ns_bind(&rules) {
          if raw == "0" {
            // 显式 NS 0：控制面自动分配保留值，仅超管可触发
            match ctx.ns_alloc.allocate() {
              Ok(n) => {
                let alloc_str = alloc_buf.format(n);
                rules[vi] = alloc_str;
              }
              Err(e) => {
                warn!(target: "wedb::audit::acl", "NS 0 自动分配失败: user='{username}', err={e:?}");
                buf.write_error_fmt(format_args!("{e}"));
                return;
              }
            }
          } else if let Ok(n) = raw.parse::<u64>()
            && (1..=wedb_acl::MAX_TENANT_NAMESPACE).contains(&n)
          {
            // 显式 NS N：先持久化推进分配水位再落用户，杜绝后续自增分配与 N 碰撞
            if let Err(e) = ctx.ns_alloc.advance_to(n) {
              warn!(target: "wedb::audit::acl", "推进名字空间水位失败: ns={n}, err={e:?}");
              buf.write_error_fmt(format_args!("{e}"));
              return;
            }
          }
        }
      }
      match scope.set_user_async(username, &rules).await {
        Ok(_) => {
          info!(target: "wedb::audit::acl", "用户 ACL 规则已更新: user='{username}', rules={rules:?}");
          buf.write_ok();
        }
        Err(e) => {
          warn!(target: "wedb::audit::acl", "用户 ACL 规则更新失败: user='{username}', rules={rules:?}, err={e:?}");
          write_err_line(buf, &e.to_string());
        }
      }
    } else if cmd == RespCommand::ACL_DELUSER || sub.eq_ignore_ascii_case("DELUSER") {
      let start_idx = if cmd == RespCommand::ACL_DELUSER {
        0
      } else {
        1
      };
      if args.len() <= start_idx {
        buf.write_error(b"ERR wrong number of arguments for 'acl|deluser' command");
        return;
      }
      let mut deleted = 0i64;
      for &arg in &args[start_idx..] {
        if let Ok(u) = from_utf8(arg) {
          match scope.del_user_async(u).await {
            Ok(true) => {
              info!(target: "wedb::audit::acl", "ACL 用户已删除: user='{u}'");
              deleted += 1;
            }
            Ok(false) => {
              debug!(target: "wedb::audit::acl", "待删除 ACL 用户不存在: user='{u}'");
            }
            Err(e) => {
              warn!(target: "wedb::audit::acl", "删除 ACL 用户失败: user='{u}', err={e:?}");
            }
          }
        }
      }
      buf.write_integer(deleted);
    } else if cmd == RespCommand::ACL_GETUSER || sub.eq_ignore_ascii_case("GETUSER") {
      let username_idx = if cmd == RespCommand::ACL_GETUSER {
        0
      } else {
        1
      };
      if args.len() <= username_idx {
        buf.write_error(b"ERR wrong number of arguments for 'acl|getuser' command");
        return;
      }
      let username = match from_utf8(args[username_idx]) {
        Ok(u) => u,
        Err(_) => {
          buf.write_error(b"ERR invalid username");
          return;
        }
      };
      match scope.get_user_async(username).await {
        Ok(Some(handle)) => {
          let u = handle.read();
          let rules = u.describe_rules();
          buf.write_array_header(rules.len());
          for r in &rules {
            buf.write_bulk_string(r.as_bytes());
          }
        }
        Ok(None) => buf.write_null(),
        Err(e) => buf.write_error_fmt(format_args!("{e}")),
      }
    } else if cmd == RespCommand::ACL_GENPASS || sub.eq_ignore_ascii_case("GENPASS") {
      let bits = if cmd == RespCommand::ACL_GENPASS {
        args
          .first()
          .and_then(|s| ParseUtils::try_read_int(s))
          .unwrap_or(256)
      } else {
        args
          .get(1)
          .and_then(ParseUtils::try_read_int)
          .unwrap_or(256)
      };
      if bits <= 0 {
        buf.write_error(b"ERR ACL GENPASS argument must be the number of bits for the output password, a positive number up to 4096");
        return;
      }
      match wedb_acl::AclPassword::genpass(Some(bits as usize)) {
        Ok(pass) => buf.write_bulk_string(pass.as_bytes()),
        Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
      }
    } else if cmd == RespCommand::ACL_SAVE || sub.eq_ignore_ascii_case("SAVE") {
      // 全量快照落库整体经 persist_mutex 串行化（含各名字空间租户），对齐 Redis ACL SAVE 语义；
      // 任一写入失败即中止并回传错误，杜绝半保存状态被误认为已持久化
      if let Err(e) = ctx.acl.save_all_to_storage().await {
        warn!(target: "wedb::audit::acl", "ACL SAVE 持久化失败: err={e:?}");
        buf.write_error_fmt(format_args!("ERR {e}"));
        return;
      }
      info!(target: "wedb::audit::acl", "ACL 规则状态已保存至持久化存储");
      buf.write_ok();
    } else if cmd == RespCommand::ACL_LOAD || sub.eq_ignore_ascii_case("LOAD") {
      match ctx
        .acl
        .init_from_storage(ctx.args.requirepass.as_deref())
        .await
      {
        Ok(_) => {
          info!(target: "wedb::audit::acl", "ACL 规则已从持久化存储重新加载");
          buf.write_ok();
        }
        Err(e) => {
          warn!(target: "wedb::audit::acl", "ACL LOAD 重新加载失败: err={e:?}");
          buf.write_error_fmt(format_args!("ERR {e}"));
        }
      }
    } else if cmd == RespCommand::ACL_CAT || sub.eq_ignore_ascii_case("CAT") {
      let cat_arg = if cmd == RespCommand::ACL_CAT {
        args.first().and_then(|s| from_utf8(s).ok())
      } else {
        args.get(1).and_then(|s| from_utf8(s).ok())
      };
      if let Some(cat) = cat_arg {
        if let Some(cmds) = wedb_acl::category_commands(cat) {
          buf.write_array_header(cmds.len());
          for c in cmds {
            buf.write_bulk_string(c.as_bytes());
          }
        } else {
          buf.write_error_fmt(format_args!("ERR Unknown category '{cat}'"));
        }
      } else {
        buf.write_array_header(wedb_acl::ALL_CATEGORIES.len());
        for c in wedb_acl::ALL_CATEGORIES {
          buf.write_bulk_string(c.as_bytes());
        }
      }
    } else {
      buf.write_ok();
    }
  }

  #[inline]
  fn handle_select(
    session: &mut ServerSession,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    if args.len() != 1 {
      buf.write_error(b"ERR wrong number of arguments for 'select' command");
    } else if let Some(db_idx) = ParseUtils::try_read_ulong(args[0]) {
      session.active_db = db_idx;
      session.store_session.set_active_db(db_idx);
      buf.write_ok();
    } else {
      buf.write_error(b"ERR value is not an integer or out of range");
    }
  }

  /// EXPIRE / PEXPIRE / EXPIREAT / PEXPIREAT（对标 C# KeyAdminCommands.NetworkEXPIRE）
  ///
  /// 相对时长换算绝对毫秒后经底层 TTL 记录落盘。返回码对齐 C# Garnet 契约：
  /// 键不存在（底层 -2）折算为 0（Garnet NOTFOUND 直接回 `:0`），
  /// 过去时间戳（底层 2，已物理删除）视同设置成功回 1
  async fn handle_expire(
    session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    if args.len() < 2 || args.len() > 4 {
      buf.write_error_fmt(format_args!(
        "ERR wrong number of arguments for '{}' command",
        cmd.as_str().to_ascii_lowercase()
      ));
      return;
    }
    let Some(expiration) = ParseUtils::try_read_long(args[1]) else {
      buf.write_error(consts::err::INT_OUT_OF_RANGE);
      return;
    };
    let opt = match parse_expire_options(&args[2..]) {
      Ok(o) => o,
      Err(msg) => {
        buf.write_error(msg.as_bytes());
        return;
      }
    };
    let now_ms = Clock::now_since_epoch().as_millis() as i64;
    let expire_at_ms: i64 = match cmd {
      RespCommand::EXPIRE => now_ms.saturating_add(expiration.saturating_mul(1000)),
      RespCommand::PEXPIRE => now_ms.saturating_add(expiration),
      RespCommand::EXPIREAT => expiration.saturating_mul(1000),
      _ => expiration,
    };
    let expire_at_u64 = expire_at_ms.max(0) as u64;
    match session
      .store_session
      .expire_at(args[0], expire_at_u64, opt)
      .await
    {
      Ok(code) => buf.write_integer(match code {
        -2 => 0,
        2 => 1,
        other => other as i64,
      }),
      Err(err) if err.is_wrong_type() => buf.write_error(consts::err::WRONG_TYPE),
      Err(err) => buf.write_error_fmt(format_args!("ERR {err}")),
    }
  }

  /// 处理 GETEX key [EX seconds | PX milliseconds | EXAT unix-time-seconds | PXAT unix-time-milliseconds | PERSIST]
  async fn handle_getex(
    session: &mut ServerSession,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    if args.is_empty() {
      buf.write_error(b"ERR wrong number of arguments for 'getex' command");
      return;
    }
    let key = args[0];
    let mut persist = false;
    let mut expire_at_ms: Option<u64> = None;

    if args.len() > 1 {
      let mut i = 1;
      let now_ms = Clock::now_since_epoch().as_millis() as i64;
      while i < args.len() {
        let opt = args[i];
        if opt.eq_ignore_ascii_case(b"PERSIST") {
          if persist || expire_at_ms.is_some() || i + 1 != args.len() {
            buf.write_error(consts::err::SYNTAX);
            return;
          }
          persist = true;
          i += 1;
        } else if opt.eq_ignore_ascii_case(b"EX") {
          if persist || expire_at_ms.is_some() || i + 1 >= args.len() {
            buf.write_error(consts::err::SYNTAX);
            return;
          }
          let Some(secs) = ParseUtils::try_read_long(args[i + 1]) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return;
          };
          let at = now_ms.saturating_add(secs.saturating_mul(1000));
          expire_at_ms = Some(at.max(0) as u64);
          i += 2;
        } else if opt.eq_ignore_ascii_case(b"PX") {
          if persist || expire_at_ms.is_some() || i + 1 >= args.len() {
            buf.write_error(consts::err::SYNTAX);
            return;
          }
          let Some(ms) = ParseUtils::try_read_long(args[i + 1]) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return;
          };
          let at = now_ms.saturating_add(ms);
          expire_at_ms = Some(at.max(0) as u64);
          i += 2;
        } else if opt.eq_ignore_ascii_case(b"EXAT") {
          if persist || expire_at_ms.is_some() || i + 1 >= args.len() {
            buf.write_error(consts::err::SYNTAX);
            return;
          }
          let Some(secs) = ParseUtils::try_read_long(args[i + 1]) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return;
          };
          let at = secs.saturating_mul(1000);
          expire_at_ms = Some(at.max(0) as u64);
          i += 2;
        } else if opt.eq_ignore_ascii_case(b"PXAT") {
          if persist || expire_at_ms.is_some() || i + 1 >= args.len() {
            buf.write_error(consts::err::SYNTAX);
            return;
          }
          let Some(ms) = ParseUtils::try_read_long(args[i + 1]) else {
            buf.write_error(consts::err::INT_OUT_OF_RANGE);
            return;
          };
          expire_at_ms = Some(ms.max(0) as u64);
          i += 2;
        } else {
          buf.write_error(consts::err::SYNTAX);
          return;
        }
      }
    }
    let mut old: Option<Vec<u8>> = None;
    match session
      .store_session
      .read_string_with(key, |v| old = Some(v.to_vec()))
      .await
    {
      Ok(_) => {}
      Err(err) if err.is_wrong_type() => {
        buf.write_error(consts::err::WRONG_TYPE);
        return;
      }
      Err(err) => {
        buf.write_error_fmt(format_args!("ERR {err}"));
        return;
      }
    }

    let Some(val) = old else {
      buf.write_null();
      return;
    };

    if persist {
      let _ = session.store_session.persist(key).await;
    } else if let Some(exp_ms) = expire_at_ms {
      let _ = session
        .store_session
        .expire_at(key, exp_ms, TtlOpt::NONE)
        .await;
    }

    buf.write_bulk_string(&val);
  }

  /// PUBSUB CHANNELS [pattern] / NUMSUB [channel...] / NUMPAT
  /// （对标 C# PubSubCommands 的内省子命令族）
  fn handle_pubsub(
    ctx: &Arc<ServerContext>,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    let (sub_cmd, sub_args): (&str, &[&[u8]]) = if cmd == RespCommand::Pubsub {
      let sub = args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("");
      (
        sub,
        if args.is_empty() {
          &[]
        } else {
          &args.as_slice()[1..]
        },
      )
    } else {
      let sub = match cmd {
        RespCommand::PubsubChannels => "CHANNELS",
        RespCommand::PubsubNumsub => "NUMSUB",
        RespCommand::PubsubNumpat => "NUMPAT",
        _ => "",
      };
      (sub, args.as_slice())
    };

    match sub_cmd {
      "CHANNELS" => {
        let pattern = sub_args.first().copied();
        let channels = ctx.pubsub.channels(pattern);
        buf.write_array_header(channels.len());
        for ch in &channels {
          buf.write_bulk_string(ch);
        }
      }
      "NUMSUB" => {
        // 展平为 [channel, count, ...] 交错数组（Redis 口径）
        buf.write_array_header(sub_args.len() * 2);
        for ch in sub_args {
          buf.write_bulk_string(ch);
          buf.write_integer(ctx.pubsub.numsub(ch) as i64);
        }
      }
      "NUMPAT" => buf.write_integer(ctx.pubsub.numpat() as i64),
      _ => buf.write_error_fmt(format_args!(
        "ERR Unknown subcommand or wrong number of arguments for '{sub_cmd}'. Try PUBSUB HELP."
      )),
    }
  }

  #[inline]
  fn write_client_info(session: &ServerSession, buf: &mut SendBuffer) {
    let mut itoa_id = itoa::Buffer::new();
    let id_str = itoa_id.format(session.id);
    let mut itoa_db = itoa::Buffer::new();
    let db_str = itoa_db.format(session.active_db);
    let mut info = [0u8; 256];
    let mut offset = 0;
    let mut append = |slice: &[u8]| {
      let next = offset + slice.len();
      info[offset..next].copy_from_slice(slice);
      offset = next;
    };
    append(b"id=");
    append(id_str.as_bytes());
    append(b" addr=127.0.0.1:0 fd=0 name= age=0 idle=0 flags=N db=");
    append(db_str.as_bytes());
    append(b" sub=0 psub=0 multi=-1 qbuf=0 qbuf-free=0 argv-mem=0 obl=0 oll=0 omem=0 tot-mem=0 events=r cmd=client\n");
    buf.write_bulk_string(&info[..offset]);
  }

  /// 处理 CLIENT 相关管理与状态查询指令
  fn handle_client(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    let (sub_cmd, sub_args): (&str, &[&[u8]]) = if cmd == RespCommand::CLIENT {
      let sub = args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("");
      (
        sub,
        if args.is_empty() {
          &[]
        } else {
          &args.as_slice()[1..]
        },
      )
    } else {
      let sub = match cmd {
        RespCommand::CLIENT_ID => "ID",
        RespCommand::CLIENT_INFO => "INFO",
        RespCommand::CLIENT_LIST => "LIST",
        RespCommand::CLIENT_KILL => "KILL",
        RespCommand::CLIENT_GETNAME => "GETNAME",
        RespCommand::CLIENT_SETNAME => "SETNAME",
        RespCommand::CLIENT_SETINFO => "SETINFO",
        RespCommand::CLIENT_UNBLOCK => "UNBLOCK",
        _ => "",
      };
      (sub, args.as_slice())
    };

    if sub_cmd.is_empty() {
      buf.write_error(b"ERR wrong number of arguments for 'client' command");
      return;
    }

    if sub_cmd.eq_ignore_ascii_case("ID") {
      if !sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'client|id' command");
      } else {
        buf.write_integer(session.id as i64);
      }
    } else if sub_cmd.eq_ignore_ascii_case("GETNAME") {
      if !sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'client|getname' command");
      } else {
        buf.write_null();
      }
    } else if sub_cmd.eq_ignore_ascii_case("SETNAME") {
      if sub_args.len() != 1 {
        buf.write_error(b"ERR wrong number of arguments for 'client|setname' command");
      } else {
        buf.write_ok();
      }
    } else if sub_cmd.eq_ignore_ascii_case("INFO") {
      if !sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'client|info' command");
      } else {
        Self::write_client_info(session, buf);
      }
    } else if sub_cmd.eq_ignore_ascii_case("LIST") {
      Self::write_client_info(session, buf);
    } else if sub_cmd.eq_ignore_ascii_case("KILL") {
      if sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'client|kill' command");
      } else {
        buf.write_ok();
      }
    } else if sub_cmd.eq_ignore_ascii_case("SETINFO") {
      if sub_args.len() != 2 {
        buf.write_error(b"ERR wrong number of arguments for 'client|setinfo' command");
      } else {
        buf.write_ok();
      }
    } else if sub_cmd.eq_ignore_ascii_case("UNBLOCK") {
      if sub_args.is_empty() || sub_args.len() > 2 {
        buf.write_error(b"ERR wrong number of arguments for 'client|unblock' command");
        return;
      }
      let Some(target_id) = ParseUtils::try_read_long(sub_args[0]) else {
        buf.write_error(consts::err::INT_OUT_OF_RANGE);
        return;
      };
      let is_error = if sub_args.len() == 2 {
        if sub_args[1].eq_ignore_ascii_case(b"TIMEOUT") {
          false
        } else if sub_args[1].eq_ignore_ascii_case(b"ERROR") {
          true
        } else {
          buf.write_error(consts::err::CLIENT_UNBLOCK_REASON);
          return;
        }
      } else {
        false
      };
      let unblocked = ctx.blocking.try_unblock(target_id as u64, is_error);
      buf.write_integer(if unblocked { 1 } else { 0 });
    } else {
      buf.write_error_fmt(format_args!(
        "ERR Unknown subcommand or wrong number of arguments for '{sub_cmd}'. Try CLIENT HELP."
      ));
    }
  }

  /// 处理 CONFIG 相关配置查询与修改指令
  fn handle_config(
    _ctx: &Arc<ServerContext>,
    _session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    let (sub_cmd, sub_args): (&str, &[&[u8]]) = if cmd == RespCommand::CONFIG {
      let sub = args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("");
      (
        sub,
        if args.is_empty() {
          &[]
        } else {
          &args.as_slice()[1..]
        },
      )
    } else {
      let sub = match cmd {
        RespCommand::CONFIG_GET => "GET",
        RespCommand::CONFIG_SET => "SET",
        RespCommand::CONFIG_REWRITE => "REWRITE",
        _ => "",
      };
      (sub, args.as_slice())
    };

    if sub_cmd.is_empty() {
      buf.write_error(b"ERR wrong number of arguments for 'config' command");
      return;
    }

    if sub_cmd.eq_ignore_ascii_case("GET") {
      if sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'config|get' command");
      } else {
        buf.write_array_header(0);
      }
    } else if sub_cmd.eq_ignore_ascii_case("SET") {
      if sub_args.len() < 2 || sub_args.len() % 2 != 0 {
        buf.write_error(b"ERR wrong number of arguments for 'config|set' command");
      } else {
        buf.write_ok();
      }
    } else if sub_cmd.eq_ignore_ascii_case("REWRITE") {
      if !sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'config|rewrite' command");
      } else {
        buf.write_ok();
      }
    } else {
      buf.write_error_fmt(format_args!(
        "ERR Unknown subcommand or wrong number of arguments for '{sub_cmd}'. Try CONFIG HELP."
      ));
    }
  }

  /// 处理 COMMAND 相关指令元数据探测
  fn handle_command(
    ctx: &Arc<ServerContext>,
    _session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    let (sub_cmd, sub_args): (&str, &[&[u8]]) = if cmd == RespCommand::COMMAND {
      let sub = args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("");
      (
        sub,
        if args.is_empty() {
          &[]
        } else {
          &args.as_slice()[1..]
        },
      )
    } else {
      let sub = match cmd {
        RespCommand::COMMAND_COUNT => "COUNT",
        RespCommand::COMMAND_DOCS => "DOCS",
        RespCommand::COMMAND_INFO => "INFO",
        RespCommand::COMMAND_GETKEYS => "GETKEYS",
        RespCommand::COMMAND_GETKEYSANDFLAGS => "GETKEYSANDFLAGS",
        _ => "",
      };
      (sub, args.as_slice())
    };

    if sub_cmd.is_empty() {
      buf.write_array_header(0);
      return;
    }

    if sub_cmd.eq_ignore_ascii_case("COUNT") {
      if !sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'command|count' command");
      } else {
        buf.write_integer(368);
      }
    } else if sub_cmd.eq_ignore_ascii_case("DOCS") {
      buf.write_array_header(0);
    } else if sub_cmd.eq_ignore_ascii_case("INFO") {
      if sub_args.is_empty() {
        // 内建命令元数据表未建模，聚合输出为空数组
        buf.write_array_header(0);
      } else {
        // 模块自定义命令按 Redis COMMAND INFO 形状聚合补齐，未知名回 NULL
        buf.write_array_header(sub_args.len());
        for name in sub_args {
          if !modules::try_write_module_command_info(ctx, name, buf) {
            buf.write_null();
          }
        }
      }
    } else if sub_cmd.eq_ignore_ascii_case("GETKEYS")
      || sub_cmd.eq_ignore_ascii_case("GETKEYSANDFLAGS")
    {
      if sub_args.is_empty() {
        buf.write_error(b"ERR Invalid arguments specified for command");
      } else {
        buf.write_array_header(0);
      }
    } else {
      buf.write_error_fmt(format_args!(
        "ERR Unknown subcommand or wrong number of arguments for '{sub_cmd}'. Try COMMAND HELP."
      ));
    }
  }

  /// 处理 BLPOP / BRPOP 阻塞弹出
  async fn handle_blocking_pop(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    if args.len() < 2 {
      buf.write_error(if cmd == RespCommand::BLPOP {
        b"ERR wrong number of arguments for 'blpop' command"
      } else {
        b"ERR wrong number of arguments for 'brpop' command"
      });
      return Ok(());
    }

    let timeout_arg = args[args.len() - 1];
    let Some(timeout_sec) = ParseUtils::try_read_double(timeout_arg, false) else {
      buf.write_error(consts::err::TIMEOUT_NOT_FLOAT);
      return Ok(());
    };
    if timeout_sec < 0.0 {
      buf.write_error(consts::err::TIMEOUT_NEGATIVE);
      return Ok(());
    }

    let is_left = cmd == RespCommand::BLPOP;
    let keys_len = args.len() - 1;

    // 1. 优先按顺序尝试从底层存储中立即弹出已存在元素
    for i in 0..keys_len {
      let key = args[i];
      let popped = if is_left {
        session.store_session.lpop(key, 1).await?
      } else {
        session.store_session.rpop(key, 1).await?
      };
      if let Some(item) = popped.first() {
        buf.write_array_header(2);
        buf.write_bulk_string(key);
        buf.write_bulk_string(item);
        return Ok(());
      }
    }

    // 2. 阻塞注册键须为名字空间限定全名（与生产者 notify_waiters 同一编码，杜绝跨会话串扰）
    let full_keys: Vec<bytes::Bytes> = args[..keys_len]
      .iter()
      .map(|k| session.full_key(k))
      .collect();

    let deadline = if timeout_sec > 0.0 {
      Some(Instant::now() + Duration::from_secs_f64(timeout_sec))
    } else {
      None
    };

    // 3. 信号驱动异步阻塞循环：底层存储为唯一真实数据源
    loop {
      let remaining_sec = match deadline {
        Some(dl) => {
          let now = Instant::now();
          if now >= dl {
            buf.write_null_array();
            return Ok(());
          }
          (dl - now).as_secs_f64()
        }
        None => 0.0,
      };

      let result = ctx
        .blocking
        .get_collection_item(session.id, cmd, &full_keys, remaining_sec, vec![])
        .await;

      if result.is_force_unblocked() {
        buf.write_error(consts::err::UNBLOCKED_CLIENT);
        return Ok(());
      } else if result.is_type_mismatch() {
        buf.write_error(consts::err::WRONG_TYPE);
        return Ok(());
      } else if let Some(full_k) = result.key {
        // 唤醒键为全名键，回剥会话前缀得到用户键后方可操作存储与回复
        let Some(user_key) = full_keys.iter().position(|f| *f == full_k).map(|i| args[i]) else {
          continue;
        };
        if let Some(item) = result.item {
          buf.write_array_header(2);
          buf.write_bulk_string(user_key);
          buf.write_bulk_string(&item);
          return Ok(());
        }
        // 信号唤醒：从真实存储中弹出元素
        let popped = if is_left {
          session.store_session.lpop(user_key, 1).await?
        } else {
          session.store_session.rpop(user_key, 1).await?
        };
        if let Some(item) = popped.first() {
          buf.write_array_header(2);
          buf.write_bulk_string(user_key);
          buf.write_bulk_string(item);
          return Ok(());
        }
        // 若被并发消费抢先弹空，继续循环等待直到 deadline
      } else {
        buf.write_null_array();
        return Ok(());
      }
    }
  }

  /// 处理 BLMOVE 阻塞转移
  async fn handle_blocking_move(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    if args.len() != 5 {
      buf.write_error(b"ERR wrong number of arguments for 'blmove' command");
      return Ok(());
    }
    let src_key = args[0];
    let dst_key = args[1];
    let (Some(src_dir), Some(dst_dir)) = (
      wedb_blocking::Direction::from_bytes(args[2]),
      wedb_blocking::Direction::from_bytes(args[3]),
    ) else {
      buf.write_error(consts::err::SYNTAX);
      return Ok(());
    };
    let Some(timeout_sec) = ParseUtils::try_read_double(args[4], false) else {
      buf.write_error(consts::err::TIMEOUT_NOT_FLOAT);
      return Ok(());
    };
    if timeout_sec < 0.0 {
      buf.write_error(consts::err::TIMEOUT_NEGATIVE);
      return Ok(());
    }

    let src_is_left = src_dir == wedb_blocking::Direction::Left;
    let dst_is_left = dst_dir == wedb_blocking::Direction::Left;

    // 1. 尝试立即从源列表弹出并推入目标列表
    let popped = if src_is_left {
      session.store_session.lpop(src_key, 1).await?
    } else {
      session.store_session.rpop(src_key, 1).await?
    };
    if let Some(item) = popped.first() {
      if dst_is_left {
        session
          .store_session
          .lpush(dst_key, [item.as_slice()])
          .await?;
      } else {
        session
          .store_session
          .rpush(dst_key, [item.as_slice()])
          .await?;
      }
      // 阻塞 broker 全部走名字空间限定全名键（与注册侧同编码）
      ctx.blocking.notify_waiters(&session.full_key(dst_key), 1);
      buf.write_bulk_string(item);
      return Ok(());
    }

    let deadline = if timeout_sec > 0.0 {
      Some(Instant::now() + Duration::from_secs_f64(timeout_sec))
    } else {
      None
    };

    let src_full = session.full_key(src_key);
    let keys = [src_full];
    loop {
      let remaining_sec = match deadline {
        Some(dl) => {
          let now = Instant::now();
          if now >= dl {
            buf.write_null();
            return Ok(());
          }
          (dl - now).as_secs_f64()
        }
        None => 0.0,
      };

      let result = ctx
        .blocking
        .get_collection_item(
          session.id,
          RespCommand::Blmove,
          &keys,
          remaining_sec,
          vec![],
        )
        .await;

      if result.is_force_unblocked() {
        buf.write_error(consts::err::UNBLOCKED_CLIENT);
        return Ok(());
      } else if result.is_type_mismatch() {
        buf.write_error(consts::err::WRONG_TYPE);
        return Ok(());
      } else if result.key.is_some() {
        if let Some(item) = result.item {
          buf.write_bulk_string(&item);
          return Ok(());
        }
        let popped = if src_is_left {
          session.store_session.lpop(src_key, 1).await?
        } else {
          session.store_session.rpop(src_key, 1).await?
        };
        if let Some(item) = popped.first() {
          if dst_is_left {
            session
              .store_session
              .lpush(dst_key, [item.as_slice()])
              .await?;
          } else {
            session
              .store_session
              .rpush(dst_key, [item.as_slice()])
              .await?;
          }
          ctx.blocking.notify_waiters(&session.full_key(dst_key), 1);
          buf.write_bulk_string(item);
          return Ok(());
        }
      } else {
        buf.write_null();
        return Ok(());
      }
    }
  }

  /// 处理 BRPOPLPUSH 阻塞右出左入
  async fn handle_brpoplpush(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    if args.len() != 3 {
      buf.write_error(b"ERR wrong number of arguments for 'brpoplpush' command");
      return Ok(());
    }
    let src_key = args[0];
    let dst_key = args[1];
    let Some(timeout_sec) = ParseUtils::try_read_double(args[2], false) else {
      buf.write_error(consts::err::TIMEOUT_NOT_FLOAT);
      return Ok(());
    };
    if timeout_sec < 0.0 {
      buf.write_error(consts::err::TIMEOUT_NEGATIVE);
      return Ok(());
    }

    // 1. 尝试立即从源列表右端弹出并推入目标列表左端
    let popped = session.store_session.rpop(src_key, 1).await?;
    if let Some(item) = popped.first() {
      session
        .store_session
        .lpush(dst_key, [item.as_slice()])
        .await?;
      // 阻塞 broker 全部走名字空间限定全名键（与注册侧同编码）
      ctx.blocking.notify_waiters(&session.full_key(dst_key), 1);
      buf.write_bulk_string(item);
      return Ok(());
    }

    let deadline = if timeout_sec > 0.0 {
      Some(Instant::now() + Duration::from_secs_f64(timeout_sec))
    } else {
      None
    };

    let src_full = session.full_key(src_key);
    let keys = [src_full];
    loop {
      let remaining_sec = match deadline {
        Some(dl) => {
          let now = Instant::now();
          if now >= dl {
            buf.write_null();
            return Ok(());
          }
          (dl - now).as_secs_f64()
        }
        None => 0.0,
      };

      let result = ctx
        .blocking
        .get_collection_item(
          session.id,
          RespCommand::Brpoplpush,
          &keys,
          remaining_sec,
          vec![],
        )
        .await;

      if result.is_force_unblocked() {
        buf.write_error(consts::err::UNBLOCKED_CLIENT);
        return Ok(());
      } else if result.is_type_mismatch() {
        buf.write_error(consts::err::WRONG_TYPE);
        return Ok(());
      } else if result.key.is_some() {
        if let Some(item) = result.item {
          buf.write_bulk_string(&item);
          return Ok(());
        }
        let popped = session.store_session.rpop(src_key, 1).await?;
        if let Some(item) = popped.first() {
          session
            .store_session
            .lpush(dst_key, [item.as_slice()])
            .await?;
          ctx.blocking.notify_waiters(&session.full_key(dst_key), 1);
          buf.write_bulk_string(item);
          return Ok(());
        }
      } else {
        buf.write_null();
        return Ok(());
      }
    }
  }

  /// 处理 BZPOPMIN / BZPOPMAX 阻塞弹出
  async fn handle_blocking_zpop(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    if args.len() < 2 {
      buf.write_error(if cmd == RespCommand::BZPOPMIN {
        b"ERR wrong number of arguments for 'bzpopmin' command"
      } else {
        b"ERR wrong number of arguments for 'bzpopmax' command"
      });
      return Ok(());
    }

    let timeout_arg = args[args.len() - 1];
    let Some(timeout_sec) = ParseUtils::try_read_double(timeout_arg, false) else {
      buf.write_error(consts::err::TIMEOUT_NOT_FLOAT);
      return Ok(());
    };
    if timeout_sec < 0.0 {
      buf.write_error(consts::err::TIMEOUT_NEGATIVE);
      return Ok(());
    }

    let is_min = cmd == RespCommand::BZPOPMIN;
    let keys_len = args.len() - 1;

    for i in 0..keys_len {
      let key = args[i];
      let popped = if is_min {
        session.store_session.zpopmin(key, 1).await?
      } else {
        session.store_session.zpopmax(key, 1).await?
      };
      if let Some(item) = popped.first() {
        buf.write_array_header(3);
        buf.write_bulk_string(key);
        buf.write_bulk_string(&item.0);
        buf.write_double_bulk(item.1);
        return Ok(());
      }
    }

    // 阻塞注册键须为名字空间限定全名（与生产者 notify_waiters 同一编码）
    let full_keys: Vec<bytes::Bytes> = args[..keys_len]
      .iter()
      .map(|k| session.full_key(k))
      .collect();

    let deadline = if timeout_sec > 0.0 {
      Some(Instant::now() + Duration::from_secs_f64(timeout_sec))
    } else {
      None
    };

    loop {
      let remaining_sec = match deadline {
        Some(dl) => {
          let now = Instant::now();
          if now >= dl {
            buf.write_null_array();
            return Ok(());
          }
          (dl - now).as_secs_f64()
        }
        None => 0.0,
      };

      let result = ctx
        .blocking
        .get_collection_item(session.id, cmd, &full_keys, remaining_sec, vec![])
        .await;

      if result.is_force_unblocked() {
        buf.write_error(consts::err::UNBLOCKED_CLIENT);
        return Ok(());
      } else if result.is_type_mismatch() {
        buf.write_error(consts::err::WRONG_TYPE);
        return Ok(());
      } else if let Some(full_k) = result.key {
        // 全名键回剥会话前缀得到用户键后方可操作存储与回复
        let Some(user_key) = full_keys.iter().position(|f| *f == full_k).map(|i| args[i]) else {
          continue;
        };
        let popped = if is_min {
          session.store_session.zpopmin(user_key, 1).await?
        } else {
          session.store_session.zpopmax(user_key, 1).await?
        };
        if let Some(item) = popped.first() {
          buf.write_array_header(3);
          buf.write_bulk_string(user_key);
          buf.write_bulk_string(&item.0);
          buf.write_double_bulk(item.1);
          return Ok(());
        }
      } else {
        buf.write_null_array();
        return Ok(());
      }
    }
  }

  /// 处理 BZMPOP 阻塞多键弹出
  async fn handle_blocking_zmpop(
    ctx: &Arc<ServerContext>,
    session: &mut ServerSession,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    if args.len() < 4 {
      buf.write_error(b"ERR wrong number of arguments for 'bzmpop' command");
      return Ok(());
    }

    let Some(timeout_sec) = ParseUtils::try_read_double(args[0], false) else {
      buf.write_error(consts::err::TIMEOUT_NOT_FLOAT);
      return Ok(());
    };
    if timeout_sec < 0.0 {
      buf.write_error(consts::err::TIMEOUT_NEGATIVE);
      return Ok(());
    }

    let Some(numkeys) = ParseUtils::try_read_long(args[1]) else {
      buf.write_error(consts::err::INT_OUT_OF_RANGE);
      return Ok(());
    };
    let numkeys = numkeys as usize;
    if numkeys == 0 || numkeys + 2 > args.len() {
      buf.write_error(b"ERR numkeys must be greater than 0 and match key arguments");
      return Ok(());
    }

    let keys = &args[2..=numkeys + 1];
    let mut idx = numkeys + 2;
    let is_min;
    let mut count = 1usize;
    if idx < args.len() {
      if args[idx].eq_ignore_ascii_case(b"MIN") {
        is_min = true;
        idx += 1;
      } else if args[idx].eq_ignore_ascii_case(b"MAX") {
        is_min = false;
        idx += 1;
      } else {
        buf.write_error(consts::err::SYNTAX);
        return Ok(());
      }
    } else {
      buf.write_error(consts::err::SYNTAX);
      return Ok(());
    }

    if idx < args.len() {
      if args[idx].eq_ignore_ascii_case(b"COUNT") && idx + 1 < args.len() {
        if let Some(c) = ParseUtils::try_read_long(args[idx + 1]) {
          count = (c as usize).min(65536);
        } else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        }
      } else {
        buf.write_error(consts::err::SYNTAX);
        return Ok(());
      }
    }

    for &k in keys {
      let items = if is_min {
        session.store_session.zpopmin(k, count).await?
      } else {
        session.store_session.zpopmax(k, count).await?
      };
      if !items.is_empty() {
        buf.write_array_header(2);
        buf.write_bulk_string(k);
        buf.write_array_header(items.len() * 2);
        for item in &items {
          buf.write_bulk_string(&item.0);
          buf.write_double_bulk(item.1);
        }
        return Ok(());
      }
    }

    // 阻塞注册键须为名字空间限定全名（与生产者 notify_waiters 同一编码）
    let keys_bytes: Vec<bytes::Bytes> = keys.iter().map(|k| session.full_key(k)).collect();

    let deadline = if timeout_sec > 0.0 {
      Some(Instant::now() + Duration::from_secs_f64(timeout_sec))
    } else {
      None
    };

    loop {
      let remaining_sec = match deadline {
        Some(dl) => {
          let now = Instant::now();
          if now >= dl {
            buf.write_null();
            return Ok(());
          }
          (dl - now).as_secs_f64()
        }
        None => 0.0,
      };

      let result = ctx
        .blocking
        .get_collection_item(
          session.id,
          RespCommand::Bzmpop,
          &keys_bytes,
          remaining_sec,
          vec![],
        )
        .await;

      if result.is_force_unblocked() {
        buf.write_error(consts::err::UNBLOCKED_CLIENT);
        return Ok(());
      } else if result.is_type_mismatch() {
        buf.write_error(consts::err::WRONG_TYPE);
        return Ok(());
      } else if let Some(full_k) = result.key {
        // 全名键回剥会话前缀得到用户键后方可操作存储与回复
        let Some(user_key) = keys_bytes
          .iter()
          .position(|f| *f == full_k)
          .map(|i| keys[i])
        else {
          continue;
        };
        let items = if is_min {
          session.store_session.zpopmin(user_key, count).await?
        } else {
          session.store_session.zpopmax(user_key, count).await?
        };
        if !items.is_empty() {
          buf.write_array_header(2);
          buf.write_bulk_string(user_key);
          buf.write_array_header(items.len() * 2);
          for item in &items {
            buf.write_bulk_string(&item.0);
            buf.write_double_bulk(item.1);
          }
          return Ok(());
        }
      } else {
        buf.write_null();
        return Ok(());
      }
    }
  }

  /// 处理 GEOSEARCH / GEORADIUS 系列空间检索命令
  async fn handle_geosearch(
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    session: &ServerSession,
    buf: &mut SendBuffer,
  ) -> Result<()> {
    let mut curr;
    let dest_key: Option<&[u8]>;
    let src_key: &[u8];

    if cmd == RespCommand::GEOSEARCHSTORE {
      if args.len() < 7 {
        buf.write_error(b"ERR wrong number of arguments for 'geosearchstore' command");
        return Ok(());
      }
      dest_key = Some(args[0]);
      src_key = args[1];
      curr = 2;
    } else {
      let min_len = match cmd {
        RespCommand::GEORADIUS | RespCommand::GEORADIUS_RO => 5,
        RespCommand::GEORADIUSBYMEMBER | RespCommand::GEORADIUSBYMEMBER_RO => 4,
        RespCommand::GEOSEARCH => 6,
        _ => 2,
      };
      if args.len() < min_len {
        buf.write_error(consts::err::WRONG_NUM_ARGS);
        return Ok(());
      }
      dest_key = None;
      src_key = args[0];
      curr = 1;
    }

    let mut center = None;
    let mut shape = None;
    let mut sort = GeoSortOrder::None;
    let mut count = None;
    let mut count_any = false;
    let mut with_coord = false;
    let mut with_dist = false;
    let mut with_hash = false;
    let mut store_key: Option<&[u8]> = None;
    let mut store_dist = false;
    let mut dist_unit = GeoDistanceUnit::M;

    if cmd == RespCommand::GEORADIUS || cmd == RespCommand::GEORADIUS_RO {
      let Some(lon) = ParseUtils::try_read_double(args[curr], false) else {
        buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
        return Ok(());
      };
      let Some(lat) = ParseUtils::try_read_double(args[curr + 1], false) else {
        buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
        return Ok(());
      };
      let Some(radius) = ParseUtils::try_read_double(args[curr + 2], false) else {
        buf.write_error(consts::err::NOT_VALID_RADIUS);
        return Ok(());
      };
      if radius < 0.0 {
        buf.write_error(consts::err::RADIUS_IS_NEGATIVE);
        return Ok(());
      }
      let Some(unit) = GeoDistanceUnit::from_bytes(args[curr + 3]) else {
        buf.write_error(consts::err::NOT_VALID_GEO_DISTANCE_UNIT);
        return Ok(());
      };
      center = Some(GeoSearchCenter::Coord { lon, lat });
      shape = Some(GeoSearchShape::Radius { radius, unit });
      dist_unit = unit;
      curr += 4;
    } else if cmd == RespCommand::GEORADIUSBYMEMBER || cmd == RespCommand::GEORADIUSBYMEMBER_RO {
      let member = args[curr];
      let Some(radius) = ParseUtils::try_read_double(args[curr + 1], false) else {
        buf.write_error(consts::err::NOT_VALID_RADIUS);
        return Ok(());
      };
      if radius < 0.0 {
        buf.write_error(consts::err::RADIUS_IS_NEGATIVE);
        return Ok(());
      }
      let Some(unit) = GeoDistanceUnit::from_bytes(args[curr + 2]) else {
        buf.write_error(consts::err::NOT_VALID_GEO_DISTANCE_UNIT);
        return Ok(());
      };
      center = Some(GeoSearchCenter::Member(member));
      shape = Some(GeoSearchShape::Radius { radius, unit });
      dist_unit = unit;
      curr += 3;
    }

    while curr < args.len() {
      let opt = args[curr];
      if opt.eq_ignore_ascii_case(b"FROMMEMBER")
        && (cmd == RespCommand::GEOSEARCH || cmd == RespCommand::GEOSEARCHSTORE)
      {
        if center.is_some() || curr + 1 >= args.len() {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        center = Some(GeoSearchCenter::Member(args[curr + 1]));
        curr += 2;
      } else if opt.eq_ignore_ascii_case(b"FROMLONLAT")
        && (cmd == RespCommand::GEOSEARCH || cmd == RespCommand::GEOSEARCHSTORE)
      {
        if center.is_some() || curr + 2 >= args.len() {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        let Some(lon) = ParseUtils::try_read_double(args[curr + 1], false) else {
          buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
          return Ok(());
        };
        let Some(lat) = ParseUtils::try_read_double(args[curr + 2], false) else {
          buf.write_error(consts::err::FLOAT_OUT_OF_RANGE);
          return Ok(());
        };
        center = Some(GeoSearchCenter::Coord { lon, lat });
        curr += 3;
      } else if opt.eq_ignore_ascii_case(b"BYRADIUS")
        && (cmd == RespCommand::GEOSEARCH || cmd == RespCommand::GEOSEARCHSTORE)
      {
        if shape.is_some() || curr + 2 >= args.len() {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        let Some(radius) = ParseUtils::try_read_double(args[curr + 1], false) else {
          buf.write_error(consts::err::NOT_VALID_RADIUS);
          return Ok(());
        };
        if radius < 0.0 {
          buf.write_error(consts::err::RADIUS_IS_NEGATIVE);
          return Ok(());
        }
        let Some(unit) = GeoDistanceUnit::from_bytes(args[curr + 2]) else {
          buf.write_error(consts::err::NOT_VALID_GEO_DISTANCE_UNIT);
          return Ok(());
        };
        shape = Some(GeoSearchShape::Radius { radius, unit });
        dist_unit = unit;
        curr += 3;
      } else if opt.eq_ignore_ascii_case(b"BYBOX")
        && (cmd == RespCommand::GEOSEARCH || cmd == RespCommand::GEOSEARCHSTORE)
      {
        if shape.is_some() || curr + 3 >= args.len() {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        let Some(width) = ParseUtils::try_read_double(args[curr + 1], false) else {
          buf.write_error(consts::err::NOT_VALID_WIDTH);
          return Ok(());
        };
        let Some(height) = ParseUtils::try_read_double(args[curr + 2], false) else {
          buf.write_error(consts::err::NOT_VALID_HEIGHT);
          return Ok(());
        };
        if width < 0.0 || height < 0.0 {
          buf.write_error(consts::err::HEIGHT_OR_WIDTH_NEGATIVE);
          return Ok(());
        }
        let Some(unit) = GeoDistanceUnit::from_bytes(args[curr + 3]) else {
          buf.write_error(consts::err::NOT_VALID_GEO_DISTANCE_UNIT);
          return Ok(());
        };
        shape = Some(GeoSearchShape::Box {
          width,
          height,
          unit,
        });
        dist_unit = unit;
        curr += 4;
      } else if opt.eq_ignore_ascii_case(b"ASC") {
        sort = GeoSortOrder::Asc;
        curr += 1;
      } else if opt.eq_ignore_ascii_case(b"DESC") {
        sort = GeoSortOrder::Desc;
        curr += 1;
      } else if opt.eq_ignore_ascii_case(b"COUNT") {
        if curr + 1 >= args.len() {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        let Some(c) = ParseUtils::try_read_int(args[curr + 1]) else {
          buf.write_error(consts::err::INT_OUT_OF_RANGE);
          return Ok(());
        };
        if c <= 0 {
          buf.write_error(consts::err::COUNT_IS_NOT_POSITIVE);
          return Ok(());
        }
        count = Some(c as usize);
        curr += 2;
        if curr < args.len() && args[curr].eq_ignore_ascii_case(b"ANY") {
          count_any = true;
          curr += 1;
        }
      } else if opt.eq_ignore_ascii_case(b"WITHCOORD") {
        with_coord = true;
        curr += 1;
      } else if opt.eq_ignore_ascii_case(b"WITHDIST") {
        with_dist = true;
        curr += 1;
      } else if opt.eq_ignore_ascii_case(b"WITHHASH") {
        with_hash = true;
        curr += 1;
      } else if opt.eq_ignore_ascii_case(b"STORE")
        && (cmd == RespCommand::GEORADIUS || cmd == RespCommand::GEORADIUSBYMEMBER)
      {
        if curr + 1 >= args.len() {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
        store_key = Some(args[curr + 1]);
        curr += 2;
      } else if opt.eq_ignore_ascii_case(b"STOREDIST") {
        if cmd == RespCommand::GEOSEARCHSTORE {
          store_dist = true;
          curr += 1;
        } else if cmd == RespCommand::GEORADIUS || cmd == RespCommand::GEORADIUSBYMEMBER {
          if curr + 1 >= args.len() {
            buf.write_error(consts::err::SYNTAX);
            return Ok(());
          }
          store_key = Some(args[curr + 1]);
          store_dist = true;
          curr += 2;
        } else {
          buf.write_error(consts::err::SYNTAX);
          return Ok(());
        }
      } else {
        buf.write_error(consts::err::SYNTAX);
        return Ok(());
      }
    }

    let (Some(center), Some(shape)) = (center, shape) else {
      buf.write_error(consts::err::SYNTAX);
      return Ok(());
    };

    let search_opts = wedb_redis::GeoSearchOpt {
      center,
      shape,
      sort,
      count,
      count_any,
    };

    let target_store_key = dest_key.or(store_key);
    if let Some(target) = target_store_key {
      let stored_count = session
        .store_session
        .geosearchstore(target, src_key, search_opts, store_dist, dist_unit)
        .await?;
      buf.write_integer(stored_count as i64);
    } else {
      let results = match session.store_session.geosearch(src_key, search_opts).await {
        Ok(r) => r,
        Err(err) if err.is_zset_member_not_found() => {
          buf.write_error(consts::err::ZSET_MEMBER_NOT_FOUND);
          return Ok(());
        }
        Err(err) if err.is_wrong_type() => {
          buf.write_error(consts::err::WRONG_TYPE);
          return Ok(());
        }
        Err(err) => return Err(err.into()),
      };

      if results.is_empty() {
        buf.write_array_header(0);
        return Ok(());
      }

      let has_extra = with_coord || with_dist || with_hash;
      let inner_len = 1 + (with_dist as usize) + (with_hash as usize) + (with_coord as usize);

      buf.write_array_header(results.len());
      for item in &results {
        if has_extra {
          buf.write_array_header(inner_len);
        }
        buf.write_bulk_string(&item.member);
        if with_dist {
          let converted = dist_unit.from_meters(item.distance);
          buf.write_double_bulk(converted);
        }
        if with_hash {
          buf.write_integer(item.score as i64);
        }
        if with_coord {
          buf.write_array_header(2);
          buf.write_double_bulk(item.coord.0);
          buf.write_double_bulk(item.coord.1);
        }
      }
    }

    Ok(())
  }

  /// 解析 CLUSTER ADDSLOTS / DELSLOTS 的哈希槽参数列表
  #[inline]
  fn parse_cluster_slots(sub_args: &[&[u8]], buf: &mut SendBuffer) -> Option<Vec<u16>> {
    let mut slots = Vec::with_capacity(sub_args.len());
    for &arg in sub_args {
      let Some(slot) = from_utf8(arg).ok().and_then(|s| s.parse::<u16>().ok()) else {
        buf.write_error(b"ERR Invalid slot ID specified");
        return None;
      };
      slots.push(slot);
    }
    Some(slots)
  }

  /// 处理 CLUSTER 相关命令
  fn handle_cluster(
    ctx: &Arc<ServerContext>,
    _session: &mut ServerSession,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    buf: &mut SendBuffer,
  ) {
    let Some(ref cluster) = ctx.cluster else {
      buf.write_error(b"ERR This instance has cluster support disabled");
      return;
    };

    let (sub, offset): (&str, usize) = if cmd == RespCommand::CLUSTER {
      let s = args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("");
      (s, 1)
    } else {
      match cmd {
        RespCommand::CLUSTER_NODES => ("NODES", 0),
        RespCommand::CLUSTER_SLOTS => ("SLOTS", 0),
        RespCommand::CLUSTER_INFO => ("INFO", 0),
        RespCommand::CLUSTER_KEYSLOT => ("KEYSLOT", 0),
        RespCommand::CLUSTER_MYID => ("MYID", 0),
        RespCommand::CLUSTER_MEET => ("MEET", 0),
        RespCommand::CLUSTER_FORGET => ("FORGET", 0),
        RespCommand::CLUSTER_RESET => ("RESET", 0),
        RespCommand::CLUSTER_ADDSLOTS => ("ADDSLOTS", 0),
        RespCommand::CLUSTER_DELSLOTS => ("DELSLOTS", 0),
        RespCommand::CLUSTER_BUMPEPOCH => ("BUMPEPOCH", 0),
        RespCommand::CLUSTER_SETSLOT => ("SETSLOT", 0),
        RespCommand::CLUSTER_MIGRATE => ("MIGRATE", 0),
        _ => ("", 0),
      }
    };

    let sub_args = &args[offset.min(args.len())..];

    if cmd == RespCommand::CLUSTER_NODES || sub.eq_ignore_ascii_case("NODES") {
      let cfg = cluster.current_config();
      let nodes = format_cluster_nodes(&cfg);
      buf.write_bulk_string(nodes.as_bytes());
    } else if cmd == RespCommand::CLUSTER_SLOTS || sub.eq_ignore_ascii_case("SLOTS") {
      let cfg = cluster.current_config();
      let slots_resp = format_cluster_slots(&cfg);
      buf.write_raw(slots_resp.as_bytes());
    } else if cmd == RespCommand::CLUSTER_INFO || sub.eq_ignore_ascii_case("INFO") {
      let cfg = cluster.current_config();
      let info = format_cluster_info(&cfg);
      buf.write_bulk_string(info.as_bytes());
    } else if cmd == RespCommand::CLUSTER_KEYSLOT || sub.eq_ignore_ascii_case("KEYSLOT") {
      if let [k] = sub_args {
        let slot = hash_slot(k);
        buf.write_integer(slot as i64);
      } else {
        buf.write_error(b"ERR wrong number of arguments for 'cluster keyslot'");
      }
    } else if sub.eq_ignore_ascii_case("MYID") {
      cluster.with_config(|c| {
        if let Some(id) = c.local_node_id() {
          buf.write_bulk_string(id.as_bytes());
        } else {
          buf.write_null();
        }
      });
    } else if sub.eq_ignore_ascii_case("MEET") {
      if sub_args.len() < 2 {
        buf.write_error(b"ERR wrong number of arguments for 'cluster meet'");
        return;
      }
      let (Some(ip), Some(port)) = (
        from_utf8(sub_args[0]).ok(),
        from_utf8(sub_args[1])
          .ok()
          .and_then(|s| s.parse::<u16>().ok()),
      ) else {
        buf.write_error(b"ERR Invalid node address or port specified");
        return;
      };
      if port == 0 {
        buf.write_error(b"ERR Invalid node address or port specified");
        return;
      }
      if is_myself_addr(&ctx.args.bind, ctx.args.port, ip, port) {
        buf.write_error(b"ERR Can't MEET myself");
        return;
      }
      match cluster.try_meet(ip, port, None) {
        Ok(_) => buf.write_ok(),
        Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
      }
    } else if sub.eq_ignore_ascii_case("FORGET") {
      if sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'cluster forget'");
        return;
      }
      let Some(node_id) = from_utf8(sub_args[0]).ok() else {
        buf.write_error(b"ERR Invalid node ID specified");
        return;
      };
      match cluster.try_remove_worker(node_id, 60) {
        Ok(_) => buf.write_ok(),
        Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
      }
    } else if sub.eq_ignore_ascii_case("ADDSLOTS") {
      if sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'cluster addslots'");
        return;
      }
      if let Some(slots) = Self::parse_cluster_slots(sub_args, buf) {
        match cluster.try_add_slots(&slots) {
          Ok(_) => buf.write_ok(),
          Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
        }
      }
    } else if sub.eq_ignore_ascii_case("DELSLOTS") {
      if sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'cluster delslots'");
        return;
      }
      if let Some(slots) = Self::parse_cluster_slots(sub_args, buf) {
        match cluster.try_remove_slots(&slots) {
          Ok(_) => buf.write_ok(),
          Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
        }
      }
    } else if sub.eq_ignore_ascii_case("REPLICATE") {
      if sub_args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'cluster replicate'");
        return;
      }
      let Some(pri_id) = from_utf8(sub_args[0]).ok() else {
        buf.write_error(b"ERR Invalid node ID specified");
        return;
      };
      match cluster.try_replicaof(pri_id) {
        Ok(_) => buf.write_ok(),
        Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
      }
    } else if sub.eq_ignore_ascii_case("RESET") {
      let soft = if let Some(mode) = sub_args.first().and_then(|b| from_utf8(b).ok()) {
        if mode.eq_ignore_ascii_case("HARD") {
          false
        } else if mode.eq_ignore_ascii_case("SOFT") {
          true
        } else {
          buf.write_error(b"ERR Invalid CLUSTER RESET type");
          return;
        }
      } else {
        true
      };
      match cluster.try_reset(soft, false) {
        Ok(_) => buf.write_ok(),
        Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
      }
    } else if sub.eq_ignore_ascii_case("BUMPEPOCH") {
      cluster.try_bump_cluster_epoch();
      buf.write_ok();
    } else if sub.eq_ignore_ascii_case("SETSLOT") {
      if sub_args.len() < 2 {
        buf.write_error(b"ERR wrong number of arguments for 'cluster setslot'");
        return;
      }
      let Some(slot) = from_utf8(sub_args[0])
        .ok()
        .and_then(|s| s.parse::<u16>().ok())
      else {
        buf.write_error(b"ERR Invalid slot ID");
        return;
      };
      let sub_op = from_utf8(sub_args[1]).unwrap_or("");
      if sub_op.eq_ignore_ascii_case("MIGRATING") {
        if sub_args.len() < 3 {
          buf.write_error(b"ERR wrong number of arguments for 'cluster setslot migrating'");
          return;
        }
        let target_id = from_utf8(sub_args[2]).unwrap_or("");
        match cluster.try_prepare_slot_for_migration(slot, target_id) {
          Ok(_) => buf.write_ok(),
          Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
        }
      } else if sub_op.eq_ignore_ascii_case("IMPORTING") {
        if sub_args.len() < 3 {
          buf.write_error(b"ERR wrong number of arguments for 'cluster setslot importing'");
          return;
        }
        let source_id = from_utf8(sub_args[2]).unwrap_or("");
        match cluster.try_prepare_slot_for_import(slot, source_id) {
          Ok(_) => buf.write_ok(),
          Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
        }
      } else if sub_op.eq_ignore_ascii_case("NODE") {
        if sub_args.len() < 3 {
          buf.write_error(b"ERR wrong number of arguments for 'cluster setslot node'");
          return;
        }
        let new_owner = from_utf8(sub_args[2]).unwrap_or("");
        match cluster.try_prepare_slot_for_ownership_change(slot, new_owner) {
          Ok(_) => buf.write_ok(),
          Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
        }
      } else if sub_op.eq_ignore_ascii_case("STABLE") {
        match cluster.try_prepare_slot_for_stable(slot) {
          Ok(_) => buf.write_ok(),
          Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
        }
      } else {
        buf.write_error(b"ERR Invalid CLUSTER SETSLOT action or number of arguments");
      }
    } else {
      buf.write_ok();
    }
  }

  /// 格式化并写入 INFO 统计报告
  fn write_info(ctx: &Arc<ServerContext>, section: &str, buf: &mut SendBuffer) {
    let uptime = ctx.start_time.elapsed().as_secs();
    let is_repl = ctx.repl.is_replica();
    let role = if is_repl { "slave" } else { "master" };
    let history = ctx.repl.history.read();
    let repl_offset = history.replication_offset;
    let repl_id = history.primary_replid;
    let cluster_enabled = if ctx.cluster.is_some() { 1 } else { 0 };
    let connected_clients = ctx.active_connections().max(1);
    let version = crate::WedbServer::version();

    let mut info_buf = String::with_capacity(512);
    if section.eq_ignore_ascii_case("replication") {
      let _ = write!(
        info_buf,
        "# Replication\r\nrole:{}\r\nconnected_slaves:0\r\nmaster_replid:{}\r\nmaster_repl_offset:{}\r\nsecond_repl_offset:-1\r\nrepl_backlog_active:0\r\n",
        role,
        repl_id.as_str(),
        repl_offset
      );
    } else if section.eq_ignore_ascii_case("cluster") {
      let _ = write!(
        info_buf,
        "# Cluster\r\ncluster_enabled:{}\r\n",
        cluster_enabled
      );
    } else if section.eq_ignore_ascii_case("clients") {
      let _ = write!(
        info_buf,
        "# Clients\r\nconnected_clients:{}\r\n",
        connected_clients
      );
    } else if section.eq_ignore_ascii_case("server") {
      let _ = write!(
        info_buf,
        "# Server\r\nwedb_version:{version}\r\ngarnet_version:7.4.3\r\ntcp_port:{}\r\nuptime_in_seconds:{}\r\n",
        ctx.args.port, uptime
      );
    } else {
      let _ = write!(
        info_buf,
        "# Server\r\nwedb_version:{version}\r\ngarnet_version:7.4.3\r\ntcp_port:{}\r\nuptime_in_seconds:{}\r\n# Clients\r\nconnected_clients:{}\r\n# Replication\r\nrole:{}\r\nconnected_slaves:0\r\nmaster_replid:{}\r\nmaster_repl_offset:{}\r\n# Cluster\r\ncluster_enabled:{}\r\n",
        ctx.args.port,
        uptime,
        connected_clients,
        role,
        repl_id.as_str(),
        repl_offset,
        cluster_enabled
      );
    }
    buf.write_bulk_string(info_buf.as_bytes());
  }
}

/// 事务排队前的语法与参数合法性预校验
fn validate_command_syntax(
  cmd: RespCommand,
  args: &SessionParseState<'_>,
) -> result::Result<(), &'static str> {
  match cmd {
    RespCommand::DBSIZE
    | RespCommand::TIME
    | RespCommand::MULTI
    | RespCommand::EXEC
    | RespCommand::DISCARD
    | RespCommand::ASKING
    | RespCommand::READONLY
    | RespCommand::READWRITE
    | RespCommand::UNWATCH => {
      if !args.is_empty() {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::PING => {
      if args.len() > 1 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::ECHO | RespCommand::SELECT => {
      if args.len() != 1 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::GET
    | RespCommand::INCR
    | RespCommand::DECR
    | RespCommand::GETDEL
    | RespCommand::TTL
    | RespCommand::PTTL
    | RespCommand::PERSIST
    | RespCommand::EXPIRETIME
    | RespCommand::PEXPIRETIME
    | RespCommand::KEYS
    | RespCommand::TYPE
    | RespCommand::STRLEN
    | RespCommand::HLEN
    | RespCommand::HGETALL
    | RespCommand::HKEYS
    | RespCommand::HVALS
    | RespCommand::LLEN
    | RespCommand::SCARD
    | RespCommand::SMEMBERS
    | RespCommand::ZCARD => {
      if args.len() != 1 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::HGET
    | RespCommand::HEXISTS
    | RespCommand::INCRBY
    | RespCommand::DECRBY
    | RespCommand::INCRBYFLOAT
    | RespCommand::GETSET
    | RespCommand::SETNX
    | RespCommand::ZSCORE
    | RespCommand::PUBLISH => {
      if args.len() != 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::WATCH => {
      if args.is_empty() {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::REPLICAOF => {
      if args.len() != 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::BLPOP | RespCommand::BRPOP => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::BLMOVE => {
      if args.len() != 5 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::BRPOPLPUSH => {
      if args.len() != 3 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::MIGRATE => {
      if args.len() < 5 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::CLUSTER_MIGRATE => {
      if args.len() < 4 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::SET => {
      if args.len() < 2 {
        return Err("ERR wrong number of arguments for 'set' command");
      }
      // 选项尾缀与执行路径共用同一解析器（EX/PX/EXAT/PXAT/KEEPTTL/NX/XX/GET）
      parse_set_options(&args[2..])?;
    }
    RespCommand::SUBSCRIBE | RespCommand::Psubscribe | RespCommand::Pubsub | RespCommand::Scan => {
      if args.is_empty() {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
      if cmd == RespCommand::Scan && args.len() % 2 != 1 {
        return Err(consts::err::SYNTAX_STR);
      }
    }
    RespCommand::DEL
    | RespCommand::EXISTS
    | RespCommand::MGET
    | RespCommand::UNLINK
    | RespCommand::GETEX => {
      if args.is_empty() {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::EXPIRE | RespCommand::PEXPIRE | RespCommand::EXPIREAT | RespCommand::PEXPIREAT => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
      // Redis 口径：先校验时间量为整数，再校验选项令牌（仅允许恰好一个 NX/XX/GT/LT，
      // 多 flag 与多余参数均折算为 syntax error，无需独立 arity 上限判断）
      if ParseUtils::try_read_long(args[1]).is_none() {
        return Err(consts::err::INT_OUT_OF_RANGE_STR);
      }
      if parse_expire_options(&args[2..]).is_err() {
        return Err(consts::err::SYNTAX_STR);
      }
    }
    RespCommand::RENAME
    | RespCommand::RENAMENX
    | RespCommand::APPEND
    | RespCommand::GETBIT
    | RespCommand::LINDEX
    | RespCommand::RPOPLPUSH
    | RespCommand::ZRANK
    | RespCommand::ZREVRANK => {
      if args.len() != 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::MSET | RespCommand::MSETNX => {
      if args.len() < 2 || !args.len().is_multiple_of(2) {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::SETEX | RespCommand::PSETEX => {
      if args.len() != 3 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
      if ParseUtils::try_read_long(args[1]).is_none_or(|t| t <= 0) {
        return Err(if cmd == RespCommand::PSETEX {
          "ERR invalid expire time in 'psetex' command"
        } else {
          "ERR invalid expire time in 'setex' command"
        });
      }
    }
    RespCommand::GETRANGE
    | RespCommand::SETRANGE
    | RespCommand::SETBIT
    | RespCommand::SUBSTR
    | RespCommand::HINCRBY
    | RespCommand::HINCRBYFLOAT
    | RespCommand::HSETNX
    | RespCommand::LRANGE
    | RespCommand::LTRIM
    | RespCommand::LREM
    | RespCommand::LSET
    | RespCommand::SMOVE
    | RespCommand::ZINCRBY
    | RespCommand::ZCOUNT
    | RespCommand::ZREMRANGEBYRANK
    | RespCommand::ZREMRANGEBYSCORE
    | RespCommand::ZREMRANGEBYLEX
    | RespCommand::ZLEXCOUNT => {
      if args.len() != 3 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::LINSERT | RespCommand::LMOVE => {
      if args.len() != 4 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::LPUSHX | RespCommand::RPUSHX | RespCommand::LPOS => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::LMPOP => {
      if args.len() < 3 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::HSET => {
      if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
        return Err("ERR wrong number of arguments for 'hset' command");
      }
    }
    RespCommand::HMSET => {
      if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
        return Err("ERR wrong number of arguments for 'hmset' command");
      }
    }
    RespCommand::HMGET | RespCommand::ZREM | RespCommand::ZMSCORE => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::BITCOUNT => {
      if args.len() != 1 && args.len() != 3 {
        return Err(consts::err::SYNTAX_STR);
      }
    }
    RespCommand::BITPOS => {
      if args.len() < 2 || args.len() > 5 {
        return Err("ERR wrong number of arguments for 'bitpos' command");
      }
    }
    RespCommand::BITOP => {
      if args.len() < 3 {
        return Err("ERR wrong number of arguments for 'bitop' command");
      }
    }
    RespCommand::BitopAnd
    | RespCommand::BitopOr
    | RespCommand::BitopXor
    | RespCommand::BitopNot
    | RespCommand::BitopDiff => {
      if args.len() < 2 {
        return Err("ERR wrong number of arguments for 'bitop' command");
      }
    }
    RespCommand::ROLE | RespCommand::SAVE => {
      if !args.is_empty() {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::LASTSAVE => {
      if args.len() > 1 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::BGSAVE => {
      if args.len() > 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::HELLO => {
      if args.len() > 6 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::PFADD => {
      if args.is_empty() {
        return Err("ERR wrong number of arguments for 'pfadd' command");
      }
    }
    RespCommand::PFCOUNT => {
      if args.is_empty() {
        return Err("ERR wrong number of arguments for 'pfcount' command");
      }
    }
    RespCommand::PFMERGE => {
      if args.is_empty() {
        return Err("ERR wrong number of arguments for 'pfmerge' command");
      }
    }
    RespCommand::SPOP | RespCommand::ZPOPMIN | RespCommand::ZPOPMAX => {
      if args.is_empty() || args.len() > 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::SINTER | RespCommand::SUNION | RespCommand::SDIFF => {
      if args.is_empty() {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::SINTERSTORE | RespCommand::SUNIONSTORE | RespCommand::SDIFFSTORE => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::SINTERCARD => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::SRANDMEMBER => {
      if args.is_empty() || args.len() > 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::ZRANDMEMBER => {
      if args.is_empty() || args.len() > 3 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::ZUNION | RespCommand::ZINTER | RespCommand::ZDIFF => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::ZUNIONSTORE | RespCommand::ZINTERSTORE | RespCommand::ZDIFFSTORE => {
      if args.len() < 3 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::ZINTERCARD => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::ZMPOP => {
      if args.len() < 3 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::BZPOPMIN | RespCommand::BZPOPMAX => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::BZMPOP => {
      if args.len() < 4 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::ZRANGESTORE => {
      if args.len() < 4 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::HDEL
    | RespCommand::LPUSH
    | RespCommand::RPUSH
    | RespCommand::SADD
    | RespCommand::SREM
    | RespCommand::SISMEMBER => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::RPOP | RespCommand::LPOP => {
      if args.is_empty() || args.len() > 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
      if args.len() > 1 {
        match ParseUtils::try_read_int(args[1]) {
          Some(c) if c < 0 => return Err("ERR value is out of range, must be positive"),
          Some(_) => {}
          None => return Err(consts::err::INT_OUT_OF_RANGE_STR),
        }
      }
    }
    RespCommand::ZADD => {
      // 选项感知校验：前导 NX/XX/GT/LT/CH/INCR 之后必须紧跟偶数个 score-member 对
      let (_, pairs_at) = match parse_zadd_options(args) {
        Ok(parsed) => parsed,
        Err(_) => return Err(consts::err::SYNTAX_STR),
      };
      let pairs = &args[pairs_at..];
      if pairs.is_empty() || !pairs.len().is_multiple_of(2) {
        return Err(consts::err::SYNTAX_STR);
      }
      for chunk in pairs.as_chunks::<2>().0 {
        if ParseUtils::try_read_double(chunk[0], false).is_none() {
          return Err("ERR value is not a valid float");
        }
      }
    }
    RespCommand::ZRANGE => {
      if args.len() < 3 {
        return Err("ERR wrong number of arguments for 'zrange' command");
      }
      if ParseUtils::try_read_int(args[1]).is_none() || ParseUtils::try_read_int(args[2]).is_none()
      {
        return Err(consts::err::INT_OUT_OF_RANGE_STR);
      }
    }
    RespCommand::HSCAN
    | RespCommand::SSCAN
    | RespCommand::ZSCAN
    | RespCommand::SMISMEMBER
    | RespCommand::HTTL
    | RespCommand::HPERSIST => {
      if args.len() < 2 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::HEXPIRE | RespCommand::HEXPIREAT if args.len() < 3 => {
      return Err(consts::err::WRONG_NUM_ARGS_STR);
    }
    RespCommand::Ricreate if args.is_empty() => {
      return Err("ERR wrong number of arguments for 'ri.create' command");
    }
    RespCommand::Riset if args.len() != 3 => {
      return Err("ERR wrong number of arguments for 'ri.set' command");
    }
    RespCommand::Riget if args.len() != 2 => {
      return Err("ERR wrong number of arguments for 'ri.get' command");
    }
    RespCommand::Vadd if args.len() < 4 => {
      return Err("ERR wrong number of arguments for 'vadd' command");
    }
    RespCommand::Vsim if args.len() < 3 => {
      return Err("ERR wrong number of arguments for 'vsim' command");
    }
    RespCommand::Vcard | RespCommand::Vdim | RespCommand::Vinfo if args.len() != 1 => {
      return Err("ERR wrong number of arguments");
    }
    RespCommand::Vemb
    | RespCommand::Vrem
    | RespCommand::Vismember
    | RespCommand::Vgetattr
    | RespCommand::Vlinks
      if args.len() < 2 =>
    {
      return Err("ERR wrong number of arguments");
    }
    RespCommand::Vsetattr if args.len() != 3 => {
      return Err("ERR wrong number of arguments for 'vsetattr' command");
    }
    RespCommand::Vrandmember if args.is_empty() || args.len() > 2 => {
      return Err("ERR wrong number of arguments for 'vrandmember' command");
    }
    RespCommand::Eval if args.len() < 2 => {
      return Err("ERR wrong number of arguments for 'eval' command");
    }
    RespCommand::Evalsha if args.len() < 2 => {
      return Err("ERR wrong number of arguments for 'evalsha' command");
    }
    RespCommand::Script if args.is_empty() => {
      return Err("ERR wrong number of arguments for 'script' command");
    }
    RespCommand::Ridel if args.len() != 2 => {
      return Err("ERR wrong number of arguments for 'ri.del' command");
    }
    RespCommand::Riscan if args.len() < 4 => {
      return Err("ERR wrong number of arguments for 'ri.scan' command");
    }
    RespCommand::GEOADD => {
      if args.len() < 4 {
        return Err("ERR wrong number of arguments for 'geoadd' command");
      }
    }
    RespCommand::GEODIST => {
      if args.len() < 3 || args.len() > 4 {
        return Err("ERR wrong number of arguments for 'geodist' command");
      }
    }
    RespCommand::GEOPOS => {
      if args.len() < 2 {
        return Err("ERR wrong number of arguments for 'geopos' command");
      }
    }
    RespCommand::GEOHASH => {
      if args.len() < 2 {
        return Err("ERR wrong number of arguments for 'geohash' command");
      }
    }
    RespCommand::GEORADIUS | RespCommand::GEORADIUS_RO => {
      if args.len() < 5 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::GEORADIUSBYMEMBER | RespCommand::GEORADIUSBYMEMBER_RO => {
      if args.len() < 4 {
        return Err(consts::err::WRONG_NUM_ARGS_STR);
      }
    }
    RespCommand::GEOSEARCH => {
      if args.len() < 6 {
        return Err("ERR wrong number of arguments for 'geosearch' command");
      }
    }
    RespCommand::GEOSEARCHSTORE => {
      if args.len() < 7 {
        return Err("ERR wrong number of arguments for 'geosearchstore' command");
      }
    }
    RespCommand::Rirange if args.len() < 3 => {
      return Err("ERR wrong number of arguments for 'ri.range' command");
    }
    RespCommand::Riexists if args.len() != 1 => {
      return Err("ERR wrong number of arguments for 'ri.exists' command");
    }
    RespCommand::Riconfig if args.len() != 1 => {
      return Err("ERR wrong number of arguments for 'ri.config' command");
    }
    RespCommand::Rimetrics if args.len() != 1 => {
      return Err("ERR wrong number of arguments for 'ri.metrics' command");
    }
    _ => {}
  }
  Ok(())
}

/// 判断该命令在事务内是否构成数据库切换（目标库与当前活跃库不同则视为切换）
///
/// SELECT 目标与当前库相同为无操作放行；SWAPDB 必然交换库一律拦截
fn is_db_switch(cmd: RespCommand, args: &SessionParseState<'_>, active_db: u64) -> bool {
  match cmd {
    RespCommand::SELECT => {
      args.len() == 1 && ParseUtils::try_read_ulong(args[0]).is_some_and(|db| db != active_db)
    }
    RespCommand::Swapdb => true,
    _ => false,
  }
}

/// 提取命令所涉及的所有主键（零堆分配，支持单键与多键切片借用）
///
/// 注：RI* 系列仅按首参数索引名单键参与事务两阶段锁（extract_keys 全部归入
/// `Single(args[0])`）；其 field 成员内嵌于范围索引树内部 (key_id, field)，
/// 并非独立存储键，无需按成员复核加锁。
fn extract_keys<'a>(cmd: RespCommand, args: &'a SessionParseState<'_>) -> CommandKeys<'a> {
  if args.is_empty() {
    return CommandKeys::None;
  }
  match cmd {
    RespCommand::DEL | RespCommand::EXISTS | RespCommand::MGET | RespCommand::UNLINK => {
      if args.len() == 1 {
        CommandKeys::Single(args[0])
      } else {
        CommandKeys::Slice(args.as_slice())
      }
    }
    RespCommand::MIGRATE => {
      if args.len() >= 3 && !args[2].is_empty() {
        CommandKeys::Single(args[2])
      } else {
        CommandKeys::None
      }
    }
    RespCommand::RENAME | RespCommand::RENAMENX => {
      if args.len() >= 2 {
        CommandKeys::Slice(&args[..2])
      } else {
        CommandKeys::None
      }
    }
    RespCommand::BLMOVE
    | RespCommand::BRPOPLPUSH
    | RespCommand::ZRANGESTORE
    | RespCommand::GEOSEARCHSTORE => {
      if args.len() >= 2 {
        CommandKeys::Slice(&args[..2])
      } else {
        CommandKeys::None
      }
    }
    RespCommand::GEORADIUS | RespCommand::GEORADIUSBYMEMBER => {
      let mut store_key = None;
      for chunk in args.windows(2) {
        if chunk[0].eq_ignore_ascii_case(b"STORE") || chunk[0].eq_ignore_ascii_case(b"STOREDIST") {
          store_key = Some(chunk[1]);
        }
      }
      if let Some(target) = store_key {
        let mut arr = [b"".as_slice(); 8];
        arr[0] = args[0];
        arr[1] = target;
        CommandKeys::Small(arr, 2)
      } else {
        CommandKeys::Single(args[0])
      }
    }
    RespCommand::BLPOP | RespCommand::BRPOP | RespCommand::BZPOPMIN | RespCommand::BZPOPMAX => {
      if args.len() > 1 {
        CommandKeys::Slice(&args[..args.len() - 1])
      } else {
        CommandKeys::None
      }
    }
    RespCommand::SMOVE => {
      if args.len() >= 2 {
        CommandKeys::Slice(&args[..2])
      } else {
        CommandKeys::None
      }
    }
    RespCommand::SINTER
    | RespCommand::SUNION
    | RespCommand::SDIFF
    | RespCommand::SINTERSTORE
    | RespCommand::SUNIONSTORE
    | RespCommand::SDIFFSTORE
    | RespCommand::ZUNION
    | RespCommand::ZINTER
    | RespCommand::ZDIFF
    | RespCommand::ZUNIONSTORE
    | RespCommand::ZINTERSTORE
    | RespCommand::ZDIFFSTORE => CommandKeys::Slice(args.as_slice()),
    RespCommand::MSET | RespCommand::MSETNX => {
      if args.len() >= 2 {
        let key_count = args.len() / 2;
        if key_count <= 8 {
          let mut arr: [&'a [u8]; 8] = [&[]; 8];
          for (i, k) in args.iter().step_by(2).take(key_count).enumerate() {
            arr[i] = *k;
          }
          CommandKeys::Small(arr, key_count)
        } else {
          let keys: Vec<&'a [u8]> = args.iter().step_by(2).copied().collect();
          CommandKeys::Vec(keys)
        }
      } else {
        CommandKeys::None
      }
    }
    RespCommand::GET
    | RespCommand::SET
    | RespCommand::INCR
    | RespCommand::DECR
    | RespCommand::INCRBY
    | RespCommand::DECRBY
    | RespCommand::INCRBYFLOAT
    | RespCommand::GETSET
    | RespCommand::GETDEL
    | RespCommand::GETEX
    | RespCommand::SETNX
    | RespCommand::SETEX
    | RespCommand::PSETEX
    | RespCommand::SUBSTR
    | RespCommand::EXPIRE
    | RespCommand::TTL
    | RespCommand::PTTL
    | RespCommand::PERSIST
    | RespCommand::EXPIREAT
    | RespCommand::PEXPIREAT
    | RespCommand::PEXPIRE
    | RespCommand::EXPIRETIME
    | RespCommand::PEXPIRETIME
    | RespCommand::TYPE
    | RespCommand::APPEND
    | RespCommand::STRLEN
    | RespCommand::GETRANGE
    | RespCommand::SETRANGE
    | RespCommand::SETBIT
    | RespCommand::GETBIT
    | RespCommand::BITCOUNT
    | RespCommand::BITPOS
    | RespCommand::PFADD
    | RespCommand::HSET
    | RespCommand::HGET
    | RespCommand::HDEL
    | RespCommand::HLEN
    | RespCommand::HGETALL
    | RespCommand::HKEYS
    | RespCommand::HVALS
    | RespCommand::HEXISTS
    | RespCommand::HINCRBY
    | RespCommand::HINCRBYFLOAT
    | RespCommand::HMGET
    | RespCommand::HMSET
    | RespCommand::HSETNX
    | RespCommand::HSCAN
    | RespCommand::HEXPIRE
    | RespCommand::HEXPIREAT
    | RespCommand::HTTL
    | RespCommand::HPERSIST
    | RespCommand::LPUSH
    | RespCommand::RPUSH
    | RespCommand::LPOP
    | RespCommand::RPOP
    | RespCommand::LLEN
    | RespCommand::LRANGE
    | RespCommand::LINDEX
    | RespCommand::LTRIM
    | RespCommand::SADD
    | RespCommand::SREM
    | RespCommand::SCARD
    | RespCommand::SISMEMBER
    | RespCommand::SMEMBERS
    | RespCommand::SPOP
    | RespCommand::SMISMEMBER
    | RespCommand::SSCAN
    | RespCommand::SRANDMEMBER
    | RespCommand::ZADD
    | RespCommand::ZRANGE
    | RespCommand::ZCARD
    | RespCommand::ZSCORE
    | RespCommand::ZINCRBY
    | RespCommand::ZCOUNT
    | RespCommand::ZREM
    | RespCommand::ZRANK
    | RespCommand::ZREVRANK
    | RespCommand::ZSCAN
    | RespCommand::ZPOPMIN
    | RespCommand::ZPOPMAX
    | RespCommand::ZMSCORE
    | RespCommand::ZRANDMEMBER
    | RespCommand::ZREVRANGE
    | RespCommand::ZRANGEBYSCORE
    | RespCommand::ZREVRANGEBYSCORE
    | RespCommand::ZRANGEBYLEX
    | RespCommand::ZREVRANGEBYLEX
    | RespCommand::ZREMRANGEBYRANK
    | RespCommand::ZREMRANGEBYSCORE
    | RespCommand::ZREMRANGEBYLEX
    | RespCommand::ZLEXCOUNT
    | RespCommand::Ricreate
    | RespCommand::Riset
    | RespCommand::Riget
    | RespCommand::Ridel
    | RespCommand::Riscan
    | RespCommand::Rirange
    | RespCommand::Riexists
    | RespCommand::Riconfig
    | RespCommand::Rimetrics
    | RespCommand::GEOADD
    | RespCommand::GEODIST
    | RespCommand::GEOPOS
    | RespCommand::GEOHASH
    | RespCommand::GEOSEARCH
    | RespCommand::GEORADIUS_RO
    | RespCommand::GEORADIUSBYMEMBER_RO => CommandKeys::Single(args[0]),
    RespCommand::BITOP if args.len() >= 2 => CommandKeys::Slice(&args[1..]),
    RespCommand::BitopAnd
    | RespCommand::BitopOr
    | RespCommand::BitopXor
    | RespCommand::BitopNot
    | RespCommand::BitopDiff
    | RespCommand::PFCOUNT
    | RespCommand::PFMERGE => CommandKeys::Slice(args.as_slice()),
    _ => CommandKeys::None,
  }
}

/// 判断是否为只读命令
fn is_read_command(cmd: RespCommand) -> bool {
  matches!(
    cmd,
    RespCommand::GET
      | RespCommand::Vsim
      | RespCommand::Vcard
      | RespCommand::Vdim
      | RespCommand::Vemb
      | RespCommand::Vgetattr
      | RespCommand::Vismember
      | RespCommand::Vinfo
      | RespCommand::Vlinks
      | RespCommand::Vrandmember
      | RespCommand::EXISTS
      | RespCommand::TTL
      | RespCommand::PTTL
      | RespCommand::EXPIRETIME
      | RespCommand::PEXPIRETIME
      | RespCommand::MGET
      | RespCommand::KEYS
      | RespCommand::TIME
      | RespCommand::SUBSTR
      | RespCommand::DBSIZE
      | RespCommand::TYPE
      | RespCommand::STRLEN
      | RespCommand::GETRANGE
      | RespCommand::GETBIT
      | RespCommand::BITCOUNT
      | RespCommand::BITPOS
      | RespCommand::PFCOUNT
      | RespCommand::LASTSAVE
      | RespCommand::ROLE
      | RespCommand::HELLO
      | RespCommand::HGET
      | RespCommand::HLEN
      | RespCommand::HGETALL
      | RespCommand::HKEYS
      | RespCommand::HVALS
      | RespCommand::HEXISTS
      | RespCommand::HMGET
      | RespCommand::HSCAN
      | RespCommand::HTTL
      | RespCommand::LLEN
      | RespCommand::LRANGE
      | RespCommand::LINDEX
      | RespCommand::SCARD
      | RespCommand::SISMEMBER
      | RespCommand::SMEMBERS
      | RespCommand::SMISMEMBER
      | RespCommand::SSCAN
      | RespCommand::SINTER
      | RespCommand::SINTERCARD
      | RespCommand::SUNION
      | RespCommand::SDIFF
      | RespCommand::SRANDMEMBER
      | RespCommand::LPOS
      | RespCommand::ZCARD
      | RespCommand::ZSCORE
      | RespCommand::ZRANGE
      | RespCommand::ZCOUNT
      | RespCommand::ZRANK
      | RespCommand::ZREVRANK
      | RespCommand::ZSCAN
      | RespCommand::ZMSCORE
      | RespCommand::ZRANDMEMBER
      | RespCommand::ZREVRANGE
      | RespCommand::ZRANGEBYSCORE
      | RespCommand::ZREVRANGEBYSCORE
      | RespCommand::ZRANGEBYLEX
      | RespCommand::ZREVRANGEBYLEX
      | RespCommand::ZLEXCOUNT
      | RespCommand::ZUNION
      | RespCommand::ZINTER
      | RespCommand::ZDIFF
      | RespCommand::ZINTERCARD
      | RespCommand::GEODIST
      | RespCommand::GEOPOS
      | RespCommand::GEOHASH
      | RespCommand::GEOSEARCH
      | RespCommand::GEORADIUS_RO
      | RespCommand::GEORADIUSBYMEMBER_RO
      | RespCommand::Riget
      | RespCommand::Riscan
      | RespCommand::Rirange
      | RespCommand::Riexists
      | RespCommand::Riconfig
      | RespCommand::Rimetrics
  )
}
