//! ACL 用户复合主键编码：名字空间 × 用户名
//!
//! 对标 doc/zh/ns.md 多租户隔离规范（C# Garnet 无此概念，为 WeDB 刻意改造）：
//! - `None`（系统超管全局视界）编码为单字节前缀 `0x00`；
//! - `Some(n)`（租户沙箱）编码为 OPPV 保序变长前缀 `OPPV(n + 1)`（首字节恒 ≥ 0x01）。
//!
//! 两类前缀首字节空间互斥，且 OPPV 仅凭首字节即可自定界，编码对全 u64 值域封闭：
//! (名字空间, 用户名) → 字节键为全局单射，任意二进制安全的用户名
//! （含 `\x00`、冒号、空白等）都无法伪造跨名字空间歧义键（防注入穿透）。

use core::str::from_utf8;

use wrecord::NamespaceDbCodec;

use crate::error::{Error, Result};

/// 复合主键栈缓冲容量（超长用户名平滑回退堆分配）
const KEY_STACK_CAP: usize = 64;

/// 租户命名空间合法上限（与 NamespaceDbCodec::MAX_TENANT_NAMESPACE 一致）
pub const MAX_TENANT_NAMESPACE: u64 = NamespaceDbCodec::MAX_TENANT_NAMESPACE;

/// 超管全局视界前缀字节
pub const NS_NONE_PREFIX: u8 = 0x00;

/// 前缀占用字节数（None 恒 1 字节；Some(n) 为 OPPV 变长 1..=9 字节）
///
/// 饱和加法保证域外极端值（ns = u64::MAX）绝不回绕为 0：
/// 回绕 0 会编码出 `0x00` 首字节与超管 None 前缀碰撞（release 关闭溢出检查时静默越权），
/// 饱和则恒落在 OPPV 9 字节形式，首字节 0xFF，编码对全 u64 值域 panic-free 且与 None 互斥
#[inline]
const fn prefix_len(ns: Option<u64>) -> usize {
  match ns {
    None => 1,
    Some(n) => NamespaceDbCodec::varint_len(n.saturating_add(1)),
  }
}

/// 将名字空间前缀写入 dst 首部，返回写入字节数（饱和语义见 [`prefix_len`]）
#[inline]
fn write_prefix(ns: Option<u64>, dst: &mut [u8]) -> usize {
  match ns {
    None => {
      dst[0] = NS_NONE_PREFIX;
      1
    }
    Some(n) => NamespaceDbCodec::encode_varint(n.saturating_add(1), dst),
  }
}

/// 构造堆分配的复合主键：`[Ns前缀] + [用户名字节]`
#[inline]
pub fn user_key(ns: Option<u64>, name: &str) -> Vec<u8> {
  let bytes = name.as_bytes();
  let plen = prefix_len(ns);
  let mut key = Vec::with_capacity(plen + bytes.len());
  match ns {
    None => key.push(NS_NONE_PREFIX),
    Some(n) => {
      let mut buf = [0u8; 10];
      let len = NamespaceDbCodec::encode_varint(n.saturating_add(1), &mut buf);
      key.extend_from_slice(&buf[..len]);
    }
  }
  key.extend_from_slice(bytes);
  key
}

/// 构造复合主键并零拷贝交给闭包（≤ 63 字节键全程栈上零堆分配，热路径查找专用）
#[inline]
pub fn with_user_key<R>(ns: Option<u64>, name: &str, f: impl FnOnce(&[u8]) -> R) -> R {
  let bytes = name.as_bytes();
  let total = prefix_len(ns) + bytes.len();
  if total <= KEY_STACK_CAP {
    let mut buf = [0u8; KEY_STACK_CAP];
    let plen = write_prefix(ns, &mut buf);
    buf[plen..total].copy_from_slice(bytes);
    f(&buf[..total])
  } else {
    f(&user_key(ns, name))
  }
}

