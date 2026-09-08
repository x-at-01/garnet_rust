use core::time::Duration;
use std::{fs::write, sync::Arc};

use aok::{OK, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpListener,
  runtime::{Runtime, spawn},
  time::sleep,
};
use log::info;
use tempfile::tempdir;
use wedb_repl::{
  HandshakeDriver, HandshakeStep, MasterResponse, NodeRole, REPLICATION_HISTORY_BYTES_LEN,
  ReplConfSubCmd, ReplId, ReplicaClient, ReplicaCommand, ReplicationHistory, ReplicationManager,
  ReplicationSyncManager, SyncDecision, encode_auth, encode_continue, encode_fullresync,
  encode_ping, encode_pong, encode_psync, encode_replconf_ack, encode_replconf_capa,
  encode_replconf_ip, encode_replconf_port, parse_i64_bytes, parse_master_response,
  parse_replica_command, parse_u16_bytes, parse_u64_bytes,
};

use super::support::init_test_store;

/// 五步握手协议状态流转测试（无密码及有密码认证）
#[test]
fn handshake_state_machine_flow() -> Void {
  // 1. 无密码认证的标准五步握手
  let mut driver = HandshakeDriver::new(
    6380,
    "127.0.0.1".to_string(),
    None,
    ReplId::question_mark(),
    -1,
  );
  assert_eq!(driver.step, HandshakeStep::Initial);

  // Initial 阶段生成 PING
  let cmd = driver.next_command()?;
  assert_eq!(cmd, encode_ping());

  // 收到 PONG，由于无密码，自动跃迁至 AuthDone (准备发送 Port)
  let step = driver.feed_response(&MasterResponse::Pong)?;
  assert_eq!(step, HandshakeStep::AuthDone);

  // AuthDone 阶段生成 REPLCONF listening-port 6380
  let cmd = driver.next_command()?;
  assert_eq!(cmd, encode_replconf_port(6380));

  // 收到 OK，跃迁至 PortDone (准备发送 IP)
  let step = driver.feed_response(&MasterResponse::Ok)?;
  assert_eq!(step, HandshakeStep::PortDone);

  // PortDone 阶段生成 REPLCONF ip-address 127.0.0.1
  let cmd = driver.next_command()?;
  assert_eq!(cmd, encode_replconf_ip("127.0.0.1"));

  // 收到 OK，跃迁至 IpDone (准备发送 Capa)
  let step = driver.feed_response(&MasterResponse::Ok)?;
  assert_eq!(step, HandshakeStep::IpDone);

  // IpDone 阶段生成 REPLCONF capa eof capa psync2
  let cmd = driver.next_command()?;
  assert_eq!(cmd, encode_replconf_capa(&["eof", "psync2"]));

  // 收到 OK，跃迁至 CapaDone (准备发送 PSYNC)
  let step = driver.feed_response(&MasterResponse::Ok)?;
  assert_eq!(step, HandshakeStep::CapaDone);

  // CapaDone 阶段生成 PSYNC ? -1
  let cmd = driver.next_command()?;
  assert_eq!(cmd, encode_psync(&ReplId::question_mark(), -1));

  // 收到 +FULLRESYNC 响应，完成握手跃迁至 Established
  let primary_id = ReplId::generate();
  let step = driver.feed_response(&MasterResponse::FullResync {
    replid: primary_id,
    offset: 1000,
  })?;
  assert_eq!(step, HandshakeStep::Established);

  // 2. 带密码认证的握手流程
  let mut auth_driver = HandshakeDriver::new(
    6380,
    "127.0.0.1".to_string(),
    Some("secret_pwd".to_string()),
    ReplId::question_mark(),
    -1,
  );
  auth_driver.feed_response(&MasterResponse::Pong)?;
  assert_eq!(auth_driver.step, HandshakeStep::PingDone);

  let auth_cmd = auth_driver.next_command()?;
  assert_eq!(auth_cmd, encode_auth("secret_pwd"));

  auth_driver.feed_response(&MasterResponse::Ok)?;
  assert_eq!(auth_driver.step, HandshakeStep::AuthDone);

  info!("五步握手协议状态流转验证通过");
  OK
}

