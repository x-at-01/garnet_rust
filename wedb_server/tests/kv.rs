//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test/RespTests.cs` (KV 与字符串核心命令测试)
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

/// KV 测试脚手架
struct KvTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl KvTestFixture {
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

impl Drop for KvTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 INCR, INCRBY, DECR, DECRBY, INCRBYFLOAT 等自增自减命令
/// 对应 Garnet RespTests.cs 中的数值增减测试
#[compio::test]
async fn test_incr_decr_suite() -> Void {
  info!("开始测试 INCR / DECR 数值增减命令");
  let fixture = KvTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. INCR 空 key -> :1
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nINCR\r\n$3\r\nnum\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 2. INCRBY num 10 -> :11
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nINCRBY\r\n$3\r\nnum\r\n$2\r\n10\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":11\r\n");

  // 3. DECR num -> :10
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nDECR\r\n$3\r\nnum\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":10\r\n");

  // 4. DECRBY num 5 -> :5
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nDECRBY\r\n$3\r\nnum\r\n$1\r\n5\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":5\r\n");

  // 5. INCRBYFLOAT num 2.5 -> $3\r\n7.5\r\n
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nINCRBYFLOAT\r\n$3\r\nnum\r\n$3\r\n2.5\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.contains("7.5"));

  info!("数值增减测试通过");
  OK
}

/// 测试 MSET, MGET, GETSET, SETNX, GETDEL 等批量与扩展字符串操作
/// 对应 Garnet RespTests.cs 中的批量及带条件 KV 操作
#[compio::test]
async fn test_batch_and_extended_kv() -> Void {
  info!("开始测试批量与扩展 KV 命令 (MSET, MGET, GETSET, SETNX, GETDEL)");
  let fixture = KvTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. MSET a 1 b 2
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nMSET\r\n$1\r\na\r\n$1\r\n1\r\n$1\r\nb\r\n$1\r\n2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");

  // 2. MGET a b c
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nMGET\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*3\r\n$1\r\n1\r\n$1\r\n2\r\n$-1\r\n");

  // 3. GETSET a 100 -> $1\r\n1\r\n
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nGETSET\r\n$1\r\na\r\n$3\r\n100\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$1\r\n1\r\n");

  // 4. SETNX a 200 -> :0 (已存在)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nSETNX\r\n$1\r\na\r\n$3\r\n200\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 5. SETNX newkey 200 -> :1
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nSETNX\r\n$6\r\nnewkey\r\n$3\r\n200\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 6. GETDEL newkey -> $3\r\n200\r\n
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nGETDEL\r\n$6\r\nnewkey\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$3\r\n200\r\n");

  // 7. 验证 newkey 已被删除
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$6\r\nnewkey\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");

  info!("批量与扩展 KV 操作测试通过");
  OK
}

/// 测试 KEYS, UNLINK, TIME 键空间与系统时间操作
/// 对应 Garnet RespTests.cs 中的键查找、异步删除及服务器时间
#[compio::test]
async fn test_keys_unlink_and_time() -> Void {
  info!("开始测试 KEYS, UNLINK, TIME 操作");
  let fixture = KvTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 设置测试键
  send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$4\r\nk_01\r\n$1\r\nv\r\n").await?;
  send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$4\r\nk_02\r\n$1\r\nv\r\n").await?;

  // 2. KEYS k_*
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nKEYS\r\n$3\r\nk_*\r\n").await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.contains("k_01"));
  assert!(resp_str.contains("k_02"));

  // 3. UNLINK k_01
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nUNLINK\r\n$4\r\nk_01\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 4. TIME 命令
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nTIME\r\n").await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*2\r\n$"));

  info!("KEYS, UNLINK, TIME 测试通过");
  OK
}

