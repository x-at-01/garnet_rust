use std::{slice::from_ref, str::from_utf8, sync::Arc};

use bytes::{Bytes, BytesMut};
use wdev::Device;
use wedb_acl::{AccessControlList, UserHandle};
use wedb_pubsub::{SessionHandle, SubscribeBroker};
use wedb_resp::{ParseUtils, RespCommand, SessionParseState};
use wedb_txn::{QueuedCommand, TransactionManager, WatchVersionMap};
use wedb_zset::ZAddOpt;
use windex::MultiBucketGuard;
use wedb_redis::prelude::*;
use wkv::StoreSession;

use crate::{buffer::SendBuffer, error::Result};

/// 短复合版本键栈缓冲容量（18B 会话前缀 + 46B 用户键内零堆分配，覆盖多数热键）
const VERSION_KEY_STACK_CAP: usize = 64;

/// 解析 [start, stop] 双整数区间参数（LRANGE/ZRANGE 共用，缺失/非法时取 Redis 默认值）
#[inline]
fn parse_range(args: &[&[u8]]) -> (isize, isize) {
  (
    ParseUtils::read_int(args[1]).unwrap_or(0) as isize,
    ParseUtils::read_int(args[2]).unwrap_or(-1) as isize,
  )
}

/// 按（是否带 count 参数）写出列表弹出结果：带 count 时输出数组/空数组，否则单元素/nil
#[inline]
fn write_pop_reply(out: &mut SendBuffer, items: Vec<Vec<u8>>, counted: bool) {
  if counted {
    if items.is_empty() {
      out.write_null_array();
    } else {
      out.write_array_header(items.len());
      for it in items {
        out.write_bulk_string(&it);
      }
    }
  } else if let Some(first) = items.first() {
    out.write_bulk_string(first);
  } else {
    out.write_null();
  }
}

/// 客户端会话网络上下文，维护单个连接的状态机与引擎交互管道
pub struct NetSession<D: Device + Send + Sync + 'static> {
  /// 会话全局唯一自增标识
  pub id: u64,
  /// 当前会话认证的用户身份句柄
  pub user: UserHandle,
  /// 全局访问控制列表管理器
  pub acl: Arc<AccessControlList>,
  /// 会话绑定的存储引擎操作句柄（`active_db` 原子变量承载 SELECT 的键命名空间前缀）
  pub store_session: Arc<StoreSession<D>>,
  /// 会话事务生命周期管理器
  pub txn_manager: TransactionManager,
  /// 全局键版本变更映射表（用于乐观锁验证）
  pub version_map: Arc<WatchVersionMap>,
  /// 全局发布订阅中继器
  pub pubsub_broker: Arc<SubscribeBroker>,
  /// 会话对应的发布订阅接收句柄
  pub pubsub_session: SessionHandle,
  /// 缓存式发布订阅模式标志：订阅状态仅随本会话的 (P)SUBSCRIBE/(P)UNSUBSCRIBE 命令变更，
  /// thread-per-core 模型下单连接恒定单线程读写，缓存与中继器真实状态强一致，
  /// 免去每条命令对全局中继器的一次跨核查询
  pub pubsub_mode: bool,
  /// 是否已收到关闭连接退出请求
  pub is_closed: bool,
}

