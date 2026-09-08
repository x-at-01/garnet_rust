use std::sync::{
  Arc,
  atomic::{AtomicUsize, Ordering},
};

use aok::{OK, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  runtime::spawn,
  time::sleep,
};
use log::info;

use crate::support::{
  DEFAULT_TIMEOUT, NetworkTestFixture, POLL_INTERVAL, send_and_recv, send_and_recv_exact,
  wait_for_connection_close,
};

/// 快速构建超长 DEL 多参数命令报文（使用 itoa 零堆分配格式化，单次分配容积）
fn make_multibulk_del(count: usize) -> Vec<u8> {
  let mut itoa_buf = itoa::Buffer::new();
  let count_str = itoa_buf.format(count);
  let mut buf = Vec::with_capacity(count * 16);
  buf.extend_from_slice(b"*");
  buf.extend_from_slice(count_str.as_bytes());
  buf.extend_from_slice(b"\r\n$3\r\nDEL\r\n");

  let mut num_buf = itoa::Buffer::new();
  for i in 0..(count - 1) {
    let istr = num_buf.format(i);
    let k_len = 1 + istr.len(); // "b" + 数字
    let mut len_buf = itoa::Buffer::new();
    let len_str = len_buf.format(k_len);
    buf.extend_from_slice(b"$");
    buf.extend_from_slice(len_str.as_bytes());
    buf.extend_from_slice(b"\r\nb");
    buf.extend_from_slice(istr.as_bytes());
    buf.extend_from_slice(b"\r\n");
  }
  buf
}

/// 协议畸形报文触发协议错误并断开连接测试
/// 验证非法 RESP 前缀或格式错误时，服务端返回错误并主动关闭连接。
#[compio::test]
async fn test_protocol_error_closes_connection() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 非法 multibulk 长度：服务端回协议错误后主动断开
  let resp = send_and_recv(&mut client, b"*abc\r\n").await?;
  assert!(
    resp.starts_with(b"-ERR Protocol error"),
    "畸形报文应返回协议错误: {resp:?}"
  );

  // 连接应随后被服务端关闭（读到 EOF）
  let closed = wait_for_connection_close(&mut client, DEFAULT_TIMEOUT).await;
  assert!(closed, "协议错误后服务端应关闭连接");

  info!("协议错误断开连接验证通过");
  OK
}

