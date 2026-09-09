#![cfg_attr(docsrs, feature(doc_cfg))]

//! WeDB 顶层服务守护进程 (`wedb_server`)
//!
//! 严格 1:1 对标微软 Garnet 宿主生命周期与网络架构，
//! 驱动纯 `compio` 全异步事件循环，集成存储、ACL、复制、集群、事务与发布订阅。

pub mod acl_storage;
pub mod config;
pub mod context;
pub mod dispatcher;
mod error;
pub mod modules;
pub mod ns_alloc;
pub mod range_index;
pub mod replication;
pub mod scripts;
pub mod session;
pub mod vectors;

use std::{
  fs::{create_dir_all, remove_file},
  mem::{replace, take},
  net::{IpAddr, SocketAddr, TcpStream as StdTcpStream},
  num::NonZeroUsize,
  os::unix::net::UnixStream as StdUnixStream,
  path::Path,
  rc::Rc,
  result,
  sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
  },
  thread::{Builder as ThreadBuilder, available_parallelism},
  time::Duration,
};

pub use acl_storage::StoreAclStorage;
pub use clap;
#[cfg(unix)]
use compio::net::{UnixListener, UnixStream};
use compio::{
  BufResult,
  buf::{IoBuf, IoBufMut},
  io::{AsyncRead, AsyncWriteExt},
  net::{TcpListener, TcpSocket, TcpStream},
  runtime::{CancelToken, Cancelled, FutureExt, Runtime, spawn},
  time::{sleep, timeout},
};
pub use config::ServerArgs;
use context::ServerContext;
use crossfire::{
  AsyncRx, AsyncTx, MAsyncTx, RecvTimeoutError,
  mpsc::{Array, bounded_async},
};
pub use dispatcher::{CommandDispatcher, FastPathResult};
pub use error::{Error, Result};
use futures_util::lock::Mutex as AsyncMutex;
use itoa::Buffer;
use log::{debug, error, info, warn};
use parking_lot::Mutex;
pub use session::ServerSession;
use socket2::{SockRef, TcpKeepalive};
use wcompact::LogCompactor;
use wedb_net::{PooledReceiveBuffer, SendBuffer};
use wedb_pubsub::create_session;
#[cfg(unix)]
use wedb_redis::prelude::*;
use wedb_repl::{ReplConfSubCmd, ReplicaCommand, SyncDecision, parse_replica_command};
use wedb_resp::{
  CRLF, Error as RespError, RespCommand, RespReadUtils, SessionMruCache, SessionParseState, consts,
  parse_session_command,
};

/// WeDB 顶层服务守护进程，集成网络监听与全栈生命周期
pub struct WedbServer {
  /// 全局共享上下文
  pub context: Arc<ServerContext>,
  /// 实际绑定的本地套接字地址（只写一次，完全无锁安全访问）
  local_addr: OnceLock<SocketAddr>,
  /// 会话唯一自增标识生成器
  session_id_counter: Arc<AtomicU64>,
  /// 优雅停机信号标志
  is_stopped: Arc<AtomicBool>,
  /// `run()` 主循环停机唤醒通道发送端：`stop()`/`close()` 翻转 `is_stopped` 成功后 `try_send` 即时唤醒等待
  ///
  /// 用多生产者端 `MAsyncTx`：`stop()` 与 `close()` 可自不同线程并发调用
  run_wake_tx: MAsyncTx<Array<()>>,
  /// `run()` 主循环停机唤醒通道接收端（语义上仅被首次 `run()` 取走，重入退化为纯超时兜底等待）
  run_wake_rx: Mutex<Option<AsyncRx<Array<()>>>>,
  /// 各工作线程接入循环的取消信号通道列表 (驱动级优雅打断 accept 阻塞)
  cancel_senders: Arc<Mutex<Vec<AsyncTx<Array<()>>>>>,
}

/// 客户端连接载体类型（支持 TCP 与 Unix 域套接字）
pub enum ConnectionStream {
  /// 标准 TCP 传输流
  Tcp(TcpStream),
  /// 本地 Unix 域套接字传输流
  #[cfg(unix)]
  Unix(UnixStream),
}

/// 会话网络读取器
enum SessionReader {
  Tcp(TcpStream),
  #[cfg(unix)]
  Unix(UnixStream),
}

impl SessionReader {
  #[inline]
  async fn read<B: IoBufMut>(&mut self, buf: B) -> BufResult<usize, B> {
    match self {
      Self::Tcp(s) => s.read(buf).await,
      #[cfg(unix)]
      Self::Unix(s) => s.read(buf).await,
    }
  }
}

/// 底层独占写发送器
enum DirectWriter {
  Tcp(TcpStream),
  #[cfg(unix)]
  Unix(UnixStream),
}

impl DirectWriter {
  #[inline]
  async fn write_all<B: IoBuf>(&mut self, buf: B) -> BufResult<(), B> {
    match self {
      Self::Tcp(s) => s.write_all(buf).await,
      #[cfg(unix)]
      Self::Unix(s) => s.write_all(buf).await,
    }
  }
}

/// 会话网络写发送器枚举（严格对标 C# Garnet networkSender 独占与按需共享模式）
enum SessionWriter {
  /// 独占直驱模式：99.999% 常规客户端连接零锁直写网络
  Direct(DirectWriter),
  /// 共享互斥模式：仅当会话激活发布订阅（SUBSCRIBE）时按需升级，安全支持跨协程消息并发推送
  Shared(Rc<AsyncMutex<DirectWriter>>),
  /// 状态转换临时占位
  Transitioning,
}

impl SessionWriter {
  #[inline]
  async fn write_all(&mut self, payload: Vec<u8>) -> BufResult<(), Vec<u8>> {
    match self {
      Self::Direct(ws) => ws.write_all(payload).await,
      Self::Shared(shared) => {
        let mut guard = shared.lock().await;
        guard.write_all(payload).await
      }
      Self::Transitioning => unreachable!(),
    }
  }
}

/// TCP 监听套接字等待队列容量
const TCP_LISTEN_BACKLOG: i32 = 1024;
/// 停机兜底唤醒单次 connect 的最坏等待上限（毫秒），与历史阻塞实现语义保持一致
const WAKE_CONNECT_TIMEOUT_MS: u64 = 20;
/// 停机兜底唤醒短命线程名（仅停机冷路径临时 spawn，一次停机至多一个）
const STOP_WAKE_THREAD_NAME: &str = "wedb-stop-wake";
/// `run()` 事件驱动等待的停机感知兜底超时：正常停机（`stop`/`close`）经唤醒通道即时退出；
/// 旁路直接翻转 `context.is_running`（公开 `Arc<AtomicBool>`，无法 hook 唤醒）最长经此间隔感知，
/// 同时将常态空转唤醒从历史 200ms 轮询的约 5 次/秒压到 1 次/秒
const RUN_FALLBACK_TIMEOUT: Duration = Duration::from_secs(1);

