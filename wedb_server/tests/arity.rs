//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test/RespCommandTests.cs` (定长参数命令 Arity 严格校验)
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

/// 参数数量严格校验测试脚手架
struct ArityTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl ArityTestFixture {
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

impl Drop for ArityTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试定长参数命令的多余参数或缺失参数严格校验拦截
/// 对应 Garnet RespCommandTests.cs 中的 Arity 检查规范
#[compio::test]
async fn test_arity_strict_enforcement() -> Void {
  info!("开始测试定长参数命令多参或少参严格拦截");
  let fixture = ArityTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. GET 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nGET\r\n$2\r\nk1\r\n$2\r\nk2\r\n").await?;
  assert!(
    from_utf8(&resp)?.contains("ERR wrong number of arguments"),
    "GET 多参放行缺陷: {:?}",
    from_utf8(&resp)
  );

  // 2. TTL 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nTTL\r\n$2\r\nk1\r\n$2\r\nk2\r\n").await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 3. HLEN 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*3\r\n$4\r\nHLEN\r\n$2\r\nk1\r\n$2\r\nk2\r\n").await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 4. HGETALL 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$7\r\nHGETALL\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 5. HKEYS 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nHKEYS\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 6. HVALS 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nHVALS\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 7. HGET 传入 3 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nHGET\r\n$2\r\nk1\r\n$2\r\nf1\r\n$2\r\nf2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 8. HEXISTS 传入 3 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$7\r\nHEXISTS\r\n$2\r\nk1\r\n$2\r\nf1\r\n$2\r\nf2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 9. LLEN 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*3\r\n$4\r\nLLEN\r\n$2\r\nk1\r\n$2\r\nk2\r\n").await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 10. SCARD 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nSCARD\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 11. SMEMBERS 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$8\r\nSMEMBERS\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 12. ZCARD 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nZCARD\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 13. ZSCORE 传入 3 个参数 -> 必须拦截
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZSCORE\r\n$2\r\nk1\r\n$2\r\nm1\r\n$2\r\nm2\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 14. ECHO 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*3\r\n$4\r\nECHO\r\n$1\r\na\r\n$1\r\nb\r\n").await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 15. SELECT 传入 2 个参数 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*3\r\n$6\r\nSELECT\r\n$1\r\n0\r\n$1\r\n1\r\n").await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 16. DBSIZE 传入参数 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nDBSIZE\r\n$1\r\nx\r\n").await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 17. WATCH 空参数 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nWATCH\r\n").await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  // 18. UNWATCH 传参 -> 必须拦截
  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nUNWATCH\r\n$1\r\nx\r\n").await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments"));

  info!("Arity 严格拦截校验测试通过");
  OK
}