/// 握手状态机错误响应容错与重置测试
#[test]
fn handshake_driver_error_responses_and_reset() -> Void {
  let mut driver = HandshakeDriver::new(
    6379,
    "127.0.0.1".to_string(),
    Some("mypass".to_string()),
    ReplId::question_mark(),
    -1,
  );

  // 1. 保活阶段收到 -NOAUTH Authentication required -> 自动进入密码认证阶段
  let noauth_resp = MasterResponse::Error("NOAUTH Authentication required.".to_string());
  let step = driver.feed_response(&noauth_resp)?;
  assert_eq!(step, HandshakeStep::PingDone);

  // 2. 认证阶段收到 -WRONGPASS -> 抛出 AuthFailed 错误
  let wrongpass_resp =
    MasterResponse::Error("WRONGPASS invalid username-password pair".to_string());
  let err = driver.feed_response(&wrongpass_resp).unwrap_err();
  assert!(matches!(err, wedb_repl::Error::AuthFailed(_)));

  // 3. 调用 reset 重置状态机
  driver.reset();
  assert_eq!(driver.step, HandshakeStep::Initial);

  // 4. 未配密码时收到 -NOAUTH -> 抛出 HandshakeFailed 错误
  let mut no_pwd_driver = HandshakeDriver::new(
    6379,
    "127.0.0.1".to_string(),
    None,
    ReplId::question_mark(),
    -1,
  );
  let err2 = no_pwd_driver.feed_response(&noauth_resp).unwrap_err();
  assert!(matches!(err2, wedb_repl::Error::HandshakeFailed(_)));

  info!("握手状态机错误响应容错与重置验证通过");
  OK
}

/// PSYNC2 裸 +CONTINUE（无 replid 参数）必须沿用从节点当前缓存的复制编号，而非清空为全 0
#[test]
fn handshake_bare_continue_keeps_cached_replid() -> Void {
  let runtime = Runtime::new()?;
  runtime.block_on(async {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;

    let server_task = spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();
      let mut buf = vec![0u8; 1024];

      // PING -> +PONG
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      assert!(res.unwrap() > 0);
      let BufResult(res, _) = stream.write_all(b"+PONG\r\n").await;
      res.unwrap();

      // 连续三轮 REPLCONF -> +OK（一次性发送制造粘包）
      buf.clear();
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      assert!(res.unwrap() > 0);
      let BufResult(res, _) = stream.write_all(b"+OK\r\n+OK\r\n+OK\r\n").await;
      res.unwrap();

      // PSYNC -> 裸 +CONTINUE（无 replid，PSYNC2 语义：沿用从节点缓存编号）
      buf.clear();
      let BufResult(res, _) = stream.read(buf).await;
      assert!(res.unwrap() > 0);
      let BufResult(res, _) = stream.write_all(b"+CONTINUE\r\n").await;
      res.unwrap();
    });

    let (store, _dir) = init_test_store()?;
    let manager = Arc::new(ReplicationManager::new(NodeRole::Replica, 0));

    // 预先缓存一份主节点复制编号，模拟曾与主节点建立过复制流的从节点
    let cached_id = ReplId::from_str_val("0123456789abcdef0123456789abcdef01234567")?;
    manager.try_update_my_primary_repl_id(cached_id);

    let client = ReplicaClient::new(
      local_addr.to_string(),
      6381,
      "127.0.0.1".to_string(),
      None,
      manager.clone(),
      store,
      None,
    );

    let (_stream, decision) = client.connect_and_handshake().await?;
    assert!(matches!(decision, SyncDecision::PartialResync { .. }));
    // 关键断言：缓存编号未被裸 +CONTINUE 清空为全 0
    assert_eq!(manager.get_primary_repl_id(), cached_id);

    let _ = server_task.await;
    OK
  })?;

  info!("PSYNC2 裸 +CONTINUE 沿用缓存编号验证通过");
  OK
}

