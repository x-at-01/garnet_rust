use std::time::Duration;

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
  time::sleep,
};
use tempfile::tempdir;
use wedb_server::{ServerArgs, WedbServer};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 辅助函数：通过原始 TCP 发送 RESP 命令并读取响应
async fn send_cmd(stream: &mut TcpStream, cmd: &[u8]) -> Result<String> {
  let BufResult(res, _) = stream.write_all(cmd.to_vec()).await;
  res?;
  let buf = Vec::with_capacity(4096);
  let BufResult(res, mut returned_buf) = stream.read(buf).await;
  let n = res?;
  returned_buf.truncate(n);
  Ok(String::from_utf8(returned_buf)?)
}

#[compio::test]
async fn test_range_index_network_lifecycle() -> Void {
  let tmp = tempdir()?;
  let args = ServerArgs {
    port: 0,
    bind: "127.0.0.1".into(),
    dir: tmp.path().to_string_lossy().to_string(),
    ..Default::default()
  };

  let server = WedbServer::new(args).await?;
  let addr = server.start().await?;
  sleep(Duration::from_millis(50)).await;

  let mut client = TcpStream::connect(addr).await?;

  // 1. RI.CREATE myindex MEMORY CACHESIZE 65536 MINRECORD 8 MAXRECORD 256 MAXKEYLEN 32
  let resp = send_cmd(
    &mut client,
    b"*11\r\n$9\r\nRI.CREATE\r\n$7\r\nmyindex\r\n$6\r\nMEMORY\r\n$9\r\nCACHESIZE\r\n$5\r\n65536\r\n$9\r\nMINRECORD\r\n$1\r\n8\r\n$9\r\nMAXRECORD\r\n$3\r\n256\r\n$9\r\nMAXKEYLEN\r\n$2\r\n32\r\n",
  )
  .await?;
  assert_eq!(resp, "+OK\r\n");

  // 2. 重复 RI.CREATE 返回 ERR index already exists
  let resp = send_cmd(
    &mut client,
    b"*3\r\n$9\r\nRI.CREATE\r\n$7\r\nmyindex\r\n$6\r\nMEMORY\r\n",
  )
  .await?;
  assert!(resp.contains("ERR index already exists"));

  // 3. TYPE myindex 返回 +rangeindex\r\n
  let resp = send_cmd(&mut client, b"*2\r\n$4\r\nTYPE\r\n$7\r\nmyindex\r\n").await?;
  assert_eq!(resp, "+rangeindex\r\n");

  // 4. RI.EXISTS myindex 返回 :1
  let resp = send_cmd(&mut client, b"*2\r\n$9\r\nRI.EXISTS\r\n$7\r\nmyindex\r\n").await?;
  assert_eq!(resp, ":1\r\n");

  // 5. RI.EXISTS non_existent 返回 :0
  let resp = send_cmd(&mut client, b"*2\r\n$9\r\nRI.EXISTS\r\n$5\r\nno_ex\r\n").await?;
  assert_eq!(resp, ":0\r\n");

  // 6. RI.CONFIG myindex 返回 12 项数组
  let resp = send_cmd(&mut client, b"*2\r\n$9\r\nRI.CONFIG\r\n$7\r\nmyindex\r\n").await?;
  assert!(resp.starts_with("*12\r\n"));
  assert!(resp.contains("storage_backend"));
  assert!(resp.contains("MEMORY"));
  assert!(resp.contains("cache_size"));
  assert!(resp.contains("65536"));

  // 7. RI.METRICS myindex 返回 8 项数组
  let resp = send_cmd(&mut client, b"*2\r\n$10\r\nRI.METRICS\r\n$7\r\nmyindex\r\n").await?;
  assert!(resp.starts_with("*8\r\n"));
  assert!(resp.contains("tree_handle"));
  assert!(resp.contains("is_live"));
  assert!(resp.contains("true"));

  // 8. RI.SET myindex field1 value1
  let resp = send_cmd(
    &mut client,
    b"*4\r\n$6\r\nRI.SET\r\n$7\r\nmyindex\r\n$6\r\nfield1\r\n$6\r\nvalue1\r\n",
  )
  .await?;
  assert_eq!(resp, "+OK\r\n");

  // 9. RI.GET myindex field1
  let resp = send_cmd(
    &mut client,
    b"*3\r\n$6\r\nRI.GET\r\n$7\r\nmyindex\r\n$6\r\nfield1\r\n",
  )
  .await?;
  assert_eq!(resp, "$6\r\nvalue1\r\n");

  // 10. RI.GET myindex nosuchfield 返回 null
  let resp = send_cmd(
    &mut client,
    b"*3\r\n$6\r\nRI.GET\r\n$7\r\nmyindex\r\n$11\r\nnosuchfield\r\n",
  )
  .await?;
  assert_eq!(resp, "$-1\r\n");

  // 11. 针对 RangeIndex 键调用普通 GET / SET 触发 WRONGTYPE
  let resp = send_cmd(&mut client, b"*2\r\n$3\r\nGET\r\n$7\r\nmyindex\r\n").await?;
  assert!(resp.starts_with("-WRONGTYPE"));

  let resp = send_cmd(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$7\r\nmyindex\r\n$3\r\nfoo\r\n",
  )
  .await?;
  assert!(resp.starts_with("-WRONGTYPE"));

  // 12. RI.DEL myindex field1 返回 :1
  let resp = send_cmd(
    &mut client,
    b"*3\r\n$6\r\nRI.DEL\r\n$7\r\nmyindex\r\n$6\r\nfield1\r\n",
  )
  .await?;
  assert_eq!(resp, ":1\r\n");

  // 13. 再次 RI.GET 返回 null
  let resp = send_cmd(
    &mut client,
    b"*3\r\n$6\r\nRI.GET\r\n$7\r\nmyindex\r\n$6\r\nfield1\r\n",
  )
  .await?;
  assert_eq!(resp, "$-1\r\n");

  // 14. 磁盘模式 RangeIndex 创建与扫描测试
  let resp = send_cmd(
    &mut client,
    b"*5\r\n$9\r\nRI.CREATE\r\n$7\r\ndiskidx\r\n$4\r\nDISK\r\n$9\r\nMINRECORD\r\n$1\r\n4\r\n",
  )
  .await?;
  assert_eq!(resp, "+OK\r\n");

  // 插入排序测试项
  for i in 0..5 {
    let mut k = String::from("k");
    let mut ibuf = itoa::Buffer::new();
    k.push_str(ibuf.format(i));
    let mut v = String::from("v");
    let mut vbuf = itoa::Buffer::new();
    v.push_str(vbuf.format(i));
    let mut cmd = String::from("*4\r\n$6\r\nRI.SET\r\n$7\r\ndiskidx\r\n$");
    let mut klen = itoa::Buffer::new();
    cmd.push_str(klen.format(k.len()));
    cmd.push_str("\r\n");
    cmd.push_str(k.as_str());
    cmd.push_str("\r\n$");
    let mut vlen = itoa::Buffer::new();
    cmd.push_str(vlen.format(v.len()));
    cmd.push_str("\r\n");
    cmd.push_str(v.as_str());
    cmd.push_str("\r\n");
    let resp = send_cmd(&mut client, cmd.as_bytes()).await?;
    assert_eq!(resp, "+OK\r\n");
  }

  // RI.SCAN diskidx k0 COUNT 3
  let resp = send_cmd(
    &mut client,
    b"*5\r\n$7\r\nRI.SCAN\r\n$7\r\ndiskidx\r\n$2\r\nk0\r\n$5\r\nCOUNT\r\n$1\r\n3\r\n",
  )
  .await?;
  assert!(resp.starts_with("*3\r\n"));

  // RI.RANGE diskidx k1 k3
  let resp = send_cmd(
    &mut client,
    b"*4\r\n$8\r\nRI.RANGE\r\n$7\r\ndiskidx\r\n$2\r\nk1\r\n$2\r\nk3\r\n",
  )
  .await?;
  assert!(resp.starts_with("*3\r\n"));

  // 15. DEL diskidx 删除键与底层树
  let resp = send_cmd(&mut client, b"*2\r\n$3\r\nDEL\r\n$7\r\ndiskidx\r\n").await?;
  assert_eq!(resp, ":1\r\n");

  // 删除后再 RI.GET 报错
  let resp = send_cmd(
    &mut client,
    b"*3\r\n$6\r\nRI.GET\r\n$7\r\ndiskidx\r\n$2\r\nk1\r\n",
  )
  .await?;
  assert!(resp.contains("ERR range index not found"));

  server.stop().await?;
  OK
}
