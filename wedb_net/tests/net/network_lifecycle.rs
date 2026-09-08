use std::time::Instant;

use aok::{OK, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
  time::sleep,
};
use log::info;

use crate::support::{
  DEFAULT_TIMEOUT, NetworkTestFixture, POLL_INTERVAL, READ_CHUNK_SIZE, send_and_recv,
  send_and_recv_exact,
};

/// 模拟突发关闭套接字的重复次数
const ABRUPT_DROP_COUNT: usize = 3;
/// 并发客户端数量
const CLIENT_COUNT: usize = 5;
/// 注入并快速断开的连接数量
const INJECTION_COUNT: usize = 3;

/// 静态测试键值写读指令常量
const SET_MYKEY_CMD: &[u8] = b"*3\r\n$3\r\nSET\r\n$5\r\nmykey\r\n$7\r\nabcdefg\r\n";
const GET_MYKEY_CMD: &[u8] = b"*2\r\n$3\r\nGET\r\n$5\r\nmykey\r\n";
const EXPECTED_MYKEY_RESP: &[u8] = b"$7\r\nabcdefg\r\n";

/// 对标 Garnet NetworkTests.NetworkExceptions: 网络异常中断与故障恢复测试
/// 在多次突发连接中断后，服务端保持稳定，客户端重新连接并验证 SET/GET 读写完整性。
#[compio::test]
async fn test_network_exceptions_recovery() -> Void {
  let fixture = NetworkTestFixture::setup().await?;

  // 1. 模拟多次突发连接中断（发起连接后未正常握手立即关闭套接字）
  for _ in 0..ABRUPT_DROP_COUNT {
    let raw_socket = fixture.connect_client().await;
    if let Ok(stream) = raw_socket {
      drop(stream);
    }
    sleep(POLL_INTERVAL).await;
  }

  // 2. 故障恢复后建立正常连接，验证键值读写完整性（使用编译期常量零运行时开销）
  let mut client = fixture.connect_client().await?;
  let resp = send_and_recv(&mut client, SET_MYKEY_CMD).await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, GET_MYKEY_CMD).await?;
  assert_eq!(resp, EXPECTED_MYKEY_RESP);

  info!("网络异常中断与故障恢复测试通过");
  OK
}

/// 对标 Garnet NetworkTests.TlsClientDisconnectCleansUpHandler: 客户端断开连接时服务端释放处理器测试
/// 客户端突然断开连接（模拟 FIN/RST），服务端感知并释放连接资源，活跃连接数归零且累计销毁数递增，后续新连接正常可用。
#[compio::test]
async fn test_client_abrupt_disconnect_cleans_up_handler() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let server = fixture.server();
  let disposed_before = server.total_connections_disposed();

  let mut clients = Vec::with_capacity(CLIENT_COUNT);

  // 1. 连接多个客户端并验证指令响应
  for i in 0..CLIENT_COUNT {
    let mut client = fixture.connect_client().await?;
    let set_cmd = format!("*3\r\n$3\r\nSET\r\n$4\r\nkey{}\r\n$6\r\nvalue{}\r\n", i, i);
    let resp = send_and_recv(&mut client, set_cmd.as_bytes()).await?;
    assert_eq!(resp, b"+OK\r\n");
    clients.push(client);
  }

  // 等待服务端活跃连接数登记
  let registered = fixture
    .wait_for_active_connections(CLIENT_COUNT, DEFAULT_TIMEOUT)
    .await;
  assert!(registered, "服务端应将所有客户端连接登记为活跃连接");

  // 2. 突然丢弃并关闭所有客户端套接字模拟远程对端突然断开
  drop(clients);

  // 等待服务端检测到对端断开并完成连接资源释放回收
  let cleaned = fixture
    .wait_for_active_connections(0, DEFAULT_TIMEOUT)
    .await;
  assert!(
    cleaned,
    "所有客户端断开后，活跃连接数必须归零，严防资源泄漏"
  );
  assert!(
    server.total_connections_disposed() - disposed_before >= CLIENT_COUNT,
    "累计释放连接数应至少递增断开的客户端数量"
  );

  // 3. 验证清理后服务端仍能正常接收新连接并执行命令
  let mut new_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut new_client,
    b"*3\r\n$3\r\nSET\r\n$13\r\nafter_cleanup\r\n$5\r\nworks\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  info!("客户端异常断开连接资源清理测试通过");
  OK
}