/// 五步握手网络半包与多帧粘包集成测试
#[test]
fn handshake_half_packet_and_sticky_packet() -> Void {
  let runtime = Runtime::new()?;
  runtime.block_on(async {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let local_addr = listener.local_addr()?;

    let server_task = spawn(async move {
      let (mut stream, _) = listener.accept().await.unwrap();

      // 阶段 1: 期望接收 PING -> 拆分为两个半包返回 "+" 和 "PONG\r\n"
      let mut buf = vec![0u8; 1024];
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      let n = res.unwrap();
      assert!(n > 0);

      let BufResult(res, _) = stream.write_all(b"+").await;
      res.unwrap();
      sleep(Duration::from_millis(10)).await;
      let BufResult(res, _) = stream.write_all(b"PONG\r\n").await;
      res.unwrap();

      // 阶段 2: 期望接收 REPLCONF listening-port
      buf.clear();
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      let n = res.unwrap();
      assert!(n > 0);

      // 阶段 3 & 4: 故意构造粘包！一次性返回两个 +OK\r\n
      let BufResult(res, _) = stream.write_all(b"+OK\r\n+OK\r\n").await;
      res.unwrap();

      // 阶段 5: 接收后续 REPLCONF capa
      buf.clear();
      let BufResult(res, returned_buf) = stream.read(buf).await;
      buf = returned_buf;
      let n = res.unwrap();
      assert!(n > 0);

      let BufResult(res, _) = stream.write_all(b"+OK\r\n").await;
      res.unwrap();

      // 阶段 6: 接收 PSYNC 并返回 +CONTINUE <replid>\r\n
      buf.clear();
      let BufResult(res, _) = stream.read(buf).await;
      let n = res.unwrap();
      assert!(n > 0);

      let resp = b"+CONTINUE aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\r\n";
      let BufResult(res, _) = stream.write_all(resp).await;
      res.unwrap();
    });

    let (store, _dir) = init_test_store()?;
    let manager = Arc::new(ReplicationManager::new(NodeRole::Replica, 0));

    let client = ReplicaClient::new(
      local_addr.to_string(),
      6381,
      "127.0.0.1".to_string(),
      None,
      manager.clone(),
      store,
      None,
    );

    let (_stream, decision) = client.connect_and_handshake().await?;
    assert!(matches!(decision, SyncDecision::PartialResync { .. }));

    let _ = server_task.await;
    OK
  })?;

  info!("五步握手网络半包与多帧粘包集成测试通过");
  OK
}

/// RESP 复制协议帧编解码与解析测试
#[test]
fn protocol_frames_encode_decode() -> Void {
  // 1. 测试从节点命令解析: PING
  let ping_bytes = encode_ping();
  let (cmd, n) = parse_replica_command(ping_bytes)?.unwrap();
  assert_eq!(cmd, ReplicaCommand::Ping);
  assert_eq!(n, ping_bytes.len());

  // 2. 测试从节点命令解析: AUTH
  let auth_bytes = encode_auth("pass123");
  let (cmd, _) = parse_replica_command(&auth_bytes)?.unwrap();
  assert_eq!(cmd, ReplicaCommand::Auth("pass123".to_string()));

  // 3. 测试从节点命令解析: REPLCONF ACK
  let ack_bytes = encode_replconf_ack(9999);
  let (cmd, _) = parse_replica_command(&ack_bytes)?.unwrap();
  assert_eq!(cmd, ReplicaCommand::ReplConf(ReplConfSubCmd::Ack(9999)));

  // 4. 测试从节点命令解析: PSYNC
  let replid = ReplId::generate();
  let psync_bytes = encode_psync(&replid, 12345);
  let (cmd, _) = parse_replica_command(&psync_bytes)?.unwrap();
  assert_eq!(
    cmd,
    ReplicaCommand::Psync {
      replid,
      offset: 12345,
    }
  );

  // 5. 测试主节点响应解析: +PONG\r\n
  let pong_bytes = encode_pong();
  let (resp, _) = parse_master_response(pong_bytes)?.unwrap();
  assert_eq!(resp, MasterResponse::Pong);

  // 6. 测试主节点响应解析: +CONTINUE <replid>\r\n
  let cont_bytes = encode_continue(&replid);
  let (resp, _) = parse_master_response(&cont_bytes)?.unwrap();
  assert_eq!(resp, MasterResponse::Continue { replid });

  // 7. 测试主节点响应解析: +FULLRESYNC <replid> <offset>\r\n
  let full_bytes = encode_fullresync(&replid, 8888);
  let (resp, _) = parse_master_response(&full_bytes)?.unwrap();
  assert_eq!(
    resp,
    MasterResponse::FullResync {
      replid,
      offset: 8888,
    }
  );

  // 8. 测试主节点错误响应: -ERR invalid password\r\n
  let err_bytes = b"-ERR invalid password\r\n";
  let (resp, _) = parse_master_response(err_bytes)?.unwrap();
  assert_eq!(
    resp,
    MasterResponse::Error("ERR invalid password".to_string())
  );

  // 9. 协议帧常量与编码器输出一致性校验
  assert_eq!(encode_ping(), b"*1\r\n$4\r\nPING\r\n");
  assert_eq!(
    encode_auth("mypassword"),
    b"*2\r\n$4\r\nAUTH\r\n$10\r\nmypassword\r\n"
  );
  assert_eq!(
    encode_replconf_port(6380),
    b"*3\r\n$8\r\nREPLCONF\r\n$14\r\nlistening-port\r\n$4\r\n6380\r\n"
  );
  assert_eq!(
    encode_replconf_ip("127.0.0.1"),
    b"*3\r\n$8\r\nREPLCONF\r\n$10\r\nip-address\r\n$9\r\n127.0.0.1\r\n"
  );
  assert_eq!(
    encode_replconf_ack(1024).as_slice(),
    &b"*3\r\n$8\r\nREPLCONF\r\n$3\r\nACK\r\n$4\r\n1024\r\n"[..]
  );
  assert_eq!(
    encode_replconf_capa(&["eof", "psync2"]),
    b"*5\r\n$8\r\nREPLCONF\r\n$4\r\ncapa\r\n$3\r\neof\r\n$4\r\ncapa\r\n$6\r\npsync2\r\n"
  );

  info!("RESP 复制协议帧编解码测试通过");
  OK
}