/// 从复合主键还原 (名字空间, 用户名)，非法编码返回 None
#[inline]
pub fn decode_user_key(key: &[u8]) -> Option<(Option<u64>, &str)> {
  let (&first, rest) = key.split_first()?;
  if first == NS_NONE_PREFIX {
    return Some((None, from_utf8(rest).ok()?));
  }
  let (raw, varint_len) = NamespaceDbCodec::decode_varint(key).ok()?;
  let name = from_utf8(key.get(varint_len..)?).ok()?;
  // Some(n) 存储 OPPV(n+1)，减 1 还原；raw ≥ 1 恒成立（0 已被 None 前缀独占）
  Some((Some(raw.checked_sub(1)?), name))
}

/// 从复合主键仅提取名字空间（跳过用户名 UTF-8 校验，零拷贝快查）
#[inline]
pub fn decode_ns(key: &[u8]) -> Option<Option<u64>> {
  let &first = key.first()?;
  if first == NS_NONE_PREFIX {
    return Some(None);
  }
  let (raw, _) = NamespaceDbCodec::decode_varint(key).ok()?;
  Some(Some(raw.checked_sub(1)?))
}

/// 检查复合主键的名字空间是否匹配目标（超管 None 前缀 0x00 极速 O(1) 判定）
#[inline]
pub fn matches_ns(key: &[u8], target_ns: Option<u64>) -> bool {
  match target_ns {
    None => key.first() == Some(&NS_NONE_PREFIX),
    Some(n) => decode_ns(key) == Some(Some(n)),
  }
}

/// 解析 ACL `ns` 规则值：`none`/`all` → 超管全局视界；`1..=MAX_TENANT_NAMESPACE` → 租户沙箱
///
/// `0` 为控制面自动分配保留值（由上层先经 NamespaceAllocator 分配再下发具体 n），解析层拒绝；
/// 数字须为**纯 ASCII 数字串**（`+1`/`-1`/空白等一律拒绝），超上限同样拒绝，
/// 杜绝跨过租户上限的键编码歧义。
#[inline]
pub fn parse_ns(val: &str) -> Result<Option<u64>> {
  if val.eq_ignore_ascii_case("none") || val.eq_ignore_ascii_case("all") {
    return Ok(None);
  }
  match parse_strict_u64(val) {
    Some(n) if (1..=MAX_TENANT_NAMESPACE).contains(&n) => Ok(Some(n)),
    _ => Err(Error::InvalidNamespace(val.to_string())),
  }
}

/// 校验用户名合法性：非空且不含 `#`
///
/// `#` 是 AUTH 凭据 `用户名#空间id` 语法的保留分隔符（见 [`parse_user_token`]），
/// 用户名层面禁用即可保证切分无歧义，无需任何转义规则
#[inline]
pub fn validate_username(name: &str) -> Result<()> {
  if name.is_empty() || name.contains('#') {
    return Err(Error::InvalidUsername(name.to_string()));
  }
  Ok(())
}

/// 解析 AUTH 凭据用户令牌：`username`（超管全局桶）或 `username#<空间id>`（指定租户沙箱）
///
/// 语义强约束（doc/zh/ns.md）：
/// - 无 `#`：仅在超管全局桶点查——超级用户（`ns none`）的登录方式，与既有
///   `AUTH default` 完全兼容；
/// - `username#<ns>`：`ns` 必须为数字空间 id（`1..=MAX_TENANT_NAMESPACE`），
///   绑定了名字空间的租户用户**只能**以该形式登录，点查对应沙箱；
/// - `0` 为控制面自动分配保留值；`none`/`all` 属 SETUSER 规则语法而非登录语法，
///   均拒绝；语法非法与口令错误（WRONGPASS）严格区分。
///
/// 凭据完整携带 `(空间, 用户名)` 二元身份，认证退化为单点查 O(1)，
/// 无需同名跨空间候选扫描；租户用户在全局桶不可见，杜绝省略 `#ns` 的越权尝试
#[inline]
pub fn parse_user_token(token: &str) -> Result<(&str, Option<u64>)> {
  match token.split_once('#') {
    None => {
      validate_username(token)?;
      Ok((token, None))
    }
    Some((name, ns_val)) => {
      validate_username(name)?;
      let ns = parse_tenant_id(ns_val)?;
      Ok((name, Some(ns)))
    }
  }
}

