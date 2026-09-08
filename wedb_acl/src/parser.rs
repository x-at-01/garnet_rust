use std::str::SplitWhitespace;

use wedb_resp::RespCommand;

use crate::{
  error::{Error, Result},
  ns::{parse_ns, validate_username},
  password::AclPassword,
  user::User,
};

/// 规则行 token 不足的统一错误文案（对标 C# "Malformed ACL rule"）
const ERR_TOO_SHORT: &str = "rule line too short";

/// `ns` 复合规则缺失绑定值的错误文案
const NS_MISSING_VALUE: &str = "ns rule requires a value";

/// 预扫描规则中的 `ns` 绑定值（重复出现取最后一条）
///
/// 返回 `(绑定值所在下标, 原始值切片)`；`None` 表示未指定绑定。
/// `pub` 供 wedb_server 控制面（ACL SETUSER 的 NS 0 自动分配改写）复用，
/// 杜绝多处手写同构扫描。
pub fn scan_ns_bind<'a>(rules: &[&'a str]) -> Result<Option<(usize, &'a str)>> {
  let mut bind = None;
  let mut it = rules.iter();
  let mut idx = 0usize;
  while let Some(rule) = it.next() {
    idx += 1;
    if rule.eq_ignore_ascii_case("ns") {
      let Some(val) = it.next() else {
        return Err(Error::InvalidRule(NS_MISSING_VALUE.to_string()));
      };
      let val_idx = idx;
      idx += 1;
      bind = Some((val_idx, *val));
    }
  }
  Ok(bind)
}

/// 校验自定义命令名称是否合法
///
/// 首字符必须为 ASCII 字母或数字；后续字符允许字母数字及 `.`、`_`、`-`、`|`
pub fn is_valid_custom_command_name(name: &str) -> bool {
  let bytes = name.as_bytes();
  let Some((&first, rest)) = bytes.split_first() else {
    return false;
  };
  first.is_ascii_alphanumeric()
    && rest
      .iter()
      .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'|'))
}

/// 校验解析结果可否用于 ACL 规则（对标 C# IsInvalidCommandToAcl）
///
/// NONE/INVALID 非真实命令；`normalize_for_acls() != self` 说明是别名归一命令
/// （如 setexnx、bitop_and），ACL 规则应拒绝并回退按自定义命令名注册
#[inline]
fn valid_cmd(cmd: RespCommand) -> Option<RespCommand> {
  if cmd != RespCommand::NONE && cmd != RespCommand::INVALID && cmd.normalize_for_acls() == cmd {
    Some(cmd)
  } else {
    None
  }
}

/// 命令查找表的最大可查名称字节长度（对齐 `RespCommand::lookup` 门限，超出必查不到）
const LOOKUP_NAME_MAX_LEN: usize = 24;