/// 复制协议边界容错、问号简写、大小写兼容与 CRLF 防护测试
#[test]
fn protocol_edge_cases_and_security() -> Void {
  // 1. ReplId 问号简写 '?' 解析
  let q_id = ReplId::from_str_val("?")?;
  assert!(q_id.is_question_mark());

  let manager = ReplicationManager::new(NodeRole::Primary, 0);
  let decision = manager.handle_psync("?", -1, 0, 1000)?;
  assert!(matches!(decision, SyncDecision::FullResync { .. }));

  // 2. PSYNC 1 兼容：+CONTINUE 无 replid 参数
  let cont_no_id = b"+CONTINUE\r\n";
  let (resp1, _) = parse_master_response(cont_no_id)?.unwrap();
  assert_eq!(
    resp1,
    MasterResponse::Continue {
      replid: ReplId::empty()
    }
  );

  // 3. 小写协议指令兼容：+continue 与 +fullresync
  let test_id = ReplId::generate();
  let cont_lower = format!("+continue {}\r\n", test_id.as_str());
  let (resp2, _) = parse_master_response(cont_lower.as_bytes())?.unwrap();
  assert_eq!(resp2, MasterResponse::Continue { replid: test_id });

  let full_lower = format!("+fullresync {} 4321\r\n", test_id.as_str());
  let (resp3, _) = parse_master_response(full_lower.as_bytes())?.unwrap();
  assert_eq!(
    resp3,
    MasterResponse::FullResync {
      replid: test_id,
      offset: 4321
    }
  );

  // 4. BulkString 缺少 CRLF 安全校验
  let bad_bulk = b"*1\r\n$4\r\nPINGXX";
  let err_res = parse_replica_command(bad_bulk);
  assert!(err_res.is_err(), "非法 CRLF 必须报错拦截");

  // 5. 畸形问号编号（仅首字符为 '?' 的 40 字节编号）必须报错拦截，
  //    严禁静默降级为问号协商（判定与 ReplId::from_bytes 严格一致）
  let malformed_q =
    b"*3\r\n$5\r\nPSYNC\r\n$40\r\n?xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n$2\r\n-1\r\n";
  assert!(
    parse_replica_command(malformed_q).is_err(),
    "畸形问号编号必须报错拦截"
  );

  // 6. 全 '?' 40 字节编号与单 '?' 简写等价，均按问号协商处理
  let q40 = b"*3\r\n$5\r\nPSYNC\r\n$40\r\n????????????????????????????????????????\r\n$2\r\n-1\r\n";
  let (cmd, _) = parse_replica_command(q40)?.expect("全 '?' 编号必须解析成功");
  assert!(matches!(cmd, ReplicaCommand::Psync { offset: -1, .. }));

  info!("复制协议边界容错与安全防护测试通过");
  OK
}

