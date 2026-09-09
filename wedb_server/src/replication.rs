//! 从节点复制循环（对标 C# `ReplicaReplaySession` + `AofSyncTask` 的对端）
//!
//! `--replicaof` 后常驻运行：握手协商 → 效果帧流摄取（本地保真落盘 +
//! 帧重放应用）→ 周期 ACK；断线按缓存位点重连，主侧积压缓冲仍在则
//! 增量接续，越界则全量重同步。

use std::{
  sync::Arc,
  time::{Duration, Instant},
};

use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
};
use log::{info, warn};
use wedb_aof::AofReplayer;

use crate::{context::ServerContext, error::Result};

/// 单帧定长前缀：op(1) + klen(4) + vlen(4)
const FRAME_PREFIX_LEN: usize = 9;

/// ACK 汇报最小间隔
const ACK_INTERVAL: Duration = Duration::from_millis(500);

/// 重连退避基数（指数递增至上限）
const RECONNECT_MIN_MS: u64 = 200;
const RECONNECT_MAX_MS: u64 = 5_000;

/// 解析 "ip:port"
fn parse_endpoint(addr: &str) -> Result<(String, u16)> {
  let (ip, port) = addr
    .rsplit_once(':')
    .ok_or_else(|| crate::error::Error::Custom(format!("replicaof 地址非法: {addr}")))?;
  Ok((
    ip.to_string(),
    port
      .parse::<u16>()
      .map_err(|e| crate::error::Error::Custom(format!("replicaof 端口非法: {e}")))?,
  ))
}

/// 从节点复制常驻循环（`--replicaof` 时由服务启动任务拉起）
pub async fn run_replica_loop(ctx: Arc<ServerContext>, primary: &str) -> Result<()> {
  let (ip, port) = parse_endpoint(primary)?;
  let addr = format!("{ip}:{port}");
  let mut backoff_ms = RECONNECT_MIN_MS;
  // 复制身份与位点缓存（断线重连据此请求增量接续）
  let mut cached_replid = "?".repeat(40);
  let mut received_bytes = 0u64;

  info!("从节点复制循环启动: primary={addr}");

  while ctx.is_running() {
    match replica_session(&ctx, &addr, &mut cached_replid, &mut received_bytes).await {
      Ok(()) => {
        backoff_ms = RECONNECT_MIN_MS;
      }
      Err(e) => {
        warn!("复制会话中断（{received_bytes} 字节已收）: err={e}, {backoff_ms}ms 后重连");
      }
    }
    if !ctx.is_running() {
      break;
    }
    compio::time::sleep(Duration::from_millis(backoff_ms)).await;
    backoff_ms = (backoff_ms * 2).min(RECONNECT_MAX_MS);
  }

  info!("从节点复制循环退出");
  Ok(())
}

