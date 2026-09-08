use std::str::from_utf8;

use arrayvec::ArrayVec;
use itoa::Buffer;
use memchr::memchr;

use crate::{
  error::{Error, Result},
  history::ReplId,
};

/// 静态复制协议帧常量
pub const PING_FRAME: &[u8] = b"*1\r\n$4\r\nPING\r\n";
pub const PONG_FRAME: &[u8] = b"+PONG\r\n";
pub const OK_FRAME: &[u8] = b"+OK\r\n";
pub const REPLCONF_GETACK_FRAME: &[u8] = b"*3\r\n$8\r\nREPLCONF\r\n$6\r\nGETACK\r\n$1\r\n*\r\n";

/// 复制协议单条命令允许的最大参数数量上限（防御恶意超大数组声明引发内存分配放大）
const MAX_REPLICA_CMD_ARGS: usize = 64;

/// 快速从 ASCII 字节切片解析 u64 (零分配、无额外 UTF-8 校验)
#[inline]
pub fn parse_u64_bytes(bytes: &[u8]) -> Option<u64> {
  if bytes.is_empty() {
    return None;
  }
  let mut n: u64 = 0;
  for &b in bytes {
    if b.is_ascii_digit() {
      n = n.checked_mul(10)?.checked_add((b - b'0') as u64)?;
    } else {
      return None;
    }
  }
  Some(n)
}

/// 快速从 ASCII 字节切片解析 i64 (支持正负号，负数累加完整支持 i64::MIN)
#[inline]
pub fn parse_i64_bytes(bytes: &[u8]) -> Option<i64> {
  if bytes.is_empty() {
    return None;
  }
  let (is_neg, digits) = if bytes[0] == b'-' {
    (true, &bytes[1..])
  } else if bytes[0] == b'+' {
    (false, &bytes[1..])
  } else {
    (false, bytes)
  };
  if digits.is_empty() {
    return None;
  }
  let mut n: i64 = 0;
  for &b in digits {
    if b.is_ascii_digit() {
      let digit = (b - b'0') as i64;
      n = n.checked_mul(10)?.checked_sub(digit)?;
    } else {
      return None;
    }
  }
  if is_neg { Some(n) } else { n.checked_neg() }
}

/// 快速从 ASCII 字节切片解析 u16
#[inline]
pub fn parse_u16_bytes(bytes: &[u8]) -> Option<u16> {
  let v = parse_u64_bytes(bytes)?;
  u16::try_from(v).ok()
}

/// 快速查找回车换行分隔符
#[inline]
fn find_crlf(buf: &[u8]) -> Option<usize> {
  let mut offset = 0;
  while let Some(pos) = memchr(b'\r', &buf[offset..]) {
    let idx = offset + pos;
    if idx + 1 < buf.len() && buf[idx + 1] == b'\n' {
      return Some(idx);
    }
    offset = idx + 1;
  }
  None
}

/// 复制配置子命令枚举
#[derive(Debug, Clone, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub enum ReplConfSubCmd {
  /// 汇报从节点服务端口
  ListeningPort(u16),
  /// 汇报从节点对外 IP 地址
  IpAddress(String),
  /// 协商支持的特性能力
  Capa(Vec<String>),
  /// 心跳位点汇报确认
  Ack(u64),
  /// 主节点向从节点发起位点探测
  GetAck,
  /// 未知或扩展子命令
  Other { name: String, args: Vec<String> },
}

/// 从节点发送给主节点的复制命令
#[derive(Debug, Clone, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub enum ReplicaCommand {
  /// 保活探针
  Ping,
  /// 密码认证
  Auth(String),
  /// 复制协商与确认
  ReplConf(ReplConfSubCmd),
  /// 复制位点协商与同步请求
  Psync { replid: ReplId, offset: i64 },
}

/// 主节点响应从节点的复制帧
#[derive(Debug, Clone, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub enum MasterResponse {
  /// 保活响应
  Pong,
  /// 操作成功
  Ok,
  /// 允许增量同步接续
  Continue { replid: ReplId },
  /// 触发全量快照同步
  FullResync { replid: ReplId, offset: u64 },
  /// 错误响应
  Error(String),
}

/// 编码保活探针命令帧
#[inline]
pub const fn encode_ping() -> &'static [u8] {
  PING_FRAME
}