/// 严格十进制无符号解析：仅接受纯 ASCII 数字串
///
/// `u64::from_str` 会接受前导 `+`（`"+1"` 被当作 `1`），此处先行字符校验，
/// 从语法上杜绝 `#+1` / `ns +1` 等带符号形式的伪装等价；溢出同样拒绝
#[inline]
fn parse_strict_u64(val: &str) -> Option<u64> {
  if val.is_empty() || !val.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  val.parse().ok()
}

/// 解析纯数字租户空间 id（`1..=MAX_TENANT_NAMESPACE`，`0` 为控制面保留值）
#[inline]
fn parse_tenant_id(val: &str) -> Result<u64> {
  match parse_strict_u64(val) {
    Some(n) if (1..=MAX_TENANT_NAMESPACE).contains(&n) => Ok(n),
    _ => Err(Error::InvalidNamespace(val.to_string())),
  }
}

#[cfg(test)]
mod tests {
  use aok::{OK, Void};

  use super::{
    MAX_TENANT_NAMESPACE, NS_NONE_PREFIX, decode_ns, decode_user_key, matches_ns, parse_ns,
    parse_user_token, user_key, validate_username, with_user_key,
  };

  #[test]
  fn test_user_key_roundtrip() -> Void {
    // 超管 None 前缀
    let k = user_key(None, "alice");
    assert_eq!(k[0], NS_NONE_PREFIX);
    assert_eq!(decode_user_key(&k), Some((None, "alice")));

    // 租户 Some(n) 前缀（单字节 1..=127）
    let k = user_key(Some(5), "alice");
    assert_eq!(decode_user_key(&k), Some((Some(5), "alice")));

    // 多字节 OPPV 前缀（n=200 → 2 字节前缀 + 5 字节用户名）
    let k = user_key(Some(200), "alice");
    assert_eq!(k.len(), 7);
    assert_eq!(decode_user_key(&k), Some((Some(200), "alice")));

    // 超大 n（9 字节前缀 + 5 字节用户名）
    let k = user_key(Some(MAX_TENANT_NAMESPACE), "alice");
    assert_eq!(k.len(), 14);
    assert_eq!(
      decode_user_key(&k),
      Some((Some(MAX_TENANT_NAMESPACE), "alice"))
    );

    // 二进制安全用户名（含 \x00 与前缀字节同值字符不产生歧义）
    let k = user_key(Some(5), "\x005alice");
    assert_eq!(decode_user_key(&k), Some((Some(5), "\x005alice")));

    // 同名不同空间 → 键必然不同；Some(0) 与 None 前缀也不可碰撞（编码全值域单射）
    assert_ne!(user_key(None, "alice"), user_key(Some(1), "alice"));
    assert_ne!(user_key(Some(1), "alice"), user_key(Some(2), "alice"));
    assert_ne!(user_key(None, "alice"), user_key(Some(0), "alice"));
    assert_eq!(
      decode_user_key(&user_key(Some(0), "alice")),
      Some((Some(0), "alice"))
    );

    // 非法编码
    assert_eq!(decode_user_key(b""), None);
    assert_eq!(decode_user_key(&[0xF0, 0x01]), None);

    // 域外极端值 ns = u64::MAX：饱和加法不回绕（回绕 0 会与 None 前缀 0x00 碰撞），
    // 编码 panic-free 且首字节落入 OPPV 9 字节形式 0xFF，与超管全局桶绝对互斥
    let k = user_key(Some(u64::MAX), "alice");
    assert_eq!(k[0], 0xFF);
    assert_ne!(k, user_key(None, "alice"));
    assert_eq!(k, user_key(Some(u64::MAX - 1), "alice")); // 域外饱和共享同一极端键
    assert_eq!(decode_user_key(&k), Some((Some(u64::MAX - 1), "alice")));
    with_user_key(Some(u64::MAX), "alice", |buf| assert_eq!(buf, &k));

    OK
  }