/// 解析单条命令或子命令名称（如 "set", "client|id", "ri.create" 等）
pub fn parse_command_name(name: &str) -> Result<RespCommand> {
  if name.is_empty() {
    return Err(Error::CommandDoesNotExist(name.to_string()));
  }

  let bytes = name.as_bytes();

  // 1. 检查是否包含子命令分隔符 '|'
  if let Some((parent, sub)) = name.split_once('|') {
    if let Some((parent_cmd, _)) = RespCommand::lookup(parent.as_bytes())
      && let Some(sub_cmd) = RespCommand::lookup_subcommand(parent_cmd, sub.as_bytes())
      && let Some(cmd) = valid_cmd(sub_cmd)
    {
      return Ok(cmd);
    }
    // 尝试将 '|' 替换为 '_' 再次查找（栈缓冲零堆分配；长度以查找表门限为界）
    if bytes.len() <= LOOKUP_NAME_MAX_LEN {
      let mut buf = [0u8; LOOKUP_NAME_MAX_LEN];
      let len = bytes.len();
      buf[..len].copy_from_slice(bytes);
      for b in &mut buf[..len] {
        if *b == b'|' {
          *b = b'_';
        }
      }
      if let Some((cmd, _)) = RespCommand::lookup(&buf[..len])
        && let Some(cmd) = valid_cmd(cmd)
      {
        return Ok(cmd);
      }
    }
  }

  // 2. 检查点分隔命令（如 "RI.CREATE"）
  if let Some((cmd, _)) = RespCommand::lookup(bytes)
    && let Some(cmd) = valid_cmd(cmd)
  {
    return Ok(cmd);
  }

  if bytes.contains(&b'.') && bytes.len() <= LOOKUP_NAME_MAX_LEN {
    let mut buf = [0u8; LOOKUP_NAME_MAX_LEN];
    let mut len = 0;
    for &b in bytes {
      if b != b'.' {
        buf[len] = b;
        len += 1;
      }
    }
    if let Some((cmd, _)) = RespCommand::lookup(&buf[..len])
      && let Some(cmd) = valid_cmd(cmd)
    {
      return Ok(cmd);
    }
  }

  // 3. 兼容历史别名
  if name.eq_ignore_ascii_case("SLAVEOF") {
    return Ok(RespCommand::SECONDARYOF);
  }
  if name.eq_ignore_ascii_case("CLUSTER|SET-CONFIG-EPOCH") {
    return Ok(RespCommand::CLUSTER_SETCONFIGEPOCH);
  }

  Err(Error::CommandDoesNotExist(name.to_string()))
}

/// 解析规则行头部：校验 `user` 关键字与用户名，返回 (用户名, 剩余操作迭代器)
pub(crate) fn split_rule_line(line: &str) -> Result<(&str, SplitWhitespace<'_>)> {
  let mut tokens = line.split_whitespace();
  let Some(first) = tokens.next() else {
    return Err(Error::InvalidRule(ERR_TOO_SHORT.to_string()));
  };
  if !first.eq_ignore_ascii_case("user") {
    return Err(Error::MissingUserKeyword);
  }
  let Some(username) = tokens.next() else {
    return Err(Error::InvalidRule(ERR_TOO_SHORT.to_string()));
  };
  // 用户名禁含 `#`（AUTH 凭据「用户名#空间」保留分隔符），与 set_user 同一口径
  validate_username(username)?;
  Ok((username, tokens))
}

/// ACL 解析器工具结构体，提供 Redis 6 ACL DSL 语法解析
pub struct AclParser;

/// 应用单条 `+<cmd>` / `-<cmd>` 操作（grant 授权 / revoke 收回的单一实现）
fn apply_command_op(user: &mut User, cmd_name: &str, grant: bool) -> Result<()> {
  if let Ok(cmd) = parse_command_name(cmd_name) {
    if grant {
      user.add_command(cmd);
    } else {
      user.remove_command(cmd);
    }
  } else if is_valid_custom_command_name(cmd_name) {
    if grant {
      user.add_custom_command(cmd_name)?;
    } else {
      user.remove_custom_command(cmd_name)?;
    }
  } else {
    return Err(Error::CommandDoesNotExist(cmd_name.to_string()));
  }
  Ok(())
}