/// 复制协议解析防 DoS 与算术溢出极限安全防护测试
#[test]
fn protocol_malicious_input_guards() -> Void {
  // 1. 恶意超大参数数组声明必须被上限拦截
  let evil_args = b"*99999999\r\n$4\r\nPING\r\n";
  let res = parse_replica_command(evil_args);
  assert!(res.is_err(), "超过参数上限必须报错拦截");

  // 2. u64::MAX 级别的 BulkString 长度声明由 checked 算术安全拦截
  let huge_len = b"*1\r\n$18446744073709551615\r\nAB";
  let res = parse_replica_command(huge_len);
  assert!(res.is_err(), "超大字符串长度声明必须报错拦截");

  // 3. 上限内的合法 PSYNC 命令解析
  let psync = b"*3\r\n$5\r\nPSYNC\r\n$1\r\n?\r\n$2\r\n-1\r\n";
  let (cmd, _) = parse_replica_command(psync)?.expect("合法 PSYNC 必须解析成功");
  assert!(matches!(cmd, ReplicaCommand::Psync { offset: -1, .. }));

  info!("复制协议恶意输入防护验证通过");
  OK
}

/// 快速整数解析器（parse_u64_bytes, parse_i64_bytes, parse_u16_bytes）全边界验证
#[test]
fn fast_integer_parsers() -> Void {
  // 1. u64 解析
  assert_eq!(parse_u64_bytes(b"0"), Some(0));
  assert_eq!(parse_u64_bytes(b"0000"), Some(0));
  assert_eq!(parse_u64_bytes(b"000123"), Some(123));
  assert_eq!(parse_u64_bytes(b"123456789"), Some(123456789));
  assert_eq!(parse_u64_bytes(b"18446744073709551615"), Some(u64::MAX));
  assert_eq!(parse_u64_bytes(b"18446744073709551616"), None);
  assert_eq!(parse_u64_bytes(b"99999999999999999999999"), None);
  assert_eq!(parse_u64_bytes(b""), None);
  assert_eq!(parse_u64_bytes(b"+123"), None);
  assert_eq!(parse_u64_bytes(b"-1"), None);
  assert_eq!(parse_u64_bytes(b"123a"), None);
  assert_eq!(parse_u64_bytes(b" 123"), None);

  // 2. i64 解析
  assert_eq!(parse_i64_bytes(b"0"), Some(0));
  assert_eq!(parse_i64_bytes(b"-0"), Some(0));
  assert_eq!(parse_i64_bytes(b"+0"), Some(0));
  assert_eq!(parse_i64_bytes(b"0000"), Some(0));
  assert_eq!(parse_i64_bytes(b"-0000"), Some(0));
  assert_eq!(parse_i64_bytes(b"+0000"), Some(0));
  assert_eq!(parse_i64_bytes(b"000123"), Some(123));
  assert_eq!(parse_i64_bytes(b"-000123"), Some(-123));
  assert_eq!(parse_i64_bytes(b"+000123"), Some(123));
  assert_eq!(parse_i64_bytes(b"12345"), Some(12345));
  assert_eq!(parse_i64_bytes(b"-1"), Some(-1));
  assert_eq!(parse_i64_bytes(b"+42"), Some(42));
  assert_eq!(parse_i64_bytes(b"-9223372036854775808"), Some(i64::MIN));
  assert_eq!(parse_i64_bytes(b"9223372036854775807"), Some(i64::MAX));
  assert_eq!(parse_i64_bytes(b"+9223372036854775807"), Some(i64::MAX));
  assert_eq!(parse_i64_bytes(b"-9223372036854775809"), None);
  assert_eq!(parse_i64_bytes(b"9223372036854775808"), None);
  assert_eq!(parse_i64_bytes(b"+9223372036854775808"), None);
  assert_eq!(parse_i64_bytes(b"-99999999999999999999999"), None);
  assert_eq!(parse_i64_bytes(b"99999999999999999999999"), None);
  assert_eq!(parse_i64_bytes(b"-"), None);
  assert_eq!(parse_i64_bytes(b"+"), None);
  assert_eq!(parse_i64_bytes(b""), None);
  assert_eq!(parse_i64_bytes(b"123-45"), None);

  // 3. u16 解析
  assert_eq!(parse_u16_bytes(b"0"), Some(0));
  assert_eq!(parse_u16_bytes(b"000080"), Some(80));
  assert_eq!(parse_u16_bytes(b"6379"), Some(6379));
  assert_eq!(parse_u16_bytes(b"65535"), Some(65535));
  assert_eq!(parse_u16_bytes(b"65536"), None);
  assert_eq!(parse_u16_bytes(b"99999"), None);
  assert_eq!(parse_u16_bytes(b""), None);
  assert_eq!(parse_u16_bytes(b"-1"), None);

  info!("快速整数解析器全边界验证通过");
  OK
}