  #[test]
  fn test_matches_and_decode_ns() -> Void {
    let k_none = user_key(None, "bob");
    assert_eq!(decode_ns(&k_none), Some(None));
    assert!(matches_ns(&k_none, None));
    assert!(!matches_ns(&k_none, Some(1)));

    let k_tenant = user_key(Some(42), "bob");
    assert_eq!(decode_ns(&k_tenant), Some(Some(42)));
    assert!(matches_ns(&k_tenant, Some(42)));
    assert!(!matches_ns(&k_tenant, None));
    assert!(!matches_ns(&k_tenant, Some(43)));

    assert_eq!(decode_ns(b""), None);
    assert!(!matches_ns(b"", None));
    assert!(!matches_ns(b"", Some(1)));
    OK
  }

  #[test]
  fn test_with_user_key_matches_user_key() -> Void {
    for ns in [None, Some(1), Some(127), Some(128), Some(u64::MAX - 1024)] {
      let expected = user_key(ns, "user:name");
      with_user_key(ns, "user:name", |k| assert_eq!(k, &expected));
    }
    // 超过 64 字节栈缓冲平滑回退堆分配测试
    let long_name = "x".repeat(128);
    for ns in [None, Some(1), Some(99999)] {
      let expected = user_key(ns, &long_name);
      with_user_key(ns, &long_name, |k| assert_eq!(k, &expected));
      assert_eq!(decode_user_key(&expected), Some((ns, long_name.as_str())));
    }
    OK
  }

  #[test]
  fn test_parse_ns() -> Void {
    assert_eq!(parse_ns("none").unwrap(), None);
    assert_eq!(parse_ns("ALL").unwrap(), None);
    assert_eq!(parse_ns("1").unwrap(), Some(1));
    assert_eq!(parse_ns("100").unwrap(), Some(100));
    assert!(parse_ns("0").is_err(), "0 为控制面自动分配保留值");
    assert!(parse_ns("-1").is_err());
    assert!(parse_ns("+1").is_err(), "带符号形式伪装等价 1，拒绝");
    assert!(parse_ns(" 1").is_err(), "含空白拒绝");
    assert!(parse_ns("").is_err());
    assert!(parse_ns("abc").is_err());
    // 超过租户上限
    assert!(parse_ns("18446744073709551615").is_err());
    assert_eq!(
      parse_ns("18446744073709550591").unwrap(),
      Some(MAX_TENANT_NAMESPACE)
    );
    OK
  }

  #[test]
  fn test_parse_user_token_and_validate_username() -> Void {
    // 无 # → 超管全局桶（向后兼容 AUTH default）；租户用户必须携带 #空间id
    assert_eq!(parse_user_token("alice").unwrap(), ("alice", None));

    // username#数字 → 租户沙箱
    assert_eq!(parse_user_token("alice#5").unwrap(), ("alice", Some(5)));
    assert_eq!(
      parse_user_token("alice#18446744073709550591").unwrap(),
      ("alice", Some(MAX_TENANT_NAMESPACE))
    );

    // none/all 属 SETUSER 规则语法，登录语法拒绝；空空间值同样拒绝
    assert!(parse_user_token("alice#none").is_err());
    assert!(parse_user_token("alice#ALL").is_err());
    assert!(parse_user_token("alice#").is_err());

    // 语法非法
    assert!(parse_user_token("#5").is_err(), "空用户名");
    assert!(parse_user_token("a#0").is_err(), "0 为控制面保留值");
    assert!(parse_user_token("a#b").is_err(), "非法空间值");
    assert!(
      parse_user_token("a#b#c").is_err(),
      "值段含 # 非纯数字即拒绝"
    );
    assert!(
      parse_user_token("a#+1").is_err(),
      "带符号 +1 伪装等价 1，拒绝"
    );
    assert!(parse_user_token("a#-1").is_err(), "负数拒绝");
    assert!(parse_user_token("a# 1").is_err(), "含空白拒绝");
    assert!(
      parse_user_token("a#99999999999999999999").is_err(),
      "u64 溢出拒绝"
    );

    // validate_username
    assert!(validate_username("alice").is_ok());
    assert!(validate_username("").is_err());
    assert!(validate_username("a#b").is_err());
    OK
  }
}
