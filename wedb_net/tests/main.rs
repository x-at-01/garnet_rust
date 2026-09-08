use std::{
  net::SocketAddr,
  sync::Arc,
  time::{Duration, Instant},
};

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
use wdev::SegmentedDevice;
use wedb_net::{NetConfig, WedbServer};
use wkv::{StoreConfig, WedbStore};

/// 冒烟测试默认配置常量
const SMOKE_PAGE_SIZE: usize = 64 * 1024;
const SMOKE_TABLE_SIZE: usize = 1024;
const SMOKE_LOG_PAGES: usize = 16;
const SMOKE_MUTABLE_FRACTION: f64 = 0.5;
const SMOKE_BUF_SIZE: usize = 2048;
const SMOKE_TIMEOUT: Duration = Duration::from_secs(3);
const SMOKE_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 辅助函数：启动基于真实临时存储设备的端到端冒烟测试网络服务
async fn start_smoke_server() -> Result<(WedbServer<SegmentedDevice>, SocketAddr, TempDir)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("wedb_smoke.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store_cfg = StoreConfig::new(
    SMOKE_TABLE_SIZE,
    SMOKE_PAGE_SIZE,
    SMOKE_LOG_PAGES,
    SMOKE_MUTABLE_FRACTION,
  )?;
  let store = Arc::new(WedbStore::open(store_cfg, device)?);
  let net_cfg = NetConfig::new(([127, 0, 0, 1], 0));
  let server = WedbServer::new(net_cfg, store);
  let addr = server.start().await?;
  Ok((server, addr, dir))
}

/// 辅助函数：向客户端流发送请求并读取响应报文（原地裁剪避免二次分配）
async fn send_and_recv(stream: &mut TcpStream, req: &[u8]) -> Result<Vec<u8>> {
  let BufResult(write_res, _) = stream.write_all(req.to_vec()).await;
  write_res?;
  let buf = Vec::with_capacity(SMOKE_BUF_SIZE);
  let BufResult(read_res, mut buf) = stream.read(buf).await;
  let n = read_res?;
  buf.truncate(n);
  Ok(buf)
}