/// 对标 Garnet NetworkTests.DisposeCallsDisposeImplWithoutSaeaBackup: 服务清理与连接释放测试
/// 快速连接并立即断开，等待连接全部销毁，调用 purge 与 dispose_active_handlers，确保无句柄泄漏。
#[compio::test]
async fn test_dispose_calls_dispose_impl() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let server = fixture.server();

  // 初始无活跃连接
  assert_eq!(server.active_connections(), 0);

  let received_before = server.total_connections_received();

  // 快速建立连接并迅速断开
  for _ in 0..INJECTION_COUNT {
    if let Ok(client) = fixture.connect_client().await {
      drop(client);
    }
  }

  // 等待服务端接收与释放
  let received = fixture
    .wait_for_connections_received(received_before + INJECTION_COUNT, DEFAULT_TIMEOUT)
    .await;
  assert!(received, "服务端应接收到全部连接注入");

  let cleaned = fixture
    .wait_for_active_connections(0, DEFAULT_TIMEOUT)
    .await;
  assert!(cleaned, "连接应全部完成释放");

  // 执行缓冲池清理与停机资源释放
  server.network_pool.purge();
  server.shutdown();
  assert!(server.is_disposed());

  info!("服务清理销毁测试通过");
  OK
}

/// 服务端优雅停机信号与资源清理测试
/// 对标 Garnet GarnetServerBase TearDown/Dispose: 服务端正常处理命令后触发停机，新连接被拒绝。
#[compio::test]
async fn test_graceful_shutdown() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let addr = fixture.addr;

  let mut client = fixture.connect_client().await?;
  let resp = send_and_recv(&mut client, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  // 触发停机
  fixture.server.shutdown();
  assert!(fixture.server.is_disposed());

  // 轮询确认新连接被拒绝（确定性测试，避免固定死等）
  let mut refused = false;
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  while Instant::now() < deadline {
    if TcpStream::connect(addr).await.is_err() {
      refused = true;
      break;
    }
    sleep(POLL_INTERVAL).await;
  }
  assert!(refused, "停机后监听套接字已关闭，新连接应被拒绝");

  info!("优雅停机测试完成");
  OK
}

/// 半开请求突发断连后的资源回收与服务恢复
/// 对标 Garnet 异常断线恢复逻辑：客户端发送不完整命令突然关闭连接，服务端正确回收且不影响后续服务。
#[compio::test]
async fn test_partial_request_abrupt_disconnect_recovery() -> Void {
  let fixture = NetworkTestFixture::setup().await?;

  // 1. 发送半条命令后立即断开
  {
    let mut client = fixture.connect_client().await?;
    let (write_res, _) = client
      .write_all(b"*3\r\n$3\r\nSET\r\n$4\r\nha".to_vec())
      .await
      .into();
    write_res?;
    drop(client);
  }

  // 等待服务端感知断开并完成资源回收
  let cleaned = fixture
    .wait_for_active_connections(0, DEFAULT_TIMEOUT)
    .await;
  assert!(cleaned, "突发断连后活跃连接应归零");

  // 2. 服务端恢复能力：新连接可正常完整读写
  let mut client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$4\r\nhalf\r\n$4\r\ndone\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$4\r\nhalf\r\n").await?;
  assert_eq!(resp, b"$4\r\ndone\r\n");

  info!("半开请求突发断连资源回收验证通过");
  OK
}

/// 客户端在事务中途突发断开连接后的资源回收与锁释放
/// 对标 Garnet TransactionManager 清理逻辑：会话断连时自动清理 WATCH 与未提交事务上下文。
#[compio::test]
async fn test_transaction_abrupt_disconnect_cleanup() -> Void {
  let fixture = NetworkTestFixture::setup().await?;

  // 1. 客户端 1 开启监视与事务并入队命令后突发断连
  {
    let mut client1 = fixture.connect_client().await?;
    let resp = send_and_recv(&mut client1, b"*2\r\n$5\r\nWATCH\r\n$5\r\ntxkey\r\n").await?;
    assert_eq!(resp, b"+OK\r\n");

    let resp = send_and_recv(&mut client1, b"MULTI\r\n").await?;
    assert_eq!(resp, b"+OK\r\n");

    let resp = send_and_recv(
      &mut client1,
      b"*3\r\n$3\r\nSET\r\n$5\r\ntxkey\r\n$5\r\nval_1\r\n",
    )
    .await?;
    assert_eq!(resp, b"+QUEUED\r\n");

    // 突发直接关闭套接字
    drop(client1);
  }

  // 等待服务端感知连接关闭并回收
  let cleaned = fixture
    .wait_for_active_connections(0, DEFAULT_TIMEOUT)
    .await;
  assert!(cleaned, "突发断开后活跃连接数应归零");

  // 2. 客户端 2 连接并正常执行事务，验证锁与状态已彻底释放
  let mut client2 = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut client2,
    b"*3\r\n$3\r\nSET\r\n$5\r\ntxkey\r\n$5\r\nval_2\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client2, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client2, b"*2\r\n$3\r\nGET\r\n$5\r\ntxkey\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client2, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*1\r\n$5\r\nval_2\r\n");

  info!("事务中途突发断开资源与锁释放验证通过");
  OK
}

