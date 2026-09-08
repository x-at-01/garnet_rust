//! 键过期 (EXPIRE) 家族集成测试，对标 Garnet RespTests / ExpiredKeyDeletionTask 语义：
//! EXPIRE/PEXPIRE/EXPIREAT/PEXPIREAT（NX/XX/GT/LT 选项）、TTL/PTTL、PERSIST、
//! EXPIRETIME/PEXPIRETIME、SETEX/PSETEX 与 SET 过期选项（EX/PX/EXAT/PXAT/KEEPTTL）
use std::{
  net::SocketAddr,
  str::from_utf8,
  sync::Arc,
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
  time::sleep,
};
use log::info;
use tempfile::{TempDir, tempdir};
use wedb_server::{ServerArgs, WedbServer};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 当前 UNIX 毫秒时间戳
fn now_ms() -> i64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap()
    .as_millis() as i64
}

/// 解析整数回复 `:123\r\n`
fn parse_int(resp: &[u8]) -> Result<i64> {
  let s = from_utf8(resp)?;
  Ok(s.trim()[1..].parse()?)
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

/// EXPIRE 家族测试脚手架
struct ExpireTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl ExpireTestFixture {
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

impl Drop for ExpireTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 EXPIRE 写入后即时 GET 命中、TTL/PTTL 区间、短 TTL 后惰性过期删除
#[compio::test]
async fn test_expire_roundtrip_and_lazy_expiry() -> Void {
  info!("开始测试 EXPIRE 即时命中与短 TTL 惰性过期");
  let fixture = ExpireTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. SET + EXPIRE -> :1，紧随的 GET 仍命中
  send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$3\r\nex1\r\n$1\r\nv\r\n").await?;
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$3\r\nex1\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nex1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$1\r\nv\r\n");

  // 2. TTL 秒级向上取整（活键不得回 0），PTTL 剩余毫秒落在 (0, 1000]
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\nex1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nPTTL\r\n$3\r\nex1\r\n").await?;
  let p = parse_int(&resp)?;
  assert!((1..=1000).contains(&p), "PTTL 区间异常: {p}");

  // 3. PEXPIRE 1500ms 续期 -> TTL 向上取整为 2（截断口径会误回 1）
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$7\r\nPEXPIRE\r\n$3\r\nex1\r\n$4\r\n1500\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\nex1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 4. TTL 到期后惰性删除：GET nil、TTL -2、PERSIST/EXPIRETIME 同步视作不存在
  sleep(Duration::from_millis(1700)).await;
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nex1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\nex1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-2\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$3\r\nex1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$10\r\nEXPIRETIME\r\n$3\r\nex1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-2\r\n");

  info!("EXPIRE 即时命中与惰性过期测试通过");
  OK
}

/// 测试 EXPIRE 第三参数 NX/XX/GT/LT 全分支、非法选项与负数时长即时删除
#[compio::test]
async fn test_expire_option_branches() -> Void {
  info!("开始测试 EXPIRE NX/XX/GT/LT 选项分支");
  let fixture = ExpireTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;
  let set = |key: &[u8], val: &[u8]| -> Vec<u8> {
    let mut req = format!("*3\r\n$3\r\nSET\r\n${}\r\n", key.len()).into_bytes();
    req.extend_from_slice(key);
    req.extend_from_slice(format!("\r\n${}\r\n", val.len()).as_bytes());
    req.extend_from_slice(val);
    req.extend_from_slice(b"\r\n");
    req
  };

  // 1. NX：仅当无 TTL 时生效
  send_and_recv(&mut client, &set(b"nx1", b"v")).await?;
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\nnx1\r\n$3\r\n100\r\n$2\r\nNX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\nnx1\r\n$2\r\n50\r\n$2\r\nNX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 2. XX：仅当已有 TTL 时生效
  send_and_recv(&mut client, &set(b"xx1", b"v")).await?;
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\nxx1\r\n$3\r\n100\r\n$2\r\nXX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\nnx1\r\n$3\r\n200\r\n$2\r\nXX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 3. GT：新过期时间必须更大
  send_and_recv(&mut client, &set(b"gt1", b"v")).await?;
  send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$3\r\ngt1\r\n$3\r\n100\r\n",
  )
  .await?;
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\ngt1\r\n$3\r\n200\r\n$2\r\nGT\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\ngt1\r\n$3\r\n150\r\n$2\r\nGT\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 4. LT：新过期时间必须更小
  send_and_recv(&mut client, &set(b"lt1", b"v")).await?;
  send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$3\r\nlt1\r\n$3\r\n100\r\n",
  )
  .await?;
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\nlt1\r\n$2\r\n50\r\n$2\r\nLT\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\nlt1\r\n$2\r\n80\r\n$2\r\nLT\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 5. 不存在键统一回 :0
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$6\r\nno_key\r\n$3\r\n100\r\n$2\r\nNX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 6. 非法选项 / 多选项 -> syntax error；非法时间量 -> integer 错误
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nEXPIRE\r\n$3\r\nnx1\r\n$2\r\n10\r\n$3\r\nFOO\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "-ERR syntax error\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nEXPIRE\r\n$3\r\nnx1\r\n$2\r\n10\r\n$2\r\nNX\r\n$2\r\nGT\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "-ERR syntax error\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$3\r\nnx1\r\n$3\r\nabc\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "-ERR value is not an integer or out of range\r\n"
  );

  // 7. 负数时长等价过去时间戳：立即物理删除并回 :1
  send_and_recv(&mut client, &set(b"neg1", b"v")).await?;
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$4\r\nneg1\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$4\r\nneg1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$4\r\nneg1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-2\r\n");

  info!("EXPIRE 选项分支测试通过");
  OK
}