/// 编码保活成功响应帧
#[inline]
pub const fn encode_pong() -> &'static [u8] {
  PONG_FRAME
}

/// 编码成功确认响应帧
#[inline]
pub const fn encode_ok() -> &'static [u8] {
  OK_FRAME
}

/// 编码认证命令（预分配内存，零临时字符串堆分配）
pub fn encode_auth(password: &str) -> Vec<u8> {
  let mut len_buf = Buffer::new();
  let len_str = len_buf.format(password.len());
  let mut out = Vec::with_capacity(16 + len_str.len() + password.len() + 2);
  out.extend_from_slice(b"*2\r\n$4\r\nAUTH\r\n$");
  out.extend_from_slice(len_str.as_bytes());
  out.extend_from_slice(b"\r\n");
  out.extend_from_slice(password.as_bytes());
  out.extend_from_slice(b"\r\n");
  out
}

/// 编码从节点监听端口汇报命令（使用 itoa 零中间格式化）
pub fn encode_replconf_port(port: u16) -> Vec<u8> {
  let mut port_buf = Buffer::new();
  let port_str = port_buf.format(port);
  let mut len_buf = Buffer::new();
  let len_str = len_buf.format(port_str.len());
  let mut out = Vec::with_capacity(40 + len_str.len() + port_str.len());
  out.extend_from_slice(b"*3\r\n$8\r\nREPLCONF\r\n$14\r\nlistening-port\r\n$");
  out.extend_from_slice(len_str.as_bytes());
  out.extend_from_slice(b"\r\n");
  out.extend_from_slice(port_str.as_bytes());
  out.extend_from_slice(b"\r\n");
  out
}

/// 编码从节点 IP 汇报命令
pub fn encode_replconf_ip(ip: &str) -> Vec<u8> {
  let mut len_buf = Buffer::new();
  let len_str = len_buf.format(ip.len());
  let mut out = Vec::with_capacity(36 + len_str.len() + ip.len());
  out.extend_from_slice(b"*3\r\n$8\r\nREPLCONF\r\n$10\r\nip-address\r\n$");
  out.extend_from_slice(len_str.as_bytes());
  out.extend_from_slice(b"\r\n");
  out.extend_from_slice(ip.as_bytes());
  out.extend_from_slice(b"\r\n");
  out
}

/// 编码特性能力协商命令
pub fn encode_replconf_capa(capas: &[&str]) -> Vec<u8> {
  let array_len = 1 + capas.len() * 2;
  let mut arr_buf = Buffer::new();
  let arr_str = arr_buf.format(array_len);
  let mut out = Vec::with_capacity(32 + capas.len() * 24);
  out.push(b'*');
  out.extend_from_slice(arr_str.as_bytes());
  out.extend_from_slice(b"\r\n$8\r\nREPLCONF\r\n");
  for capa in capas {
    let mut len_buf = Buffer::new();
    let len_str = len_buf.format(capa.len());
    out.extend_from_slice(b"$4\r\ncapa\r\n$");
    out.extend_from_slice(len_str.as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(capa.as_bytes());
    out.extend_from_slice(b"\r\n");
  }
  out
}

/// REPLCONF ACK 心跳帧最大字节数：固定头 28 + 长度 ≤2 + CRLF 4 + 偏移 ≤20 = ≤54，取 64 对齐
const ACK_FRAME_MAX_LEN: usize = 64;
const _: () = assert!(28 + 2 + 4 + 20 <= ACK_FRAME_MAX_LEN);

/// 编码心跳位点确认命令（栈上定容缓冲，零堆分配热路径；compio 原生支持 ArrayVec 直写套接字）
pub fn encode_replconf_ack(offset: u64) -> ArrayVec<u8, ACK_FRAME_MAX_LEN> {
  let mut off_buf = Buffer::new();
  let off_str = off_buf.format(offset);
  let mut len_buf = Buffer::new();
  let len_str = len_buf.format(off_str.len());
  let mut out = ArrayVec::new();
  out.extend(b"*3\r\n$8\r\nREPLCONF\r\n$3\r\nACK\r\n$".iter().copied());
  out.extend(len_str.as_bytes().iter().copied());
  out.extend(b"\r\n".iter().copied());
  out.extend(off_str.as_bytes().iter().copied());
  out.extend(b"\r\n".iter().copied());
  out
}

/// 编码位点探测请求命令
#[inline]
pub const fn encode_replconf_getack() -> &'static [u8] {
  REPLCONF_GETACK_FRAME
}

