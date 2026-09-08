use std::{mem, str, sync::Arc};

use aok::{OK, Result};
use bytes::Bytes;
use parking_lot::RwLock;
use wedb_resp::RespCommand;
use wedb_txn::{ExecResult, QueuedCommand, TransactionManager, WatchVersionMap};
use whasher::papaya::HashMap as PapayaMap;

/// RESP 协议通用常量应答
pub const OK_RESP: &str = "+OK\r\n";
pub const QUEUED_RESP: &str = "+QUEUED\r\n";
pub const NULL_BULK_RESP: &str = "$-1\r\n";
pub const NULL_ARRAY_RESP: &str = "*-1\r\n";

/// 字节常量构造辅助函数 (零拷贝静态字节串)
#[inline]
pub fn b(s: &'static str) -> Bytes {
  Bytes::from_static(s.as_bytes())
}

/// 快速将整数写入 RESP 缓冲区（使用 itoa 零堆分配）
#[inline]
pub fn append_integer(out: &mut String, val: impl itoa::Integer, itoa_buf: &mut itoa::Buffer) {
  out.push(':');
  out.push_str(itoa_buf.format(val));
  out.push_str("\r\n");
}

/// 快速将字节切片作为 Bulk String 写入 RESP 缓冲区
#[inline]
pub fn append_bulk_string(out: &mut String, val: &[u8], itoa_buf: &mut itoa::Buffer) {
  out.push('$');
  out.push_str(itoa_buf.format(val.len()));
  out.push_str("\r\n");
  if let Ok(s) = str::from_utf8(val) {
    out.push_str(s);
  } else {
    out.push_str(&String::from_utf8_lossy(val));
  }
  out.push_str("\r\n");
}

/// 格式化整数为 RESP 协议字符串（:num\r\n）
#[inline]
pub fn format_integer(val: impl itoa::Integer) -> String {
  let mut itoa_buf = itoa::Buffer::new();
  let mut s = String::with_capacity(16);
  append_integer(&mut s, val, &mut itoa_buf);
  s
}

/// 格式化字节切片为 RESP Bulk String 协议字符串（$len\r\nbytes\r\n）
#[inline]
pub fn format_bulk_string(val: &[u8]) -> String {
  let mut itoa_buf = itoa::Buffer::new();
  let mut s = String::with_capacity(val.len() + 16);
  append_bulk_string(&mut s, val, &mut itoa_buf);
  s
}

/// 解析文本命令类型（零堆分配 ASCII 大小写忽略比对）
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientCmd<'a> {
  Multi,
  Exec,
  Discard,
  Watch,
  Unwatch,
  Set,
  Get,
  Lpush,
  Lpop,
  SetWithEtag,
  GetWithEtag,
  GetIfNotMatch,
  SetIfMatch,
  Other(&'a str),
}

impl<'a> ClientCmd<'a> {
  #[inline]
  pub fn parse(s: &'a str) -> Self {
    if s.eq_ignore_ascii_case("MULTI") {
      Self::Multi
    } else if s.eq_ignore_ascii_case("EXEC") {
      Self::Exec
    } else if s.eq_ignore_ascii_case("DISCARD") {
      Self::Discard
    } else if s.eq_ignore_ascii_case("WATCH")
      || s.eq_ignore_ascii_case("WATCHMS")
      || s.eq_ignore_ascii_case("WATCHOS")
    {
      Self::Watch
    } else if s.eq_ignore_ascii_case("UNWATCH") {
      Self::Unwatch
    } else if s.eq_ignore_ascii_case("SET") {
      Self::Set
    } else if s.eq_ignore_ascii_case("GET") {
      Self::Get
    } else if s.eq_ignore_ascii_case("LPUSH") {
      Self::Lpush
    } else if s.eq_ignore_ascii_case("LPOP") {
      Self::Lpop
    } else if s.eq_ignore_ascii_case("SETWITHETAG") {
      Self::SetWithEtag
    } else if s.eq_ignore_ascii_case("GETWITHETAG") {
      Self::GetWithEtag
    } else if s.eq_ignore_ascii_case("GETIFNOTMATCH") {
      Self::GetIfNotMatch
    } else if s.eq_ignore_ascii_case("SETIFMATCH") {
      Self::SetIfMatch
    } else {
      Self::Other(s)
    }
  }