/// 测试 PERSIST 移除 TTL 与 EXPIRETIME/PEXPIRETIME 绝对过期点查询
#[compio::test]
async fn test_persist_and_expiretime() -> Void {
  info!("开始测试 PERSIST 与 EXPIRETIME/PEXPIRETIME");
  let fixture = ExpireTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;
  let base_ms = now_ms();

  // 1. 永久键 PERSIST -> :0
  send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\np1\r\n$1\r\nv\r\n").await?;
  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$2\r\np1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 2. PERSIST 移除 TTL -> :1，TTL 归 -1，再次 PERSIST -> :0
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$2\r\np1\r\n$3\r\n100\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$2\r\np1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$2\r\np1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nPERSIST\r\n$2\r\np1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 3. PEXPIREAT 写入绝对过期点：EXPIRETIME 取整秒、PEXPIRETIME 直回毫秒
  let resp = send_and_recv(
    &mut client,
    format!(
      "*3\r\n$9\r\nPEXPIREAT\r\n$2\r\np1\r\n${}\r\n{}\r\n",
      (base_ms + 60_000).to_string().len(),
      base_ms + 60_000
    )
    .as_bytes(),
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$10\r\nEXPIRETIME\r\n$2\r\np1\r\n").await?;
  let exp_sec = parse_int(&resp)?;
  assert!(
    (base_ms / 1000 + 58..=base_ms / 1000 + 61).contains(&exp_sec),
    "EXPIRETIME 秒级异常: {exp_sec}"
  );
  let resp = send_and_recv(&mut client, b"*2\r\n$11\r\nPEXPIRETIME\r\n$2\r\np1\r\n").await?;
  let exp_ms = parse_int(&resp)?;
  assert!(
    (base_ms + 58_000..=base_ms + 61_000).contains(&exp_ms),
    "PEXPIRETIME 毫秒异常: {exp_ms}"
  );

  // 4. 无 TTL 键 -> :-1；不存在键 -> :-2
  send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\np2\r\n$1\r\nv\r\n").await?;
  let resp = send_and_recv(&mut client, b"*2\r\n$10\r\nEXPIRETIME\r\n$2\r\np2\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$11\r\nPEXPIRETIME\r\n$6\r\nno_key\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-2\r\n");

  info!("PERSIST 与 EXPIRETIME 测试通过");
  OK
}

/// 测试 SETEX/PSETEX 落盘 TTL，以及 SET 的 EX/PXAT/EXAT/KEEPTTL/NX/GET 选项
#[compio::test]
async fn test_setex_psetex_and_set_ttl_options() -> Void {
  info!("开始测试 SETEX/PSETEX 与 SET 过期选项");
  let fixture = ExpireTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;
  let base_ms = now_ms();

  // 1. SETEX 写入后 TTL>0，GET 命中
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nSETEX\r\n$2\r\ns1\r\n$3\r\n100\r\n$5\r\nhello\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\ns1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$5\r\nhello\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$2\r\ns1\r\n").await?;
  let ttl = parse_int(&resp)?;
  assert!((99..=100).contains(&ttl), "SETEX 后 TTL 异常: {ttl}");

  // 2. SETEX/PSETEX 非法过期时间
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nSETEX\r\n$5\r\ns_bad\r\n$2\r\n-1\r\n$1\r\nx\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "-ERR invalid expire time in 'setex' command\r\n"
  );
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nPSETEX\r\n$6\r\ns_bad2\r\n$1\r\n0\r\n$1\r\nx\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "-ERR invalid expire time in 'psetex' command\r\n"
  );

  // 3. PSETEX 毫秒 TTL
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nPSETEX\r\n$2\r\ns2\r\n$5\r\n60000\r\n$5\r\nworld\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nPTTL\r\n$2\r\ns2\r\n").await?;
  let p = parse_int(&resp)?;
  assert!((59_000..=60_000).contains(&p), "PSETEX 后 PTTL 异常: {p}");

  // 4. SET EX 路由 TTL；KEEPTTL 续写值保留过期点；普通 SET 清除 TTL
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$3\r\nSET\r\n$3\r\nse1\r\n$1\r\na\r\n$2\r\nEX\r\n$3\r\n100\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\nse1\r\n").await?;
  let ttl = parse_int(&resp)?;
  assert!((99..=100).contains(&ttl), "SET EX 后 TTL 异常: {ttl}");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$3\r\nSET\r\n$3\r\nse1\r\n$1\r\nb\r\n$7\r\nKEEPTTL\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\nse1\r\n").await?;
  let ttl = parse_int(&resp)?;
  assert!((95..=100).contains(&ttl), "KEEPTTL 未保留过期点: {ttl}");
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$3\r\nse1\r\n$1\r\nc\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\nse1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-1\r\n");

  // 5. SET PXAT 绝对毫秒过期；EXAT 过去时间戳立即删除
  let pxat = base_ms + 30_000;
  let resp = send_and_recv(
    &mut client,
    format!(
      "*5\r\n$3\r\nSET\r\n$3\r\nse2\r\n$1\r\nv\r\n$4\r\nPXAT\r\n${}\r\n{}\r\n",
      pxat.to_string().len(),
      pxat
    )
    .as_bytes(),
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\nse2\r\n").await?;
  let ttl = parse_int(&resp)?;
  assert!((28..=30).contains(&ttl), "SET PXAT 后 TTL 异常: {ttl}");
  let exat = base_ms / 1000 - 5;
  let resp = send_and_recv(
    &mut client,
    format!(
      "*5\r\n$3\r\nSET\r\n$3\r\nse3\r\n$1\r\nv\r\n$4\r\nEXAT\r\n${}\r\n{}\r\n",
      exat.to_string().len(),
      exat
    )
    .as_bytes(),
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nEXISTS\r\n$3\r\nse3\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 6. SET NX/XX/GET 语义：条件不满足回 nil 不覆盖；GET 回旧值
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$3\r\nSET\r\n$3\r\nse1\r\n$1\r\nd\r\n$2\r\nNX\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nse1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$1\r\nc\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$3\r\nSET\r\n$3\r\nse1\r\n$1\r\ne\r\n$3\r\nGET\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$1\r\nc\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nse1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$1\r\ne\r\n");

  // 7. SET 选项冲突与非法值
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$3\r\nSET\r\n$6\r\ns_bad3\r\n$1\r\nx\r\n$2\r\nEX\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "-ERR invalid expire time in 'set' command\r\n"
  );
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$3\r\nSET\r\n$6\r\ns_bad4\r\n$1\r\nx\r\n$3\r\nFOO\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "-ERR syntax error\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$3\r\nSET\r\n$6\r\ns_bad5\r\n$1\r\nx\r\n$2\r\nEX\r\n$2\r\n10\r\n$7\r\nKEEPTTL\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "-ERR syntax error\r\n");

  info!("SETEX/PSETEX 与 SET 过期选项测试通过");
  OK
}