impl WedbServer {
  /// 根据命令行参数创建服务守护进程实例 (含 Checkpoint 崩溃恢复)
  pub async fn new(args: ServerArgs) -> Result<Self> {
    let context = Arc::new(ServerContext::new(args).await?);
    let (run_wake_tx, run_wake_rx) = bounded_async::<()>(1);
    Ok(Self {
      context,
      local_addr: OnceLock::new(),
      session_id_counter: Arc::new(AtomicU64::new(1)),
      is_stopped: Arc::new(AtomicBool::new(false)),
      run_wake_tx,
      run_wake_rx: Mutex::new(Some(run_wake_rx)),
      cancel_senders: Arc::new(Mutex::new(Vec::new())),
    })
  }

  /// 获取绑定的本地监听地址（若已启动）
  #[inline]
  pub fn local_addr(&self) -> Option<SocketAddr> {
    self.local_addr.get().copied()
  }

  /// 启动服务并在后台异步监听处理连接，返回实际绑定的本地套接字地址
  pub async fn start(&self) -> Result<SocketAddr> {
    self
      .start_threaded(Some(const { NonZeroUsize::new(1).unwrap() }))
      .await
  }

  /// 启动服务并在多核线程每核 (Thread-per-Core) 独立 compio 运行时与 SO_REUSEPORT 监听器上并发处理客户端连接
  ///
  /// 若 `worker_threads` 为 None，默认使用系统可用物理核心数 `available_parallelism`。
  /// 每个工作线程运行独立的 compio 事件循环与本地驱动，极大释放多核并发处理能力。
  pub async fn start_threaded(&self, worker_threads: Option<NonZeroUsize>) -> Result<SocketAddr> {
    let addr = if let Ok(ip) = self.context.args.bind.parse::<IpAddr>() {
      SocketAddr::new(ip, self.context.args.port)
    } else {
      // 主机名解析走 compio 异步解析（内部 spawn_blocking），避免 getaddrinfo 阻塞 reactor
      use compio::net::ToSocketAddrsAsync;
      (self.context.args.bind.as_str(), self.context.args.port)
        .to_socket_addrs_async()
        .await
        .map_err(Error::Io)?
        .next()
        .ok_or_else(|| Error::Custom("未能解析监听主机地址".into()))?
    };

    let primary_listener = bind_reuseport(addr).await?;
    let actual_addr = primary_listener.local_addr()?;
    let _ = self.local_addr.set(actual_addr);

    let nthreads = worker_threads.unwrap_or_else(|| {
      available_parallelism().unwrap_or(const { NonZeroUsize::new(1).unwrap() })
    });

    info!(
      "WeDB 守护进程多核网络监听已就绪 ({} 核心 SO_REUSEPORT): {}",
      nthreads, actual_addr
    );

    // 从底层存储加载初始化 ACL 默认用户持久化状态
    if let Err(e) = self
      .context
      .acl
      .init_from_storage(self.context.args.requirepass.as_deref())
      .await
    {
      error!("致命错误：从持久化存储初始化 ACL 状态失败: {e}");
      return Err(Error::Acl(e));
    }

    // 从节点复制常驻循环（--replicaof 时拉起：握手 → 效果帧摄取 → ACK，
    // 对标 Garnet ReplicationManager 的从侧同步任务）
    if let Some(primary) = self.context.args.replicaof.clone() {
      let replica_ctx = Arc::clone(&self.context);
      spawn(async move {
        if let Err(e) = replication::run_replica_loop(replica_ctx, &primary).await {
          error!("从节点复制循环异常退出: {e}");
        }
      })
      .detach();
    }

    let ctx = Arc::clone(&self.context);
    let id_gen = Arc::clone(&self.session_id_counter);
    let is_stopped = Arc::clone(&self.is_stopped);
    let (primary_tx, primary_rx) = bounded_async::<()>(1);
    self.cancel_senders.lock().push(primary_tx.into());

    spawn(Self::run_accept_loop(
      primary_listener,
      ctx,
      id_gen,
      is_stopped,
      primary_rx,
    ))
    .detach();

    // 周期自动紧缩后台任务（对标 Garnet StoreWrapper.StartPrimaryTasks 注册的
    // CompactionTask：CompactionFrequencySecs > 0 时按固定频率执行单轮紧缩）。
    // 参数唯一来源为底层 StoreConfig（server 级透传，不重复暴露配置项）。
    let compact_freq = self.context.store.config.compaction_freq_secs;
    if compact_freq > 0 {
      let ctx = Arc::clone(&self.context);
      let (compact_tx, compact_rx) = bounded_async::<()>(1);
      self.cancel_senders.lock().push(compact_tx.into());
      spawn(Self::run_compaction_task(compact_freq, ctx, compact_rx)).detach();
      info!(
        "周期自动紧缩任务已启动: 间隔={compact_freq}s, 单轮推进上限={}B",
        self.context.store.config.compaction_max_seek_bytes
      );
    }

    // AOF 周期提交后台任务（对标 Garnet CommitTaskAsync：CommitFrequencyMs > 0 时
    // 按固定频率把 AOF 缓冲提交落盘；0 = 每批应答前同步提交；-1 = 仅 SAVE/停机提交）
    if let Some(aof_commit_ms) = self.context.aof.periodic_commit_ms() {
      let ctx = Arc::clone(&self.context);
      let (aof_tx, aof_rx) = bounded_async::<()>(1);
      self.cancel_senders.lock().push(aof_tx.into());
      spawn(Self::run_aof_commit_task(aof_commit_ms, ctx, aof_rx)).detach();
      info!("AOF 周期提交任务已启动: 间隔={aof_commit_ms}ms");
    }

    for _ in 1..nthreads.get() {
      let ctx = Arc::clone(&self.context);
      let id_gen = Arc::clone(&self.session_id_counter);
      let is_stopped = Arc::clone(&self.is_stopped);
      let (worker_tx, worker_rx) = bounded_async::<()>(1);
      self.cancel_senders.lock().push(worker_tx.into());
      ThreadBuilder::new()
        .name("wedb-worker".into())
        .spawn(move || match Runtime::new() {
          Ok(rt) => {
            rt.block_on(async move {
              match bind_reuseport(actual_addr).await {
                Ok(listener) => {
                  Self::run_accept_loop(listener, ctx, id_gen, is_stopped, worker_rx).await;
                }
                Err(e) => {
                  error!("工作线程绑定 SO_REUSEPORT 端口失败: {}", e);
                }
              }
            });
          }
          Err(e) => {
            error!("工作线程创建 compio 运行时失败: {}", e);
          }
        })
        .map_err(Error::Io)?;
    }

    #[cfg(unix)]
    if let Some(ref u_path) = self.context.args.unixsocket {
      let path = Path::new(u_path);
      if path.exists() {
        let _ = remove_file(path);
      }
      if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
      {
        let _ = create_dir_all(parent);
      }
      let unix_listener = UnixListener::bind(path).await.map_err(Error::Io)?;
      info!("WeDB Unix 域套接字监听已就绪: {}", path.display());
      let ctx = Arc::clone(&self.context);
      let id_gen = Arc::clone(&self.session_id_counter);
      let is_stopped = Arc::clone(&self.is_stopped);
      let (unix_tx, unix_rx) = bounded_async::<()>(1);
      self.cancel_senders.lock().push(unix_tx.into());

      spawn(Self::run_unix_accept_loop(
        unix_listener,
        ctx,
        id_gen,
        is_stopped,
        unix_rx,
      ))
      .detach();
    }

    Ok(actual_addr)
  }