/// 编码同步协商请求命令
pub fn encode_psync(replid: &ReplId, offset: i64) -> Vec<u8> {
  let replid_bytes = replid.as_bytes();
  let mut off_buf = Buffer::new();
  let off_str = off_buf.format(offset);
  let mut off_len_buf = Buffer::new();
  let off_len_str = off_len_buf.format(off_str.len());
  let mut out = Vec::with_capacity(48 + replid_bytes.len() + off_str.len());
  out.extend_from_slice(b"*3\r\n$5\r\nPSYNC\r\n$40\r\n");
  out.extend_from_slice(replid_bytes);
  out.extend_from_slice(b"\r\n$");
  out.extend_from_slice(off_len_str.as_bytes());
  out.extend_from_slice(b"\r\n");
  out.extend_from_slice(off_str.as_bytes());
  out.extend_from_slice(b"\r\n");
  out
}

/// 编码允许增量同步接续响应（定长 52 字节精准预分配）
pub fn encode_continue(replid: &ReplId) -> Vec<u8> {
  let mut out = Vec::with_capacity(52);
  out.extend_from_slice(b"+CONTINUE ");
  out.extend_from_slice(replid.as_bytes());
  out.extend_from_slice(b"\r\n");
  out
}

/// 编码触发全量快照同步响应
pub fn encode_fullresync(replid: &ReplId, offset: u64) -> Vec<u8> {
  let mut off_buf = Buffer::new();
  let off_str = off_buf.format(offset);
  let mut out = Vec::with_capacity(14 + 40 + 1 + off_str.len() + 2);
  out.extend_from_slice(b"+FULLRESYNC ");
  out.extend_from_slice(replid.as_bytes());
  out.push(b' ');
  out.extend_from_slice(off_str.as_bytes());
  out.extend_from_slice(b"\r\n");
  out
}

/// 解析主节点返回的响应帧，返回解析结果与消耗字节数
pub fn parse_master_response(input: &[u8]) -> Result<Option<(MasterResponse, usize)>> {
  if input.is_empty() {
    return Ok(None);
  }

  // 快速查找回车换行分隔符
  let Some(crlf_pos) = find_crlf(input) else {
    return Ok(None);
  };

  let line = &input[..crlf_pos];
  let consumed = crlf_pos + 2;

  if line.is_empty() {
    return Err(Error::Protocol("空的响应行".to_string()));
  }

  match line[0] {
    b'+' => {
      let content = match from_utf8(&line[1..]) {
        Ok(s) => s.trim(),
        Err(_) => return Err(Error::Protocol("非法的 UTF-8 单行状态响应".to_string())),
      };

      let mut words = content.split_whitespace();
      let first_word = words.next().unwrap_or("");
      if first_word.eq_ignore_ascii_case("PONG") {
        Ok(Some((MasterResponse::Pong, consumed)))
      } else if first_word.eq_ignore_ascii_case("OK") {
        Ok(Some((MasterResponse::Ok, consumed)))
      } else if first_word.eq_ignore_ascii_case("CONTINUE") {
        let replid = match words.next() {
          Some(id_str) => ReplId::from_str_val(id_str)?,
          None => ReplId::empty(),
        };
        Ok(Some((MasterResponse::Continue { replid }, consumed)))
      } else if first_word.eq_ignore_ascii_case("FULLRESYNC") {
        let bad_format = || Error::Protocol(format!("非法的 FULLRESYNC 响应格式: {content}"));
        let id_part = words.next().ok_or_else(bad_format)?;
        let offset_part = words.next().ok_or_else(bad_format)?;
        let replid = ReplId::from_str_val(id_part)?;
        let offset = parse_u64_bytes(offset_part.as_bytes())
          .ok_or_else(|| Error::Protocol(format!("非法的 FULLRESYNC offset: {offset_part}")))?;
        Ok(Some((
          MasterResponse::FullResync { replid, offset },
          consumed,
        )))
      } else {
        Err(Error::Protocol(format!("未知的单行状态响应: {content}")))
      }
    }
    b'-' => {
      let err_msg = match from_utf8(&line[1..]) {
        Ok(s) => s.trim().to_string(),
        Err(_) => "非法的 UTF-8 错误响应".to_string(),
      };
      Ok(Some((MasterResponse::Error(err_msg), consumed)))
    }
    _ => Err(Error::Protocol(format!(
      "非预期的主节点响应起始符: {}",
      line[0] as char
    ))),
  }
}