  #[inline]
  pub fn to_resp_command(self) -> RespCommand {
    match self {
      Self::Set => RespCommand::SET,
      Self::Get => RespCommand::GET,
      Self::Lpush => RespCommand::LPUSH,
      Self::SetWithEtag => RespCommand::SETWITHETAG,
      Self::GetWithEtag => RespCommand::GETWITHETAG,
      Self::GetIfNotMatch => RespCommand::GETIFNOTMATCH,
      Self::SetIfMatch => RespCommand::SETIFMATCH,
      _ => RespCommand::ECHO,
    }
  }
}

/// 模拟 Garnet 内存数据库存储后端，支持字符串、哈希、列表以及 ETag 命令
#[derive(Clone, Default)]
pub struct MockDatabase {
  /// 字符串键值存储 (基于 papaya 无锁高并发字典)
  pub strings: Arc<PapayaMap<Bytes, Bytes>>,
  /// 哈希键值存储：键 -> (字段 -> 值)
  pub hashes: Arc<PapayaMap<Bytes, Arc<PapayaMap<Bytes, Bytes>>>>,
  /// 列表键值存储：键 -> 元素列表
  pub lists: Arc<PapayaMap<Bytes, Arc<RwLock<Vec<Bytes>>>>>,
  /// ETag 存储：键 -> 当前 ETag 计数
  pub etags: Arc<PapayaMap<Bytes, u64>>,
  /// 全局监视版本映射表（用于乐观锁冲突检测）
  pub version_map: Arc<WatchVersionMap>,
}

impl MockDatabase {
  pub fn new() -> Self {
    Self::default()
  }

  /// 创建连接会话客户端
  pub fn create_client(&self) -> TestClient {
    TestClient {
      db: self.clone(),
      txn: TransactionManager::new(),
    }
  }

  /// 写入字符串键值并自增版本号
  pub fn string_set(&self, key: impl AsRef<[u8]>, val: impl AsRef<[u8]>) {
    let k = Bytes::copy_from_slice(key.as_ref());
    let v = Bytes::copy_from_slice(val.as_ref());
    self.strings.pin().insert(k, v);
    self.version_map.bump_version_key(key.as_ref());
  }

  /// 读取字符串键值
  pub fn string_get(&self, key: impl AsRef<[u8]>) -> Option<Bytes> {
    self.strings.pin().get(key.as_ref()).cloned()
  }

  /// 创建事务句柄
  pub fn create_transaction(&self) -> Result<TestTransaction> {
    let mut txn = TransactionManager::new();
    txn.multi()?;
    Ok(TestTransaction {
      db: self.clone(),
      txn,
      get_tasks: Vec::new(),
    })
  }
}

/// 异步读取任务句柄
pub struct TaskHandle(Arc<RwLock<Option<Bytes>>>);

impl TaskHandle {
  pub fn result(&self) -> Option<Bytes> {
    self.0.read().clone()
  }
}

/// 事务封装对象，用于对标 Garnet 客户端事务 API
pub struct TestTransaction {
  db: MockDatabase,
  txn: TransactionManager,
  get_tasks: Vec<(usize, Arc<RwLock<Option<Bytes>>>)>,
}

impl TestTransaction {
  /// 事务内异步排队字符串写入操作
  pub fn string_set_async(&mut self, key: impl AsRef<[u8]>, val: impl AsRef<[u8]>) -> Result<()> {
    let k = Bytes::copy_from_slice(key.as_ref());
    let v = Bytes::copy_from_slice(val.as_ref());
    self
      .txn
      .queue_command(QueuedCommand::new(RespCommand::SET, vec![k, v]))?;
    OK
  }

  /// 事务内异步排队字符串读取操作
  pub fn string_get_async(&mut self, key: impl AsRef<[u8]>) -> Result<TaskHandle> {
    let k = Bytes::copy_from_slice(key.as_ref());
    let idx = self.txn.queued_count();
    self
      .txn
      .queue_command(QueuedCommand::new(RespCommand::GET, vec![k]))?;

    let handle_cell = Arc::new(RwLock::new(None));
    self.get_tasks.push((idx, Arc::clone(&handle_cell)));
    Ok(TaskHandle(handle_cell))
  }