/// 测试 TTL, PTTL, PERSIST, EXPIRE, EXPIREAT, PEXPIRE, PEXPIREAT, EXPIRETIME, PEXPIRETIME
/// 对应 Garnet RespTests.cs 中的键过期与生命周期测试
#[compio::test]
async fn test_ttl_persist_expire_suite() -> Void {
  info!("开始测试 TTL, PERSIST, EXPIRE 等生命周期命令");
  let fixture = KvTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 未存在 key 的 TTL 与 PTTL -> :-2
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$8\r\nno_exist\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-2\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nPTTL\r\n$8\r\nno_exist\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-2\r\n");

  // 2. 写入常规 key
  send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$5\r\nmykey\r\n$3\r\nval\r\n",
  )
  .await?;

  // 3. 常规永久 key 的 TTL 与 PTTL -> :-1
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$5\r\nmykey\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nPTTL\r\n$5\r\nmykey\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-1\r\n");

  // 4. PERSIST 永久 key -> :0 (无关联超时时间)
  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$5\r\nmykey\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 5. PERSIST 不存在的 key -> :0
  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$8\r\nno_exist\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 6. EXPIRE 存在 key -> :1, 不存在 key -> :0 (Garnet 契约：NOTFOUND 回 :0)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$5\r\nmykey\r\n$3\r\n100\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$8\r\nno_exist\r\n$3\r\n100\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 6.1 已设 TTL 的 key：TTL 秒级剩余约为 100；EXPIRE NX 条件不满足 -> :0
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$5\r\nmykey\r\n").await?;
  let ttl: i64 = from_utf8(&resp)?.trim()[1..].parse()?;
  assert!((99..=100).contains(&ttl), "TTL 剩余异常: {ttl}");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$5\r\nmykey\r\n$2\r\n50\r\n$2\r\nNX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 7. EXPIREAT, PEXPIRE, PEXPIREAT
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$8\r\nEXPIREAT\r\n$5\r\nmykey\r\n$10\r\n2000000000\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$7\r\nPEXPIRE\r\n$5\r\nmykey\r\n$6\r\n100000\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$9\r\nPEXPIREAT\r\n$5\r\nmykey\r\n$13\r\n2000000000000\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 8. EXPIRETIME 与 PEXPIRETIME：上一步 PEXPIREAT 已写入真实过期点，应返回该时间戳
  let resp = send_and_recv(&mut client, b"*2\r\n$10\r\nEXPIRETIME\r\n$5\r\nmykey\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":2000000000\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*2\r\n$10\r\nEXPIRETIME\r\n$8\r\nno_exist\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":-2\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$11\r\nPEXPIRETIME\r\n$5\r\nmykey\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":2000000000000\r\n");

  info!("生命周期命令测试通过");
  OK
}

/// 测试 SETEX, PSETEX, SUBSTR 字符串设定与切片截取
/// 对应 Garnet RespTests.cs 中的带超时写入及子串截取
#[compio::test]
async fn test_setex_psetex_substr_suite() -> Void {
  info!("开始测试 SETEX, PSETEX, SUBSTR 操作");
  let fixture = KvTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. SETEX 正确写入
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nSETEX\r\n$6\r\nexpkey\r\n$2\r\n60\r\n$5\r\nhello\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$6\r\nexpkey\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$5\r\nhello\r\n");

  // 2. SETEX 非法过期时间
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nSETEX\r\n$6\r\nbadkey\r\n$2\r\n-1\r\n$3\r\nfoo\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("-ERR invalid expire time"));

  // 3. PSETEX 毫秒设置
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nPSETEX\r\n$7\r\npexpkey\r\n$5\r\n50000\r\n$5\r\nworld\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");

  // 4. SUBSTR 截取子串
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nSUBSTR\r\n$6\r\nexpkey\r\n$1\r\n0\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$2\r\nhe\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nSUBSTR\r\n$6\r\nexpkey\r\n$1\r\n0\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$5\r\nhello\r\n");

  // 5. SUBSTR 不存在 key -> $0\r\n\r\n
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nSUBSTR\r\n$8\r\nnonexist\r\n$1\r\n0\r\n$1\r\n5\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$0\r\n\r\n");

  info!("SETEX, PSETEX, SUBSTR 测试通过");
  OK
}