  /// 监听并接受客户端连接的通用事件循环（通过 CancelToken 支持完成式 I/O 驱动级秒级优雅取消）
  async fn run_accept_loop(
    listener: TcpListener,
    ctx: Arc<ServerContext>,
    id_gen: Arc<AtomicU64>,
    is_stopped: Arc<AtomicBool>,
    cancel_rx: AsyncRx<Array<()>>,
  ) {
    let cancel_token = CancelToken::new();
    let watcher_cancel = cancel_token.clone();
    spawn(async move {
      let _ = cancel_rx.recv().await;
      watcher_cancel.cancel();
    })
    .detach();

    while ctx.is_running() && !is_stopped.load(Ordering::Relaxed) {
      let accept_res = listener
        .accept()
        .with_cancel(cancel_token.clone())
        .fail_fast()
        .await;

      match accept_res {
        Ok(Ok((stream, client_addr))) => {
          if !ctx.is_running() || is_stopped.load(Ordering::Relaxed) {
            break;
          }
          debug!("接收到客户端连接: {}", client_addr);
          let session_id = id_gen.fetch_add(1, Ordering::Relaxed);
          let conn_ctx = Arc::clone(&ctx);
          spawn(async move {
            let _ =
              Self::handle_connection(ConnectionStream::Tcp(stream), session_id, conn_ctx).await;
          })
          .detach();
        }
        Ok(Err(e)) => {
          if !ctx.is_running() || is_stopped.load(Ordering::Relaxed) {
            break;
          }
          error!("接收客户端连接失败: {}", e);
        }
        Err(Cancelled) => {
          debug!("工作线程收到取消令牌, 立即退出接入事件循环");
          break;
        }
      }
    }
  }

  /// 副本推送任务晋升：应答行立即冲刷（先于任何帧流落盘），writer 升级共享
  /// 互斥模式，交接专属推送协程；注册副本会话驱动 ACK 安全水位
  /// （对标 Garnet TrySyncHandler 完成协商后的 HandleSyncStream 阶段）
  async fn promote_replica_push_task(
    ctx: Arc<ServerContext>,
    writer: &mut SessionWriter,
    send_buf: &mut SendBuffer,
    node_id: String,
    start_offset: u64,
    full_resync: bool,
  ) {
    let payload = send_buf.take();
    let BufResult(res, buf) = writer.write_all(payload).await;
    if res.is_ok() {
      send_buf.recycle(buf);
    }

    ctx.repl.register_replica(node_id, 0, String::new());

    let old = replace(writer, SessionWriter::Transitioning);
    if let SessionWriter::Direct(dw) = old {
      let shared = Rc::new(AsyncMutex::new(dw));
      spawn(Self::run_replica_push_task(
        ctx,
        Rc::clone(&shared),
        start_offset,
        full_resync,
      ))
      .detach();
      *writer = SessionWriter::Shared(shared);
    }
  }

  /// 副本推送协程：FullResync 时先发 `+SNAPSHOT <len>` 行，再把 AOF 全量帧
  /// （[begin, C0) 权威区间，原子快照采集，对标 C# diskless 全量阶段）直推
  /// 本副本专用 socket——不进共享积压缓冲，杜绝与实时写交错、对存量副本的
  /// 重复推送与容量淘汰死锁；随后自 C0 起轮询积压缓冲推送实时增量，
  /// 快照段与增量段以 C0 为界不重不漏
  async fn run_replica_push_task(
    ctx: Arc<ServerContext>,
    shared: Rc<AsyncMutex<DirectWriter>>,
    start_offset: u64,
    full_resync: bool,
  ) {
    let mut last = start_offset;
    if full_resync {
      let Some((snapshot_len, mut iter)) = ctx.aof.committed_snapshot() else {
        return;
      };
      let mut guard = shared.lock().await;
      let head = format!("+SNAPSHOT {snapshot_len}\r\n");
      let BufResult(res, _) = guard.write_all(head.into_bytes()).await;
      if res.is_err() {
        warn!("副本 SNAPSHOT 行写出失败（连接断开）");
        return;
      }
      let mut pushed = 0u64;
      loop {
        match iter.next().await {
          Ok(Some(rec)) => {
            // 快照段推完整记录帧（头 + 负载），与声明口径（逻辑地址字节）一致，
            // 从侧 enqueue_raw 保真落盘后本地日志与主逐字节一致
            let mut full_frame = rec.header.to_bytes().to_vec();
            full_frame.extend_from_slice(&rec.payload);
            let len = full_frame.len();
            let BufResult(res, _) = guard.write_all(full_frame).await;
            if res.is_err() {
              warn!("副本全量推送中断（连接断开）");
              return;
            }
            pushed += len as u64;
          }
          Ok(None) => break,
          Err(e) => {
            warn!("副本全量扫描失败: {e}");
            return;
          }
        }
      }
      debug_assert_eq!(pushed, snapshot_len, "快照字节数应与声明一致");
      last = snapshot_len;
    }

    while ctx.is_running() {
      if !ctx.repl.is_backlog_in_range(last) {
        // 请求位点已被环形积压淘汰，从节点须走全量重同步
        warn!(
          "副本位点 {} 超出积压缓冲保留区间 {:?}，断开以触发全量重同步",
          last,
          ctx.repl.backlog_offsets()
        );
        break;
      }
      let (_, tail) = ctx.repl.backlog_offsets();
      if tail > last {
        let data = ctx.repl.read_backlog(last, usize::MAX);
        let len = data.len() as u64;
        let mut guard = shared.lock().await;
        let BufResult(res, _) = guard.write_all(data).await;
        if res.is_err() {
          break;
        }
        drop(guard);
        last += len;
      }
      sleep(Duration::from_millis(10)).await;
    }
    debug!("副本推送协程退出: last={last}");
  }

