//! Lua 脚本（EVAL/EVALSHA/SCRIPT）与向量集（V*）命令的端到端集成测试

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

/// 通过原始 TCP 发送 RESP 命令并读取响应
async fn send_cmd(stream: &mut TcpStream, cmd: &[u8]) -> Result<String> {
  let BufResult(res, _) = stream.write_all(cmd.to_vec()).await;
  res?;
  let buf = Vec::with_capacity(4096);
  let BufResult(res, mut returned_buf) = stream.read(buf).await;
  let n = res?;
  returned_buf.truncate(n);
  Ok(String::from_utf8(returned_buf)?)
}

/// 将参数编码为 RESP 数组报文
fn cmd(args: &[&[u8]]) -> Vec<u8> {
  let mut ibuf = itoa::Buffer::new();
  let mut out = Vec::with_capacity(128);
  out.push(b'*');
  out.extend_from_slice(ibuf.format(args.len()).as_bytes());
  out.extend_from_slice(b"\r\n");
  for a in args {
    out.push(b'$');
    out.extend_from_slice(ibuf.format(a.len()).as_bytes());
    out.extend_from_slice(b"\r\n");
    out.extend_from_slice(a);
    out.extend_from_slice(b"\r\n");
  }
  out
}

/// FP32 向量的 RESP bulk 参数
fn fp32(vals: &[f32]) -> Vec<u8> {
  vals.iter().flat_map(|f| f.to_le_bytes()).collect()
}

#[compio::test]
async fn test_script_eval_roundtrip() -> Void {
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

  // EVAL "redis.call('SET', KEYS[1], ARGV[1])" 1 k1 v1 -> +OK
  let resp = send_cmd(
    &mut client,
    &cmd(&[
      b"EVAL",
      b"return redis.call('SET', KEYS[1], ARGV[1])",
      b"1",
      b"k1",
      b"v1",
    ]),
  )
  .await?;
  assert_eq!(resp, "+OK\r\n");

  // EVAL GET 回读
  let resp = send_cmd(
    &mut client,
    &cmd(&[b"EVAL", b"return redis.call('GET', KEYS[1])", b"1", b"k1"]),
  )
  .await?;
  assert_eq!(resp, "$2\r\nv1\r\n");

  // 脚本返回整数 / 数组 / 布尔 / nil
  let resp = send_cmd(&mut client, &cmd(&[b"EVAL", b"return 7", b"0"])).await?;
  assert_eq!(resp, ":7\r\n");
  let resp = send_cmd(
    &mut client,
    &cmd(&[b"EVAL", b"return {1, 'x', true}", b"0"]),
  )
  .await?;
  assert_eq!(resp, "*3\r\n:1\r\n$1\r\nx\r\n:1\r\n");
  let resp = send_cmd(&mut client, &cmd(&[b"EVAL", b"return nil", b"0"])).await?;
  assert_eq!(resp, "$-1\r\n");

  // 错误传播：redis.call 未知命令
  let resp = send_cmd(
    &mut client,
    &cmd(&[b"EVAL", b"return redis.call('NOSUCHCMD')", b"0"]),
  )
  .await?;
  assert!(resp.starts_with("-ERR unknown command"), "{resp}");

  // pcall 捕获为 {err=...}
  let resp = send_cmd(
    &mut client,
    &cmd(&[
      b"EVAL",
      "local r = redis.pcall('GET') return type(r.err)".as_bytes(),
      b"0",
    ]),
  )
  .await?;
  assert_eq!(resp, "$6\r\nstring\r\n");

  // 普通客户端可见脚本写入的数据
  let resp = send_cmd(&mut client, &cmd(&[b"GET", b"k1"])).await?;
  assert_eq!(resp, "$2\r\nv1\r\n");
  OK
}

#[compio::test]
async fn test_script_cache_commands() -> Void {
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

  // SCRIPT LOAD 返回 40 位十六进制 sha
  let resp = send_cmd(&mut client, &cmd(&[b"SCRIPT", b"LOAD", b"return 42"])).await?;
  assert!(resp.starts_with("$40\r\n"), "应为 $40 sha: {resp}");
  let sha: Vec<u8> = resp
    .trim_start_matches("$40\r\n")
    .trim_end_matches("\r\n")
    .bytes()
    .collect();
  assert_eq!(sha.len(), 40);

  // EVALSHA 命中
  let resp = send_cmd(&mut client, &cmd(&[b"EVALSHA", &sha, b"0"])).await?;
  assert_eq!(resp, ":42\r\n");

  // SCRIPT EXISTS
  let resp = send_cmd(&mut client, &cmd(&[b"SCRIPT", b"EXISTS", &sha, b"00"])).await?;
  assert_eq!(resp, "*2\r\n:1\r\n:0\r\n");

  // SCRIPT FLUSH 后 NOSCRIPT
  let resp = send_cmd(&mut client, &cmd(&[b"SCRIPT", b"FLUSH"])).await?;
  assert_eq!(resp, "+OK\r\n");
  let resp = send_cmd(&mut client, &cmd(&[b"EVALSHA", &sha, b"0"])).await?;
  assert!(resp.starts_with("-NOSCRIPT"), "{resp}");
  OK
}

