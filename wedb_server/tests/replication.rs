//! 主从复制端到端测试（对标 Garnet ReplicationManager 集成路径）：
//! 从节点 --replicaof 接入后，主节点写效果（AOF 效果帧）实时推流，
//! 从节点保真落盘 + 帧重放应用；直连从端口可读到已复制数据。

use std::{net::SocketAddr, sync::Arc, time::Duration};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
};
use log::info;
use tempfile::tempdir;
use wedb_server::ServerArgs;

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 构造启用 AOF 的最小化服务参数
fn aof_args(dir: &std::path::Path, replicaof: Option<String>) -> ServerArgs {
  ServerArgs {
    port: 0,
    dir: dir.to_string_lossy().to_string(),
    quiet: true,
    aof_enabled: true,
    replicaof,
    store_memory_budget: Some(256 * 1024 * 1024),
    ..Default::default()
  }
}

/// 发送请求并读取响应
async fn send_and_recv(stream: &mut TcpStream, req: &[u8]) -> Result<Vec<u8>> {
  let BufResult(res, _) = stream.write_all(req.to_vec()).await;
  res?;
  let buf = Vec::with_capacity(4096);
  let BufResult(res, mut buf) = stream.read(buf).await;
  let n = res?;
  buf.truncate(n);
  Ok(buf)
}

async fn start(server: &Arc<wedb_server::WedbServer>) -> Result<SocketAddr> {
  Ok(server.start().await?)
}

#[compio::test]
async fn test_replication_frame_stream_end_to_end() -> Void {
  let dir_master = tempdir()?;
  let dir_replica = tempdir()?;

  let master = Arc::new(wedb_server::WedbServer::new(aof_args(dir_master.path(), None)).await?);
  let master_addr = start(&master).await?;

  let replica = Arc::new(
    wedb_server::WedbServer::new(aof_args(dir_replica.path(), Some(master_addr.to_string())))
      .await?,
  );
  let replica_addr = start(&replica).await?;

  // 等待从侧握手 + 全量同步（空库）完成
  compio::time::sleep(Duration::from_millis(800)).await;

  let mut master_client = TcpStream::connect(master_addr).await?;
  let mut replica_client = TcpStream::connect(replica_addr).await?;

  // 1. 主写 SET → 实时推流 → 从可读
  assert_eq!(
    send_and_recv(
      &mut master_client,
      b"*3\r\n$3\r\nSET\r\n$9\r\nrepl:key1\r\n$9\r\nrepl:val1\r\n"
    )
    .await?,
    b"+OK\r\n"
  );
  compio::time::sleep(Duration::from_millis(500)).await;
  assert_eq!(
    send_and_recv(
      &mut replica_client,
      b"*2\r\n$3\r\nGET\r\n$9\r\nrepl:key1\r\n"
    )
    .await?,
    b"$9\r\nrepl:val1\r\n",
    "从节点应复制到 key1"
  );

  // 2. 覆盖写传播最新效果
  assert_eq!(
    send_and_recv(
      &mut master_client,
      b"*3\r\n$3\r\nSET\r\n$9\r\nrepl:key1\r\n$9\r\nrepl:val2\r\n"
    )
    .await?,
    b"+OK\r\n"
  );
  compio::time::sleep(Duration::from_millis(500)).await;
  assert_eq!(
    send_and_recv(
      &mut replica_client,
      b"*2\r\n$3\r\nGET\r\n$9\r\nrepl:key1\r\n"
    )
    .await?,
    b"$9\r\nrepl:val2\r\n",
    "从节点应复制到覆盖写"
  );

  // 3. DEL 传播（墓碑帧）
  assert_eq!(
    send_and_recv(
      &mut master_client,
      b"*2\r\n$3\r\nDEL\r\n$9\r\nrepl:key1\r\n"
    )
    .await?,
    b":1\r\n"
  );
  compio::time::sleep(Duration::from_millis(500)).await;
  assert_eq!(
    send_and_recv(
      &mut replica_client,
      b"*2\r\n$3\r\nGET\r\n$9\r\nrepl:key1\r\n"
    )
    .await?,
    b"$-1\r\n",
    "从节点应复制到删除"
  );

  replica_client = TcpStream::connect(replica_addr).await?;
  let _ = replica_client;

  master.stop().await?;
  replica.stop().await?;

  info!("主从复制效果帧流端到端测试通过");
  OK
}