  /// 事务内异步排队通用执行命令
  pub fn execute_async(&mut self, cmd_name: &str, args: &[&str]) -> Result<()> {
    let cmd = match cmd_name {
      "SET" => RespCommand::SET,
      "GET" => RespCommand::GET,
      "HEXPIRE" => RespCommand::HEXPIRE,
      _ => RespCommand::NONE,
    };
    let bargs: Vec<Bytes> = args
      .iter()
      .map(|a| Bytes::copy_from_slice(a.as_bytes()))
      .collect();
    self.txn.queue_command(QueuedCommand::new(cmd, bargs))?;
    OK
  }

  /// 事务内异步排队哈希字段写入操作
  pub fn hash_set_async(&mut self, key: impl AsRef<[u8]>, fields: &[(&str, &str)]) -> Result<()> {
    let mut args = Vec::with_capacity(1 + fields.len() * 2);
    args.push(Bytes::copy_from_slice(key.as_ref()));
    for (f, v) in fields {
      args.push(Bytes::copy_from_slice(f.as_bytes()));
      args.push(Bytes::copy_from_slice(v.as_bytes()));
    }
    self
      .txn
      .queue_command(QueuedCommand::new(RespCommand::HSET, args))?;
    OK
  }

  /// 提交事务并回写异步任务结果
  pub fn execute(mut self) -> bool {
    let db = self.db.clone();
    let get_tasks = mem::take(&mut self.get_tasks);

    let res = self.txn.exec(&db.version_map, |queued| match queued.cmd {
      RespCommand::SET => {
        let k = queued.args[0].clone();
        let v = queued.args[1].clone();
        db.strings.pin().insert(k, v);
        Bytes::from_static(b"+OK\r\n")
      }
      RespCommand::GET => {
        let k = &queued.args[0];
        let val = db.strings.pin().get(k).cloned();
        val.unwrap_or_default()
      }
      RespCommand::HSET => {
        let k = queued.args[0].clone();
        let pin = db.hashes.pin();
        let sub = if let Some(m) = pin.get(&k) {
          Arc::clone(m)
        } else {
          Arc::clone(pin.get_or_insert_with(k, || Arc::new(PapayaMap::new())))
        };
        let sub_pin = sub.pin();
        for chunk in queued.args[1..].as_chunks::<2>().0 {
          sub_pin.insert(chunk[0].clone(), chunk[1].clone());
        }
        Bytes::from_static(b":1\r\n")
      }
      _ => Bytes::from_static(b"+OK\r\n"),
    });

    match res {
      Ok(ExecResult::Success(responses)) => {
        for (idx, cell) in get_tasks {
          if let Some(r) = responses.get(idx) {
            *cell.write() = Some(r.clone());
          }
        }
        true
      }
      _ => false,
    }
  }
}

/// 模拟 Garnet 测试轻量客户端会话
pub struct TestClient {
  pub db: MockDatabase,
  pub txn: TransactionManager,
}