/// 测试 EXPDELSCAN 按需主动扫描清理过期键（对标 Garnet ExpiredKeyDeletionTests）
#[compio::test]
async fn test_expdelscan_on_demand_and_validation() -> Void {
  info!("开始测试 EXPDELSCAN 按需扫描与合法性校验");
  // 1. 默认服务器：后台定时任务关闭，允许执行 EXPDELSCAN
  let fixture = ExpireTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 写入 3 个短 TTL 键，1 个长 TTL 键
  for i in 0..3 {
    let k = format!("expdel:{i}");
    send_and_recv(
      &mut client,
      format!("*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n$1\r\nv\r\n", k.len(), k).as_bytes(),
    )
    .await?;
    send_and_recv(
      &mut client,
      format!("*3\r\n$6\r\nEXPIRE\r\n${}\r\n{}\r\n$1\r\n1\r\n", k.len(), k).as_bytes(),
    )
    .await?;
  }
  send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$11\r\nexpdel:live\r\n$1\r\nv\r\n",
  )
  .await?;
  send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$11\r\nexpdel:live\r\n$3\r\n100\r\n",
  )
  .await?;

  // 等待 3 个短 TTL 键到期
  sleep(Duration::from_millis(1200)).await;

  // 执行 EXPDELSCAN -> 应该扫描到记录并清理 3 个过期键
  let resp = send_and_recv(&mut client, b"*1\r\n$10\r\nEXPDELSCAN\r\n").await?;
  let resp_str = from_utf8(&resp)?;
  assert!(
    resp_str.starts_with("*2\r\n:3\r\n"),
    "EXPDELSCAN 应返回删除 3 个过期键: {resp_str}"
  );

  // 验证 3 个短 TTL 键已不存在，存活键仍然存在
  for i in 0..3 {
    let k = format!("expdel:{i}");
    let r = send_and_recv(
      &mut client,
      format!("*2\r\n$6\r\nEXISTS\r\n${}\r\n{}\r\n", k.len(), k).as_bytes(),
    )
    .await?;
    assert_eq!(from_utf8(&r)?, ":0\r\n");
  }
  let r = send_and_recv(&mut client, b"*2\r\n$6\r\nEXISTS\r\n$11\r\nexpdel:live\r\n").await?;
  assert_eq!(from_utf8(&r)?, ":1\r\n");

  drop(client);
  drop(fixture);

  // 2. 开启后台扫描任务时，EXPDELSCAN 必须拒绝执行（对齐 Garnet 规范）
  let dir = tempdir()?;
  let dir_path = dir.path().to_string_lossy().to_string();
  let args = ServerArgs {
    port: 0,
    dir: dir_path,
    quiet: true,
    gc_enabled: true,
    expired_scan_interval_ms: 1000,
    ..Default::default()
  };
  let server = Arc::new(WedbServer::new(args).await?);
  let addr = server.start().await?;
  let mut client = TcpStream::connect(addr).await?;

  let resp = send_and_recv(&mut client, b"*1\r\n$10\r\nEXPDELSCAN\r\n").await?;
  assert_eq!(
    from_utf8(&resp)?,
    "-ERR Cannot execute EXPDELSCAN with background expired key deletion scan enabled\r\n"
  );

  server.dispose();
  info!("EXPDELSCAN 测试通过");
  OK
}