/// 冒烟测试一：基础连接控制与回显命令端到端验证
#[compio::test]
async fn test_smoke_basic_commands() -> Void {
  let (_server, addr, _dir) = start_smoke_server().await?;
  let mut client = TcpStream::connect(addr).await?;

  // 1. 无参心跳命令
  let resp = send_and_recv(&mut client, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  // 2. 带参数心跳
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nPING\r\n$5\r\nhello\r\n").await?;
  assert_eq!(resp, b"$5\r\nhello\r\n");

  // 3. 回显命令
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nECHO\r\n$5\r\nworld\r\n").await?;
  assert_eq!(resp, b"$5\r\nworld\r\n");

  // 4. 选择切换数据库
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$1\r\n1\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 5. 退出连接命令
  let resp = send_and_recv(&mut client, b"QUIT\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  info!("基础控制命令冒烟测试通过");
  OK
}

/// 冒烟测试二：字符串键值读写增删查端到端验证
#[compio::test]
async fn test_smoke_string_crud() -> Void {
  let (_server, addr, _dir) = start_smoke_server().await?;
  let mut client = TcpStream::connect(addr).await?;

  // 1. 读取不存在的键返回空批量字符串
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  // 2. 设置键值对
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 3. 读取键值
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n").await?;
  assert_eq!(resp, b"$3\r\nbar\r\n");

  // 4. 检查键是否存在
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nEXISTS\r\n$3\r\nfoo\r\n").await?;
  assert_eq!(resp, b":1\r\n");

  // 5. SETNX 互斥设置冲突
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nSETNX\r\n$3\r\nfoo\r\n$5\r\nbar99\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");

  // 6. 删除键
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nDEL\r\n$3\r\nfoo\r\n").await?;
  assert_eq!(resp, b":1\r\n");

  // 7. MSET 与 MGET 批量操作
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nMSET\r\n$2\r\nk1\r\n$2\r\nv1\r\n$2\r\nk2\r\n$2\r\nv2\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nMGET\r\n$2\r\nk1\r\n$2\r\nk2\r\n$2\r\nk3\r\n",
  )
  .await?;
  assert_eq!(resp, b"*3\r\n$2\r\nv1\r\n$2\r\nv2\r\n$-1\r\n");

  info!("字符串数据操作冒烟测试通过");
  OK
}

/// 冒烟测试三：复合数据结构（哈希、列表、集合、有序集合）端到端验证
#[compio::test]
async fn test_smoke_data_structures() -> Void {
  let (_server, addr, _dir) = start_smoke_server().await?;
  let mut client = TcpStream::connect(addr).await?;

  // 1. 哈希字典操作
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nHSET\r\n$6\r\nmyhash\r\n$2\r\nf1\r\n$2\r\nv1\r\n$2\r\nf2\r\n$2\r\nv2\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nHGET\r\n$6\r\nmyhash\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(resp, b"$2\r\nv1\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nHLEN\r\n$6\r\nmyhash\r\n").await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$7\r\nHEXISTS\r\n$6\r\nmyhash\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nHDEL\r\n$6\r\nmyhash\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 2. 列表操作
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nLPUSH\r\n$6\r\nmylist\r\n$2\r\ne1\r\n$2\r\ne2\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nLLEN\r\n$6\r\nmylist\r\n").await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nLPOP\r\n$6\r\nmylist\r\n").await?;
  assert_eq!(resp, b"$2\r\ne2\r\n");

  // 3. 集合操作
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nSADD\r\n$5\r\nmyset\r\n$2\r\nm1\r\n$2\r\nm2\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nSCARD\r\n$5\r\nmyset\r\n").await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$9\r\nSISMEMBER\r\n$5\r\nmyset\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nSREM\r\n$5\r\nmyset\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 4. 有序集合操作
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nZADD\r\n$6\r\nmyzset\r\n$3\r\n1.5\r\n$2\r\nz1\r\n$3\r\n2.5\r\n$2\r\nz2\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nZCARD\r\n$6\r\nmyzset\r\n").await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nZSCORE\r\n$6\r\nmyzset\r\n$2\r\nz1\r\n",
  )
  .await?;
  assert_eq!(resp, b"$3\r\n1.5\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZRANGE\r\n$6\r\nmyzset\r\n$1\r\n0\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert_eq!(resp, b"*2\r\n$2\r\nz1\r\n$2\r\nz2\r\n");

  info!("复合数据结构冒烟测试通过");
  OK
}