/// 单次复制会话：握手 → 全量/增量帧流摄取 → ACK，连接断开即返回
async fn replica_session(
  ctx: &Arc<ServerContext>,
  addr: &str,
  cached_replid: &mut String,
  received_bytes: &mut u64,
) -> Result<()> {
  let mut stream = TcpStream::connect(addr).await?;
  let mut pending: Vec<u8> = Vec::with_capacity(16 * 1024);

  // 握手（RESP 序）：PING → REPLCONF listening-port → REPLCONF capa → PSYNC
  let local_port = ctx.args.port;
  handshake_step(&mut stream, &mut pending, b"*1\r\n$4\r\nPING\r\n").await?;
  handshake_step(
    &mut stream,
    &mut pending,
    format!(
      "*3\r\n$8\r\nREPLCONF\r\n$14\r\nlistening-port\r\n${}\r\n{}\r\n",
      local_port.to_string().len(),
      local_port
    )
    .as_bytes(),
  )
  .await?;
  handshake_step(
    &mut stream,
    &mut pending,
    b"*3\r\n$8\r\nREPLCONF\r\n$4\r\ncapa\r\n$3\r\neof\r\n",
  )
  .await?;

  let psync = format!(
    "*3\r\n$5\r\nPSYNC\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
    cached_replid.len(),
    cached_replid,
    received_bytes.to_string().len(),
    received_bytes
  );
  let sync_reply = handshake_step(&mut stream, &mut pending, psync.as_bytes()).await?;
  let reply = String::from_utf8_lossy(&sync_reply).to_string();
  if let Some(rest) = reply.strip_prefix("+FULLRESYNC") {
    // 全量重同步：采纳主身份，重置本地日志位点后重收全量帧
    // （要求从节点为空库或可容忍旧数据被全量帧前缀覆盖，对标 Redis FLUSHALL 语义）
    let mut parts = rest.split_whitespace();
    if let Some(replid) = parts.next() {
      *cached_replid = replid.to_string();
    }
    ctx.aof.reset().await?;
    *received_bytes = 0;
    info!("全量重同步开始（本地日志位点已重置）");
  } else if let Some(rest) = reply.strip_prefix("+CONTINUE") {
    if let Some(replid) = rest.split_whitespace().next() {
      *cached_replid = replid.to_string();
    }
    info!("增量接续: offset={received_bytes}");
  } else {
    return Err(crate::error::Error::Custom(format!(
      "PSYNC 应答异常: {reply:?}"
    )));
  }

  // 帧摄取：本地保真落盘 + 帧重放应用（对标 C# UnsafeEnqueueRaw + ProcessAofRecord）
  let replayer = AofReplayer::new(&ctx.store)?;
  let mut ack_deadline = Instant::now();

  loop {
    if !ctx.is_running() {
      return Ok(());
    }

    let BufResult(res, buf) = stream.read(vec![0u8; 64 * 1024]).await;
    let n = match res {
      Ok(n) if n > 0 => n,
      Ok(_) => return Err(crate::error::Error::Custom("主节点连接关闭".into())),
      Err(e) => return Err(crate::error::Error::Io(e)),
    };
    pending.extend_from_slice(&buf[..n]);

    // 自定界解帧循环：帧头含 klen/vlen，长度自描述
    let mut consumed = 0usize;
    while pending.len() - consumed >= FRAME_PREFIX_LEN {
      let head = &pending[consumed..consumed + FRAME_PREFIX_LEN];
      // op 有效性即流同步哨兵：非法 op 说明位点错位（缺数据优于错数据），
      // 断开连接走全量重同步
      if wedb_aof::AofOp::from_byte(head[0]).is_none() {
        return Err(crate::error::Error::Custom(format!(
          "帧流位点错位（非法 op={:#x}），触发全量重同步",
          head[0]
        )));
      }
      let klen = u32::from_le_bytes(head[1..5].try_into().unwrap()) as usize;
      let vlen = u32::from_le_bytes(head[5..9].try_into().unwrap()) as usize;
      let frame_len = FRAME_PREFIX_LEN + klen + vlen;
      if pending.len() - consumed < frame_len {
        break;
      }
      let frame = &pending[consumed..consumed + frame_len];
      ctx.aof.replica_ingest(frame).await?;
      replayer.apply(frame).await?;
      let _ = addr; // 本地链连续性由 enqueue_raw 顺序写保证
      consumed += frame_len;
    }
    if consumed > 0 {
      pending.drain(..consumed);
      *received_bytes += consumed as u64;
    }

    if Instant::now() >= ack_deadline {
      let ack = format!(
        "*3\r\n$8\r\nREPLCONF\r\n$3\r\nACK\r\n${}\r\n{}\r\n",
        received_bytes.to_string().len(),
        received_bytes
      );
      let BufResult(res, _) = stream.write_all(ack.into_bytes()).await;
      if res.is_err() {
        return Err(crate::error::Error::Custom("ACK 写出失败".into()));
      }
      ack_deadline = Instant::now() + ACK_INTERVAL;
    }
  }
}

/// 握手单步：写一条 RESP 命令并读取一行主节点应答
///
/// 应答行不进 pending 流缓冲（握手与帧流分界清晰）
async fn handshake_step(
  stream: &mut TcpStream,
  pending: &mut Vec<u8>,
  cmd: &[u8],
) -> Result<Vec<u8>> {
  info!(
    "复制握手 → {}",
    String::from_utf8_lossy(cmd).replace("\r\n", "|")
  );
  let BufResult(res, _) = stream.write_all(cmd.to_vec()).await;
  res.map_err(crate::error::Error::Io)?;

  loop {
    // 应答行为单行 "\r\n" 结尾（+OK/+PONG/+CONTINUE/+FULLRESYNC/-ERR）；
    // 行尾之后的帧流字节保留在 pending 中，交由帧摄取循环继续消费
    if let Some(pos) = pending.windows(2).position(|w| w == b"\r\n") {
      let line = pending.drain(..pos + 2).collect::<Vec<u8>>();
      info!(
        "复制握手 ← {}",
        String::from_utf8_lossy(&line).replace("\r\n", "|")
      );
      return Ok(line);
    }
    let BufResult(res, buf) = stream.read(vec![0u8; 1024]).await;
    let n = res.map_err(crate::error::Error::Io)?;
    if n == 0 {
      return Err(crate::error::Error::Custom("握手阶段连接关闭".into()));
    }
    pending.extend_from_slice(&buf[..n]);
  }
}
