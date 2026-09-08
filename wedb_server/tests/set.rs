//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test.collections/` 集合命令测试 (SADD, SINTER, SINTERSTORE, SINTERCARD, SUNION, SDIFF, SRANDMEMBER)
use std::{net::SocketAddr, str::from_utf8, sync::Arc};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
};
use gxhash::HashSet;
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

/// 集合测试脚手架
struct SetTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl SetTestFixture {
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

impl Drop for SetTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 SADD, SINTER, SINTERSTORE, SINTERCARD, SUNION, SUNIONSTORE, SDIFF, SDIFFSTORE, SRANDMEMBER 等集合命令
/// 对应 Garnet 集合套件测试规范
#[compio::test]
async fn test_set_operations() -> Void {
  info!("开始测试集合核心命令");
  let fixture = SetTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. SADD set1 a b c -> :3
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nSADD\r\n$4\r\nset1\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":3\r\n");

  // 2. SADD set2 b c d -> :3
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nSADD\r\n$4\r\nset2\r\n$1\r\nb\r\n$1\r\nc\r\n$1\r\nd\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":3\r\n");

  // 3. SINTER set1 set2 -> 2 items: b and c
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nSINTER\r\n$4\r\nset1\r\n$4\r\nset2\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*2\r\n"));
  assert!(resp_str.contains("$1\r\nb\r\n"));
  assert!(resp_str.contains("$1\r\nc\r\n"));

  // 4. SINTERSTORE dest_inter set1 set2 -> :2
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$11\r\nSINTERSTORE\r\n$10\r\ndest_inter\r\n$4\r\nset1\r\n$4\r\nset2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 5. SINTERCARD 2 set1 set2 -> :2
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$10\r\nSINTERCARD\r\n$1\r\n2\r\n$4\r\nset1\r\n$4\r\nset2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 6. SINTERCARD 2 set1 set2 LIMIT 1 -> :1
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$10\r\nSINTERCARD\r\n$1\r\n2\r\n$4\r\nset1\r\n$4\r\nset2\r\n$5\r\nLIMIT\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 7. SUNION set1 set2 -> 4 items (a, b, c, d)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nSUNION\r\n$4\r\nset1\r\n$4\r\nset2\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*4\r\n"));
  assert!(resp_str.contains("$1\r\na\r\n"));
  assert!(resp_str.contains("$1\r\nd\r\n"));

  // 8. SUNIONSTORE dest_union set1 set2 -> :4
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$11\r\nSUNIONSTORE\r\n$10\r\ndest_union\r\n$4\r\nset1\r\n$4\r\nset2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":4\r\n");

  // 9. SDIFF set1 set2 -> 1 item (a)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nSDIFF\r\n$4\r\nset1\r\n$4\r\nset2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*1\r\n$1\r\na\r\n");

  // 10. SDIFFSTORE dest_diff set1 set2 -> :1
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$10\r\nSDIFFSTORE\r\n$9\r\ndest_diff\r\n$4\r\nset1\r\n$4\r\nset2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 11. SRANDMEMBER set1 (单元素)
  let resp = send_and_recv(&mut client, b"*2\r\n$11\r\nSRANDMEMBER\r\n$4\r\nset1\r\n").await?;
  let resp_str = from_utf8(&resp)?;
  let valid_members: HashSet<&str> = ["$1\r\na\r\n", "$1\r\nb\r\n", "$1\r\nc\r\n"]
    .into_iter()
    .collect();
  assert!(valid_members.contains(resp_str));

  // 12. SRANDMEMBER set1 2 (多元素正数)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nSRANDMEMBER\r\n$4\r\nset1\r\n$1\r\n2\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*2\r\n"));

  info!("集合核心命令测试通过");
  OK
}
