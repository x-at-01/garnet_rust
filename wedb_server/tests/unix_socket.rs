#![cfg(unix)]

use std::sync::Arc;

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::UnixStream,
};
use log::info;
use tempfile::tempdir;
use wedb_server::{ServerArgs, WedbServer};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 辅助向 Unix 域套接字发送请求并读取响应
async fn send_and_recv_unix(stream: &mut UnixStream, req: &[u8]) -> Result<Vec<u8>> {
  let BufResult(res, _) = stream.write_all(req.to_vec()).await;
  res?;
  let buf = Vec::with_capacity(4096);
  let BufResult(res, mut buf) = stream.read(buf).await;
  let n = res?;
  buf.truncate(n);
  Ok(buf)
}

#[compio::test]
async fn test_server_unix_domain_socket() -> Void {
  let dir = tempdir()?;
  let sock_path = dir.path().join("wedb_test.sock");
  let sock_str = sock_path.to_string_lossy().to_string();

  let args = ServerArgs {
    port: 0,
    dir: dir.path().to_string_lossy().to_string(),
    unixsocket: Some(sock_str.clone()),
    quiet: true,
    ..Default::default()
  };

  let server = Arc::new(WedbServer::new(args).await?);
  let tcp_addr = server.start().await?;
  info!(
    "WeDB 测试服务已启动, TCP 监听: {}, Unix 域套接字监听: {}",
    tcp_addr, sock_str
  );

  // 1. 验证套接字文件已创建
  assert!(sock_path.exists(), "Unix 域套接字文件应存在");

  // 2. 客户端 1 连接 Unix 域套接字并执行 SET / GET
  let mut client1 = UnixStream::connect(&sock_path).await?;
  let resp = send_and_recv_unix(
    &mut client1,
    b"*3\r\n$3\r\nSET\r\n$4\r\nu_k1\r\n$4\r\nu_v1\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv_unix(&mut client1, b"*2\r\n$3\r\nGET\r\n$4\r\nu_k1\r\n").await?;
  assert_eq!(resp, b"$4\r\nu_v1\r\n");

  // 3. 客户端 2 并发连接验证流水线批量请求
  let mut client2 = UnixStream::connect(&sock_path).await?;
  let pipe_cmd =
    b"*3\r\n$3\r\nSET\r\n$4\r\nu_k2\r\n$4\r\nu_v2\r\n*2\r\n$3\r\nGET\r\n$4\r\nu_k2\r\n";
  let resp = send_and_recv_unix(&mut client2, pipe_cmd).await?;
  assert_eq!(resp, b"+OK\r\n$4\r\nu_v2\r\n");

  // 4. 交叉读取：客户端 1 读取客户端 2 写入的键
  let resp = send_and_recv_unix(&mut client1, b"*2\r\n$3\r\nGET\r\n$4\r\nu_k2\r\n").await?;
  assert_eq!(resp, b"$4\r\nu_v2\r\n");

  drop(client1);
  drop(client2);

  // 5. 优雅停机并验证套接字文件自动清理
  server.stop().await?;
  assert!(server.is_disposed(), "服务端应标记为已停止");
  assert!(!sock_path.exists(), "停机后 Unix 域套接字文件应被自动清理");

  info!("Unix 域套接字测试完全通过!");
  OK
}