/// PSYNC 同步判定状态机全矩阵测试
#[test]
fn psync_decision_matrix() -> Void {
  let hist = ReplicationHistory::new(1000);
  let hist_failover = hist.failover_update(2000);
  let curr_id = hist_failover.primary_replid;
  let prev_id = hist_failover.primary_replid2;
  let wal_begin = 500u64;
  let wal_tail = 3000u64;

  // 1. 全新从节点初次同步 (PSYNC ? -1) -> 必须触发全量同步
  let d1 = ReplicationSyncManager::decide_sync(
    &hist_failover,
    wal_begin,
    wal_tail,
    &ReplId::question_mark(),
    -1,
  );
  assert_eq!(
    d1,
    SyncDecision::FullResync {
      replid: curr_id,
      snapshot_offset: wal_tail,
    }
  );

  // 2. 当前主 ID 匹配，且位点在有效 WAL 范围内 -> 增量追赶
  let d2 = ReplicationSyncManager::decide_sync(&hist_failover, wal_begin, wal_tail, &curr_id, 2500);
  assert_eq!(
    d2,
    SyncDecision::PartialResync {
      replid: curr_id,
      start_offset: 2500,
    }
  );

  // 3. 当前主 ID 匹配，但位点已截断淘汰 -> 全量同步
  let d3 = ReplicationSyncManager::decide_sync(&hist_failover, wal_begin, wal_tail, &curr_id, 400);
  assert_eq!(
    d3,
    SyncDecision::FullResync {
      replid: curr_id,
      snapshot_offset: wal_tail,
    }
  );

  // 4. 当前主 ID 匹配，但位点超前于 wal_tail -> 全量同步
  let d4 = ReplicationSyncManager::decide_sync(&hist_failover, wal_begin, wal_tail, &curr_id, 3500);
  assert_eq!(
    d4,
    SyncDecision::FullResync {
      replid: curr_id,
      snapshot_offset: wal_tail,
    }
  );

  // 5. 上一代 ID2 匹配，且位点在有效 WAL 范围且 <= offset2 (2000) -> 允许增量追赶
  let d5 = ReplicationSyncManager::decide_sync(&hist_failover, wal_begin, wal_tail, &prev_id, 1500);
  assert_eq!(
    d5,
    SyncDecision::PartialResync {
      replid: curr_id,
      start_offset: 1500,
    }
  );

  // 6. 上一代 ID2 匹配，但位点超过了切换断点边界 offset2 (2001 > 2000) -> 全量同步
  let d6 = ReplicationSyncManager::decide_sync(&hist_failover, wal_begin, wal_tail, &prev_id, 2001);
  assert_eq!(
    d6,
    SyncDecision::FullResync {
      replid: curr_id,
      snapshot_offset: wal_tail,
    }
  );

  // 7. 未知的 ReplId -> 全量同步
  let unknown_id = ReplId::generate();
  let d7 =
    ReplicationSyncManager::decide_sync(&hist_failover, wal_begin, wal_tail, &unknown_id, 1500);
  assert_eq!(
    d7,
    SyncDecision::FullResync {
      replid: curr_id,
      snapshot_offset: wal_tail,
    }
  );

  // 8. 全 0 空编号（无上一代的占位编号）凭 u64::MAX 边界位点的旧缺陷可骗取增量，
  //    现必须被拒绝回退全量（使用未经历故障转移的 hist：primary_replid2 为空且 offset2 = u64::MAX）
  let hist_fresh = ReplicationHistory::new(1000);
  let d8 =
    ReplicationSyncManager::decide_sync(&hist_fresh, wal_begin, wal_tail, &ReplId::empty(), 1500);
  assert_eq!(
    d8,
    SyncDecision::FullResync {
      replid: hist_fresh.primary_replid,
      snapshot_offset: wal_tail,
    }
  );

  info!("PSYNC 同步判定状态机全矩阵测试通过");
  OK
}