impl AclParser {
  /// 将单个 ACL 操作标记应用到目标 User（对标 C# ACLParser.ApplyACLOpToUser）
  pub fn apply_op(user: &mut User, op: &str) -> Result<()> {
    let op = op.trim();
    if op.is_empty() {
      return Ok(());
    }

    if op.eq_ignore_ascii_case("on") {
      user.enabled = true;
    } else if op.eq_ignore_ascii_case("off") {
      user.enabled = false;
    } else if op.eq_ignore_ascii_case("nopass") {
      user.clear_passwords();
      user.nopass = true;
    } else if op.eq_ignore_ascii_case("resetpass") {
      user.clear_passwords();
      user.nopass = false;
    } else if op.eq_ignore_ascii_case("reset") {
      user.reset();
    } else if let Some(cleartext) = op.strip_prefix('>') {
      user.nopass = false;
      user.add_password_hash(AclPassword::from_cleartext(cleartext));
    } else if let Some(cleartext) = op.strip_prefix('<') {
      user.remove_password_hash(&AclPassword::from_cleartext(cleartext));
    } else if let Some(hex_hash) = op.strip_prefix('#') {
      let pwd = AclPassword::from_hash_hex(hex_hash)?;
      user.nopass = false;
      user.add_password_hash(pwd);
    } else if let Some(hex_hash) = op.strip_prefix('!') {
      let pwd = AclPassword::from_hash_hex(hex_hash)?;
      user.remove_password_hash(&pwd);
    } else if let Some(cat) = op.strip_prefix("+@") {
      user.add_category(cat)?;
    } else if let Some(cat) = op.strip_prefix("-@") {
      user.remove_category(cat)?;
    } else if op.eq_ignore_ascii_case("allcommands") {
      user.add_category("all")?;
    } else if op.eq_ignore_ascii_case("nocommands") {
      user.remove_category("all")?;
    } else if let Some(cmd_name) = op.strip_prefix('+') {
      apply_command_op(user, cmd_name, true)?;
    } else if let Some(cmd_name) = op.strip_prefix('-') {
      apply_command_op(user, cmd_name, false)?;
    } else if op == "~*" || op.eq_ignore_ascii_case("allkeys") {
      user.set_all_keys();
    } else if op.eq_ignore_ascii_case("resetkeys") {
      user.reset_keys();
    } else if let Some(pat) = op.strip_prefix("%RW~") {
      user.add_key_pattern(pat, true, true);
    } else if let Some(pat) = op.strip_prefix("%R~") {
      user.add_key_pattern(pat, true, false);
    } else if let Some(pat) = op.strip_prefix("%W~") {
      user.add_key_pattern(pat, false, true);
    } else if let Some(pat) = op.strip_prefix('~') {
      user.add_key_pattern(pat, true, true);
    } else if op == "&*" || op.eq_ignore_ascii_case("allchannels") {
      user.set_all_channels();
    } else if op.eq_ignore_ascii_case("resetchannels") {
      user.reset_channels();
    } else if let Some(pat) = op.strip_prefix('&') {
      user.add_channel_pattern(pat);
    } else {
      return Err(Error::UnknownOperation(op.to_string()));
    }

    Ok(())
  }

  /// 批量应用操作标记数组到目标 User
  ///
  /// `ns <value>` 为复合规则（值占下一个标记），其余每条规则独立生效
  pub fn apply_rules<'a>(user: &mut User, rules: impl IntoIterator<Item = &'a str>) -> Result<()> {
    let mut it = rules.into_iter();
    while let Some(rule) = it.next() {
      // 名字空间绑定规则（对标 doc/zh/ns.md 六.2 SETUSER NS 语法）
      if rule.eq_ignore_ascii_case("ns") {
        let Some(val) = it.next() else {
          return Err(Error::InvalidRule(NS_MISSING_VALUE.to_string()));
        };
        user.namespace = parse_ns(val)?;
      } else {
        Self::apply_op(user, rule)?;
      }
    }
    Ok(())
  }

  /// 解析整行 ACL 规则为新 User 实体（对标 C# ACLParser.ParseACLRule）
  ///
  /// 要求至少含一条操作（`user <name>` 缺操作时报 Malformed），与 C# 行为一致
  pub fn parse_rule_line(line: &str) -> Result<User> {
    let (username, ops) = split_rule_line(line)?;
    let mut ops = ops.peekable();
    if ops.peek().is_none() {
      return Err(Error::InvalidRule(ERR_TOO_SHORT.to_string()));
    }
    let mut user = User::new(username);
    Self::apply_rules(&mut user, ops)?;
    Ok(user)
  }
}