impl<D: Device + Send + Sync + 'static> NetSession<D> {
  /// 创建新的网络会话
  pub fn new(
    id: u64,
    acl: Arc<AccessControlList>,
    store_session: Arc<StoreSession<D>>,
    version_map: Arc<WatchVersionMap>,
    pubsub_broker: Arc<SubscribeBroker>,
    pubsub_session: SessionHandle,
  ) -> Self {
    let user = acl.default_user();
    Self {
      id,
      user,
      acl,
      store_session,
      txn_manager: TransactionManager::new(),
      version_map,
      pubsub_broker,
      pubsub_session,
      pubsub_mode: false,
      is_closed: false,
    }
  }

  /// 从全局中继器刷新缓存式发布订阅模式标志（仅在订阅族命令后调用）
  #[inline]
  fn refresh_pubsub_mode(&mut self) {
    self.pubsub_mode = self.pubsub_broker.session_subscription_count(self.id) > 0;
  }

  /// 未知命令处理：事务开启中则置脏中止（对齐 Redis CLIENT_DIRTY_EXEC，
  /// 使后续 EXEC 统一报 EXECABORT；对齐 C# Aborted 态 NetworkSKIP 语义）
  #[inline]
  pub fn on_unknown_command(&mut self) {
    if self.txn_manager.is_in_txn() {
      self.txn_manager.abort();
    }
  }

  /// 辅助更新指定键的版本号，通知所有并发监视该键的事务失效
  ///
  /// 版本键为会话复合键（会话前缀 ++ 用户键，精确到 `(ns, db, key)`）：与 WATCH
  /// 注册侧、事务提交广播侧三方同构，SELECT 切库后跨库同名键互不误伤。
  /// 短键走栈缓冲零堆分配，长键回退堆缓冲。
  #[inline]
  pub fn bump_key_version(&self, key: &[u8]) {
    let prefix = self.store_session.session_prefix();
    let total = prefix.len() + key.len();
    if total <= VERSION_KEY_STACK_CAP {
      let mut buf = [0u8; VERSION_KEY_STACK_CAP];
      buf[..prefix.len()].copy_from_slice(prefix.as_slice());
      buf[prefix.len()..total].copy_from_slice(key);
      self.version_map.bump_version_key(&buf[..total]);
    } else {
      let mut buf = Vec::with_capacity(total);
      buf.extend_from_slice(prefix.as_slice());
      buf.extend_from_slice(key);
      self.version_map.bump_version_key(&buf);
    }
  }

  /// 构造会话复合版本键（会话前缀 ++ 用户键，精确到 `(ns, db, key)`）
  ///
  /// 与 [`StoreSession`] 的键命名空间前缀编码同源，WATCH 注册以此字节序列
  /// 记录版本基线，使乐观锁判定不串扰其它逻辑数据库的同名键。
  #[inline]
  fn full_version_key(&self, key: &[u8]) -> Bytes {
    let prefix = self.store_session.session_prefix();
    let mut buf = BytesMut::with_capacity(prefix.len() + key.len());
    buf.extend_from_slice(prefix.as_slice());
    buf.extend_from_slice(key);
    buf.freeze()
  }

  /// 派发并执行单个协议命令管线
  ///
  /// 返回布尔值指示是否请求断开连接
  pub async fn execute(
    &mut self,
    cmd: RespCommand,
    args: &SessionParseState<'_>,
    out: &mut SendBuffer,
  ) -> Result<bool> {
    if self.is_closed {
      return Ok(true);
    }

    // 空数组帧（*0 / *-1）与空内联行（裸 CRLF）解析为 NONE：
    // 按 Redis 语义静默忽略，不回包、不做权限校验、不进入事务队列（防止空帧中止事务）
    if matches!(cmd, RespCommand::NONE) {
      return Ok(false);
    }

    // 1. 访问控制列表权限校验（对齐 C# RespServerSession.ProcessMessages：先 ACL 后订阅限制）
    if !self.user.read().can_execute(cmd) {
      out.write_error(b"NOPERM this user has no permissions to run the specified command");
      return Ok(false);
    }

    // 2. 发布订阅模式限制（RESP2 下仅订阅族/PING/QUIT 可执行；缓存标志，零跨核查询）
    if self.pubsub_mode
      && !matches!(
        cmd,
        RespCommand::SUBSCRIBE
          | RespCommand::UNSUBSCRIBE
          | RespCommand::PSUBSCRIBE
          | RespCommand::PUNSUBSCRIBE
          | RespCommand::PING
          | RespCommand::QUIT
      )
    {
      out.write_error_fmt(format_args!(
        "ERR Can't execute '{cmd:?}' in (P)SUBSCRIBE mode"
      ));
      return Ok(false);
    }

    // 3. 事务状态机处理
    if self.txn_manager.is_in_txn() {
      match cmd {
        RespCommand::EXEC => {
          self.handle_exec(out).await?;
          return Ok(false);
        }
        RespCommand::DISCARD => {
          self.txn_manager.discard()?;
          out.write_ok();
          return Ok(false);
        }
        RespCommand::MULTI => {
          // 对齐 C# NetworkMULTI（TxnRespCommands.cs）：嵌套 MULTI 报错并中止事务，
          // 后续 EXEC 统一报 EXECABORT
          self.txn_manager.abort();
          out.write_error(b"ERR MULTI calls can not be nested");
          return Ok(false);
        }
        RespCommand::QUIT => {
          self.txn_manager.reset();
          out.write_ok();
          self.is_closed = true;
          return Ok(true);
        }
        RespCommand::WATCH | RespCommand::WATCHMS | RespCommand::WATCHOS => {
          // 事务内 WATCH 族优先路由 TransactionManager::watch()：其内部 is_in_txn
          // 拦截仅回 WatchInsideMulti 错误（键参数在拦截路径不被消费），不置脏、
          // 不迁移事务状态机（对齐 Garnet NetworkSKIP 的 isWatch 分支与 Redis 语义），
          // 后续 EXEC 正常提交
          if let Err(e) = self.txn_manager.watch(Bytes::new(), &self.version_map) {
            out.write_error(e.to_string().as_bytes());
          }
          return Ok(false);
        }
        _ => {
          // 对齐 C# NetworkSKIP 的 isMultiDbCommand 分支：事务中异库 SELECT 会使
          // 排队期按原库前缀提取的键锁与执行期命名空间错位，破坏事务一致性，
          // 故立即报错并中止事务；同库 SELECT 幂等放行入队（对齐 C# 放行口径，
          // 非法参数同样放行，由执行期统一报错）
          if cmd == RespCommand::SELECT {
            if args.len() != 1 {
              self.txn_manager.abort();
              out.write_error(b"ERR wrong number of arguments for 'select' command");
              return Ok(false);
            }
            if let Some(db) = ParseUtils::try_read_ulong(args[0])
              && db != self.store_session.active_db()
            {
              self.txn_manager.abort();
              out.write_error(b"ERR SELECT is currently unsupported inside a transaction.");
              return Ok(false);
            }
          }
          if QueuedCommand::is_allowed_in_txn(cmd) {
            // 对齐 Garnet MultiProcessCommand 的入队期 arity 校验：参数个数非法
            // 立即报错并置脏中止（后续 EXEC 统一 EXECABORT），不放行入队延后到
            // 执行期逐条报错，杜绝键规格提取对残缺参数的越界风险（如 MIGRATE）
            if !cmd.check_arity(args.len()) {
              self.txn_manager.abort();
              let name = cmd.as_str().to_ascii_lowercase();
              out.write_error_fmt(format_args!(
                "ERR wrong number of arguments for '{name}' command"
              ));
              return Ok(false);
            }
            let args_bytes: Vec<Bytes> = args.iter().map(|&a| Bytes::copy_from_slice(a)).collect();
            let queued = QueuedCommand::new(cmd, args_bytes);
            // 锁集合按会话复合键哈希登记：与 WATCH 基线、提交期版本广播三方同构
            let prefix = self.store_session.session_prefix();
            if self
              .txn_manager
              .queue_command_with_key_prefix(queued, prefix.as_slice())
              .is_err()
            {
              out.write_error(b"EXECABORT Transaction discarded because of previous errors.");
            } else {
              out.write_queued();
            }
          } else {
            self.txn_manager.abort();
            out.write_error_fmt(format_args!(
              "ERR command '{cmd:?}' is not allowed in transaction"
            ));
          }
          return Ok(false);
        }
      }
    }

    // 4. 普通即时命令执行
    self
      .execute_single_command(cmd, args.as_slice(), out, self.pubsub_mode, false)
      .await
  }

  /// 执行事务提交命令并依次返回每个排队命令的执行结果
  ///
  /// 严格对齐 C# `TransactionManager.Run` 的两段式顺序：
  /// prepare_lockset（构建死锁防御锁集合）→ 物理哈希加锁 → validate_watches（持锁校验监视版本）
  /// → begin_run → 执行 → commit。先加锁后校验，彻底闭合"校验-加锁"之间的 TOCTOU 竞态窗口。
  async fn handle_exec(&mut self, out: &mut SendBuffer) -> Result<()> {
    // 阶段一：校验事务状态并构建死锁防御锁集合（不校验监视版本、不迁移状态机）
    if !self.txn_manager.prepare_lockset()? {
      // 事务在排队阶段已中止：统一报 EXECABORT（对标 C# NetworkEXEC 的 Aborted 分支）
      out.write_error(b"EXECABORT Transaction discarded because of previous errors.");
      return Ok(());
    }

    // 阶段二：按哈希桶两阶段锁对事务涉及的所有键加物理锁（对标 Garnet TxnKeyEntry.LockAllKeys，
    // 锁内部自带按桶下标排序与去重，杜绝死锁）
    let entries = self.txn_manager.key_entries();
    let mut lock_items = Vec::with_capacity(entries.len());
    lock_items.extend(
      entries
        .iter()
        .map(|entry| (entry.key_hash, entry.lock_type.is_exclusive())),
    );
    let store_session = Arc::clone(&self.store_session);
    let hash_guard = store_session
      .store
      .index
      .acquire_hash_locks(&lock_items)
      .map_err(wkv::Error::from)?;

    // 阶段三：在持有物理键锁的前提下校验受监视键版本，竞态窗口已由锁闭合
    if !self.txn_manager.validate_watches(&self.version_map) {
      // 乐观锁冲突：先释放物理锁，再清理锁集合（对齐 Garnet UnlockAllKeys 先于条目清空的释放顺序）
      drop(hash_guard);
      self.txn_manager.key_entries_mut().unlock_all_keys();
      out.write_null_array();
      return Ok(());
    }

    // 阶段四：进入运行态，取出命令队列，持锁逐条执行
    self.txn_manager.begin_run();
    let commands = self.txn_manager.take_queue();

    out.write_array_header(commands.len());
    let mut slices: Vec<&[u8]> = Vec::with_capacity(16);
    let mut exec_res = Ok(());
    for qcmd in &commands {
      slices.clear();
      slices.extend(qcmd.args.iter().map(|b| b.as_ref()));
      // locked=true：命令已在事务排他键锁保护下运行，单命令锁脚手架跳过重复加锁
      if let Err(e) = self
        .execute_single_command(qcmd.cmd, &slices, out, false, true)
        .await
      {
        exec_res = Err(e);
        break;
      }
    }

    match exec_res {
      // 持锁提交：对排他键自增版本广播监视失效（先提交后解锁，对齐 Garnet Commit → UnlockAllKeys）
      Ok(()) => {
        self.txn_manager.commit(&self.version_map);
      }
      // 执行异常：先释放物理锁再强制复位事务状态机，防止会话滞留 Running 态导致后续命令异常
      Err(e) => {
        drop(hash_guard);
        self.txn_manager.reset_all(false);
        return Err(e);
      }
    }
    Ok(())
  }

  /// 执行单个非排队业务命令
  ///
  /// `locked` 指示是否已处于外部物理键锁保护下（事务 EXEC 运行态由
  /// [`Self::handle_exec`] 统一按锁集合加锁）：
  /// - `false`（普通单命令）：写命令先经单命令锁脚手架排他加锁，检查-执行-版本递增
  ///   全程持锁，与事务 EXEC 的加锁脚手架在同一哈希桶锁上线性化；
  /// - `true`（事务运行态）：跳过加锁（桶锁不可重入，重复加锁将自锁死锁）。
  async fn execute_single_command(
    &mut self,
    cmd: RespCommand,
    args: &[&[u8]],
    out: &mut SendBuffer,
    in_pubsub: bool,
    locked: bool,
  ) -> Result<bool> {
    // 单命令写路径锁脚手架：仅覆盖存储层无内部键锁的直写命令（白名单）。
    // 集合类命令（hash/set/zset 系列与 MSET/RENAME 等）的 store 公开方法已自带
    // 内部排他键锁（对标 C# Tsavorite gillock），会话层重复加锁会在桶锁自旋上
    // 等待自身直至 LockTimeout，禁止纳入；kv 直写（upsert/delete）与 list 命令
    // 无内部锁，在此两阶段排他加锁，闭合与持锁事务并发时的"检查-执行"
    // （SET NX/XX/SETNX）与"写-版本递增"线性化窗口。
    // 局部持有索引 Arc 使锁守卫生命周期与会话借用解耦，避免占用 &mut self。
    let index = if !locked
      && !args.is_empty()
      && matches!(
        cmd,
        RespCommand::SET
          | RespCommand::SETNX
          | RespCommand::DEL
          | RespCommand::LPUSH
          | RespCommand::RPUSH
          | RespCommand::LPOP
          | RespCommand::RPOP
      ) {
      Some(Arc::clone(&self.store_session.store.index))
    } else {
      None
    };
    let _lock_guard: Option<MultiBucketGuard<'_>> = if let Some(index) = &index {
      let guard = match cmd {
        // 多键删除：全部参数为写键
        RespCommand::DEL => index.acquire_keys_lock_exclusive(args),
        // 单键直写（SET/SETNX 的检查-执行窗口与 list 推出弹出均以 args[0] 为写键）
        _ => index.acquire_keys_lock_exclusive(from_ref(&args[0])),
      }
      .map_err(wkv::Error::from)?;
      Some(guard)
    } else {
      None
    };

    match cmd {
      // 基础控制命令
      RespCommand::PING => {
        if in_pubsub {
          out.write_array_header(2);
          out.write_bulk_string(b"pong");
          if let Some(&msg) = args.first() {
            out.write_bulk_string(msg);
          } else {
            out.write_bulk_string(b"");
          }
        } else if args.is_empty() {
          out.write_pong();
        } else if args.len() == 1 {
          out.write_bulk_string(args[0]);
        } else {
          out.write_error(b"ERR wrong number of arguments for 'ping' command");
        }
      }

      RespCommand::ECHO => {
        if args.len() != 1 {
          out.write_error(b"ERR wrong number of arguments for 'echo' command");
        } else {
          out.write_bulk_string(args[0]);
        }
      }

      RespCommand::AUTH => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'auth' command");
        } else if args.len() == 1 {
          let Ok(pass) = from_utf8(args[0]) else {
            out.write_error(b"ERR invalid UTF-8 in password");
            return Ok(false);
          };
          if self.acl.auth_default(pass) {
            self.user = self.acl.default_user();
            out.write_ok();
          } else {
            out.write_error(b"WRONGPASS invalid username-password pair or user is disabled.");
          }
        } else if args.len() == 2 {
          let (Ok(user_str), Ok(pass_str)) = (from_utf8(args[0]), from_utf8(args[1])) else {
            out.write_error(b"ERR invalid UTF-8 in username or password");
            return Ok(false);
          };
          if self.acl.auth(user_str, pass_str) {
            self.user = self.acl.find_user(user_str)?;
            out.write_ok();
          } else {
            out.write_error(b"WRONGPASS invalid username-password pair or user is disabled.");
          }
        } else {
          out.write_error(b"ERR wrong number of arguments for 'auth' command");
        }
      }

      RespCommand::SELECT => {
        if args.len() != 1 {
          out.write_error(b"ERR wrong number of arguments for 'select' command");
        } else if let Some(db) = ParseUtils::try_read_ulong(args[0]) {
          // 会话键命名空间前缀由 StoreSession 的 active_db 原子变量按需重算，
          // 即时生效：后续所有键操作自动隔离进新逻辑数据库（对齐 C# Garnet NetworkSELECT）
          self.store_session.set_active_db(db);
          out.write_ok();
        } else {
          out.write_error(b"ERR value is not an integer or out of range");
        }
      }

      RespCommand::QUIT => {
        out.write_ok();
        self.is_closed = true;
        return Ok(true);
      }

      RespCommand::INFO => {
        out.write_bulk_string(b"# Server\r\nwedb_version:0.1.0\r\nredis_mode:standalone\r\n");
      }

      RespCommand::COMMAND => {
        out.write_array_header(0);
      }

      // 事务前置命令
      RespCommand::MULTI => {
        self.txn_manager.multi()?;
        out.write_ok();
      }

      RespCommand::DISCARD => {
        out.write_error(b"ERR DISCARD without MULTI");
      }

      RespCommand::EXEC => {
        out.write_error(b"ERR EXEC without MULTI");
      }

      RespCommand::WATCH => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'watch' command");
        } else {
          for &arg in args {
            let fk = self.full_version_key(arg);
            self.txn_manager.watch(fk, &self.version_map)?;
          }
          out.write_ok();
        }
      }

      RespCommand::UNWATCH => {
        self.txn_manager.unwatch();
        out.write_ok();
      }

      // 键值字符串命令
      RespCommand::GET => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'get' command");
        } else {
          let val = self.store_session.read(args[0]).await?;
          match val {
            Some(v) => out.write_bulk_string(&v),
            None => out.write_null(),
          }
        }
      }

      RespCommand::SET => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'set' command");
        } else {
          let key = args[0];
          let val = args[1];
          let mut nx = false;
          let mut xx = false;

          for &opt in &args[2..] {
            if opt.eq_ignore_ascii_case(b"NX") {
              nx = true;
            } else if opt.eq_ignore_ascii_case(b"XX") {
              xx = true;
            }
          }

          if nx && xx {
            out.write_error(b"ERR syntax error");
            return Ok(false);
          }

          if nx && self.store_session.contains_key(key).await? {
            out.write_null();
            return Ok(false);
          }
          if xx && !self.store_session.contains_key(key).await? {
            out.write_null();
            return Ok(false);
          }

          self.store_session.upsert(key, val).await?;
          self.bump_key_version(key);
          out.write_ok();
        }
      }

      RespCommand::SETNX => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'setnx' command");
        } else {
          let key = args[0];
          let val = args[1];
          if self.store_session.contains_key(key).await? {
            out.write_integer(0);
          } else {
            self.store_session.upsert(key, val).await?;
            self.bump_key_version(key);
            out.write_integer(1);
          }
        }
      }

      RespCommand::DEL => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'del' command");
        } else {
          let mut count = 0i64;
          for &key in args {
            if self.store_session.delete(key).await? {
              count += 1;
              self.bump_key_version(key);
            }
          }
          out.write_integer(count);
        }
      }

      RespCommand::EXISTS => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'exists' command");
        } else {
          let mut count = 0i64;
          for &key in args {
            if self.store_session.contains_key(key).await? {
              count += 1;
            }
          }
          out.write_integer(count);
        }
      }

      RespCommand::MGET => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'mget' command");
        } else {
          // 批量路径：底层 12 项预取流水线批量读（mget_each），缺失/错型均 nil。
          // 先整体收集再统一写出——批量读回调不可回滚，磁盘收割失败时整体丢弃
          // 本命令部分响应后走错误处理，杜绝半截数组回报
          let vals = self.store_session.mget(args).await?;
          out.write_array_header(vals.len());
          for val in vals {
            match val {
              Some(v) => out.write_bulk_string(&v),
              None => out.write_null(),
            }
          }
        }
      }

      RespCommand::MSET => {
        if args.len() < 2 || !args.len().is_multiple_of(2) {
          out.write_error(b"ERR wrong number of arguments for 'mset' command");
        } else {
          for chunk in args.as_chunks::<2>().0 {
            self.store_session.upsert(chunk[0], chunk[1]).await?;
            self.bump_key_version(chunk[0]);
          }
          out.write_ok();
        }
      }

      // 哈希字典命令
      RespCommand::HSET => {
        if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
          out.write_error(b"ERR wrong number of arguments for 'hset' command");
        } else {
          let key = args[0];
          let pairs = args[1..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| (chunk[0], chunk[1]));
          let added = self.store_session.hmset(key, pairs).await?;
          self.bump_key_version(key);
          out.write_integer(added as i64);
        }
      }

      RespCommand::HGET => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'hget' command");
        } else {
          match self.store_session.hget(args[0], args[1]).await? {
            Some(v) => out.write_bulk_string(&v),
            None => out.write_null(),
          }
        }
      }

      RespCommand::HMSET => {
        if args.len() < 3 || !(args.len() - 1).is_multiple_of(2) {
          out.write_error(b"ERR wrong number of arguments for 'hmset' command");
        } else {
          let key = args[0];
          let pairs = args[1..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|chunk| (chunk[0], chunk[1]));
          self.store_session.hmset(key, pairs).await?;
          self.bump_key_version(key);
          out.write_ok();
        }
      }

      RespCommand::HMGET => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'hmget' command");
        } else {
          let res = self.store_session.hmget(args[0], &args[1..]).await?;
          out.write_array_header(res.len());
          for item in res {
            match item {
              Some(v) => out.write_bulk_string(&v),
              None => out.write_null(),
            }
          }
        }
      }

      RespCommand::HDEL => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'hdel' command");
        } else {
          let deleted = self.store_session.hdel(args[0], &args[1..]).await?;
          self.bump_key_version(args[0]);
          out.write_integer(deleted as i64);
        }
      }

      RespCommand::HLEN => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'hlen' command");
        } else {
          let len = self.store_session.hlen(args[0]).await?;
          out.write_integer(len as i64);
        }
      }

      RespCommand::HEXISTS => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'hexists' command");
        } else {
          let exists = self.store_session.hexists(args[0], args[1]).await?;
          out.write_integer(if exists { 1 } else { 0 });
        }
      }

      RespCommand::HGETALL => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'hgetall' command");
        } else {
          let pairs = self.store_session.hgetall(args[0]).await?;
          out.write_array_header(pairs.len() * 2);
          for (f, v) in pairs {
            out.write_bulk_string(&f);
            out.write_bulk_string(&v);
          }
        }
      }

      // 列表命令
      RespCommand::LPUSH => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'lpush' command");
        } else {
          let key = args[0];
          let count = self
            .store_session
            .lpush(key, args[1..].iter().copied())
            .await?;
          self.bump_key_version(key);
          out.write_integer(count as i64);
        }
      }

      RespCommand::RPUSH => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'rpush' command");
        } else {
          let key = args[0];
          let count = self
            .store_session
            .rpush(key, args[1..].iter().copied())
            .await?;
          self.bump_key_version(key);
          out.write_integer(count as i64);
        }
      }

      RespCommand::LPOP | RespCommand::RPOP => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'pop' command");
        } else {
          // count 参数校验（对齐 Redis：负数或非整数报错；count=0 短路返回空数组）
          let count = match args.get(1) {
            Some(&raw) => match ParseUtils::read_int(raw) {
              Ok(n) if n >= 0 => n as usize,
              _ => {
                out.write_error(b"ERR value is out of range, must be positive");
                return Ok(false);
              }
            },
            None => 1,
          };
          if count == 0 {
            out.write_array_header(0);
            return Ok(false);
          }
          let key = args[0];
          let items = if cmd == RespCommand::LPOP {
            self.store_session.lpop(key, count).await?
          } else {
            self.store_session.rpop(key, count).await?
          };
          if !items.is_empty() {
            self.bump_key_version(key);
          }
          write_pop_reply(out, items, args.len() > 1);
        }
      }

      RespCommand::LLEN => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'llen' command");
        } else {
          let len = self.store_session.llen(args[0]).await?;
          out.write_integer(len as i64);
        }
      }

      RespCommand::LRANGE => {
        if args.len() < 3 {
          out.write_error(b"ERR wrong number of arguments for 'lrange' command");
        } else {
          let (start, stop) = parse_range(args);
          let items = self.store_session.lrange(args[0], start, stop).await?;
          out.write_array_header(items.len());
          for it in items {
            out.write_bulk_string(&it);
          }
        }
      }

      // 集合命令
      RespCommand::SADD => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'sadd' command");
        } else {
          let key = args[0];
          let count = self
            .store_session
            .sadd(key, args[1..].iter().copied())
            .await?;
          self.bump_key_version(key);
          out.write_integer(count as i64);
        }
      }

      RespCommand::SMEMBERS => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'smembers' command");
        } else {
          let members = self.store_session.smembers(args[0]).await?;
          out.write_array_header(members.len());
          for m in members {
            out.write_bulk_string(&m);
          }
        }
      }

      RespCommand::SREM => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'srem' command");
        } else {
          let count = self.store_session.srem(args[0], &args[1..]).await?;
          self.bump_key_version(args[0]);
          out.write_integer(count as i64);
        }
      }

      RespCommand::SCARD => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'scard' command");
        } else {
          let count = self.store_session.scard(args[0]).await?;
          out.write_integer(count as i64);
        }
      }

      RespCommand::SISMEMBER => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'sismember' command");
        } else {
          let is_member = self.store_session.sismember(args[0], args[1]).await?;
          out.write_integer(if is_member { 1 } else { 0 });
        }
      }

      // 有序集合命令
      RespCommand::ZADD => {
        if args.len() < 3 {
          out.write_error(b"ERR wrong number of arguments for 'zadd' command");
        } else {
          // 前导选项解析（对齐 Redis/C# ZADD [NX|XX] [GT|LT] [CH] [INCR] 语法，
          // 选项 token 贪婪消费，存储引擎 zmadd 已原生支持全部选项语义）
          let mut opt = ZAddOpt::default();
          let mut incr = false;
          let mut i = 1;
          while let Some(&tok) = args.get(i) {
            if tok.eq_ignore_ascii_case(b"NX") {
              opt.nx = true;
            } else if tok.eq_ignore_ascii_case(b"XX") {
              opt.xx = true;
            } else if tok.eq_ignore_ascii_case(b"GT") {
              opt.gt = true;
            } else if tok.eq_ignore_ascii_case(b"LT") {
              opt.lt = true;
            } else if tok.eq_ignore_ascii_case(b"CH") {
              opt.ch = true;
            } else if tok.eq_ignore_ascii_case(b"INCR") {
              incr = true;
            } else {
              break;
            }
            i += 1;
          }
          let rest = &args[i..];
          if incr {
            out.write_error(b"ERR INCR option is not supported");
          } else if rest.len() < 2 || !rest.len().is_multiple_of(2) {
            out.write_error(b"ERR syntax error");
          } else {
            let mut pairs = Vec::with_capacity(rest.len() / 2);
            let mut valid = true;
            for chunk in rest.as_chunks::<2>().0 {
              match ParseUtils::read_double(chunk[0], true) {
                Ok(score) => pairs.push((score, chunk[1])),
                Err(_) => {
                  valid = false;
                  break;
                }
              }
            }
            if !valid {
              out.write_error(b"ERR value is not a valid float");
            } else {
              let key = args[0];
              let count = self.store_session.zmadd(key, pairs, opt).await?;
              self.bump_key_version(key);
              out.write_integer(count as i64);
            }
          }
        }
      }

      RespCommand::ZRANGE => {
        if args.len() < 3 {
          out.write_error(b"ERR wrong number of arguments for 'zrange' command");
        } else {
          let (start, stop) = parse_range(args);
          let with_scores = args.len() > 3 && args[3].eq_ignore_ascii_case(b"WITHSCORES");
          let items = self
            .store_session
            .zrange(args[0], start, stop, false)
            .await?;

          if with_scores {
            out.write_array_header(items.len() * 2);
            for (m, s) in items {
              out.write_bulk_string(&m);
              out.write_double_bulk(s);
            }
          } else {
            out.write_array_header(items.len());
            for (m, _) in items {
              out.write_bulk_string(&m);
            }
          }
        }
      }

      RespCommand::ZCARD => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'zcard' command");
        } else {
          let count = self.store_session.zcard(args[0]).await?;
          out.write_integer(count as i64);
        }
      }

      RespCommand::ZSCORE => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'zscore' command");
        } else {
          match self.store_session.zscore(args[0], args[1]).await? {
            Some(score) => {
              out.write_double_bulk(score);
            }
            None => out.write_null(),
          }
        }
      }

      RespCommand::ZREM => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'zrem' command");
        } else {
          let count = self.store_session.zrem(args[0], &args[1..]).await?;
          self.bump_key_version(args[0]);
          out.write_integer(count as i64);
        }
      }

      // 发布订阅命令
      RespCommand::SUBSCRIBE => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'subscribe' command");
        } else {
          for &ch in args {
            self.pubsub_broker.subscribe(ch, &self.pubsub_session);
            let count = self.pubsub_broker.session_subscription_count(self.id);
            out.write_sub_reply(b"subscribe", Some(ch), count);
          }
          self.refresh_pubsub_mode();
        }
      }

      RespCommand::UNSUBSCRIBE => {
        if args.is_empty() {
          let channels = self.pubsub_broker.list_all_subscriptions(self.id);
          if channels.is_empty() {
            out.write_sub_reply(b"unsubscribe", None, 0);
          } else {
            for ch in channels {
              self.pubsub_broker.unsubscribe(&ch, &self.pubsub_session);
              let count = self.pubsub_broker.session_subscription_count(self.id);
              out.write_sub_reply(b"unsubscribe", Some(&ch), count);
            }
          }
        } else {
          for &ch in args {
            self.pubsub_broker.unsubscribe(ch, &self.pubsub_session);
            let count = self.pubsub_broker.session_subscription_count(self.id);
            out.write_sub_reply(b"unsubscribe", Some(ch), count);
          }
        }
        self.refresh_pubsub_mode();
      }

      RespCommand::PSUBSCRIBE => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'psubscribe' command");
        } else {
          for &pat in args {
            self
              .pubsub_broker
              .pattern_subscribe(pat, &self.pubsub_session);
            let count = self.pubsub_broker.session_subscription_count(self.id);
            out.write_sub_reply(b"psubscribe", Some(pat), count);
          }
          self.refresh_pubsub_mode();
        }
      }

      RespCommand::PUNSUBSCRIBE => {
        if args.is_empty() {
          let patterns = self.pubsub_broker.list_all_pattern_subscriptions(self.id);
          if patterns.is_empty() {
            out.write_sub_reply(b"punsubscribe", None, 0);
          } else {
            for pat in patterns {
              self
                .pubsub_broker
                .pattern_unsubscribe(&pat, &self.pubsub_session);
              let count = self.pubsub_broker.session_subscription_count(self.id);
              out.write_sub_reply(b"punsubscribe", Some(&pat), count);
            }
          }
        } else {
          for &pat in args {
            self
              .pubsub_broker
              .pattern_unsubscribe(pat, &self.pubsub_session);
            let count = self.pubsub_broker.session_subscription_count(self.id);
            out.write_sub_reply(b"punsubscribe", Some(pat), count);
          }
        }
        self.refresh_pubsub_mode();
      }

      RespCommand::PUBLISH => {
        if args.len() < 2 {
          out.write_error(b"ERR wrong number of arguments for 'publish' command");
        } else {
          let count = self.pubsub_broker.publish_now(args[0], args[1]);
          out.write_integer(count as i64);
        }
      }

      RespCommand::PUBSUB => {
        if args.is_empty() {
          out.write_error(b"ERR wrong number of arguments for 'pubsub' command");
        } else if args[0].eq_ignore_ascii_case(b"CHANNELS") {
          let pat = args.get(1).copied();
          let chs = self.pubsub_broker.get_channels(pat);
          out.write_array_header(chs.len());
          for c in chs {
            out.write_bulk_string(&c);
          }
        } else if args[0].eq_ignore_ascii_case(b"NUMSUB") {
          out.write_array_header(args.len().saturating_sub(1) * 2);
          for &c in &args[1..] {
            out.write_bulk_string(c);
            let count = self.pubsub_broker.num_subscriptions(c);
            out.write_integer(count as i64);
          }
        } else if args[0].eq_ignore_ascii_case(b"NUMPAT") {
          let count = self.pubsub_broker.num_pattern_subscriptions();
          out.write_integer(count as i64);
        } else {
          out.write_error(b"ERR unknown subcommand or wrong number of arguments for 'pubsub'");
        }
      }

      _ => {
        out.write_error_fmt(format_args!("ERR unknown command '{cmd:?}'"));
      }
    }

    Ok(false)
  }
}