  /// AOF 周期提交后台循环（对标 Garnet CommitTaskAsync）
  async fn run_aof_commit_task(
    commit_ms: u64,
    ctx: Arc<ServerContext>,
    cancel_rx: AsyncRx<Array<()>>,
  ) {
    let cancel_token = CancelToken::new();
    let watcher_cancel = cancel_token.clone();
    spawn(async move {
      let _ = cancel_rx.recv().await;
      watcher_cancel.cancel();
    })
    .detach();

    while ctx.is_running() {
      if let Err(e) = ctx.aof.commit().await {
        warn!("AOF 周期提交失败（不中断循环，下轮重试）: {e}");
      }
      // 间隔等待可被停机令牌即时打断（与紧缩任务同一套 CancelToken 机制）
      if sleep(Duration::from_millis(commit_ms))
        .with_cancel(cancel_token.clone())
        .fail_fast()
        .await
        .is_err()
      {
        break;
      }
    }
    debug!("AOF 周期提交任务收到停机信号, 优雅退出");
  }

  /// 周期自动紧缩后台任务（对标 Garnet `StoreWrapper.CompactionTaskAsync` 周期循环）
  ///
  /// 按固定间隔唤醒，自 begin_address 起单轮最多推进 `compaction_max_seek_bytes`
  /// 执行一轮 Lookup 惰性紧缩（[`LogCompactor::compact_lazy`]），滚动回收删除产生的
  /// 日志垃圾；停机令牌触发时优雅退出；单轮紧缩失败仅告警不中断循环，绝不 panic。
  async fn run_compaction_task(
    freq_secs: u64,
    ctx: Arc<ServerContext>,
    cancel_rx: AsyncRx<Array<()>>,
  ) {
    let cancel_token = CancelToken::new();
    let watcher_cancel = cancel_token.clone();
    spawn(async move {
      let _ = cancel_rx.recv().await;
      watcher_cancel.cancel();
    })
    .detach();

    let max_seek_bytes = ctx.store.config.compaction_max_seek_bytes;
    let compactor = LogCompactor::new(Arc::clone(&ctx.store));
    while ctx.is_running() {
      match compactor.compact_lazy(max_seek_bytes).await {
        Ok(stats) if !stats.is_empty() => info!(
          "周期紧缩完成: 扫描={}, 迁移={}, 丢弃={}, 释放={}B, 新起始地址={:#x}",
          stats.scanned_records,
          stats.live_copied,
          stats.dead_dropped,
          stats.bytes_freed,
          stats.new_begin_address
        ),
        Ok(_) => {}
        Err(e) => warn!("周期紧缩失败（不中断循环，下轮重试）: {e}"),
      }

      // 间隔等待可被停机令牌即时打断（与 accept 循环同一套 CancelToken 优雅停机机制）
      if sleep(Duration::from_secs(freq_secs))
        .with_cancel(cancel_token.clone())
        .fail_fast()
        .await
        .is_err()
      {
        break;
      }
    }
    debug!("周期紧缩任务收到停机信号, 优雅退出");
  }

  /// 监听并接受 Unix 域套接字客户端连接的专用事件循环（通过 CancelToken 支持完成式 I/O 驱动级秒级优雅取消）
  #[cfg(unix)]
  async fn run_unix_accept_loop(
    listener: UnixListener,
    ctx: Arc<ServerContext>,
    id_gen: Arc<AtomicU64>,
    is_stopped: Arc<AtomicBool>,
    cancel_rx: AsyncRx<Array<()>>,
  ) {
    let cancel_token = CancelToken::new();
    let watcher_cancel = cancel_token.clone();
    spawn(async move {
      let _ = cancel_rx.recv().await;
      watcher_cancel.cancel();
    })
    .detach();

    while ctx.is_running() && !is_stopped.load(Ordering::Relaxed) {
      let accept_res = listener
        .accept()
        .with_cancel(cancel_token.clone())
        .fail_fast()
        .await;

      match accept_res {
        Ok(Ok((stream, _))) => {
          if !ctx.is_running() || is_stopped.load(Ordering::Relaxed) {
            break;
          }
          debug!("接收到 Unix 域套接字客户端连接");
          let session_id = id_gen.fetch_add(1, Ordering::Relaxed);
          let conn_ctx = Arc::clone(&ctx);
          spawn(async move {
            let _ =
              Self::handle_connection(ConnectionStream::Unix(stream), session_id, conn_ctx).await;
          })
          .detach();
        }
        Ok(Err(e)) => {
          if !ctx.is_running() || is_stopped.load(Ordering::Relaxed) {
            break;
          }
          error!("接收 Unix 域套接字客户端连接失败: {}", e);
        }
        Err(Cancelled) => {
          debug!("Unix 监听循环收到取消令牌, 立即退出接入事件循环");
          break;
        }
      }
    }
  }

  /// 在当前异步流中阻塞运行主服务循环直至停机
  ///
  /// 事件驱动等待：正常停机由 `stop()`/`close()` 翻转 `is_stopped` 成功后经通道即时唤醒返回，
  /// 消除历史 200ms 轮询的停机延迟与空转唤醒；旁路直接翻转 `context.is_running`
  /// （公开 `Arc<AtomicBool>`，无法 hook）由 `RUN_FALLBACK_TIMEOUT` 兜底感知。
  pub async fn run(&self) -> Result<()> {
    let _ = self.start().await?;
    // 接收端仅被首次 run() 取走；重入时退化为纯超时兜底等待，停机语义不变
    let wake_rx = self.run_wake_rx.lock().take();
    while self.context.is_running() && !self.is_stopped.load(Ordering::Relaxed) {
      let Some(rx) = wake_rx.as_ref() else {
        sleep(RUN_FALLBACK_TIMEOUT).await;
        continue;
      };
      // 发送端存于实例自身，run() 借用 &self 期间实例不可能 drop，
      // 断开分支仅剩防御意义：直接终结等待而非报错或挂死
      if matches!(
        rx.recv_with_timer(sleep(RUN_FALLBACK_TIMEOUT)).await,
        Err(RecvTimeoutError::Disconnected)
      ) {
        break;
      }
    }
    Ok(())
  }