/// 慢客户端背压测试：客户端写出大量大响应命令后暂不读取
/// 服务端响应量远超写出通道定额时读循环在写通道上挂起（不无界积压内存），
/// 客户端恢复读取后全部响应零丢失零错位回收，会话保持健康。
#[compio::test]
async fn test_slow_client_backpressure_no_loss() -> Void {
  /// 大页容量（容纳 32KB 值的记录）
  const PAGE_SIZE: usize = 256 * 1024;
  /// 单个值字节数
  const VALUE_LEN: usize = 32 * 1024;
  /// 流水线 GET 命令数（总响应量 800×32KB≈25MB，远超写出通道 256×64KB 定额）
  const GET_COUNT: usize = 800;
  /// 单条 GET 命令报文
  const GET_BIG: &[u8] = b"*2\r\n$3\r\nGET\r\n$3\r\nbig\r\n";

  let fixture = NetworkTestFixture::setup_with_page_size(PAGE_SIZE).await?;
  let mut client = fixture.connect_client().await?;

  // 1. 写入 32KB 大值并确认
  let value: Vec<u8> = (0..VALUE_LEN).map(|i| (i % 251) as u8).collect();
  let mut set_cmd = format!("*3\r\n$3\r\nSET\r\n$3\r\nbig\r\n${VALUE_LEN}\r\n").into_bytes();
  set_cmd.extend_from_slice(&value);
  set_cmd.extend_from_slice(b"\r\n");
  assert_eq!(
    send_and_recv_exact(&mut client, &set_cmd, 5).await?,
    b"+OK\r\n"
  );

  // 单条 GET 响应的期望字节：$32768\r\n + value + \r\n
  let mut one_reply = Vec::with_capacity(VALUE_LEN + 12);
  one_reply.extend_from_slice(format!("${VALUE_LEN}\r\n").as_bytes());
  one_reply.extend_from_slice(&value);
  one_reply.extend_from_slice(b"\r\n");

  // 2. 客户端一次性写出全部 GET 后暂不读取：触发服务端对慢客户端的写通道背压
  let mut pipeline = Vec::with_capacity(GET_COUNT * GET_BIG.len());
  for _ in 0..GET_COUNT {
    pipeline.extend_from_slice(GET_BIG);
  }
  let (write_res, _) = client.write_all(pipeline).await.into();
  write_res?;

  // 3. 流式精确回收全部响应：校验零丢失、零错位
  let reply_len = one_reply.len();
  let mut pending: Vec<u8> = Vec::with_capacity(READ_CHUNK_SIZE);
  let mut rbuf = Vec::with_capacity(READ_CHUNK_SIZE);
  for _ in 0..GET_COUNT {
    while pending.len() < reply_len {
      rbuf.clear();
      let BufResult(read_res, returned_buf) = client.read(rbuf).await;
      rbuf = returned_buf;
      let n = read_res?;
      assert!(n > 0, "服务端在响应回收完成前关闭了连接");
      pending.extend_from_slice(&rbuf[..n]);
    }
    assert!(pending[..reply_len] == one_reply[..], "响应流错位或丢失");
    pending.drain(..reply_len);
  }

  // 4. 背压解除后会话保持健康
  assert_eq!(send_and_recv(&mut client, b"PING\r\n").await?, b"+PONG\r\n");

  info!("慢客户端背压零丢失验证通过");
  OK
}

/// 订阅客户端突发断连后的发布订阅资源级联清理
/// 验证连接断开时级联注销频道订阅、推送协程正常退出，后续发布无孤儿监听。
#[compio::test]
async fn test_pubsub_disconnect_cleanup() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let server = fixture.server();
  let mut publisher = fixture.connect_client().await?;

  // 1. 订阅者订阅 news 频道后突发断连
  {
    let mut subscriber = fixture.connect_client().await?;
    let resp = send_and_recv(&mut subscriber, b"*2\r\n$9\r\nSUBSCRIBE\r\n$4\r\nnews\r\n").await?;
    assert_eq!(resp, b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n");
    assert_eq!(server.pubsub_broker.num_subscriptions(b"news"), 1);
    drop(subscriber);
  }

  // 2. 轮询等待服务端级联清理订阅注册与后台推送协程
  let deadline = Instant::now() + DEFAULT_TIMEOUT;
  while server.pubsub_broker.num_subscriptions(b"news") > 0 && Instant::now() < deadline {
    sleep(POLL_INTERVAL).await;
  }
  assert_eq!(
    server.pubsub_broker.num_subscriptions(b"news"),
    0,
    "断连后订阅注册应被级联清理"
  );

  let cleaned = fixture
    .wait_for_active_connections(1, DEFAULT_TIMEOUT)
    .await;
  assert!(cleaned, "断连后仅保留发布者连接");

  // 3. 清理后发布消息返回零订阅者，且服务保持健康
  let resp = send_and_recv(
    &mut publisher,
    b"*3\r\n$7\r\nPUBLISH\r\n$4\r\nnews\r\n$2\r\nhi\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");

  info!("订阅断连级联清理验证通过");
  OK
}
