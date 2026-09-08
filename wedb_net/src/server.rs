use std::{
  net::SocketAddr,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
};

use compio::runtime::spawn;
use log::info;
use parking_lot::Mutex;
use wdev::Device;
use wedb_acl::AccessControlList;
use wedb_pubsub::SubscribeBroker;
use wedb_txn::{DEFAULT_VERSION_MAP_CAPACITY, WatchVersionMap};
use wkv::WedbStore;

use crate::{
  config::NetConfig,
  error::{Error, Result},
  listener::NetListener,
  pool::LimitedFixedBufferPool,
};

/// 原生高性能网络服务器实例
///
/// 整合混合日志存储引擎、访问控制列表、乐观锁事务管理器与零拷贝发布订阅中继器，
/// 驱动纯异步非阻塞事件循环。
pub struct WedbServer<D: Device + Send + Sync + 'static> {
  /// 网络配置项
  pub config: Arc<NetConfig>,
  /// 混合日志存储引擎实例
  pub store: Arc<WedbStore<D>>,
  /// 访问控制列表用户权限管理器
  pub acl: Arc<AccessControlList>,
  /// 高并发发布订阅中继器
  pub pubsub_broker: Arc<SubscribeBroker>,
  /// 乐观并发控制键版本映射表
  pub version_map: Arc<WatchVersionMap>,
  /// 全局网络缓冲池
  pub network_pool: Arc<LimitedFixedBufferPool>,
  /// 内部监听器引用容器
  listener: Mutex<Option<Arc<NetListener>>>,
  /// 优雅停机信号标识
  is_shutdown: Arc<AtomicBool>,
}

impl<D: Device + Send + Sync + 'static> WedbServer<D> {
  /// 创建新的服务器实例
  pub fn new(config: NetConfig, store: Arc<WedbStore<D>>) -> Self {
    Self {
      config: Arc::new(config),
      store,
      acl: Arc::new(AccessControlList::default()),
      pubsub_broker: Arc::new(SubscribeBroker::new()),
      version_map: Arc::new(WatchVersionMap::new(DEFAULT_VERSION_MAP_CAPACITY)),
      network_pool: LimitedFixedBufferPool::default_pool(),
      listener: Mutex::new(None),
      is_shutdown: Arc::new(AtomicBool::new(false)),
    }
  }

  /// 注入自定义访问控制列表权限管理器
  #[inline]
  pub fn with_acl(mut self, acl: Arc<AccessControlList>) -> Self {
    self.acl = acl;
    self
  }

  /// 注入自定义发布订阅中继器
  #[inline]
  pub fn with_pubsub(mut self, pubsub_broker: Arc<SubscribeBroker>) -> Self {
    self.pubsub_broker = pubsub_broker;
    self
  }

  /// 注入自定义全局键版本映射表
  #[inline]
  pub fn with_version_map(mut self, version_map: Arc<WatchVersionMap>) -> Self {
    self.version_map = version_map;
    self
  }

  /// 注入自定义全局网络缓冲池
  #[inline]
  pub fn with_network_pool(mut self, network_pool: Arc<LimitedFixedBufferPool>) -> Self {
    self.network_pool = network_pool;
    self
  }

  /// 获取当前活跃客户端连接数
  #[inline]
  pub fn active_connections(&self) -> usize {
    self
      .listener
      .lock()
      .as_ref()
      .map_or(0, |l| l.active_connections())
  }

  /// 获取累计接收连接数
  #[inline]
  pub fn total_connections_received(&self) -> usize {
    self
      .listener
      .lock()
      .as_ref()
      .map_or(0, |l| l.total_connections_received())
  }

  /// 获取累计断开销毁连接数
  #[inline]
  pub fn total_connections_disposed(&self) -> usize {
    self
      .listener
      .lock()
      .as_ref()
      .map_or(0, |l| l.total_connections_disposed())
  }

  /// 重置累计接收连接数
  #[inline]
  pub fn reset_connections_received(&self) {
    if let Some(listener) = self.listener.lock().as_ref() {
      listener.reset_connections_received();
    }
  }

  /// 重置累计断开销毁连接数
  #[inline]
  pub fn reset_connections_disposed(&self) {
    if let Some(listener) = self.listener.lock().as_ref() {
      listener.reset_connections_disposed();
    }
  }

  /// 获取监听器绑定的本地套接字地址
  #[inline]
  pub fn local_addr(&self) -> Result<SocketAddr> {
    self
      .listener
      .lock()
      .as_ref()
      .map_or(Err(Error::ServerClosed), |l| l.local_addr())
  }

  /// 启动网络监听服务并在当前异步协程中持续服务，直至外部调用停机方法
  pub async fn serve(&self) -> Result<()> {
    let listener = Arc::new(
      NetListener::bind_with_pool(Arc::clone(&self.config), Arc::clone(&self.network_pool)).await?,
    );
    *self.listener.lock() = Some(Arc::clone(&listener));

    let addr = listener.local_addr()?;
    info!("WeDB 网络服务已启动, 监听地址: {}", addr);

    let store = Arc::clone(&self.store);
    let acl = Arc::clone(&self.acl);
    let pubsub = Arc::clone(&self.pubsub_broker);
    let vm = Arc::clone(&self.version_map);

    listener.run(store, acl, pubsub, vm).await
  }

  /// 启动网络监听服务并将其移入后台协程执行，返回实际绑定的本地套接字地址
  pub async fn start(&self) -> Result<SocketAddr> {
    let listener = Arc::new(
      NetListener::bind_with_pool(Arc::clone(&self.config), Arc::clone(&self.network_pool)).await?,
    );
    let local_addr = listener.local_addr()?;
    *self.listener.lock() = Some(Arc::clone(&listener));

    let store = Arc::clone(&self.store);
    let acl = Arc::clone(&self.acl);
    let pubsub = Arc::clone(&self.pubsub_broker);
    let vm = Arc::clone(&self.version_map);
    let runner_listener = Arc::clone(&listener);

    spawn(async move {
      let _ = runner_listener.run(store, acl, pubsub, vm).await;
    })
    .detach();

    info!("WeDB 网络服务后台任务已启动, 监听地址: {}", local_addr);
    Ok(local_addr)
  }

  /// 检查是否已销毁停机
  #[inline]
  pub fn is_disposed(&self) -> bool {
    self.is_shutdown.load(Ordering::SeqCst)
  }

  /// 发送优雅停机信号并关闭监听器
  pub fn shutdown(&self) {
    if !self.is_shutdown.swap(true, Ordering::SeqCst) {
      info!("收到停机信号, 正在关闭网络服务监听器...");
      if let Some(listener) = self.listener.lock().take() {
        listener.stop();
      }
    }
  }
}

impl<D: Device + Send + Sync + 'static> Drop for WedbServer<D> {
  fn drop(&mut self) {
    self.shutdown();
  }
}