/// 解析从节点发送的复制命令帧，返回解析结果与消耗字节数
pub fn parse_replica_command(input: &[u8]) -> Result<Option<(ReplicaCommand, usize)>> {
  if input.is_empty() {
    return Ok(None);
  }

  // 校验起始符是否为数组标志，非数组时兼容支持常见内联命令 (PING\r\n / AUTH pwd\r\n)
  if input[0] != b'*' {
    let Some(first_crlf) = find_crlf(input) else {
      return Ok(None);
    };
    let line = &input[..first_crlf];
    let consumed = first_crlf + 2;
    let line_str = match from_utf8(line) {
      Ok(s) => s.trim(),
      Err(_) => return Err(Error::Protocol("非法的内联命令 UTF-8".to_string())),
    };
    let mut words = line_str.split_whitespace();
    let Some(first_word) = words.next() else {
      return Err(Error::Protocol("空内联命令".to_string()));
    };
    if first_word.eq_ignore_ascii_case("PING") {
      return Ok(Some((ReplicaCommand::Ping, consumed)));
    } else if first_word.eq_ignore_ascii_case("AUTH") {
      let Some(pwd) = words.next() else {
        return Err(Error::Protocol("内联 AUTH 缺少密码参数".to_string()));
      };
      return Ok(Some((ReplicaCommand::Auth(pwd.to_string()), consumed)));
    }
    return Err(Error::Protocol(format!(
      "非预期的命令起始符: {}",
      input[0] as char
    )));
  }

  let Some(first_crlf) = find_crlf(input) else {
    return Ok(None);
  };

  let num_args: usize = parse_u64_bytes(&input[1..first_crlf])
    .map(|v| v as usize)
    .ok_or_else(|| Error::Protocol("非法的数组长度".to_string()))?;

  if num_args > MAX_REPLICA_CMD_ARGS {
    return Err(Error::Protocol(format!(
      "复制命令参数数量超出上限: {num_args} > {MAX_REPLICA_CMD_ARGS}"
    )));
  }

  let mut cursor = first_crlf + 2;
  // 使用零拷贝字节切片收集参数，消除所有命令名称与参数的中间字符串分配
  let mut args: Vec<&[u8]> = Vec::with_capacity(num_args);

  for _ in 0..num_args {
    if cursor >= input.len() {
      return Ok(None);
    }
    if input[cursor] != b'$' {
      return Err(Error::Protocol(format!(
        "非预期的字符串起始符: {}",
        input[cursor] as char
      )));
    }

    let Some(len_crlf_rel) = find_crlf(&input[cursor..]) else {
      return Ok(None);
    };
    let str_len: usize = parse_u64_bytes(&input[cursor + 1..cursor + len_crlf_rel])
      .map(|v| v as usize)
      .ok_or_else(|| Error::Protocol("非法的字符串长度".to_string()))?;

    cursor += len_crlf_rel + 2;
    // 全程使用 checked 算术，防恶意超大长度声明触发算术溢出 panic
    let Some(data_end) = cursor.checked_add(str_len) else {
      return Err(Error::Protocol("非法的字符串长度".to_string()));
    };
    if data_end.checked_add(2).is_none_or(|end| end > input.len()) {
      return Ok(None);
    }
    if input[data_end] != b'\r' || input[data_end + 1] != b'\n' {
      return Err(Error::Protocol(
        "非法的 BulkString 结尾定界符，期望 CRLF".to_string(),
      ));
    }

    args.push(&input[cursor..data_end]);
    cursor = data_end + 2;
  }

  if args.is_empty() {
    return Err(Error::Protocol("空命令参数数组".to_string()));
  }

  let cmd_name = args[0];
  if cmd_name.eq_ignore_ascii_case(b"PING") {
    Ok(Some((ReplicaCommand::Ping, cursor)))
  } else if cmd_name.eq_ignore_ascii_case(b"AUTH") {
    if args.len() < 2 {
      return Err(Error::Protocol("AUTH 缺少密码参数".to_string()));
    }
    let pwd = from_utf8(args[1])
      .map_err(|_| Error::Protocol("AUTH 密码非合法 UTF-8".to_string()))?
      .to_string();
    Ok(Some((ReplicaCommand::Auth(pwd), cursor)))
  } else if cmd_name.eq_ignore_ascii_case(b"REPLCONF") {
    if args.len() < 2 {
      return Err(Error::Protocol("REPLCONF 缺少子命令".to_string()));
    }
    let subcmd_bytes = args[1];
    let replconf = if subcmd_bytes.eq_ignore_ascii_case(b"listening-port") {
      if args.len() < 3 {
        return Err(Error::Protocol("listening-port 缺少端口参数".to_string()));
      }
      let port: u16 = parse_u16_bytes(args[2]).ok_or_else(|| {
        Error::Protocol(format!("非法端口: {}", String::from_utf8_lossy(args[2])))
      })?;
      ReplConfSubCmd::ListeningPort(port)
    } else if subcmd_bytes.eq_ignore_ascii_case(b"ip-address") {
      if args.len() < 3 {
        return Err(Error::Protocol("ip-address 缺少 IP 参数".to_string()));
      }
      let ip = from_utf8(args[2])
        .map_err(|_| Error::Protocol("非法 IP UTF-8".to_string()))?
        .to_string();
      ReplConfSubCmd::IpAddress(ip)
    } else if subcmd_bytes.eq_ignore_ascii_case(b"capa") {
      let mut capas = Vec::new();
      let mut i = 1;
      while i + 1 < args.len() {
        if args[i].eq_ignore_ascii_case(b"capa") {
          let capa_str = from_utf8(args[i + 1])
            .map_err(|_| Error::Protocol("非法 capa UTF-8".to_string()))?
            .to_string();
          capas.push(capa_str);
          i += 2;
        } else {
          i += 1;
        }
      }
      ReplConfSubCmd::Capa(capas)
    } else if subcmd_bytes.eq_ignore_ascii_case(b"ack") {
      if args.len() < 3 {
        return Err(Error::Protocol("ACK 缺少位点参数".to_string()));
      }
      let offset: u64 = parse_u64_bytes(args[2]).ok_or_else(|| {
        Error::Protocol(format!("非法位点: {}", String::from_utf8_lossy(args[2])))
      })?;
      ReplConfSubCmd::Ack(offset)
    } else if subcmd_bytes.eq_ignore_ascii_case(b"getack") {
      ReplConfSubCmd::GetAck
    } else {
      let name = from_utf8(subcmd_bytes)
        .map_err(|_| Error::Protocol("非法子命令名 UTF-8".to_string()))?
        .to_ascii_lowercase();
      let mut other_args = Vec::with_capacity(args.len().saturating_sub(2));
      for &arg in &args[2..] {
        let arg_str = from_utf8(arg)
          .map_err(|_| Error::Protocol("非法参数 UTF-8".to_string()))?
          .to_string();
        other_args.push(arg_str);
      }
      ReplConfSubCmd::Other {
        name,
        args: other_args,
      }
    };
    Ok(Some((ReplicaCommand::ReplConf(replconf), cursor)))
  } else if cmd_name.eq_ignore_ascii_case(b"PSYNC") {
    if args.len() < 3 {
      return Err(Error::Protocol(
        "PSYNC 缺少 replid 或 offset 参数".to_string(),
      ));
    }
    let replid_bytes = args[1];
    // 问号编号判定与 ReplId::from_bytes 严格一致（单 '?' 或全 '?' 40 字节），
    // 拒绝 "?xxx..." 之类畸形编号静默降级为问号协商
    let replid = if replid_bytes == b"?" || replid_bytes.iter().all(|&b| b == b'?') {
      ReplId::question_mark()
    } else {
      ReplId::from_bytes(replid_bytes)?
    };
    let offset: i64 = parse_i64_bytes(args[2]).ok_or_else(|| {
      Error::Protocol(format!(
        "非法 PSYNC offset: {}",
        String::from_utf8_lossy(args[2])
      ))
    })?;
    Ok(Some((ReplicaCommand::Psync { replid, offset }, cursor)))
  } else {
    let name_str = from_utf8(cmd_name).unwrap_or("<invalid utf8>");
    Err(Error::Protocol(format!("非预期的复制命令: {name_str}")))
  }
}
