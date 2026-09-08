use std::{net::SocketAddr, num::NonZeroUsize, str::from_utf8, sync::Arc};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
};
use log::info;
use tempfile::tempdir;
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

#[compio::test]
async fn test_server_full_integration() -> Void {
  let dir = tempdir()?;
  let args = ServerArgs {
    port: 0,
    dir: dir.path().to_string_lossy().to_string(),
    cluster_enabled: true,
    requirepass: Some("test_password".to_string()),
    quiet: true,
    ..Default::default()
  };

  let server = Arc::new(WedbServer::new(args).await?);
  let addr: SocketAddr = server.start().await?;
  info!("WeDB 集成测试服务已就绪, 监听地址: {}", addr);

  let mut client = TcpStream::connect(addr).await?;

  // 1. 未认证前拦截
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"-NOAUTH Authentication required.\r\n");

  // 2. 密码错误认证拦截
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nAUTH\r\n$6\r\nwrong1\r\n").await?;
  assert_eq!(
    resp,
    b"-WRONGPASS invalid username-password pair or user is disabled.\r\n"
  );

  // 3. 正确密码认证通过
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nAUTH\r\n$13\r\ntest_password\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 4. PING / PONG / ECHO
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nECHO\r\n$5\r\nhello\r\n").await?;
  assert_eq!(resp, b"$5\r\nhello\r\n");

  // 5. KV 基础存储命令 (SET, GET, EXISTS, DEL)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$6\r\nmy_key\r\n$8\r\nmy_value\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$6\r\nmy_key\r\n").await?;
  assert_eq!(resp, b"$8\r\nmy_value\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nEXISTS\r\n$6\r\nmy_key\r\n").await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nDEL\r\n$6\r\nmy_key\r\n").await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$6\r\nmy_key\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  // 6. 富数据结构: HASH (HSET, HGET, HLEN, HDEL)
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nHSET\r\n$7\r\nmy_hash\r\n$2\r\nf1\r\n$2\r\nv1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nHGET\r\n$7\r\nmy_hash\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(resp, b"$2\r\nv1\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nHLEN\r\n$7\r\nmy_hash\r\n").await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nHDEL\r\n$7\r\nmy_hash\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 7. 富数据结构: LIST (LPUSH, LLEN, RPOP)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nLPUSH\r\n$7\r\nmy_list\r\n$5\r\nitem1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nLLEN\r\n$7\r\nmy_list\r\n").await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nRPOP\r\n$7\r\nmy_list\r\n").await?;
  assert_eq!(resp, b"$5\r\nitem1\r\n");

  // 8. 富数据结构: SET (SADD, SCARD, SREM)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nSADD\r\n$6\r\nmy_set\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nSCARD\r\n$6\r\nmy_set\r\n").await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nSREM\r\n$6\r\nmy_set\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 9. 集群协议 (CLUSTER INFO, CLUSTER NODES, CLUSTER KEYSLOT)
  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nCLUSTER\r\n$4\r\nINFO\r\n").await?;
  let info_str = from_utf8(&resp).unwrap_or("");
  assert!(info_str.contains("cluster_state:ok"));
  assert!(info_str.contains("cluster_slots_assigned:16384"));

  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nCLUSTER\r\n$5\r\nNODES\r\n").await?;
  let nodes_str = from_utf8(&resp).unwrap_or("");
  assert!(nodes_str.contains("myself,master"));

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$7\r\nCLUSTER\r\n$7\r\nKEYSLOT\r\n$3\r\nfoo\r\n",
  )
  .await?;
  assert_eq!(resp, b":12182\r\n");

  // 10. 事务原子执行 (MULTI, QUEUED, EXEC)
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$4\r\ntx_k\r\n$4\r\ntx_v\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$4\r\ntx_k\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await?;
  assert_eq!(resp, b"*2\r\n+OK\r\n$4\r\ntx_v\r\n");

  // 11. 优雅停机
  server.stop().await?;
  assert!(server.is_disposed());

  info!("WeDB 完整集成测试全部通过!");
  OK
}

#[compio::test]
async fn test_server_threaded_multicore() -> Void {
  let dir = tempdir()?;
  let args = ServerArgs {
    port: 0,
    dir: dir.path().to_string_lossy().to_string(),
    quiet: true,
    ..Default::default()
  };

  let server = Arc::new(WedbServer::new(args).await?);
  let addr = server
    .start_threaded(Some(NonZeroUsize::new(2).unwrap()))
    .await?;
  info!("WeDB 多核 SO_REUSEPORT 测试服务已就绪, 监听地址: {}", addr);

  // 并发连接两个客户端并分别执行写入与读取
  let mut client1 = TcpStream::connect(addr).await?;
  let mut client2 = TcpStream::connect(addr).await?;

  let resp1 = send_and_recv(&mut client1, b"*3\r\n$3\r\nSET\r\n$2\r\nk1\r\n$2\r\nv1\r\n").await?;
  assert_eq!(resp1, b"+OK\r\n");

  let resp2 = send_and_recv(&mut client2, b"*3\r\n$3\r\nSET\r\n$2\r\nk2\r\n$2\r\nv2\r\n").await?;
  assert_eq!(resp2, b"+OK\r\n");

  let resp1 = send_and_recv(&mut client1, b"*2\r\n$3\r\nGET\r\n$2\r\nk2\r\n").await?;
  assert_eq!(resp1, b"$2\r\nv2\r\n");

  let resp2 = send_and_recv(&mut client2, b"*2\r\n$3\r\nGET\r\n$2\r\nk1\r\n").await?;
  assert_eq!(resp2, b"$2\r\nv1\r\n");

  server.stop().await?;
  assert!(server.is_disposed());
  info!("WeDB 多核 SO_REUSEPORT 测试通过!");
  OK
}
