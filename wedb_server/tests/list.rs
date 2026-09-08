//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test.collections/` 列表命令扩展测试 (LPUSHX, LINSERT, LPOS, LSET, LMOVE, RPOPLPUSH, LMPOP)
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

/// 列表测试脚手架
struct ListTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl ListTestFixture {
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

impl Drop for ListTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 LPUSHX, LINSERT, LPOS, LSET, LMOVE, RPOPLPUSH, LMPOP 等扩展命令
/// 对应 Garnet 列表集合套件测试
#[compio::test]
async fn test_list_extensions() -> Void {
  info!("开始测试列表扩展命令");
  let fixture = ListTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. LPUSHX 不存在 key -> :0
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nLPUSHX\r\n$7\r\nnon_key\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 2. LPUSH 产生 key -> :1
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nLPUSH\r\n$6\r\nmylist\r\n$1\r\nb\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 3. LPUSHX 存在 key -> :2
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nLPUSHX\r\n$6\r\nmylist\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 4. LINSERT mylist BEFORE b mid -> :3
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$7\r\nLINSERT\r\n$6\r\nmylist\r\n$6\r\nBEFORE\r\n$1\r\nb\r\n$3\r\nmid\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":3\r\n");

  // 5. LPOS mylist mid -> :1
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nLPOS\r\n$6\r\nmylist\r\n$3\r\nmid\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 6. LSET mylist 1 updated_mid -> +OK
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nLSET\r\n$6\r\nmylist\r\n$1\r\n1\r\n$11\r\nupdated_mid\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");

  // 7. LMOVE mylist destlist LEFT RIGHT -> $1\r\na\r\n
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$5\r\nLMOVE\r\n$6\r\nmylist\r\n$8\r\ndestlist\r\n$4\r\nLEFT\r\n$5\r\nRIGHT\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$1\r\na\r\n");

  // 8. RPOPLPUSH destlist mylist -> $1\r\na\r\n
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$9\r\nRPOPLPUSH\r\n$8\r\ndestlist\r\n$6\r\nmylist\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$1\r\na\r\n");

  // 9. LMPOP 1 mylist LEFT COUNT 2
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$5\r\nLMPOP\r\n$1\r\n1\r\n$6\r\nmylist\r\n$4\r\nLEFT\r\n$5\r\nCOUNT\r\n$1\r\n2\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.contains("mylist"));

  info!("列表扩展命令测试通过");
  OK
}
