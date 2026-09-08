//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test.collections/` 阻塞列表命令 (BLPOP/BRPOP)
//! 与 `../garnet/test/standalone/Garnet.test/RespAdminCommandsTests.cs` 中的 CLIENT UNBLOCK
use std::{net::SocketAddr, str::from_utf8, sync::Arc, time::Duration};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
  runtime::spawn,
  time::sleep,
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

/// 阻塞操作测试脚手架
struct BlockingTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl BlockingTestFixture {
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

impl Drop for BlockingTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 BLPOP 阻塞等待与跨连接推入唤醒端到端集成
/// 对应 Garnet 阻塞列表命令测试规范
#[compio::test]
async fn test_blocking_pop_and_push_wakeup() -> Void {
  info!("开始测试 BLPOP 阻塞等待与写入唤醒");
  let fixture = BlockingTestFixture::setup().await?;
  let mut client_waiting = fixture.connect_client().await?;
  let mut client_producer = fixture.connect_client().await?;

  // 1. 发起协程在客户端 1 上调用 BLPOP 阻塞等待 3 秒
  let waiter_handle = spawn(async move {
    let resp = send_and_recv(
      &mut client_waiting,
      b"*3\r\n$5\r\nBLPOP\r\n$8\r\nblist_k1\r\n$1\r\n3\r\n",
    )
    .await?;
    aok::Result::<Vec<u8>>::Ok(resp)
  });

  // 短暂等待让 waiter 进入阻塞状态并完成注册
  sleep(Duration::from_millis(50)).await;

  // 2. 客户端 2 推入数据
  let resp = send_and_recv(
    &mut client_producer,
    b"*3\r\n$5\r\nRPUSH\r\n$8\r\nblist_k1\r\n$7\r\nval_999\r\n",
  )
  .await?;
  assert_eq!(&resp, b":1\r\n");

  // 3. 客户端 1 应被立即唤醒并收到对应元素
  let waiter_resp = waiter_handle.await.unwrap()?;
  let s = from_utf8(&waiter_resp)?;
  assert!(s.contains("blist_k1"), "BLPOP 应返回对应 key: {s}");
  assert!(s.contains("val_999"), "BLPOP 应返回对应 value: {s}");

  // 3.1 核心断言：底层存储已真正弹出消费，杜绝双重存储脱节与重复 Pop
  let resp = send_and_recv(
    &mut client_producer,
    b"*2\r\n$4\r\nLLEN\r\n$8\r\nblist_k1\r\n",
  )
  .await?;
  assert_eq!(
    &resp, b":0\r\n",
    "BLPOP 唤醒后底层存储应已被消费完毕，LLEN 应为 0"
  );

  let resp = send_and_recv(
    &mut client_producer,
    b"*2\r\n$4\r\nLPOP\r\n$8\r\nblist_k1\r\n",
  )
  .await?;
  assert_eq!(
    &resp, b"$-1\r\n",
    "底层存储已空，再次非阻塞 LPOP 应返回 nil"
  );

  // 4. 超时测试：BLPOP 等待不存在的 key，设置 0.1 秒超时
  let resp = send_and_recv(
    &mut client_producer,
    b"*3\r\n$5\r\nBLPOP\r\n$7\r\nno_such\r\n$3\r\n0.1\r\n",
  )
  .await?;
  assert_eq!(&resp, b"*-1\r\n", "超时应返回 Null Array (*-1)");

  // 5. 超时参数非法格式校验（对齐 Garnet 标准报错）
  let resp = send_and_recv(
    &mut client_producer,
    b"*3\r\n$5\r\nBLPOP\r\n$7\r\nno_such\r\n$3\r\nabc\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ERR timeout is not a float or out of range"),
    "超时非浮点数未正确报错: {:?}",
    from_utf8(&resp)
  );

  let resp = send_and_recv(
    &mut client_producer,
    b"*3\r\n$5\r\nBLPOP\r\n$7\r\nno_such\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ERR timeout is negative"),
    "负数超时未正确报错: {:?}",
    from_utf8(&resp)
  );

  info!("BLPOP 阻塞与唤醒测试通过");
  OK
}

/// 测试 CLIENT UNBLOCK 强制解除客户端阻塞
/// 对应 Garnet RespAdminCommandsTests.cs 中的连接解阻测试
#[compio::test]
async fn test_client_unblock() -> Void {
  info!("开始测试 CLIENT UNBLOCK 强制解除阻塞");
  let fixture = BlockingTestFixture::setup().await?;
  let mut client_blocked = fixture.connect_client().await?;
  let mut client_admin = fixture.connect_client().await?;

  // 1. 获取 client_blocked 的连接 ID
  let resp = send_and_recv(&mut client_blocked, b"*2\r\n$6\r\nCLIENT\r\n$2\r\nID\r\n").await?;
  let s = from_utf8(&resp)?;
  let client_id_str = s.trim().trim_start_matches(':');
  let _client_id: u64 = client_id_str.parse()?;

  // 2. client_admin 测试 CLIENT UNBLOCK 非法原因参数校验
  let mut unblock_invalid = String::from("*4\r\n$6\r\nCLIENT\r\n$7\r\nUNBLOCK\r\n$");
  let mut ibuf = itoa::Buffer::new();
  unblock_invalid.push_str(ibuf.format(client_id_str.len()));
  unblock_invalid.push_str("\r\n");
  unblock_invalid.push_str(client_id_str);
  unblock_invalid.push_str("\r\n$7\r\nINVALID\r\n");
  let resp = send_and_recv(&mut client_admin, unblock_invalid.as_bytes()).await?;
  assert!(
    from_utf8(&resp)?.contains("ERR CLIENT UNBLOCK reason should be TIMEOUT or ERROR"),
    "CLIENT UNBLOCK 非法 reason 未拦截: {:?}",
    from_utf8(&resp)
  );

  // 3. client_blocked 发起长阻塞 BLPOP (超时 10 秒)
  let blocked_handle = spawn(async move {
    let resp = send_and_recv(
      &mut client_blocked,
      b"*3\r\n$5\r\nBLPOP\r\n$8\r\nlongwait\r\n$2\r\n10\r\n",
    )
    .await?;
    aok::Result::<Vec<u8>>::Ok(resp)
  });

  // 等待注册完成
  sleep(Duration::from_millis(50)).await;

  // 4. client_admin 发送 CLIENT UNBLOCK <id> TIMEOUT
  let mut unblock_cmd = String::from("*4\r\n$6\r\nCLIENT\r\n$7\r\nUNBLOCK\r\n$");
  let mut ibuf2 = itoa::Buffer::new();
  unblock_cmd.push_str(ibuf2.format(client_id_str.len()));
  unblock_cmd.push_str("\r\n");
  unblock_cmd.push_str(client_id_str);
  unblock_cmd.push_str("\r\n$7\r\nTIMEOUT\r\n");
  let resp = send_and_recv(&mut client_admin, unblock_cmd.as_bytes()).await?;
  assert_eq!(&resp, b":1\r\n", "CLIENT UNBLOCK 应返回成功 :1");

  // 5. client_blocked 应收到被解除阻塞的响应 (TIMEOUT 对应 Null Array)
  let blocked_resp = blocked_handle.await.unwrap()?;
  let s = from_utf8(&blocked_resp)?;
  assert!(
    s.starts_with("*-1") || s.contains("UNBLOCKED"),
    "被 UNBLOCK 唤醒响应异常: {s}"
  );

  info!("CLIENT UNBLOCK 测试通过");
  OK
}