  /// 停机公共前置：停止上下文并广播 cancel 信号，驱动级打断所有挂起的 accept 协程与 `run()` 停机等待
  fn cancel_accept_loops(&self) {
    self.context.stop();
    for tx in take(&mut *self.cancel_senders.lock()) {
      let _ = tx.try_send(());
    }
    // 容量 1 的纯唤醒信号：停机入口由 swap 互斥保证至多执行一次，满即代表信号已在途未消费（防御分支），静默忽略
    let _ = self.run_wake_tx.try_send(());
  }

  /// 异步停机：唤醒并终止所有监听事件循环，清理网络资源与套接字文件
  ///
  /// 运行于 compio runtime 之上，兜底唤醒 connect 全程异步化（compio connect + 超时），零 reactor 阻塞。
  async fn stop_listeners(&self) {
    self.cancel_accept_loops();
    if let Some(addr) = self.local_addr() {
      Self::wake_listeners(addr).await;
    }
    #[cfg(unix)]
    if let Some(ref path) = self.context.args.unixsocket {
      Self::wake_unix_listener(path).await;
      let _ = remove_file(path);
    }
  }

  /// 同步停机兜底（`close`/`dispose`/`Drop` 无法 await）：
  /// cancel 通道 `try_send` 本就是现成的打断机制，先行触发即可让挂起的 accept 协程立即退出；
  /// 阻塞式兜底唤醒 connect 则挪至仅停机冷路径临时 spawn 的短命 std 线程执行——
  /// 一次停机至多一个线程、执行完即自行退出，不引入常规运行期常驻线程，也不阻塞调用方。
  fn stop_listeners_sync(&self) {
    self.cancel_accept_loops();
    let addr = self.local_addr();
    #[cfg(unix)]
    let unix_path = self.context.args.unixsocket.clone();
    let _ = ThreadBuilder::new()
      .name(STOP_WAKE_THREAD_NAME.into())
      .spawn(move || {
        if let Some(addr) = addr {
          Self::wake_listeners_blocking(addr);
        }
        #[cfg(unix)]
        if let Some(path) = unix_path {
          let _ = StdUnixStream::connect(path);
        }
      });
    #[cfg(unix)]
    if let Some(ref path) = self.context.args.unixsocket {
      let _ = remove_file(path);
    }
  }

  /// 优雅停机并执行数据持久化刷盘
  pub async fn stop(&self) -> Result<()> {
    if !self.is_stopped.swap(true, Ordering::SeqCst) {
      info!("收到停机指令, 正在执行优雅停机...");
      self.stop_listeners().await;
      if let Err(e) = self.context.aof.commit().await {
        warn!("停机 AOF 提交失败: {e}");
      }
      self.context.store.flush_all().await?;
      info!("底层数据落盘同步完毕, 守护进程已安全停止.");
    }
    Ok(())
  }

  /// 异步唤醒所有监听该地址的 accept 循环以使其优雅退出
  ///
  /// 并发派发 compio 异步 connect（每连接 20ms 超时兜底），最坏总耗时即单次超时上限；
  /// 失败静默忽略，与历史阻塞实现语义一致，仅消除 reactor 阻塞。
  async fn wake_listeners(addr: SocketAddr) {
    let workers = available_parallelism().map_or(1, NonZeroUsize::get);
    let wakes: Vec<_> = (0..workers)
      .map(|_| {
        spawn(timeout(
          Duration::from_millis(WAKE_CONNECT_TIMEOUT_MS),
          TcpStream::connect(addr),
        ))
      })
      .collect();
    for wake in wakes {
      let _ = wake.await;
    }
  }

  /// 阻塞式兜底唤醒所有监听该地址的 accept 循环（仅在停机短命线程内调用）
  fn wake_listeners_blocking(addr: SocketAddr) {
    let workers = available_parallelism().map_or(1, NonZeroUsize::get);
    for _ in 0..workers {
      let _ = StdTcpStream::connect_timeout(&addr, Duration::from_millis(WAKE_CONNECT_TIMEOUT_MS));
    }
  }

  /// 异步唤醒 Unix 域套接字监听以使其优雅退出（compio 异步 connect + 超时，失败静默忽略）
  #[cfg(unix)]
  async fn wake_unix_listener(path: &str) {
    let _ = timeout(
      Duration::from_millis(WAKE_CONNECT_TIMEOUT_MS),
      UnixStream::connect(path),
    )
    .await;
  }

