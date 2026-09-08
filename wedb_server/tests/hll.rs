//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test.collections/` HyperLogLog 命令测试 (PFADD, PFCOUNT, PFMERGE)
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

/// HyperLogLog 测试脚手架
struct HllTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl HllTestFixture {
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

impl Drop for HllTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 PFADD, PFCOUNT, PFMERGE 以及基数估算与类型安全
/// 对应 Garnet HyperLogLog 集合套件测试规范
#[compio::test]
async fn test_hyperloglog() -> Void {
  info!("开始测试 HyperLogLog 核心命令");
  let fixture = HllTestFixture::setup().await?;
  let mut stream = fixture.connect_client().await?;

  // 1. PFADD & PFCOUNT 单键基础测试: 添加新元素 a, b, c
  let res = send_and_recv(
    &mut stream,
    b"*5\r\n$5\r\nPFADD\r\n$4\r\nhll1\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n",
  )
  .await?;
  assert_eq!(res, b":1\r\n");

  // 添加已有元素 a
  let res = send_and_recv(
    &mut stream,
    b"*3\r\n$5\r\nPFADD\r\n$4\r\nhll1\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(res, b":0\r\n");

  // 统计基数
  let res = send_and_recv(&mut stream, b"*2\r\n$7\r\nPFCOUNT\r\n$4\r\nhll1\r\n").await?;
  assert_eq!(res, b":3\r\n");

  // 批量添加 300 个不同元素
  for i in 0..300 {
    let mut elem = String::from("item_");
    let mut ibuf = itoa::Buffer::new();
    elem.push_str(ibuf.format(i));
    let mut req = String::from("*3\r\n$5\r\nPFADD\r\n$4\r\nhll1\r\n$");
    let mut ibuf2 = itoa::Buffer::new();
    req.push_str(ibuf2.format(elem.len()));
    req.push_str("\r\n");
    req.push_str(elem.as_str());
    req.push_str("\r\n");
    send_and_recv(&mut stream, req.as_bytes()).await?;
  }

  let res = send_and_recv(&mut stream, b"*2\r\n$7\r\nPFCOUNT\r\n$4\r\nhll1\r\n").await?;
  let count_str = from_utf8(&res)?;
  let count: i64 = count_str.trim().trim_start_matches(':').parse()?;
  // 真实去重元素为 303 (a, b, c + 300 个 item)，误差应当在 5% 以内
  let err = ((count - 303) as f64).abs() / 303.0;
  assert!(err < 0.05, "PFCOUNT 估算误差过大: count={count}, err={err}");

  // 2. PFCOUNT 多键联合估算与 PFMERGE: hll_a: 1, 2, 3, 4, 5
  send_and_recv(
    &mut stream,
    b"*7\r\n$5\r\nPFADD\r\n$5\r\nhll_a\r\n$1\r\n1\r\n$1\r\n2\r\n$1\r\n3\r\n$1\r\n4\r\n$1\r\n5\r\n",
  )
  .await?;

  // hll_b: 4, 5, 6, 7, 8
  send_and_recv(
    &mut stream,
    b"*7\r\n$5\r\nPFADD\r\n$5\r\nhll_b\r\n$1\r\n4\r\n$1\r\n5\r\n$1\r\n6\r\n$1\r\n7\r\n$1\r\n8\r\n",
  )
  .await?;

  // 联合估算 hll_a 和 hll_b (并集基数应为 8)
  let res = send_and_recv(
    &mut stream,
    b"*3\r\n$7\r\nPFCOUNT\r\n$5\r\nhll_a\r\n$5\r\nhll_b\r\n",
  )
  .await?;
  assert_eq!(res, b":8\r\n");

  // 合并至 hll_c
  let res = send_and_recv(
    &mut stream,
    b"*4\r\n$7\r\nPFMERGE\r\n$5\r\nhll_c\r\n$5\r\nhll_a\r\n$5\r\nhll_b\r\n",
  )
  .await?;
  assert_eq!(res, b"+OK\r\n");

  // 统计合并后的 hll_c
  let res = send_and_recv(&mut stream, b"*2\r\n$7\r\nPFCOUNT\r\n$5\r\nhll_c\r\n").await?;
  assert_eq!(res, b":8\r\n");

  // 3. 错误处理与类型安全校验: 对普通字符串执行 HLL 命令报错 WRONGTYPE
  send_and_recv(
    &mut stream,
    b"*3\r\n$3\r\nSET\r\n$7\r\nstr_key\r\n$5\r\nhello\r\n",
  )
  .await?;

  let res = send_and_recv(
    &mut stream,
    b"*3\r\n$5\r\nPFADD\r\n$7\r\nstr_key\r\n$1\r\nx\r\n",
  )
  .await?;
  assert!(from_utf8(&res)?.contains("WRONGTYPE Key is not a valid HyperLogLog string value."));

  let res = send_and_recv(&mut stream, b"*2\r\n$7\r\nPFCOUNT\r\n$7\r\nstr_key\r\n").await?;
  assert!(from_utf8(&res)?.contains("WRONGTYPE Key is not a valid HyperLogLog string value."));

  let res = send_and_recv(
    &mut stream,
    b"*3\r\n$7\r\nPFMERGE\r\n$3\r\ndst\r\n$7\r\nstr_key\r\n",
  )
  .await?;
  assert!(from_utf8(&res)?.contains("WRONGTYPE Key is not a valid HyperLogLog string value."));

  info!("HyperLogLog 测试通过");
  OK
}