/// failover_update 故障转移与双复制编号体系规范转换
#[test]
fn failover_update_and_redis_dual_id_transition() -> Void {
  // 1. ReplicationHistory 单体故障转移转换
  let hist = ReplicationHistory::new(1000);
  let initial_master_id = hist.primary_replid;
  assert_eq!(hist.primary_replid2, ReplId::empty());
  assert_eq!(hist.replication_offset2, u64::MAX);
  assert_eq!(hist.replication_offset, 1000);

  // 推进位点至 5000 并执行故障转移轮转
  let rotated = ReplicationHistory {
    replication_offset: 5000,
    ..hist.clone()
  };
  let new_id = rotated.failover_update(5000);

  assert_eq!(new_id.primary_replid2, initial_master_id);
  assert_eq!(new_id.replication_offset2, 5000);
  assert_eq!(new_id.replication_offset, 5000);
  assert_ne!(new_id.primary_replid, initial_master_id);

  // 2. ReplicationManager 级联故障转移转换
  let mgr = ReplicationManager::new(NodeRole::Replica, 2000);
  let old_primary_id = mgr.get_primary_repl_id();
  assert!(mgr.is_replica());

  let new_master_id = mgr.failover_to_primary(8000);
  assert!(mgr.is_primary());
  assert_eq!(mgr.get_primary_repl_id(), new_master_id);
  assert_eq!(mgr.get_primary_repl_id2(), old_primary_id);
  assert_eq!(mgr.get_replication_offset2(), 8000);
  assert_eq!(mgr.get_replication_offset(), 8000);

  info!("failover_update 故障转移与双 ID 转换验证通过");
  OK
}

/// 复制历史拓扑轮转与持久化测试
#[test]
fn replication_history_failover_and_persistence() -> Void {
  let dir = tempdir()?;
  let conf_path = dir.path().join("replication.conf");

  // 1. 初始化复制历史
  let hist1 = ReplicationHistory::new(100);
  assert_eq!(hist1.replication_offset, 100);
  assert!(hist1.primary_replid2.is_empty());
  assert_eq!(hist1.replication_offset2, u64::MAX);

  // 2. 故障转移升主
  let hist2 = hist1.failover_update(500);
  assert_eq!(hist2.primary_replid2, hist1.primary_replid);
  assert_ne!(hist2.primary_replid, hist1.primary_replid);
  assert_eq!(hist2.replication_offset, 500);
  assert_eq!(hist2.replication_offset2, 500);

  // 3. 二进制序列化与反序列化校验
  let bytes = hist2.to_byte_array();
  assert_eq!(bytes.len(), REPLICATION_HISTORY_BYTES_LEN);
  let decoded = ReplicationHistory::from_bytes(&bytes)?;
  assert_eq!(decoded, hist2);

  // 4. 文件持久化原子写入与读取校验
  hist2.save_to_file(&conf_path)?;
  assert!(conf_path.exists());

  let loaded = ReplicationHistory::load_from_file(&conf_path)?;
  assert_eq!(loaded, hist2);

  info!("复制历史拓扑轮转与持久化验证通过");
  OK
}