  /// 返回服务版本号（对标 Garnet GetVersion，取自 Cargo 包版本元数据）
  #[inline]
  pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
  }

  /// 从命令行参数切片构造服务实例（对标 GarnetServer(string[] args)）
  pub async fn from_argv(argv: &[&str]) -> Result<Self> {
    let args = ServerArgs::from_argv(argv)?;
    Self::new(args).await
  }

  /// 检查服务是否已停止运行
  #[inline]
  pub fn is_disposed(&self) -> bool {
    !self.context.is_running() || self.is_stopped.load(Ordering::Relaxed)
  }

  /// 关闭服务连接（对标 Garnet Dispose/Close）
  ///
  /// 同步入口无法 await：走同步停机兜底（cancel 信号先行 + 短命线程执行阻塞唤醒 connect）。
  #[inline]
  pub fn close(&self) {
    if !self.is_stopped.swap(true, Ordering::SeqCst) {
      self.stop_listeners_sync();
    }
  }

  /// 销毁并释放服务资源（对标 Garnet Dispose）
  #[inline]
  pub fn dispose(&self) {
    self.close();
  }

  /// 注册命令扩展（对标 Garnet RegisterExtensions）
  #[inline]
  pub fn register_extensions(&self) {}

  /// 获取当前活跃客户端连接数（对标 Garnet ActiveConsumers / get_conn_active）
  #[inline]
  pub fn active_connections(&self) -> usize {
    self.context.active_connections()
  }

  /// 获取当前活跃客户端连接数（对标 Garnet get_conn_active）
  #[inline]
  pub fn get_conn_active(&self) -> usize {
    self.context.active_connections()
  }

  /// 获取当前活跃消费者会话数（对标 Garnet ActiveConsumers）
  #[inline]
  pub fn active_consumers(&self) -> usize {
    self.context.active_connections()
  }

  /// 获取当前活跃集群会话数（对标 Garnet ActiveClusterSessions）
  #[inline]
  pub fn active_cluster_sessions(&self) -> usize {
    0
  }

  /// 获取累计接收连接数（对标 Garnet get_TotalConnectionsReceived）
  #[inline]
  pub fn total_connections_received(&self) -> usize {
    self.context.total_connections_received()
  }

  /// 获取累计销毁连接数（对标 Garnet get_TotalConnectionsDisposed）
  #[inline]
  pub fn total_connections_disposed(&self) -> usize {
    self.context.total_connections_disposed()
  }

  /// 重置累计接收连接数（对标 Garnet ResetConnectionsReceived）
  #[inline]
  pub fn reset_connections_received(&self) {
    self.context.reset_connections_received();
  }

  /// 重置累计断开销毁连接数（对标 Garnet ResetConnectionsDiposed）
  #[inline]
  pub fn reset_connections_disposed(&self) {
    self.context.reset_connections_disposed();
  }

  /// 销毁所有活跃连接处理器（对标 Garnet DisposeActiveHandlers）
  #[inline]
  pub fn dispose_active_handlers(&self) {
    self.close();
  }

  /// 清理缓冲池或内部资源（对标 Garnet Purge）
  #[inline]
  pub fn purge(&self) {
    self.context.network_pool.purge();
  }

  /// 处理单个客户端连接的全双工收发生命周期 (支持 TCP 与 Unix 域套接字)
  async fn handle_connection(
    stream: ConnectionStream,
    session_id: u64,
    ctx: Arc<ServerContext>,
  ) -> Result<()> {
    ctx.active_connections.fetch_add(1, Ordering::Relaxed);
    ctx.total_received.fetch_add(1, Ordering::Relaxed);

    let (mut read_stream, write_stream) = match stream {
      ConnectionStream::Tcp(s) => {
        let _ = s.set_nodelay(true);
        #[cfg(unix)]
        {
          let keepalive = TcpKeepalive::new()
            .with_time(Duration::from_secs(60))
            .with_interval(Duration::from_secs(10));
          let _ = SockRef::from(&s).set_tcp_keepalive(&keepalive);
        }
        let (r, w) = s.into_split();
        (SessionReader::Tcp(r), DirectWriter::Tcp(w))
      }
      #[cfg(unix)]
      ConnectionStream::Unix(s) => {
        let (r, w) = s.into_split();
        (SessionReader::Unix(r), DirectWriter::Unix(w))
      }
    };
    let (pubsub_session, pubsub_rx) = create_session(session_id, 1024);
    let store_session = Arc::new(ctx.store.new_session()?);
    let mut session = ServerSession::new(
      session_id,
      ctx.args.requirepass.is_some(),
      store_session,
      pubsub_session,
    );

    let mut writer = SessionWriter::Direct(write_stream);
    let mut pubsub_rx_opt = Some(pubsub_rx);
    // 从节点复制会话标识（REPLCONF ip-address/listening-port 汇报，ACK 回填键）
    let mut replica_node_id: Option<String> = None;

    let mut recv_buf =
      PooledReceiveBuffer::new(Arc::clone(&ctx.network_pool), 4096, 16 * 1024 * 1024);
    let mut send_buf = SendBuffer::new(4096);
    let mut mru_cache = SessionMruCache::default();

    loop {
      if !ctx.is_running() || session.is_closed {
        break;
      }

      if recv_buf.ensure_read_capacity(1024).is_err() {
        break;
      }
      let raw_buf = recv_buf.take_for_read();
      let BufResult(res, returned_buf) = read_stream.read(raw_buf).await;
      recv_buf.put_after_read(returned_buf);

      match res {
        Ok(0) => break,
        Ok(_n) => {}
        Err(_) => break,
      };

      let mut should_break = false;
      loop {
        let mut slice = recv_buf.unparsed_slice();
        if slice.is_empty() {
          break;
        }
        let orig_len = slice.len();
        let mut parse_state = SessionParseState::with_mru(mru_cache);
        let (cmd, consumed, parse_err) = match parse_session_command(&mut slice, &mut parse_state) {
          Ok(c) => {
            let consumed = orig_len - slice.len();
            mru_cache = parse_state.mru();
            (Some(c), consumed, None)
          }
          Err(RespError::Incomplete) => {
            // 数据未完整接收，等待更多网络数据到达
            break;
          }
          Err(RespError::UnknownCommand(ref name)) => {
            // 兼容从节点复制握手协议 REPLCONF 与 PSYNC
            if let Ok(Some((rep_cmd, rep_consumed))) =
              parse_replica_command(recv_buf.unparsed_slice())
            {
              match rep_cmd {
                ReplicaCommand::ReplConf(sub) => match sub {
                  ReplConfSubCmd::Ack(offset) => {
                    // 心跳位点回填（对标 Garnet UpdateReplicaAck），驱动安全水位推进；
                    // ACK 必须静默——任何应答字节都会污染从侧帧流造成永久错位
                    if let Some(id) = &replica_node_id {
                      ctx.repl.update_replica_ack(id, offset);
                    }
                  }
                  ReplConfSubCmd::IpAddress(ip) => {
                    let entry = replica_node_id.get_or_insert_with(|| ip.clone());
                    if !entry.contains(':') {
                      *entry = format!("{ip}:0");
                    }
                    send_buf.write_ok();
                  }
                  ReplConfSubCmd::ListeningPort(port) => {
                    let entry = replica_node_id.get_or_insert_with(|| format!("unknown:{port}"));
                    if let Some(idx) = entry.rfind(':') {
                      *entry = format!("{}:{port}", &entry[..idx]);
                    }
                    send_buf.write_ok();
                  }
                  _ => {
                    send_buf.write_ok();
                  }
                },
                ReplicaCommand::Psync { replid, offset } => {
                  // 副本协商须过认证闸：未认证连接不得经 PSYNC 拉取全量数据
                  if !session.authenticated {
                    send_buf.write_error(b"NOAUTH Authentication required.");
                  } else {
                  // 复制协商交由 ReplicationManager::handle_psync 判定增量/全量；
                  // 位点空间 = 复制积压缓冲字节位点（AOF 帧经推流端口喂入），
                  // 与主→从流载荷自洽（对标 Garnet TrySyncHandler）
                  let (wal_begin, wal_tail) = ctx.repl.backlog_offsets();
                  match ctx
                    .repl
                    .handle_psync(replid.as_str(), offset, wal_begin, wal_tail)
                  {
                    Ok(SyncDecision::PartialResync {
                      replid,
                      start_offset,
                    }) => {
                      info!("副本增量接续: start_offset={start_offset}");
                      send_buf.write_raw(b"+CONTINUE ");
                      send_buf.write_raw(replid.as_bytes());
                      send_buf.write_raw(CRLF);
                      Self::promote_replica_push_task(
                        Arc::clone(&ctx),
                        &mut writer,
                        &mut send_buf,
                        replica_node_id.clone().unwrap_or_else(|| "unknown".into()),
                        start_offset,
                        false,
                      )
                      .await;
                    }
                    Ok(SyncDecision::FullResync {
                      replid,
                      snapshot_offset,
                    }) => {
                      send_buf.write_raw(b"+FULLRESYNC ");
                      send_buf.write_raw(replid.as_bytes());
                      send_buf.write_raw(b" ");
                      let mut num_buf = Buffer::new();
                      send_buf.write_raw(num_buf.format(snapshot_offset).as_bytes());
                      send_buf.write_raw(CRLF);
                      Self::promote_replica_push_task(
                        Arc::clone(&ctx),
                        &mut writer,
                        &mut send_buf,
                        replica_node_id.clone().unwrap_or_else(|| "unknown".into()),
                        0,
                        true,
                      )
                      .await;
                    }
                    Err(e) => {
                      warn!("PSYNC 同步协商失败: err={e}");
                      send_buf.write_error_fmt(format_args!("ERR {e}"));
                    }
                  }
                  }
                }
                ReplicaCommand::Ping => {
                  send_buf.write_pong();
                }
                ReplicaCommand::Auth(_) => {
                  send_buf.write_ok();
                }
              }
              (None, rep_consumed, None)
            } else {
              match skip_unknown_command(recv_buf.unparsed_slice()) {
                Ok(skip_len) => {
                  // 模块自定义命令路由：注册表命中则就地执行（须已认证且非事务态，
                  // 对标 Garnet CustomProcedure 非事务执行约定），否则回退标准 unknown command
                  let routed = session.authenticated
                    && !session.in_txn
                    && modules::try_route_module_command(
                      &ctx,
                      &mut session,
                      name.as_bytes(),
                      recv_buf.unparsed_slice(),
                      &mut send_buf,
                    )?;
                  if !routed {
                    if session.in_txn {
                      session.txn_aborted = true;
                    }
                    if !session.authenticated {
                      send_buf.write_error(b"NOAUTH Authentication required.");
                    } else {
                      send_buf.write_error_fmt(format_args!("ERR unknown command '{name}'"));
                    }
                  }
                  (None, skip_len, None)
                }
                Err(RespError::Incomplete) => {
                  // 未知命令报文跨包未完整，等待更多网络数据
                  break;
                }
                Err(e) => (None, 0, Some(e)),
              }
            }
          }
          Err(e) => (None, 0, Some(e)),
        };

        if let Some(e) = parse_err {
          send_buf.write_error_fmt(format_args!("ERR Protocol error: {e:?}"));
          should_break = true;
          break;
        }

        let mut executed_get = false;
        if let Some(cmd) = cmd {
          if cmd == RespCommand::NONE {
            // 空行或空报文直接忽略
          } else {
            executed_get = cmd == RespCommand::GET;
            match CommandDispatcher::try_dispatch_sync(
              &ctx,
              &mut session,
              cmd,
              &parse_state,
              &mut send_buf,
            ) {
              FastPathResult::Handled => {}
              FastPathResult::NeedsAsync => {
                if let Err(e) =
                  CommandDispatcher::dispatch(&ctx, &mut session, cmd, &parse_state, &mut send_buf)
                    .await
                {
                  if e.is_wrong_type() {
                    send_buf.write_error(
                      b"WRONGTYPE Operation against a key holding the wrong kind of value",
                    );
                  } else {
                    send_buf.write_error_fmt(format_args!("ERR Dispatch error: {e}"));
                    should_break = true;
                    break;
                  }
                }
              }
            }
          }
          drop(parse_state);
        }

        if consumed == 0 {
          // 防御性跳出，防止零消费导致死循环
          break;
        }

        recv_buf.advance(consumed);

        if session.is_closed {
          should_break = true;
          break;
        }

        // 严格对照 C# Garnet NetworkGET_SG (BasicCommands.cs:208) 流水线前瞻投机批处理：
        // 若网络接收缓冲区后续紧跟连续 GET 命令，在非事务状态下直接进行前瞻解包并极速批量处理，
        // 跳过逐命令的状态机重构与异步调度开销！
        // 集群模式下必须禁用：投机路径绕过 MOVED/ASK 槽位路由，跨槽键会被错误地本地直读
        if executed_get && !session.in_txn && session.authenticated && ctx.cluster.is_none() {
          Self::speculative_pipeline_get(&mut session, &mut recv_buf, &mut send_buf).await;
          if session.is_closed {
            should_break = true;
            break;
          }
        }
      }

      recv_buf.compact();

      // 当会话激活了发布订阅模式时，按需将网络发送器平滑升级为 Shared 互斥模式并启动后台推送转发协程
      if session.is_subscription_session && matches!(writer, SessionWriter::Direct(_)) {
        let old_writer = replace(&mut writer, SessionWriter::Transitioning);
        if let SessionWriter::Direct(direct_ws) = old_writer {
          let shared = Rc::new(AsyncMutex::new(direct_ws));
          let pubsub_writer = shared.clone();
          if let Some(rx) = pubsub_rx_opt.take() {
            spawn(async move {
              while let Ok(msg) = rx.recv().await {
                let bytes = msg.to_resp2_bytes();
                let mut guard = pubsub_writer.lock().await;
                let BufResult(res, _) = guard.write_all(bytes).await;
                if res.is_err() {
                  break;
                }
              }
            })
            .detach();
          }
          writer = SessionWriter::Shared(shared);
        }
      }

      if !send_buf.is_empty() {
        // AOF 每批应答前提交落盘（0 档策略下沉于门面，对标 Garnet AofAutoCommit）：
        // 一次批刷盘确认整条 pipeline 的写效果，确保应答即持久
        ctx.aof.commit_before_response().await;
        let payload = send_buf.take();
        let BufResult(res, returned_buf) = writer.write_all(payload).await;
        if res.is_err() {
          break;
        }
        send_buf.recycle(returned_buf);
      }

      if should_break {
        break;
      }
    }

    if !send_buf.is_empty() {
      let payload = send_buf.take();
      let _ = writer.write_all(payload).await;
    }

    drop(send_buf);
    ctx.pubsub.remove_subscription(session_id);
    ctx.blocking.handle_session_disposed(session_id);
    drop(session);

    ctx.active_connections.fetch_sub(1, Ordering::Relaxed);
    ctx.total_disposed.fetch_add(1, Ordering::Relaxed);

    Ok(())
  }

  /// 对照 C# Garnet NetworkGET_SG 与 NextCommandMaybeGet (BasicCommands.cs:1962, 2000)
  ///
  /// 流水线前瞻投机批处理：检查接收缓冲区后续是否紧随标准 RESP `GET` 指令序列
  /// (`*2\r\n$3\r\nGET\r\n` 或 `*2\r\n$3\r\nget\r\n`)。
  /// 命中时直接就地快速解析 key，尝试同步内存直读 (`try_read_string_in_memory`)，
  /// 彻底绕过逐命令的顶层命令分发、解析器状态机重置与异步状态机切换开销。
  async fn speculative_pipeline_get(
    session: &mut ServerSession,
    recv_buf: &mut PooledReceiveBuffer,
    send_buf: &mut SendBuffer,
  ) {
    const PREFIX: &[u8] = b"*2\r\n$3\r\n";
    const PREFIX_LEN: usize = 13; // "*2\r\n$3\r\nGET\r\n".len()
    // 严格对照 C# Garnet TryConsumeMessages / UnsafeContext 批处理模式：
    // 在进入流水线前瞻批处理外层持有单一纪元保护，
    // 循环内部的所有 GET 直读彻底消除 enter/exit 原子指令与 TLS 寻址开销！
    let batch = session.store_session.enter_batch();

    loop {
      if session.is_closed {
        break;
      }
      let slice = recv_buf.unparsed_slice();
      if slice.len() < PREFIX_LEN {
        break;
      }
      if !slice.starts_with(PREFIX) {
        break;
      }
      if !slice[8..11].eq_ignore_ascii_case(b"GET") || slice[11] != b'\r' || slice[12] != b'\n' {
        break;
      }

      // 快速解析 key: 期望紧跟 $key_len\r\n<key>\r\n
      let rest = &slice[PREFIX_LEN..];
      let Some((key, consumed_key)) = RespReadUtils::try_slice_with_length_header(rest).ok() else {
        // 数据跨包未完整或格式特殊，退出投机快路径交由通用解析器处理
        break;
      };

      let total_consumed = PREFIX_LEN + consumed_key;

      // TTL 探针前置：投机直读是 raw 内存读、不经 store 层 read_with 的惰性过期裁决，
      // 必须先同步探测 TTL——已过期或 TTL 记录冷在磁盘（探针存疑）时跳过直读，
      // 杜绝投机直读把过期脏值写入响应缓冲
      let mut synced = false;
      if dispatcher::sync_ttl_expired(
        &session.store_session,
        key,
        coarsetime::Clock::now_since_epoch().as_millis(),
      ) == Some(false)
      {
        // 首先尝试纯同步 DRAM 内存直读快路径（完全零原子纪元进出开销）
        match batch.try_read_string_in_memory(key, |val| send_buf.write_bulk_string(val)) {
          Ok(Some(Some(()))) => synced = true,
          Ok(Some(None)) => {
            // 确切不存在（例如墓碑或空桶）
            send_buf.write_null();
            synced = true;
          }
          _ => {}
        }
      }
      if !synced {
        // 异步零拷贝读：store 层 read_with 已将过期裁决前移到读闭包执行前，
        // 过期键经 check_expired 惰性物理清除后回 nil，不会触碰响应缓冲
        match session
          .store_session
          .read_string_with(key, |val| send_buf.write_bulk_string(val))
          .await
        {
          Ok(Some(())) => {}
          Ok(None) => send_buf.write_null(),
          Err(err) if err.is_wrong_type() => {
            send_buf.write_error(consts::err::WRONG_TYPE);
          }
          Err(err) => {
            send_buf.write_error_fmt(format_args!("ERR {err}"));
            recv_buf.advance(total_consumed);
            break;
          }
        }
      }

      recv_buf.advance(total_consumed);
    }
  }
}