/// 测试 GETEX 命令（取旧值并更新/移除 TTL）
#[compio::test]
async fn test_getex_command() -> Void {
  info!("开始测试 GETEX 命令");
  let fixture = ExpireTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 不存在键 -> nil
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nGETEX\r\n$6\r\nno_key\r\n").await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");

  // 2. GETEX key EX 10 -> 返回旧值并设置 TTL
  send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$3\r\ngx1\r\n$5\r\nhello\r\n",
  )
  .await?;
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nGETEX\r\n$3\r\ngx1\r\n$2\r\nEX\r\n$2\r\n10\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$5\r\nhello\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\ngx1\r\n").await?;
  let ttl = parse_int(&resp)?;
  assert!((9..=10).contains(&ttl));

  // 3. GETEX key PERSIST -> 返回旧值并清除 TTL
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nGETEX\r\n$3\r\ngx1\r\n$7\r\nPERSIST\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$5\r\nhello\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$3\r\ngx1\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-1\r\n");

  info!("GETEX 测试通过");
  OK
}

/// 测试集合删空时自动清除 TTL，杜绝残留孤儿
#[compio::test]
async fn test_no_orphan_ttl_on_collection_clear() -> Void {
  info!("开始测试集合删空时清除 TTL 杜绝残留孤儿");
  let fixture = ExpireTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 写入 Hash 字段并设置 TTL
  send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nHSET\r\n$7\r\nh_clear\r\n$2\r\nf1\r\n$2\r\nv1\r\n",
  )
  .await?;
  send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$7\r\nh_clear\r\n$3\r\n100\r\n",
  )
  .await?;
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$7\r\nh_clear\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":100\r\n");

  // HDEL 删空最后一个字段
  send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nHDEL\r\n$7\r\nh_clear\r\n$2\r\nf1\r\n",
  )
  .await?;

  // 键已不存在，TTL 也应返回 -2（已联动彻底删除，不留孤儿）
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nEXISTS\r\n$7\r\nh_clear\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nTTL\r\n$7\r\nh_clear\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":-2\r\n");

  info!("集合删空孤儿清除测试通过");
  OK
}