/// history.meta.tmp 断电崩溃自愈与非法字符注入损坏修复
#[test]
fn history_tmp_file_crash_recovery_and_healing() -> Void {
  let dir = tempdir()?;
  let meta_path = dir.path().join("history.meta");
  let meta_tmp_path = dir.path().join("history.meta.tmp");

  // 1. 模拟主文件丢失，仅有 .tmp 完整文件
  let original_hist = ReplicationHistory::new(66666);
  write(&meta_tmp_path, original_hist.to_byte_array())?;
  assert!(!meta_path.exists());
  assert!(meta_tmp_path.exists());

  let loaded = ReplicationHistory::load_from_file(&meta_path)?;
  assert_eq!(loaded, original_hist);
  assert_eq!(loaded.replication_offset, 66666);
  assert!(meta_path.exists(), "自愈后主文件必须成功恢复");

  // 2. 模拟主文件残缺写坏，自动由 .tmp 恢复
  let corrupted_hist = ReplicationHistory::new(77777);
  write(&meta_tmp_path, corrupted_hist.to_byte_array())?;
  write(&meta_path, b"corrupted!")?;

  let recovered = ReplicationHistory::load_from_file(&meta_path)?;
  assert_eq!(recovered, corrupted_hist);
  assert_eq!(recovered.replication_offset, 77777);

  // 3. 校验 ReplId 严格字符校验与非法字符拦截
  assert!(ReplId::from_str_val("invalid_hex_char_xxxxxxxxxxxxxxxxxxxxxx!").is_err());
  assert!(ReplId::from_bytes(b"1234567890abcdef1234567890abcdef1234567G").is_err());
  let valid_id = ReplId::from_str_val("1234567890abcdef1234567890abcdef12345678")?;
  assert_eq!(valid_id.as_slice().len(), 40);

  info!("history.meta.tmp 崩溃自愈与字符安全校验验证通过");
  OK
}

/// bitcode 极速序列化与协议类型编解码一致性测试
#[test]
fn bitcode_roundtrip_and_edge_cases() -> Void {
  // 1. ReplId 编解码
  let id = ReplId::generate();
  let encoded_id = bitcode::encode(&id);
  let decoded_id: ReplId = bitcode::decode(&encoded_id)?;
  assert_eq!(decoded_id, id);

  // 2. ReplicationHistory 编解码
  let hist = ReplicationHistory::new(123456).failover_update(234567);
  let bitcode_bytes = hist.to_bitcode();
  let decoded_hist = ReplicationHistory::from_bitcode(&bitcode_bytes)?;
  assert_eq!(decoded_hist, hist);

  // 3. 基础枚举类型编解码
  use wedb_repl::{CheckpointFileType, NodeRole, RecoveryStatus, SyncDecision};

  let roles = [NodeRole::Primary, NodeRole::Replica];
  for &role in &roles {
    let enc = bitcode::encode(&role);
    let dec: NodeRole = bitcode::decode(&enc)?;
    assert_eq!(dec, role);
  }

  let statuses = [
    RecoveryStatus::NoRecovery,
    RecoveryStatus::ClusterReplicate,
    RecoveryStatus::ClusterFailover,
    RecoveryStatus::ReplicaOfNoOne,
    RecoveryStatus::CheckpointRecoveredAtReplica,
    RecoveryStatus::ReadRole,
  ];
  for &st in &statuses {
    let enc = bitcode::encode(&st);
    let dec: RecoveryStatus = bitcode::decode(&enc)?;
    assert_eq!(dec, st);
  }

  let decisions = [
    SyncDecision::PartialResync {
      replid: id,
      start_offset: 5000,
    },
    SyncDecision::FullResync {
      replid: id,
      snapshot_offset: 10000,
    },
  ];
  for dec in &decisions {
    let enc = bitcode::encode(dec);
    let decoded_dec: SyncDecision = bitcode::decode(&enc)?;
    assert_eq!(&decoded_dec, dec);
  }

  let file_types = [
    CheckpointFileType::StoreHlog,
    CheckpointFileType::StoreRangeIndexFlush,
    CheckpointFileType::StoreRangeIndexSnapshot,
  ];
  for &ft in &file_types {
    let enc = bitcode::encode(&ft);
    let dec: CheckpointFileType = bitcode::decode(&enc)?;
    assert_eq!(dec, ft);
  }

  // 4. 损坏载荷防御：任意垃圾字节解码绝不 panic，安全返回 Err
  let corrupted = [0xFF, 0xFE, 0xFD, 0xFC];
  assert!(ReplicationHistory::from_bitcode(&corrupted).is_err());
  assert!(bitcode::decode::<ReplId>(&corrupted).is_err());

  info!("bitcode 序列化往返与损坏防御测试通过");
  OK
}
