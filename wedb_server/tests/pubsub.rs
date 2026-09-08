//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test/RespPubSubTests.cs` (PubSub 订阅与无参退订)
use std::{net::SocketAddr, str::from_utf8, sync::Arc};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
};
use log::info;
use tempfile::{TempDir, tempdir};
use wedb_server::{ServerArgs, WedbServer};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 辅助发送请求并读取响应
async fn send_and_recv(stream: &mut TcpStream, req: &[u8]) -> Result<Vec<u8>> {
  let BufResult(res, _) = stream.write_all(req.to_vec()).await;
  res?;
  let buf = Vec::with_capacity(4096);
  let BufResult(res, mut buf) = stream.read(buf).await;
  let n = res?;
  buf.truncate(n);
  Ok(buf)
}

/// 发布订阅测试脚手架
struct PubSubTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl PubSubTestFixture {
  async fn setup() -> Result<Self> {
    let dir = tempdir()?;
    let dir_path = dir.path().to_string_lossy().to_string();
    let args = ServerArgs {
      port: 0,
      dir: dir_path,
      quiet: true,
      ..Default::default()
    };
    let server = Arc::new(WedbServer::new(args).await?);
    let addr = server.start().await?;
    Ok(Self {
      server,
      addr,
      _dir: dir,
    })
  }

  async fn connect_client(&self) -> Result<TcpStream> {
    let stream = TcpStream::connect(self.addr).await?;
    Ok(stream)
  }
}

impl Drop for PubSubTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 UNSUBSCRIBE 空参数退订全部频道规范
/// 对应 Garnet RespPubSubTests.cs 中的退订与模式退出测试
#[compio::test]
async fn test_unsubscribe_empty_args_compliance() -> Void {
  info!("开始测试 UNSUBSCRIBE 空参退订全部频道规范");
  let fixture = PubSubTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 先订阅两个频道
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$9\r\nSUBSCRIBE\r\n$4\r\nch01\r\n$4\r\nch02\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.contains("subscribe"), "订阅失败: {s}");

  // 2. 执行无参数 UNSUBSCRIBE
  let resp = send_and_recv(&mut client, b"*1\r\n$11\r\nUNSUBSCRIBE\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(
    !s.contains("ERR wrong number of arguments"),
    "UNSUBSCRIBE 空参不应报错: {s}"
  );
  assert!(
    s.contains("unsubscribe"),
    "UNSUBSCRIBE 应成功返回 unsubscribe 消息: {s}"
  );

  // 3. 再次无参退订（当前已无订阅）
  let resp = send_and_recv(&mut client, b"*1\r\n$11\r\nUNSUBSCRIBE\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(
    !s.contains("ERR wrong number of arguments"),
    "空订阅状态下退订不应报错: {s}"
  );

  info!("UNSUBSCRIBE 空参退订规范测试通过");
  OK
}

/// 只写不读：回执交由后续 recv_until_contains 轮询统一吸收
async fn send_only(stream: &mut TcpStream, req: &[u8]) -> Result<()> {
  let BufResult(res, _) = stream.write_all(req.to_vec()).await;
  res?;
  Ok(())
}

/// 轮询读取直到累计回执包含目标片段（broker 推送为异步刷盘，单次读存在时序竞争）
async fn recv_until_contains(stream: &mut TcpStream, needle: &str, tries: usize) -> Result<String> {
  let mut acc = String::new();
  for _ in 0..tries {
    let buf = send_and_recv(stream, b"*1\r\n$4\r\nPING\r\n").await?;
    acc.push_str(from_utf8(&buf)?);
    if acc.contains(needle) {
      break;
    }
  }
  Ok(acc)
}

/// 测试 PSUBSCRIBE 模式订阅、pmessage 广播分发、PUNSUBSCRIBE 退订与 PUBSUB 内省
/// 对标 C# RespPubSubTests.cs 中的模式订阅与 pmessage 分发测试
#[compio::test]
async fn test_psubscribe_pmessage_compliance() -> Void {
  info!("开始测试 PSUBSCRIBE 模式订阅规范");
  let fixture = PubSubTestFixture::setup().await?;
  let mut sub = fixture.connect_client().await?;
  let mut publisher = fixture.connect_client().await?;

  // 1. PSUBSCRIBE news.* -> psubscribe 回执计数 :1
  let resp = send_and_recv(&mut sub, b"*2\r\n$10\r\nPSUBSCRIBE\r\n$6\r\nnews.*\r\n").await?;
  assert_eq!(
    from_utf8(&resp)?,
    "*3\r\n$10\r\npsubscribe\r\n$6\r\nnews.*\r\n:1\r\n"
  );

  // 2. SUBSCRIBE news.ai 精准订阅 -> 回执计数 :2 (频道 + 模式合计)
  let resp = send_and_recv(&mut sub, b"*2\r\n$9\r\nSUBSCRIBE\r\n$7\r\nnews.ai\r\n").await?;
  assert_eq!(
    from_utf8(&resp)?,
    "*3\r\n$9\r\nsubscribe\r\n$7\r\nnews.ai\r\n:2\r\n"
  );

  // 3. PUBLISH news.ai：精准频道 + 模式订阅双命中 -> :2；订阅端应收到 message 帧
  let resp = send_and_recv(
    &mut publisher,
    b"*3\r\n$7\r\nPUBLISH\r\n$7\r\nnews.ai\r\n$5\r\nhello\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");
  let acc = recv_until_contains(&mut sub, "$5\r\nhello", 16).await?;
  assert!(
    acc.contains("message") && acc.contains("news.ai"),
    "message 分发缺失: {acc}"
  );

  // 4. PUBLISH news.zz：仅命中模式 -> :1；订阅端收到 pmessage <pattern> <channel> 帧
  let resp = send_and_recv(
    &mut publisher,
    b"*3\r\n$7\r\nPUBLISH\r\n$7\r\nnews.zz\r\n$3\r\nhi!\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  // 锚定第 2 条广播独有载荷 hi!：broker 推送为异步刷盘，publish#1 的 pmessage 帧
  // 可能滞后到本轮才抵达，若以非唯一的 "pmessage" 为锚会误停轮询导致断言缺帧
  let acc = recv_until_contains(&mut sub, "hi!", 16).await?;
  assert!(
    acc.contains("pmessage") && acc.contains("news.*") && acc.contains("news.zz"),
    "pmessage 帧形状异常: {acc}"
  );

  // 5. PUNSUBSCRIBE news.* -> punsubscribe 回执（轮询残留的 +PONG 用容错断言吸收）
  send_only(&mut sub, b"*2\r\n$12\r\nPUNSUBSCRIBE\r\n$6\r\nnews.*\r\n").await?;
  let acc = recv_until_contains(&mut sub, "punsubscribe", 16).await?;
  assert!(
    acc.contains("punsubscribe") && acc.contains("news.*") && acc.contains(":1\r\n"),
    "PUNSUBSCRIBE 回执异常: {acc}"
  );
  let resp = send_and_recv(&mut publisher, b"*2\r\n$6\r\nPUBSUB\r\n$6\r\nNUMPAT\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  info!("PSUBSCRIBE 模式订阅规范测试通过");
  OK
}