/// 扫描并跳过未知命令报文（无论是 RESP 数组还是内联文本行，零堆分配）
fn skip_unknown_command(mut input: &[u8]) -> result::Result<usize, RespError> {
  if input.is_empty() {
    return Err(RespError::Incomplete);
  }
  let orig_len = input.len();
  if input[0] == b'*' {
    let (array_len, header_len) = match RespReadUtils::try_read_signed_array_len(input)? {
      (Some(len), h) => (len, h),
      (None, h) => return Ok(h),
    };
    input = &input[header_len..];
    for _ in 0..array_len {
      let (_, bytes) = RespReadUtils::try_slice_with_length_header(input)?;
      input = &input[bytes..];
    }
    Ok(orig_len - input.len())
  } else {
    match find_crlf(input) {
      Some(pos) => Ok(pos + 2),
      None => Err(RespError::Incomplete),
    }
  }
}

/// 高速定位 CRLF 终止标记
#[inline]
fn find_crlf(input: &[u8]) -> Option<usize> {
  let mut pos = 0;
  while let Some(idx) = memchr::memchr(b'\r', &input[pos..]) {
    let actual_idx = pos + idx;
    if actual_idx + 1 < input.len() && input[actual_idx + 1] == b'\n' {
      return Some(actual_idx);
    }
    pos = actual_idx + 1;
  }
  None
}

impl Drop for WedbServer {
  fn drop(&mut self) {
    self.close();
  }
}

/// 创建支持多核并发监听的套接字 (SO_REUSEPORT)
async fn bind_reuseport(addr: SocketAddr) -> Result<TcpListener> {
  let socket = if addr.is_ipv6() {
    TcpSocket::new_v6().await
  } else {
    TcpSocket::new_v4().await
  }
  .map_err(Error::Io)?;

  socket.set_reuseaddr(true).map_err(Error::Io)?;
  #[cfg(all(
    unix,
    not(target_os = "solaris"),
    not(target_os = "illumos"),
    not(target_os = "cygwin")
  ))]
  socket.set_reuseport(true).map_err(Error::Io)?;

  socket.bind(addr).await.map_err(Error::Io)?;
  socket.listen(TCP_LISTEN_BACKLOG).await.map_err(Error::Io)
}