/// 测试 INCRBYFLOAT 浮点边界、非法值校验及跨类型保护
/// 对应 Garnet RespTests.cs 中的浮点运算异常保护规范
#[compio::test]
async fn test_incrbyfloat_boundary_and_validation() -> Void {
  info!("开始测试 INCRBYFLOAT 边界值与类型校验");
  let fixture = KvTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. INCRBYFLOAT 初始自增
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nINCRBYFLOAT\r\n$4\r\nfkey\r\n$4\r\n3.14\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.contains("3.14"));

  // 2. INCRBYFLOAT 负数步长
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nINCRBYFLOAT\r\n$4\r\nfkey\r\n$5\r\n-1.14\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.contains("2"));

  // 3. INCRBYFLOAT 非法浮点输入 -> ERR value is not a valid float
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nINCRBYFLOAT\r\n$4\r\nfkey\r\n$3\r\nabc\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("-ERR value is not a valid float"));

  // 4. INCRBYFLOAT nan/inf 拒绝 -> ERR increment would produce NaN or Infinity
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nINCRBYFLOAT\r\n$4\r\nfkey\r\n$3\r\nnan\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("-ERR increment would produce NaN or Infinity"));

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nINCRBYFLOAT\r\n$4\r\nfkey\r\n$3\r\ninf\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("-ERR increment would produce NaN or Infinity"));

  // 5. 跨类型冲突 WRONGTYPE
  send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nLPUSH\r\n$5\r\nlistk\r\n$1\r\nv\r\n",
  )
  .await?;
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nINCRBYFLOAT\r\n$5\r\nlistk\r\n$3\r\n1.0\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("-WRONGTYPE"));

  info!("INCRBYFLOAT 边界测试通过");
  OK
}

/// 测试 SET 附加选项 (EX / NX / XX / GET / KEEPTTL) 与 SCAN 内省
/// 对标 C# BasicCommands.NetworkSETEXNX 选项矩阵与 Redis SCAN 单轮穷尽契约
#[compio::test]
async fn test_set_options_and_scan_suite() -> Void {
  info!("开始测试 SET 选项矩阵与 SCAN");
  let fixture = KvTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. SET k v EX 100 NX（新键）：写入成功 +OK，且 TTL 真实生效（~100s）
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$3\r\nSET\r\n$3\r\nsx1\r\n$2\r\nv1\r\n$2\r\nEX\r\n$3\r\n100\r\n$2\r\nNX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\nsx1\r\n").await?;
  let ttl: i64 = from_utf8(&resp)?.trim()[1..].parse()?;
  assert!((99..=100).contains(&ttl), "SET EX TTL 异常: {ttl}");

  // 2. SET sx1 v2 NX：键已存在 -> 不写入 ($-1)，且值未被覆盖
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$3\r\nSET\r\n$3\r\nsx1\r\n$2\r\nv2\r\n$2\r\nNX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nsx1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$2\r\nv1\r\n");

  // 3. SET sx1 v3 XX：键存在 -> 写入成功；SET missing v XX：键不存在 -> $-1
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$3\r\nSET\r\n$3\r\nsx1\r\n$2\r\nv3\r\n$2\r\nXX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$3\r\nSET\r\n$7\r\nmissing\r\n$1\r\nx\r\n$2\r\nXX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");

  // 4. SET sx1 v4 GET：回旧值 v3；GET + NX 命中失败同样回旧值
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$3\r\nSET\r\n$3\r\nsx1\r\n$2\r\nv4\r\n$3\r\nGET\r\n$2\r\nNX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$2\r\nv3\r\n");

  // 5. 非法选项与非法过期 -> syntax error / invalid expire time
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$3\r\nSET\r\n$3\r\nsx1\r\n$2\r\nv5\r\n$3\r\nFOO\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "-ERR syntax error\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$3\r\nSET\r\n$3\r\nsx1\r\n$2\r\nv5\r\n$2\r\nEX\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "-ERR invalid expire time in 'set' command\r\n"
  );

  // 6. SCAN 遍历：游标 0 起单轮穷尽，命中全部键并回 next=0
  send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$4\r\nsc:a\r\n$1\r\n1\r\n").await?;
  send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$4\r\nsc:b\r\n$1\r\n2\r\n").await?;
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nSCAN\r\n$1\r\n0\r\n$5\r\nMATCH\r\n$4\r\nsc:*\r\n$5\r\nCOUNT\r\n$2\r\n10\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("*2\r\n$1\r\n0\r\n"), "SCAN 回执形状异常: {s}");
  assert!(
    s.contains("$4\r\nsc:a\r\n") && s.contains("$4\r\nsc:b\r\n"),
    "SCAN 缺少命中键: {s}"
  );

  info!("SET 选项与 SCAN 测试通过");
  OK
}