/// 冒烟测试四：事务 MULTI/EXEC/DISCARD 与乐观锁 WATCH 端到端验证
#[compio::test]
async fn test_smoke_transaction() -> Void {
  let (_server, addr, _dir) = start_smoke_server().await?;
  let mut client1 = TcpStream::connect(addr).await?;
  let mut client2 = TcpStream::connect(addr).await?;

  // 1. 开启事务排队并成功执行
  let resp = send_and_recv(&mut client1, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut client1,
    b"*3\r\n$3\r\nSET\r\n$3\r\ntxk\r\n$3\r\ntxv\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client1, b"*2\r\n$3\r\nGET\r\n$3\r\ntxk\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client1, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*2\r\n+OK\r\n$3\r\ntxv\r\n");

  // 2. 放弃事务排队
  let resp = send_and_recv(&mut client1, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut client1,
    b"*3\r\n$3\r\nSET\r\n$3\r\ntxk\r\n$6\r\ntxv_no\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client1, b"DISCARD\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client1, b"*2\r\n$3\r\nGET\r\n$3\r\ntxk\r\n").await?;
  assert_eq!(resp, b"$3\r\ntxv\r\n");

  // 3. WATCH 乐观锁检测并发冲突回滚
  let resp = send_and_recv(&mut client1, b"*2\r\n$5\r\nWATCH\r\n$2\r\nwk\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 客户端 2 并发修改
  let resp = send_and_recv(
    &mut client2,
    b"*3\r\n$3\r\nSET\r\n$2\r\nwk\r\n$8\r\nmodified\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 客户端 1 开启事务并提交
  let resp = send_and_recv(&mut client1, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut client1,
    b"*3\r\n$3\r\nSET\r\n$2\r\nwk\r\n$4\r\nfail\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client1, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*-1\r\n");

  info!("事务控制冒烟测试通过");
  OK
}

/// 冒烟测试五：发布订阅异步实时推送端到端验证
#[compio::test]
async fn test_smoke_pubsub() -> Void {
  let (_server, addr, _dir) = start_smoke_server().await?;
  let mut sub_client = TcpStream::connect(addr).await?;
  let mut pub_client = TcpStream::connect(addr).await?;

  // 1. 订阅频道
  let sub_req = b"*2\r\n$9\r\nSUBSCRIBE\r\n$4\r\nnews\r\n";
  let resp = send_and_recv(&mut sub_client, sub_req).await?;
  assert_eq!(resp, b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n");

  // 2. 发布消息
  let pub_req = b"*3\r\n$7\r\nPUBLISH\r\n$4\r\nnews\r\n$5\r\nhello\r\n";
  let resp = send_and_recv(&mut pub_client, pub_req).await?;
  assert_eq!(resp, b":1\r\n");

  // 3. 订阅者接收推送消息
  let buf = Vec::with_capacity(SMOKE_BUF_SIZE);
  let BufResult(read_res, buf) = sub_client.read(buf).await;
  let n = read_res?;
  assert_eq!(
    &buf[..n],
    b"*3\r\n$7\r\nmessage\r\n$4\r\nnews\r\n$5\r\nhello\r\n"
  );

  // 4. 处于订阅模式下的心跳 PING
  let ping_resp = send_and_recv(&mut sub_client, b"PING\r\n").await?;
  assert_eq!(ping_resp, b"*2\r\n$4\r\npong\r\n$0\r\n\r\n");

  // 5. 退订频道
  let unsub_req = b"*2\r\n$11\r\nUNSUBSCRIBE\r\n$4\r\nnews\r\n";
  let resp = send_and_recv(&mut sub_client, unsub_req).await?;
  assert_eq!(resp, b"*3\r\n$11\r\nunsubscribe\r\n$4\r\nnews\r\n:0\r\n");

  info!("发布订阅冒烟测试通过");
  OK
}

/// 冒烟测试六：多客户端并发连接与独立键读写端到端验证
#[compio::test]
async fn test_smoke_concurrent_connections() -> Void {
  let (server, addr, _dir) = start_smoke_server().await?;
  const CLIENT_COUNT: usize = 10;
  let mut handles = Vec::with_capacity(CLIENT_COUNT);

  for i in 0..CLIENT_COUNT {
    let handle = spawn(async move {
      let mut client = TcpStream::connect(addr).await?;
      let k = format!("user:{}", i);
      let v = format!("payload:{}", i);
      let set_cmd = format!(
        "*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
        k.len(),
        k.as_str(),
        v.len(),
        v.as_str()
      );
      let resp = send_and_recv(&mut client, set_cmd.as_bytes()).await?;
      assert_eq!(resp, b"+OK\r\n");

      let get_cmd = format!("*2\r\n$3\r\nGET\r\n${}\r\n{}\r\n", k.len(), k.as_str());
      let resp = send_and_recv(&mut client, get_cmd.as_bytes()).await?;
      let expected = format!("${}\r\n{}\r\n", v.len(), v.as_str());
      assert_eq!(resp, expected.as_bytes());

      OK
    });
    handles.push(handle);
  }

  for h in handles {
    h.await.unwrap()?;
  }

  // 确定性轮询等待后台协程完全处理断开连接，严防固定死等
  let deadline = Instant::now() + SMOKE_TIMEOUT;
  while server.active_connections() > 0 && Instant::now() < deadline {
    sleep(SMOKE_POLL_INTERVAL).await;
  }
  assert_eq!(server.active_connections(), 0);

  info!("并发连接冒烟测试通过");
  OK
}
