//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test.collections/` 中的 Bitmap 相关测试 (BITPOS, BITOP)
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

/// 位图操作测试脚手架
struct BitmapTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl BitmapTestFixture {
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

impl Drop for BitmapTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 BITPOS 和 BITOP 操作
/// 对应 Garnet 集合测试套件中的 Bitmap 规范
#[compio::test]
async fn test_bitmap_operations() -> Void {
  info!("开始测试 BITPOS 与 BITOP 位图操作");
  let fixture = BitmapTestFixture::setup().await?;
  let mut stream = fixture.connect_client().await?;

  // 1. BITPOS 基本测试与边界校验: SET mybm "\xff\xf0\x00" -> 11111111 11110000 00000000
  let res = send_and_recv(
    &mut stream,
    b"*3\r\n$3\r\nSET\r\n$4\r\nmybm\r\n$3\r\n\xff\xf0\x00\r\n",
  )
  .await?;
  assert_eq!(res, b"+OK\r\n");

  // 查找首个 0 (从第 12 位开始)
  let res = send_and_recv(
    &mut stream,
    b"*3\r\n$6\r\nBITPOS\r\n$4\r\nmybm\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(res, b":12\r\n");

  // 查找首个 1 (从第 0 位开始)
  let res = send_and_recv(
    &mut stream,
    b"*3\r\n$6\r\nBITPOS\r\n$4\r\nmybm\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(res, b":0\r\n");

  // 范围指定：第 0 到 0 字节查找 0（前 8 位全为 1，不存在 0，返回 -1）
  let res = send_and_recv(
    &mut stream,
    b"*5\r\n$6\r\nBITPOS\r\n$4\r\nmybm\r\n$1\r\n0\r\n$1\r\n0\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(res, b":-1\r\n");

  // 范围指定：第 1 到 1 字节查找 0（第 1 字节为 \xf0，首个 0 位于全局第 12 位）
  let res = send_and_recv(
    &mut stream,
    b"*5\r\n$6\r\nBITPOS\r\n$4\r\nmybm\r\n$1\r\n0\r\n$1\r\n1\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(res, b":12\r\n");

  // BIT 索引模式：从第 2 位到第 7 位查找 1（第 2 位本身就是 1）
  let res = send_and_recv(
    &mut stream,
    b"*6\r\n$6\r\nBITPOS\r\n$4\r\nmybm\r\n$1\r\n1\r\n$1\r\n2\r\n$1\r\n7\r\n$3\r\nBIT\r\n",
  )
  .await?;
  assert_eq!(res, b":2\r\n");

  // 非法 bit 参数报错
  let res = send_and_recv(
    &mut stream,
    b"*3\r\n$6\r\nBITPOS\r\n$4\r\nmybm\r\n$1\r\n2\r\n",
  )
  .await?;
  assert!(from_utf8(&res)?.contains("ERR The bit argument must be 1 or 0."));

  // 2. BITOP 操作 (AND, OR, XOR, NOT, DIFF): k1 = 00001111 (0x0F)
  send_and_recv(
    &mut stream,
    b"*3\r\n$3\r\nSET\r\n$2\r\nk1\r\n$1\r\n\x0f\r\n",
  )
  .await?;
  // k2 = 00110011 (0x33)
  send_and_recv(
    &mut stream,
    b"*3\r\n$3\r\nSET\r\n$2\r\nk2\r\n$1\r\n\x33\r\n",
  )
  .await?;

  // BITOP AND dest_and k1 k2 -> 00000011 (0x03)
  let res = send_and_recv(
    &mut stream,
    b"*5\r\n$5\r\nBITOP\r\n$3\r\nAND\r\n$8\r\ndest_and\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert_eq!(res, b":1\r\n");
  let res = send_and_recv(&mut stream, b"*2\r\n$3\r\nGET\r\n$8\r\ndest_and\r\n").await?;
  assert_eq!(res, b"$1\r\n\x03\r\n");

  // BITOP OR dest_or k1 k2 -> 00111111 (0x3F)
  let res = send_and_recv(
    &mut stream,
    b"*5\r\n$5\r\nBITOP\r\n$2\r\nOR\r\n$7\r\ndest_or\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert_eq!(res, b":1\r\n");
  let res = send_and_recv(&mut stream, b"*2\r\n$3\r\nGET\r\n$7\r\ndest_or\r\n").await?;
  assert_eq!(res, b"$1\r\n\x3f\r\n");

  // BITOP XOR dest_xor k1 k2 -> 00111100 (0x3C)
  let res = send_and_recv(
    &mut stream,
    b"*5\r\n$5\r\nBITOP\r\n$3\r\nXOR\r\n$8\r\ndest_xor\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert_eq!(res, b":1\r\n");
  let res = send_and_recv(&mut stream, b"*2\r\n$3\r\nGET\r\n$8\r\ndest_xor\r\n").await?;
  assert_eq!(res, b"$1\r\n\x3c\r\n");

  // BITOP NOT dest_not k1 -> 11110000 (0xF0)
  let res = send_and_recv(
    &mut stream,
    b"*4\r\n$5\r\nBITOP\r\n$3\r\nNOT\r\n$8\r\ndest_not\r\n$2\r\nk1\r\n",
  )
  .await?;
  assert_eq!(res, b":1\r\n");
  let res = send_and_recv(&mut stream, b"*2\r\n$3\r\nGET\r\n$8\r\ndest_not\r\n").await?;
  assert_eq!(res, b"$1\r\n\xf0\r\n");

  // BITOP DIFF dest_diff k1 k2 -> k1 & ~k2 = 00001111 & 11001100 = 00001100 (0x0C)
  let res = send_and_recv(
    &mut stream,
    b"*5\r\n$5\r\nBITOP\r\n$4\r\nDIFF\r\n$9\r\ndest_diff\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert_eq!(res, b":1\r\n");
  let res = send_and_recv(&mut stream, b"*2\r\n$3\r\nGET\r\n$9\r\ndest_diff\r\n").await?;
  assert_eq!(res, b"$1\r\n\x0c\r\n");

  // BITOP NOT 传入多个源 key 报错
  let res = send_and_recv(
    &mut stream,
    b"*5\r\n$5\r\nBITOP\r\n$3\r\nNOT\r\n$4\r\ndest\r\n$2\r\nk1\r\n$2\r\nk2\r\n",
  )
  .await?;
  assert!(from_utf8(&res)?.contains("ERR BITOP NOT takes only one source key"));

  // BITOP DIFF 传入少于 2 个源 key 报错
  let res = send_and_recv(
    &mut stream,
    b"*4\r\n$5\r\nBITOP\r\n$4\r\nDIFF\r\n$4\r\ndest\r\n$2\r\nk1\r\n",
  )
  .await?;
  assert!(
    from_utf8(&res)?.contains("ERR BITOP DIFF operation requires at least two source bitmaps")
  );

  info!("位图操作测试通过");
  OK
}
