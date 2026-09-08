use std::{
  net::SocketAddr,
  sync::Arc,
  time::{Duration, Instant},
};

use aok::Result;
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
  time::sleep,
};
use tempfile::{TempDir, tempdir};
use wdev::SegmentedDevice;
use wedb_net::{NetConfig, WedbServer};
use wkv::{StoreConfig, WedbStore};

/// 默认本地回环地址
pub const LOOPBACK: [u8; 4] = [127, 0, 0, 1];
/// 默认哈希索引桶数量
pub const DEFAULT_TABLE_SIZE: usize = 1024;
/// 默认混合日志页大小（64KB）
pub const DEFAULT_PAGE_SIZE: usize = 64 * 1024;
/// 默认内存页数量
pub const DEFAULT_LOG_PAGES: usize = 16;
/// 默认可变日志比例
pub const DEFAULT_MUTABLE_FRACTION: f64 = 0.5;
/// 默认轮询检测间隔
pub const POLL_INTERVAL: Duration = Duration::from_millis(5);
/// 默认等待超时时间
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(3);
/// 单次读取默认预分配容量
pub const DEFAULT_BUF_CAPACITY: usize = 2048;
/// 大块流式读取缓冲区尺寸（64KB）
pub const READ_CHUNK_SIZE: usize = 64 * 1024;

/// 网络测试脚手架：提供独立运行的真实网络测试服务与临时存储资源
pub struct NetworkTestFixture {
  pub server: Arc<WedbServer<SegmentedDevice>>,
  pub addr: SocketAddr,
  pub _dir: TempDir,
}

impl NetworkTestFixture {
  /// 创建并启动标准配置的网络测试服务
  pub async fn setup() -> Result<Self> {
    Self::setup_with_config(NetConfig::new((LOOPBACK, 0)), DEFAULT_PAGE_SIZE).await
  }

  /// 创建并启动具有连接上限限制的网络测试服务
  pub async fn setup_with_max_connections(max_conn: usize) -> Result<Self> {
    let cfg = NetConfig::new((LOOPBACK, 0)).with_max_connections(max_conn);
    Self::setup_with_config(cfg, DEFAULT_PAGE_SIZE).await
  }

  /// 创建并启动指定 HybridLog 页大小的网络测试服务（用于大页/大值测试）
  pub async fn setup_with_page_size(page_size: usize) -> Result<Self> {
    Self::setup_with_config(NetConfig::new((LOOPBACK, 0)), page_size).await
  }

  /// 依据自定义网络配置与页大小创建并启动测试服务
  pub async fn setup_with_config(net_cfg: NetConfig, page_size: usize) -> Result<Self> {
    let dir = tempdir()?;
    let db_path = dir.path().join("wedb_test.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let store_cfg = StoreConfig::new(
      DEFAULT_TABLE_SIZE,
      page_size,
      DEFAULT_LOG_PAGES,
      DEFAULT_MUTABLE_FRACTION,
    )?;
    let store = Arc::new(WedbStore::open(store_cfg, device)?);
    let server = Arc::new(WedbServer::new(net_cfg, store));
    let addr = server.start().await?;
    Ok(Self {
      server,
      addr,
      _dir: dir,
    })
  }

  /// 获取 WeDB 服务端引用
  #[inline]
  pub fn server(&self) -> &WedbServer<SegmentedDevice> {
    &self.server
  }

  /// 建立连接到该测试服务的 TCP 客户端流
  pub async fn connect_client(&self) -> Result<TcpStream> {
    let stream = TcpStream::connect(self.addr).await?;
    Ok(stream)
  }

  /// 轮询等待活跃连接数达到目标值（带超时保护）
  pub async fn wait_for_active_connections(&self, target: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
      if self.server.active_connections() == target {
        return true;
      }
      sleep(POLL_INTERVAL).await;
    }
    self.server.active_connections() == target
  }

  /// 轮询等待累计接收连接数达到或超过目标值（带超时保护）
  pub async fn wait_for_connections_received(&self, target: usize, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
      if self.server.total_connections_received() >= target {
        return true;
      }
      sleep(POLL_INTERVAL).await;
    }
    self.server.total_connections_received() >= target
  }
}

impl Drop for NetworkTestFixture {
  fn drop(&mut self) {
    self.server.shutdown();
  }
}

/// 向套接字写出请求字节并在单次读取中获取响应（避免二次切片分配）
pub async fn send_and_recv(stream: &mut TcpStream, req: &[u8]) -> Result<Vec<u8>> {
  let BufResult(write_res, _) = stream.write_all(req.to_vec()).await;
  write_res?;
  let buf = Vec::with_capacity(DEFAULT_BUF_CAPACITY);
  let BufResult(read_res, mut buf) = stream.read(buf).await;
  let n = read_res?;
  buf.truncate(n);
  Ok(buf)
}

/// 向套接字写出请求并循环读取直至累计收到指定字节数或遇到 EOF（复用读缓冲避免重复分配）
pub async fn send_and_recv_exact(
  stream: &mut TcpStream,
  req: &[u8],
  expected_len: usize,
) -> Result<Vec<u8>> {
  let BufResult(write_res, _) = stream.write_all(req.to_vec()).await;
  write_res?;
  let mut received = Vec::with_capacity(expected_len);
  let mut buf = Vec::with_capacity(READ_CHUNK_SIZE.min(expected_len.max(DEFAULT_BUF_CAPACITY)));
  while received.len() < expected_len {
    buf.clear();
    let BufResult(read_res, returned_buf) = stream.read(buf).await;
    buf = returned_buf;
    let n = read_res?;
    if n == 0 {
      break;
    }
    received.extend_from_slice(&buf[..n]);
  }
  Ok(received)
}

/// 轮询读取套接字直至服务端关闭连接（收到 EOF 或连接异常中断）
pub async fn wait_for_connection_close(stream: &mut TcpStream, timeout: Duration) -> bool {
  let deadline = Instant::now() + timeout;
  let mut buf = Vec::with_capacity(64);
  while Instant::now() < deadline {
    buf.clear();
    let BufResult(read_res, returned_buf) = stream.read(buf).await;
    buf = returned_buf;
    match read_res {
      Ok(0) | Err(_) => return true,
      Ok(_) => {}
    }
    sleep(POLL_INTERVAL).await;
  }
  false
}
