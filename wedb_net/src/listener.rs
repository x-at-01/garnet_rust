use std::{
  net::{SocketAddr, TcpStream as StdTcpStream},
  num::NonZeroUsize,
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
  },
  thread::{Builder as ThreadBuilder, available_parallelism},
  time::Duration,
};

use compio::{
  net::{TcpListener, TcpSocket},
  runtime::{CancelToken, Cancelled, FutureExt, spawn},
  time::sleep,
};
use crossfire::{
  MAsyncTx,
  mpsc::{Array, bounded_async},
};
use log::{debug, error, warn};
use parking_lot::Mutex;
use wdev::Device;
use wedb_acl::AccessControlList;
use wedb_pubsub::SubscribeBroker;
use wedb_txn::WatchVersionMap;
use wkv::WedbStore;

use crate::{
  config::NetConfig,
  connection::{NetConnection, ServerContext},
  error::{Error, Result},
  pool::LimitedFixedBufferPool,
};

/// 接入事件循环异常重试的退避间隔（毫秒），防止 fd 耗尽等持续错误导致忙等空转
const ACCEPT_RETRY_BACKOFF_MS: u64 = 10;
/// TCP 监听套接字等待队列容量
const TCP_LISTEN_BACKLOG: i32 = 1024;
/// 停机兜底唤醒单次 connect 的最坏等待上限（毫秒），与历史阻塞实现语义保持一致
const WAKE_CONNECT_TIMEOUT_MS: u64 = 20;
/// 停机兜底唤醒短命线程名（仅停机冷路径临时 spawn，一次停机至多一个）
const STOP_WAKE_THREAD_NAME: &str = "wedb-stop-wake";

/// 网络监听接入器，实现非阻塞连接接收与多连接并发分发
pub struct NetListener {
  /// 底层传输控制协议监听句柄
  listener: TcpListener,
  /// 网络配置引用
  config: Arc<NetConfig>,
  /// 全局网络缓冲池引用
  pub network_pool: Arc<LimitedFixedBufferPool>,
  /// 当前活跃连接数计数器
  active_connections: Arc<AtomicUsize>,
  /// 累计接收连接数
  total_received: Arc<AtomicUsize>,
  /// 累计断开销毁连接数
  total_disposed: Arc<AtomicUsize>,
  /// 会话标识自增生成器
  session_id_counter: Arc<AtomicU64>,
  /// 运行状态标志
  is_running: Arc<AtomicBool>,
  /// 接入取消信号发送通道 (驱动级优雅打断 accept 阻塞)
  cancel_tx: Arc<Mutex<Option<MAsyncTx<Array<()>>>>>,
}

impl NetListener {
  /// 绑定至指定配置与缓冲池的套接字地址并创建监听器
  pub async fn bind_with_pool(
    config: Arc<NetConfig>,
    network_pool: Arc<LimitedFixedBufferPool>,
  ) -> Result<Self> {
    let listener = TcpListener::bind(config.addr).await?;
    Ok(Self {
      listener,
      config,
      network_pool,
      active_connections: Arc::new(AtomicUsize::new(0)),
      total_received: Arc::new(AtomicUsize::new(0)),
      total_disposed: Arc::new(AtomicUsize::new(0)),
      session_id_counter: Arc::new(AtomicU64::new(1)),
      is_running: Arc::new(AtomicBool::new(true)),
      cancel_tx: Arc::new(Mutex::new(None)),
    })
  }

  /// 绑定至指定配置的套接字地址并创建监听器（使用全局默认缓冲池）
  pub async fn bind(config: Arc<NetConfig>) -> Result<Self> {
    Self::bind_with_pool(config, LimitedFixedBufferPool::default_pool()).await
  }

