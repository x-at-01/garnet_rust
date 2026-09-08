use std::net::{IpAddr, Ipv4Addr, SocketAddr};

/// 默认初始接收缓冲区容量（字节）：四千零九十六字节
pub const DEFAULT_INITIAL_RECV_BUF_SIZE: usize = 4 * 1024;

/// 默认最大接收缓冲区容量（字节）：一百零四万八千五百七十六字节
pub const DEFAULT_MAX_RECV_BUF_SIZE: usize = 1024 * 1024;

/// 默认初始发送缓冲区容量（字节）：四千零九十六字节
pub const DEFAULT_SEND_BUF_SIZE: usize = 4 * 1024;

/// 默认最大并发连接上限
pub const DEFAULT_MAX_CONNECTIONS: usize = 10_000;

/// 默认监听端口
pub const DEFAULT_PORT: u16 = 6379;

/// 网络引擎核心配置参数
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NetConfig {
  /// 服务监听套接字地址
  pub addr: SocketAddr,

  /// 初始连接接收缓冲区大小（字节）
  pub initial_recv_buf_size: usize,

  /// 单连接允许扩展的最大接收缓冲区大小（字节）
  pub max_recv_buf_size: usize,

  /// 发送批处理缓冲区初始大小（字节）
  pub send_buf_size: usize,

  /// 是否禁用纳格算法以降低微秒级延迟
  pub tcp_nodelay: bool,

  /// 是否启用 TCP 保活探测（由内核最终回收半开连接）
  pub tcp_keepalive: bool,

  /// 允许的最大客户端并发连接数
  pub max_connections: usize,

  /// 客户端连接默认选中的逻辑数据库编号
  pub default_db: u32,
}

impl Default for NetConfig {
  #[inline]
  fn default() -> Self {
    Self {
      addr: SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), DEFAULT_PORT),
      initial_recv_buf_size: DEFAULT_INITIAL_RECV_BUF_SIZE,
      max_recv_buf_size: DEFAULT_MAX_RECV_BUF_SIZE,
      send_buf_size: DEFAULT_SEND_BUF_SIZE,
      tcp_nodelay: true,
      tcp_keepalive: true,
      max_connections: DEFAULT_MAX_CONNECTIONS,
      default_db: 0,
    }
  }
}

impl NetConfig {
  /// 创建指定监听地址的配置实例
  #[inline]
  pub fn new(addr: impl Into<SocketAddr>) -> Self {
    Self {
      addr: addr.into(),
      ..Default::default()
    }
  }

  /// 流式设置监听套接字地址
  #[inline]
  pub fn with_addr(mut self, addr: impl Into<SocketAddr>) -> Self {
    self.addr = addr.into();
    self
  }

  /// 流式设置初始接收缓冲区大小
  #[inline]
  pub fn with_initial_recv_buf_size(mut self, size: usize) -> Self {
    self.initial_recv_buf_size = size;
    self
  }

  /// 流式设置最大接收缓冲区大小
  #[inline]
  pub fn with_max_recv_buf_size(mut self, size: usize) -> Self {
    self.max_recv_buf_size = size;
    self
  }

  /// 流式设置发送缓冲区大小
  #[inline]
  pub fn with_send_buf_size(mut self, size: usize) -> Self {
    self.send_buf_size = size;
    self
  }

  /// 流式设置最大连接数
  #[inline]
  pub fn with_max_connections(mut self, limit: usize) -> Self {
    self.max_connections = limit;
    self
  }

  /// 流式设置禁用纳格算法标志
  #[inline]
  pub fn with_tcp_nodelay(mut self, nodelay: bool) -> Self {
    self.tcp_nodelay = nodelay;
    self
  }

  /// 流式设置 TCP 保活探测标志
  #[inline]
  pub fn with_tcp_keepalive(mut self, keepalive: bool) -> Self {
    self.tcp_keepalive = keepalive;
    self
  }

  /// 流式设置客户端连接默认选中的逻辑数据库编号
  #[inline]
  pub fn with_default_db(mut self, db: u32) -> Self {
    self.default_db = db;
    self
  }
}