impl TestClient {
  /// 发送原始文本命令并返回 RESP 响应字符串
  pub fn send_command(&mut self, cmd_line: &str) -> String {
    let parts: Vec<&str> = cmd_line.split_whitespace().collect();
    if parts.is_empty() {
      return "-ERR empty command\r\n".to_string();
    }

    let cmd = ClientCmd::parse(parts[0]);

    // 处于事务开启/中止状态时的排队与提交逻辑
    if self.txn.is_skipping_operations() {
      match cmd {
        ClientCmd::Exec => {
          let db = self.db.clone();
          let res = self.txn.exec(&db.version_map, |queued| {
            let mut out = String::new();
            let mut itoa_buf = itoa::Buffer::new();
            match queued.cmd {
              RespCommand::SET => {
                let k = queued.args[0].clone();
                let v = queued.args[1].clone();
                db.strings.pin().insert(k, v);
                out.push_str(OK_RESP);
              }
              RespCommand::GET => {
                let k = &queued.args[0];
                if let Some(v) = db.strings.pin().get(k) {
                  append_bulk_string(&mut out, v, &mut itoa_buf);
                } else {
                  out.push_str(NULL_BULK_RESP);
                }
              }
              RespCommand::LPUSH => {
                let k = queued.args[0].clone();
                let v = queued.args[1].clone();
                let pin = db.lists.pin();
                let list_arc = if let Some(l) = pin.get(&k) {
                  Arc::clone(l)
                } else {
                  Arc::clone(pin.get_or_insert_with(k, || Arc::new(RwLock::new(Vec::new()))))
                };
                let mut list = list_arc.write();
                list.insert(0, v);
                append_integer(&mut out, list.len(), &mut itoa_buf);
              }
              RespCommand::SETWITHETAG => {
                let k = queued.args[0].clone();
                let v = queued.args[1].clone();
                db.strings.pin().insert(k.clone(), v);
                let etag = *db.etags.pin().update_or_insert(k, |&prev| prev + 1, 1);
                append_integer(&mut out, etag, &mut itoa_buf);
              }
              RespCommand::GETWITHETAG => {
                let k = queued.args[0].clone();
                let val_opt = db.strings.pin().get(&k).cloned();
                let etag = db.etags.pin().get(&k).copied().unwrap_or(0);
                if let Some(v) = val_opt {
                  out.push_str("*2\r\n");
                  append_integer(&mut out, etag, &mut itoa_buf);
                  append_bulk_string(&mut out, &v, &mut itoa_buf);
                } else {
                  out.push_str(NULL_ARRAY_RESP);
                }
              }
              RespCommand::GETIFNOTMATCH => {
                out.push_str("*2\r\n:1\r\n$-1\r\n");
              }
              RespCommand::SETIFMATCH => {
                let k = queued.args[0].clone();
                let v = queued.args[1].clone();
                db.strings.pin().insert(k.clone(), v);
                let etag = *db.etags.pin().update_or_insert(k, |&prev| prev + 1, 1);
                out.push_str("*2\r\n");
                append_integer(&mut out, etag, &mut itoa_buf);
                out.push_str("$-1\r\n");
              }
              _ => out.push_str(OK_RESP),
            }
            out
          });

          match res {
            Ok(ExecResult::Success(responses)) => {
              let mut itoa_buf = itoa::Buffer::new();
              let mut ret = String::with_capacity(16 + responses.len() * 16);
              ret.push('*');
              ret.push_str(itoa_buf.format(responses.len()));
              ret.push_str("\r\n");
              for r in responses {
                ret.push_str(&r);
              }
              ret
            }
            Ok(ExecResult::Conflict) => NULL_ARRAY_RESP.to_string(),
            Ok(ExecResult::Aborted) => {
              "-EXECABORT Transaction discarded because of previous errors.\r\n".to_string()
            }
            Err(e) => format!("-ERR {e:?}\r\n"),
          }
        }
        ClientCmd::Discard => {
          let _ = self.txn.discard();
          OK_RESP.to_string()
        }
        ClientCmd::Multi => {
          let _ = self.txn.multi();
          "-ERR MULTI calls can not be nested\r\n".to_string()
        }
        ClientCmd::Watch => "-ERR WATCH inside MULTI is not allowed\r\n".to_string(),
        _ => {
          let resp_cmd = cmd.to_resp_command();
          let mut bargs = Vec::with_capacity(parts.len().saturating_sub(1));
          for p in &parts[1..] {
            bargs.push(Bytes::copy_from_slice(p.as_bytes()));
          }

          if self
            .txn
            .queue_command(QueuedCommand::new(resp_cmd, bargs))
            .is_err()
          {
            self.txn.abort();
            "-ERR failed to queue\r\n".to_string()
          } else {
            QUEUED_RESP.to_string()
          }
        }
      }
    } else {
      // 事务外部非排队普通命令执行
      match cmd {
        ClientCmd::Multi => {
          let _ = self.txn.multi();
          OK_RESP.to_string()
        }
        ClientCmd::Exec => "-ERR EXEC without MULTI\r\n".to_string(),
        ClientCmd::Discard => "-ERR DISCARD without MULTI\r\n".to_string(),
        ClientCmd::Watch => {
          for key in &parts[1..] {
            let _ = self
              .txn
              .watch(Bytes::copy_from_slice(key.as_bytes()), &self.db.version_map);
          }
          OK_RESP.to_string()
        }
        ClientCmd::Unwatch => {
          self.txn.unwatch();
          OK_RESP.to_string()
        }
        ClientCmd::Set => {
          if parts.len() >= 3 {
            self.db.string_set(parts[1], parts[2]);
            OK_RESP.to_string()
          } else {
            "-ERR wrong number of arguments for 'set'\r\n".to_string()
          }
        }
        ClientCmd::Get => {
          if parts.len() >= 2 {
            if let Some(val) = self.db.string_get(parts[1]) {
              format_bulk_string(&val)
            } else {
              NULL_BULK_RESP.to_string()
            }
          } else {
            "-ERR wrong number of arguments for 'get'\r\n".to_string()
          }
        }
        ClientCmd::Lpush => {
          if parts.len() >= 3 {
            let k = Bytes::copy_from_slice(parts[1].as_bytes());
            let v = Bytes::copy_from_slice(parts[2].as_bytes());
            let pin = self.db.lists.pin();
            let list_arc = if let Some(l) = pin.get(&k) {
              Arc::clone(l)
            } else {
              Arc::clone(pin.get_or_insert_with(k.clone(), || Arc::new(RwLock::new(Vec::new()))))
            };
            let mut list = list_arc.write();
            list.insert(0, v);
            let len = list.len();
            drop(list);
            self.db.version_map.bump_version_key(&k);
            format_integer(len)
          } else {
            "-ERR wrong number of arguments for 'lpush'\r\n".to_string()
          }
        }
        ClientCmd::Lpop => {
          if parts.len() >= 2 {
            let k = Bytes::copy_from_slice(parts[1].as_bytes());
            let pin = self.db.lists.pin();
            let mut popped = None;
            if let Some(list_arc) = pin.get(&k) {
              let mut list = list_arc.write();
              if !list.is_empty() {
                popped = Some(list.remove(0));
              }
              if list.is_empty() {
                drop(list);
                pin.remove(&k);
              }
            }
            self.db.version_map.bump_version_key(&k);
            if let Some(val) = popped {
              format_bulk_string(&val)
            } else {
              NULL_BULK_RESP.to_string()
            }
          } else {
            "-ERR wrong number of arguments for 'lpop'\r\n".to_string()
          }
        }
        ClientCmd::SetWithEtag => {
          if parts.len() >= 3 {
            let k = Bytes::copy_from_slice(parts[1].as_bytes());
            let v = Bytes::copy_from_slice(parts[2].as_bytes());
            self.db.strings.pin().insert(k.clone(), v);
            let current_etag =
              *self
                .db
                .etags
                .pin()
                .update_or_insert(k.clone(), |&prev| prev + 1, 1);
            self.db.version_map.bump_version_key(&k);
            format_integer(current_etag)
          } else {
            "-ERR wrong number of arguments for 'setwithetag'\r\n".to_string()
          }
        }
        ClientCmd::GetWithEtag => {
          if parts.len() >= 2 {
            let k = Bytes::copy_from_slice(parts[1].as_bytes());
            let val_opt = self.db.strings.pin().get(&k).cloned();
            let etag = self.db.etags.pin().get(&k).copied().unwrap_or(0);
            if let Some(v) = val_opt {
              let mut itoa_buf = itoa::Buffer::new();
              let mut out = String::with_capacity(32 + v.len());
              out.push_str("*2\r\n");
              append_integer(&mut out, etag, &mut itoa_buf);
              append_bulk_string(&mut out, &v, &mut itoa_buf);
              out
            } else {
              NULL_ARRAY_RESP.to_string()
            }
          } else {
            "-ERR wrong number of arguments for 'getwithetag'\r\n".to_string()
          }
        }
        _ => OK_RESP.to_string(),
      }
    }
  }
}