#[compio::test]
async fn test_vector_set_lifecycle() -> Void {
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

  let v1 = fp32(&[1.0, 0.0, 0.0, 0.0]);
  let v2 = fp32(&[0.9, 0.1, 0.0, 0.0]);
  let v_far = fp32(&[0.0, 0.0, 0.0, 1.0]);

  // VADD 新元素 :1，重复 :0
  let resp = send_cmd(
    &mut client,
    &cmd(&[b"VADD", b"set1", b"FP32", &v1, b"alpha"]),
  )
  .await?;
  assert_eq!(resp, ":1\r\n");
  let resp = send_cmd(
    &mut client,
    &cmd(&[b"VADD", b"set1", b"FP32", &v1, b"alpha"]),
  )
  .await?;
  assert_eq!(resp, ":0\r\n");
  let _ = send_cmd(
    &mut client,
    &cmd(&[b"VADD", b"set1", b"FP32", &v2, b"beta"]),
  )
  .await?;
  let _ = send_cmd(
    &mut client,
    &cmd(&[b"VADD", b"set1", b"FP32", &v_far, b"far"]),
  )
  .await?;

  // VCARD / VDIM
  let resp = send_cmd(&mut client, &cmd(&[b"VCARD", b"set1"])).await?;
  assert_eq!(resp, ":3\r\n");
  let resp = send_cmd(&mut client, &cmd(&[b"VDIM", b"set1"])).await?;
  assert_eq!(resp, ":4\r\n");
  // 不存在的键
  let resp = send_cmd(&mut client, &cmd(&[b"VCARD", b"ghost"])).await?;
  assert_eq!(resp, ":0\r\n");
  let resp = send_cmd(&mut client, &cmd(&[b"VDIM", b"ghost"])).await?;
  assert!(resp.starts_with("-ERR Key not found"), "{resp}");

  // VSIM VALUES：查询与 alpha 同向，COUNT 2 应先返回 alpha
  let resp = send_cmd(
    &mut client,
    &cmd(&[
      b"VSIM", b"set1", b"VALUES", b"4", b"1", b"0", b"0", b"0", b"COUNT", b"2",
    ]),
  )
  .await?;
  assert!(resp.starts_with("*2\r\n"), "{resp}");
  assert!(resp.contains("alpha"));

  // VSIM ELE WITHSCORES：自身相似度为 1
  let resp = send_cmd(
    &mut client,
    &cmd(&[
      b"VSIM",
      b"set1",
      b"ELE",
      b"alpha",
      b"WITHSCORES",
      b"COUNT",
      b"1",
    ]),
  )
  .await?;
  assert!(resp.starts_with("*2\r\n"), "{resp}");
  assert!(resp.contains("$5\r\nalpha\r\n"), "{resp}");
  assert!(resp.contains("$3\r\n1.0\r\n"), "{resp}");

  // VEMB
  let resp = send_cmd(&mut client, &cmd(&[b"VEMB", b"set1", b"alpha"])).await?;
  assert!(resp.starts_with("*4\r\n"), "{resp}");
  assert!(resp.contains("1"));

  // VSETATTR / VGETATTR
  let resp = send_cmd(
    &mut client,
    &cmd(&[b"VSETATTR", b"set1", b"alpha", b"{\"g\":\"s\"}"]),
  )
  .await?;
  assert_eq!(resp, ":1\r\n");
  let resp = send_cmd(&mut client, &cmd(&[b"VGETATTR", b"set1", b"alpha"])).await?;
  assert_eq!(resp, "$9\r\n{\"g\":\"s\"}\r\n");

  // VISMEMBER / VREM
  let resp = send_cmd(&mut client, &cmd(&[b"VISMEMBER", b"set1", b"beta"])).await?;
  assert_eq!(resp, ":1\r\n");
  let resp = send_cmd(&mut client, &cmd(&[b"VREM", b"set1", b"beta"])).await?;
  assert_eq!(resp, ":1\r\n");
  let resp = send_cmd(&mut client, &cmd(&[b"VREM", b"set1", b"beta"])).await?;
  assert_eq!(resp, ":0\r\n");
  let resp = send_cmd(&mut client, &cmd(&[b"VCARD", b"set1"])).await?;
  assert_eq!(resp, ":2\r\n");

  // VLINKS
  let resp = send_cmd(&mut client, &cmd(&[b"VLINKS", b"set1", b"alpha"])).await?;
  assert!(resp.starts_with("*"), "{resp}");

  // 维度不匹配
  let v3 = fp32(&[1.0, 2.0, 3.0]);
  let resp = send_cmd(&mut client, &cmd(&[b"VADD", b"set1", b"FP32", &v3, b"bad"])).await?;
  assert!(resp.contains("dimension mismatch"), "{resp}");

  // VRANDMEMBER
  let resp = send_cmd(&mut client, &cmd(&[b"VRANDMEMBER", b"set1"])).await?;
  assert!(resp.starts_with("$"), "{resp}");

  // VINFO
  let resp = send_cmd(&mut client, &cmd(&[b"VINFO", b"set1"])).await?;
  assert!(resp.starts_with("*6\r\n"), "{resp}");
  assert!(resp.contains("f32"));
  OK
}
