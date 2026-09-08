//! Luau 值与 RESP 回复值的相互转换 (对标 Redis 脚本回复转换规则)

use luau::{MultiValue, Table, Value};
use sha1::{Digest, Sha1};
use wedb_module::RespValue;

use crate::error::{Error, Result};

/// Lua 表转 RESP 的最大递归深度（防自引用/深嵌套表递归打爆调用栈）
const MAX_NESTING: usize = 32;

/// 不支持的脚本返回值错误提示
const ERR_UNSUPPORTED_RETURN: &str = "ERR unsupported script return value";

/// 表嵌套深度超出限制错误提示
const ERR_NESTING_DEPTH_EXCEEDED: &str = "ERR lua table nesting depth exceeded";

/// SHA-1 摘要字节长度 (20 字节)
const SHA1_DIGEST_LEN: usize = 20;

/// SHA-1 十六进制字符串长度 (40 字符)
const SHA1_HEX_LEN: usize = SHA1_DIGEST_LEN * 2;

/// 十六进制字符查找表
const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

/// RESP 回复值 -> Luau 值。
///
/// Status 变为 `{ok=...}` 表，Error 由调用方决定抛错还是变 `{err=...}` 表
/// （`call` 抛错，`pcall` 变表）；NULL 批量字符串变 `false`（对标 Redis
/// lua-api 的 RESP2->Lua 转换，见 Garnet `ProcessSingleRespTerm`）。
pub fn resp_to_lua<'lua>(
  lua: luau::LuaRef<'lua>,
  v: RespValue,
  raise_error: bool,
) -> Result<Value<'lua>> {
  Ok(match v {
    RespValue::Status(s) => {
      let t = lua.create_table_with_capacity(0, 1)?;
      t.raw_set("ok", lua.create_string(&s)?)?;
      Value::Table(t)
    }
    RespValue::Error(e) => {
      if raise_error {
        return Err(Error::Reply(String::from_utf8_lossy(&e).into_owned()));
      }
      let t = lua.create_table_with_capacity(0, 1)?;
      t.raw_set("err", lua.create_string(&e)?)?;
      Value::Table(t)
    }
    RespValue::Integer(n) => Value::Integer(n),
    RespValue::Bulk(Some(d)) => Value::String(lua.create_string(&d)?),
    RespValue::Bulk(None) => Value::Boolean(false),
    RespValue::Array(items) => {
      let t = lua.create_table_with_capacity(items.len(), 0)?;
      for (i, item) in items.into_iter().enumerate() {
        t.raw_seti(i + 1, resp_to_lua(lua, item, raise_error)?)?;
      }
      Value::Table(t)
    }
  })
}

/// Luau 值 -> RESP 回复值 (对标 Redis 脚本返回值转换规则)：
/// 整数/浮点 -> 整数，字符串 -> 批量字符串，true -> 1，false/nil -> NULL，
/// `{ok=...}` -> 状态回复，`{err=...}` -> 错误回复，表 -> 数组（从下标 1 起连续取值）。
pub fn lua_to_resp(v: Value<'_>) -> Result<RespValue> {
  to_resp(v, 0)
}

/// 带 depth 的递归实现
fn to_resp(v: Value<'_>, depth: usize) -> Result<RespValue> {
  Ok(match v {
    Value::Nil => RespValue::Bulk(None),
    Value::Integer(n) => RespValue::Integer(n),
    Value::Boolean(true) => RespValue::Integer(1),
    Value::Boolean(false) => RespValue::Bulk(None),
    Value::Number(n) => RespValue::Integer(n as i64),
    Value::String(s) => RespValue::Bulk(Some(s.as_bytes().to_vec())),
    Value::Table(t) => table_to_resp(&t, depth)?,
    // 函数/UserData 等无法表示为回复
    _ => return Err(Error::Reply(ERR_UNSUPPORTED_RETURN.into())),
  })
}

/// 表转换：优先识别 ok/err 字段，否则按连续下标折叠为数组
fn table_to_resp(t: &Table<'_>, depth: usize) -> Result<RespValue> {
  if depth >= MAX_NESTING {
    return Err(Error::Reply(ERR_NESTING_DEPTH_EXCEEDED.into()));
  }
  for key in ["ok", "err"] {
    let v: Value<'_> = t.raw_get(key)?;
    if let Value::String(s) = v {
      return Ok(match key {
        "ok" => RespValue::Status(s.as_bytes().to_vec()),
        _ => RespValue::Error(s.as_bytes().to_vec()),
      });
    }
  }
  let len = t.raw_len();
  let mut items = Vec::with_capacity(len);
  for v in t.sequence_values::<Value<'_>>() {
    items.push(to_resp(v?, depth + 1)?);
  }
  Ok(RespValue::Array(items))
}

/// 将脚本返回的多个值折叠为单个回复（取第一个值，nil 折叠为 NULL 批量回复）
pub fn multi_to_resp(mut vals: MultiValue<'_>) -> Result<RespValue> {
  match vals.pop_front() {
    Some(v) => lua_to_resp(v),
    None => Ok(RespValue::Bulk(None)),
  }
}

/// 计算 SHA-1 并输出 40 位小写十六进制 (对标 redis.sha1hex / SCRIPT LOAD 返回键)
pub fn sha1_hex(data: &[u8]) -> String {
  let digest = Sha1::digest(data);
  let mut out = Vec::with_capacity(SHA1_HEX_LEN);
  for &b in digest.as_slice() {
    out.push(HEX_CHARS[(b >> 4) as usize]);
    out.push(HEX_CHARS[(b & 0x0f) as usize]);
  }
  // SAFETY: HEX_CHARS 均为合法 ASCII 字符，拼接后必为有效 UTF-8 字符串
  unsafe { String::from_utf8_unchecked(out) }
}