  /// 基于 SO_REUSEPORT 绑定至指定配置与缓冲池的套接字地址并创建监听器
  pub async fn bind_reuseport_with_pool(
    config: Arc<NetConfig>,
    network_pool: Arc<LimitedFixedBufferPool>,
  ) -> Result<Self> {
    let socket = if config.addr.is_ipv6() {
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
    socket.bind(config.addr).await.map_err(Error::Io)?;
    let listener = socket.listen(TCP_LISTEN_BACKLOG).await.map_err(Error::Io)?;

    Ok(Self {
      listener,
      config,
      network_pool,
      active_connections: Arc::new(AtomicUsize::new(0)),
      total_received: Arc::new(AtomicUsize::new(0)),
      total_disposed: Arc::new(AtomicUsize::new(0)),
      session_id_counter: Arc::new(AtomicU64::new(1)),
      is_running: Arc::new(AtomicBool::new(true)),
      cancel_tx: Arc::new(Mutex::new(None)),
    })
  }

  /// 基于 SO_REUSEPORT 绑定至指定配置的套接字地址并创建监听器（使用全局默认缓冲池）
  pub async fn bind_reuseport(config: Arc<NetConfig>) -> Result<Self> {
    Self::bind_reuseport_with_pool(config, LimitedFixedBufferPool::default_pool()).await
  }

  /// 获取监听器绑定的本地套接字地址
  #[inline]
  pub fn local_addr(&self) -> Result<SocketAddr> {
    self.listener.local_addr().map_err(Into::into)
  }

  /// 获取当前活跃客户端连接数
  #[inline]
  pub fn active_connections(&self) -> usize {
    self.active_connections.load(Ordering::Relaxed)
  }

  /// 获取累计接收连接数
  #[inline]
  pub fn total_connections_received(&self) -> usize {
    self.total_received.load(Ordering::Relaxed)
  }

  /// 获取累计销毁连接数
  #[inline]
  pub fn total_connections_disposed(&self) -> usize {
    self.total_disposed.load(Ordering::Relaxed)
  }

  /// 重置累计接收连接数
  #[inline]
  pub fn reset_connections_received(&self) {
    self.total_received.store(0, Ordering::Relaxed);
  }

  /// 重置累计销毁连接数
  #[inline]
  pub fn reset_connections_disposed(&self) {
    self.total_disposed.store(0, Ordering::Relaxed);
  }

  /// 停止监听器事件循环并唤醒挂起的接入协程
  ///
  /// 调用方为同步 `WedbServer::shutdown`/`Drop`，无法 await：cancel 通道 `try_send` 本就是
  /// 现成的打断机制，先行触发即可让挂起的 accept 协程立即退出；阻塞式兜底唤醒 connect
  /// 则挪至仅停机冷路径临时 spawn 的短命 std 线程执行——一次停机至多一个线程、
  /// 执行完即自行退出，不引入常规运行期常驻线程，也不阻塞调用方。
  pub fn stop(&self) {
    if self.is_running.swap(false, Ordering::SeqCst) {
      if let Some(tx) = self.cancel_tx.lock().take() {
        let _ = tx.try_send(());
      }
      if let Ok(addr) = self.local_addr() {
        let _ = ThreadBuilder::new()
          .name(STOP_WAKE_THREAD_NAME.into())
          .spawn(move || Self::wake_listeners_blocking(addr));
      }
    }
  }

  /// 阻塞式兜底唤醒所有监听该地址的 accept 循环（仅在停机短命线程内调用）
  fn wake_listeners_blocking(addr: SocketAddr) {
    let workers = available_parallelism().map_or(1, NonZeroUsize::get);
    for _ in 0..workers {
      let _ = StdTcpStream::connect_timeout(&addr, Duration::from_millis(WAKE_CONNECT_TIMEOUT_MS));
    }
  }

  /// 启动网络接入事件循环，阻塞直至监听器停止
  pub async fn run<D: Device + Send + Sync + 'static>(
    &self,
    store: Arc<WedbStore<D>>,
    acl: Arc<AccessControlList>,
    pubsub_broker: Arc<SubscribeBroker>,
    version_map: Arc<WatchVersionMap>,
  ) -> Result<()> {
    debug!("开始网络服务接入事件循环, 监听地址: {}", self.local_addr()?);

    let (tx, rx) = bounded_async::<()>(1);
    *self.cancel_tx.lock() = Some(tx);

    let cancel_token = CancelToken::new();
    let watcher_cancel = cancel_token.clone();
    spawn(async move {
      let _ = rx.recv().await;
      watcher_cancel.cancel();
    })
    .detach();

    let ctx = Arc::new(ServerContext {
      config: Arc::clone(&self.config),
      store,
      acl,
      pubsub_broker,
      version_map,
      network_pool: Arc::clone(&self.network_pool),
      is_running: Arc::clone(&self.is_running),
    });

    while self.is_running.load(Ordering::Relaxed) {
      let accept_res = self
        .listener
        .accept()
        .with_cancel(cancel_token.clone())
        .fail_fast()
        .await;

      match accept_res {
        Ok(Ok((stream, peer_addr))) => {
          if !self.is_running.load(Ordering::Relaxed) {
            drop(stream);
            break;
          }
          self.total_received.fetch_add(1, Ordering::Relaxed);
          let current = self.active_connections.load(Ordering::Relaxed);
          if current >= self.config.max_connections {
            warn!(
              "当前连接数已达上限 ({current} >= {}), 拒绝连接: {peer_addr}",
              self.config.max_connections
            );
            drop(stream);
            self.total_disposed.fetch_add(1, Ordering::Relaxed);
            continue;
          }

          self.active_connections.fetch_add(1, Ordering::Relaxed);
          let session_id = self.session_id_counter.fetch_add(1, Ordering::Relaxed);
          let active_conn = Arc::clone(&self.active_connections);
          let total_disposed = Arc::clone(&self.total_disposed);
          let conn_ctx = Arc::clone(&ctx);

          spawn(async move {
            debug!(
              "接受新客户端连接: session_id={}, 来源: {peer_addr}",
              session_id
            );
            if let Err(e) = NetConnection::handle(stream, session_id, conn_ctx).await {
              debug!("客户端连接处理结束: session_id={}, 信息: {e}", session_id);
            }
            active_conn.fetch_sub(1, Ordering::Relaxed);
            total_disposed.fetch_add(1, Ordering::Relaxed);
          })
          .detach();
        }
        Ok(Err(e)) => {
          if !self.is_running.load(Ordering::Relaxed) {
            break;
          }
          error!("TCP 监听接收新连接异常: {e}");
          // 退避后重试，杜绝持续异常（如 fd 耗尽）下的 accept 忙等空转
          sleep(Duration::from_millis(ACCEPT_RETRY_BACKOFF_MS)).await;
        }
        Err(Cancelled) => {
          debug!("监听到退出取消令牌, 接入循环立即终止");
          break;
        }
      }
    }

    debug!("网络服务接入循环已退出");
    Ok(())
  }
}