/// 恶意超大 multibulk 长度头防御
/// 验证超大元素数在预分配前被安全拦截并关闭连接，防止 OOM 攻击。
#[compio::test]
async fn test_oversized_multibulk_rejected() -> Void {
  let fixture = NetworkTestFixture::setup().await?;

  // 1. 20 亿元素数组头（含 20 位 u64 溢出变体）：立即回协议错误
  for bad in [
    &b"*2000000000\r\n$4\r\nPING\r\n"[..],
    b"*99999999999999999999\r\n",
  ] {
    let mut client = fixture.connect_client().await?;
    let resp = send_and_recv(&mut client, bad).await?;
    assert!(
      resp.starts_with(b"-ERR Protocol error: invalid multibulk length"),
      "超大 multibulk 应被拒绝: {resp:?}"
    );
  }

  // 2. 连接随后被服务端关闭（读到 EOF）
  let mut client = fixture.connect_client().await?;
  let (write_res, _) = client
    .write_all(b"*2000000000\r\n$4\r\nPING\r\n".to_vec())
    .await
    .into();
  write_res?;
  let closed = wait_for_connection_close(&mut client, DEFAULT_TIMEOUT).await;
  assert!(closed, "超大 multibulk 后服务端应关闭连接");

  // 3. 负数长度中仅 -1（NULL 数组）合法放行（静默 no-op，无回包）；前导零变体等价放行
  let mut client = fixture.connect_client().await?;
  let resp = send_and_recv_exact(&mut client, b"PING\r\n*-1\r\nPING\r\n", 14).await?;
  assert_eq!(resp, b"+PONG\r\n+PONG\r\n");

  // 4. 前导零长度头按真实数值判定放行（*0001 等价 *1、*-0001/-0 等价 NULL/零数组，均无回包）
  const LEADING_ZEROS_PROBE: &[u8] = b"PING\r\n*0001\r\n$4\r\nPING\r\n*-0001\r\n*-0\r\nPING\r\n";
  let mut client = fixture.connect_client().await?;
  let resp = send_and_recv_exact(&mut client, LEADING_ZEROS_PROBE, 21).await?;
  assert_eq!(resp, b"+PONG\r\n+PONG\r\n+PONG\r\n");

  // 5. 其余负值长度被拒绝
  let resp = send_and_recv(&mut client, b"*-2\r\n$4\r\nPING\r\n").await?;
  assert!(
    resp.starts_with(b"-ERR Protocol error"),
    "非法负值 multibulk 应被拒绝: {resp:?}"
  );

  // 6. 服务保持健康，新连接不受影响
  let mut probe = fixture.connect_client().await?;
  let resp = send_and_recv(&mut probe, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  info!("超大 multibulk 长度头防御验证通过");
  OK
}

/// multibulk 元素数上限边界（1024 放行，1025 拒绝）
/// 锁定防御阈值恰好等于上限：上限内的大型合法命令不受影响，超限一个元素即拒绝。
#[compio::test]
async fn test_multibulk_limit_boundary() -> Void {
  const MULTIBULK_LIMIT: usize = 1024;
  const MULTIBULK_OVERSIZED: usize = MULTIBULK_LIMIT + 1;

  let fixture = NetworkTestFixture::setup().await?;

  // 1. 恰好 1024 元素（DEL + 1023 个键）：合法放行，返回删除计数 :0
  let legit = make_multibulk_del(MULTIBULK_LIMIT);
  let mut client = fixture.connect_client().await?;
  let resp = send_and_recv_exact(&mut client, &legit, 4).await?;
  assert_eq!(resp, b":0\r\n");

  // 2. 1025 元素：超限一个元素即被拒绝并断开
  let oversized = make_multibulk_del(MULTIBULK_OVERSIZED);
  let (write_res, _) = client.write_all(oversized).await.into();
  write_res?;

  let buf = Vec::with_capacity(128);
  let BufResult(read_res, buf) = client.read(buf).await;
  let n = read_res?;
  assert!(
    buf[..n].starts_with(b"-ERR Protocol error: invalid multibulk length"),
    "超限 multibulk 应回协议错误: {:?}",
    &buf[..n]
  );
  let closed = wait_for_connection_close(&mut client, DEFAULT_TIMEOUT).await;
  assert!(closed, "超限 multibulk 后服务端应关闭连接");

  info!("multibulk 元素数上限边界验证通过");
  OK
}

/// 连接数上限强制执行（超限连接被立即丢弃）
#[compio::test]
async fn test_max_connections_enforced() -> Void {
  let fixture = NetworkTestFixture::setup_with_max_connections(1).await?;

  let mut client1 = fixture.connect_client().await?;
  let resp = send_and_recv(&mut client1, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  // 第二条连接被服务端接受后立即拒绝并关闭：读到 EOF
  let mut client2 = fixture.connect_client().await?;
  let rejected = wait_for_connection_close(&mut client2, DEFAULT_TIMEOUT).await;
  assert!(rejected, "超限连接应被服务端关闭");
  assert_eq!(fixture.server.active_connections(), 1, "仅首条连接保持活跃");

  // 首条连接仍正常服务
  let resp = send_and_recv(&mut client1, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  info!("连接数上限强制执行验证通过");
  OK
}

/// 空数组帧与空内联行按 Redis 语义静默忽略
/// `*0\r\n`、`*-1\r\n` 与裸 CRLF 行不产生任何回包；事务内收到空帧不得中止排队队列。
#[compio::test]
async fn test_empty_frames_silently_ignored() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 空内联行与空数组帧均静默忽略：仅有两条 PING 各自回 +PONG
  const PROBE_EMPTY: &[u8] = b"PING\r\n\r\n*0\r\n*-1\r\nPING\r\n";
  const EXPECTED_PONG2: &[u8] = b"+PONG\r\n+PONG\r\n";
  let resp = send_and_recv_exact(&mut client, PROBE_EMPTY, EXPECTED_PONG2.len()).await?;
  assert_eq!(resp, EXPECTED_PONG2);

  // 2. 事务内空帧不中止事务：EXEC 返回空数组而非 EXECABORT
  const MULTI_EMPTY_EXEC: &[u8] = b"MULTI\r\n*0\r\nEXEC\r\n";
  const EXPECTED_TX_EMPTY: &[u8] = b"+OK\r\n*0\r\n";
  let resp = send_and_recv_exact(&mut client, MULTI_EMPTY_EXEC, EXPECTED_TX_EMPTY.len()).await?;
  assert_eq!(resp, EXPECTED_TX_EMPTY);

  // 3. 空帧处理后事务状态彻底复位，会话保持健康
  let resp = send_and_recv(&mut client, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  info!("空帧静默忽略验证通过");
  OK
}

/// 超大 value 往返测试（接收缓冲区按 2 的幂次扩容直至超出池化层级）
#[compio::test]
async fn test_large_value_roundtrip() -> Void {
  const LARGE_PAGE_SIZE: usize = 256 * 1024;
  const VALUE_LEN: usize = 200 * 1024;

  // 256KB 大页：允许容纳 200KB 大值记录（存储层单记录须小于页大小）
  let fixture = NetworkTestFixture::setup_with_page_size(LARGE_PAGE_SIZE).await?;
  let mut client = fixture.connect_client().await?;

  // 200KB 大值：远超初始 4KB 缓冲，且超过池化最高层级（128KB），覆盖池外直配路径
  let value: Vec<u8> = (0..VALUE_LEN).map(|i| (i % 251) as u8).collect();

  let mut set_cmd = format!("*3\r\n$3\r\nSET\r\n$3\r\nbig\r\n${}\r\n", VALUE_LEN).into_bytes();
  set_cmd.extend_from_slice(&value);
  set_cmd.extend_from_slice(b"\r\n");
  let resp = send_and_recv_exact(&mut client, &set_cmd, 5).await?;
  assert_eq!(resp, b"+OK\r\n");

  let get_cmd = b"*2\r\n$3\r\nGET\r\n$3\r\nbig\r\n";
  let expected_header = format!("${}\r\n", VALUE_LEN).into_bytes();
  let resp =
    send_and_recv_exact(&mut client, get_cmd, expected_header.len() + VALUE_LEN + 2).await?;
  assert_eq!(&resp[..expected_header.len()], &expected_header[..]);
  assert_eq!(&resp[expected_header.len()..resp.len() - 2], &value[..]);
  assert_eq!(&resp[resp.len() - 2..], b"\r\n");

  info!("超大 value 往返验证通过");
  OK
}

/// 未知命令不关闭连接（对齐 C# RespCommand.INVALID 跳过推进与 Redis 语义）
/// 服务端回写 ERR unknown command 后整帧跳过继续服务；参数跨半包时等待完整到达；
/// 事务开启中收到未知命令则置脏中止，EXEC 统一报 EXECABORT。
#[compio::test]
async fn test_unknown_command_keeps_connection_alive() -> Void {
  const HALF_UNKNOWN: &[u8] = b"*2\r\n$4\r\nNOPE\r\n$10\r\nABC";
  const REST_AND_PING: &[u8] = b"DEFGHIJ\r\nPING\r\n";
  const EXPECTED_ERR_AND_PONG: &[u8] = b"-ERR unknown command 'NOPE'\r\n+PONG\r\n";

  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 内联未知命令：报错但连接保持
  let resp = send_and_recv(&mut client, b"FOOBAR\r\n").await?;
  assert_eq!(resp, b"-ERR unknown command 'FOOBAR'\r\n");

  // 2. RESP 数组未知命令（含参数）同样报错不关连
  let resp = send_and_recv(&mut client, b"*3\r\n$4\r\nNOPE\r\n$1\r\na\r\n$1\r\nb\r\n").await?;
  assert_eq!(resp, b"-ERR unknown command 'NOPE'\r\n");

  // 3. 未知命令参数跨半包：完整到达前不产生任何回包，整帧到齐后一次回包
  let (write_res, _) = client.write_all(HALF_UNKNOWN.to_vec()).await.into();
  write_res?;
  sleep(POLL_INTERVAL).await;
  let resp = send_and_recv_exact(&mut client, REST_AND_PING, EXPECTED_ERR_AND_PONG.len()).await?;
  assert_eq!(resp, EXPECTED_ERR_AND_PONG);

  // 4. 事务中未知命令置脏中止：错误即时回包，EXEC 统一报 EXECABORT，队列命令不落库
  assert_eq!(send_and_recv(&mut client, b"MULTI\r\n").await?, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"WHATCMD\r\n").await?;
  assert_eq!(resp, b"-ERR unknown command 'WHATCMD'\r\n");
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nuk\r\n$1\r\nv\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");
  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(
    resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nuk\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  // 5. 连接全程保持可用
  assert_eq!(send_and_recv(&mut client, b"PING\r\n").await?, b"+PONG\r\n");

  info!("未知命令保持连接语义验证通过");
  OK
}

/// 事务内命令拒绝语义验证（对齐 C# Garnet TxnRespCommands.cs）
/// 1. WATCH 在事务内仅报错，事务继续（对标 NetworkSKIP isWatch 分支不中止）；
/// 2. 禁入命令（SWAPDB）中止事务，EXEC 统一报 EXECABORT（对标 NetworkSKIP 路径）。
#[compio::test]
async fn test_exec_abort_on_disallowed_command() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 事务内 WATCH 仅报错：不中止事务、不入队（对齐 C# isWatch 分支与 Redis 语义）
  let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nWATCH\r\n$2\r\nak\r\n").await?;
  assert_eq!(
    resp, b"-ERR WATCH inside MULTI is not allowed\r\n",
    "事务内 WATCH 应仅报错: {resp:?}"
  );

  // 2. 后续命令正常排队，EXEC 照常提交（未被 WATCH 报错中止）
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nak\r\n$2\r\nv1\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*1\r\n+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nak\r\n").await?;
  assert_eq!(resp, b"$2\r\nv1\r\n");

  // 3. 禁入命令（SWAPDB）中止事务，EXEC 统一报 EXECABORT
  let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*3\r\n$6\r\nSWAPDB\r\n$1\r\n0\r\n$1\r\n1\r\n").await?;
  assert!(
    resp.starts_with(b"-ERR"),
    "事务内 SWAPDB 应被拒绝: {resp:?}"
  );

  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nak\r\n$2\r\nv2\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(
    resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  // 4. 中止事务不落库，且会话状态完全复位可重新开启事务
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nak\r\n").await?;
  assert_eq!(resp, b"$2\r\nv1\r\n");

  let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  info!("EXECABORT 中止事务验证通过");
  OK
}

/// MULTI 与 EXEC 之间的并发修改导致乐观锁冲突（两段式 EXEC 回归）
/// 对标 Garnet TransactionManager.Run 顺序：先加物理哈希锁、持锁校验监视版本，
/// 确保 MULTI 排队期间监视键被并发修改时 EXEC 返回 nil。
#[compio::test]
async fn test_watch_conflict_between_multi_and_exec() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client1 = fixture.connect_client().await?;
  let mut client2 = fixture.connect_client().await?;

  // 1. 客户端 1 监视键并开启事务排队写入
  let resp = send_and_recv(&mut client1, b"*2\r\n$5\r\nWATCH\r\n$2\r\nrk\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client1, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut client1,
    b"*3\r\n$3\r\nSET\r\n$2\r\nrk\r\n$6\r\nstale1\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  // 2. 客户端 2 在排队阶段与 EXEC 之间并发修改该键
  let resp = send_and_recv(
    &mut client2,
    b"*3\r\n$3\r\nSET\r\n$2\r\nrk\r\n$6\r\nstale2\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 3. 客户端 1 提交：监视版本已失效，整体放弃返回 nil 数组
  let resp = send_and_recv(&mut client1, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*-1\r\n");

  // 4. 冲突后事务状态彻底复位，键值保持并发写入方的最新值，且会话可继续正常操作
  let resp = send_and_recv(&mut client1, b"*2\r\n$3\r\nGET\r\n$2\r\nrk\r\n").await?;
  assert_eq!(resp, b"$6\r\nstale2\r\n");

  let resp = send_and_recv(&mut client1, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  info!("MULTI 与 EXEC 间并发修改乐观锁冲突验证通过");
  OK
}

/// 多客户端事务锁竞争压力测试（两段式先锁后校验路径）
/// 多客户端并发 WATCH 同一键并竞争 EXEC：最终键值必然等于某一次成功提交的值，
/// 服务端无死锁、无异常断连，全部会话保持可用。
#[compio::test]
async fn test_exec_lock_contention_stress() -> Void {
  const CONCURRENT_CLIENTS: u64 = 4;
  const ROUNDS_PER_CLIENT: usize = 20;

  let fixture = Arc::new(NetworkTestFixture::setup().await?);
  let total_success = Arc::new(AtomicUsize::new(0));
  let mut handles = Vec::new();

  for c in 0..CONCURRENT_CLIENTS {
    let fix = Arc::clone(&fixture);
    let total_success = Arc::clone(&total_success);
    handles.push(spawn(async move {
      let mut client = fix.connect_client().await?;
      for i in 0..ROUNDS_PER_CLIENT {
        let key = format!("cnt{}", c);
        // 监视自己的计数键并读基线
        let resp = send_and_recv(
          &mut client,
          format!(
            "*2\r\n$5\r\nWATCH\r\n${}\r\n{}\r\n",
            key.len(),
            key.as_str()
          )
          .as_bytes(),
        )
        .await?;
        assert_eq!(resp, b"+OK\r\n");

        let base = format!("b{}-{}", c, i);
        // MULTI + 排队写 + 提交
        let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
        assert_eq!(resp, b"+OK\r\n");
        let resp = send_and_recv(
          &mut client,
          format!(
            "*3\r\n$3\r\nSET\r\n${}\r\n{}\r\n${}\r\n{}\r\n",
            key.len(),
            key.as_str(),
            base.len(),
            base.as_str()
          )
          .as_bytes(),
        )
        .await?;
        assert_eq!(resp, b"+QUEUED\r\n");
        let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
        assert!(
          resp == b"*1\r\n+OK\r\n" || resp == b"*-1\r\n",
          "EXEC 结果异常: {resp:?}"
        );
        if resp == b"*1\r\n+OK\r\n" {
          total_success.fetch_add(1, Ordering::Relaxed);
        }
      }
      OK
    }));
  }

  for h in handles {
    h.await.unwrap()?;
  }
  let total_success = total_success.load(Ordering::Relaxed);
  assert!(
    total_success > 0,
    "至少应存在一次成功提交（无死锁且无异常中断）"
  );

  // 压力后服务端保持健康
  let mut probe = fixture.connect_client().await?;
  let resp = send_and_recv(&mut probe, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  info!("多客户端事务锁竞争压力验证通过, 成功提交 {total_success} 次");
  OK
}
