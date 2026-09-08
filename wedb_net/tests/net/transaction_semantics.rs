use std::{str::from_utf8, time::Duration};

use aok::{Error, OK, Void};
use compio::{BufResult, io::AsyncRead, net::TcpStream, runtime::spawn, time::sleep};
use log::info;

use crate::support::{DEFAULT_BUF_CAPACITY, NetworkTestFixture, POLL_INTERVAL, send_and_recv};

/// 事务 SELECT 语义回归（移交线索：对齐 C# NetworkSKIP 的 isMultiDbCommand 分支）
///
/// C# TxnRespCommands.cs NetworkSKIP：事务中异库 SELECT 将使排队期按原库前缀
/// 提取的键锁与执行期命名空间错位，故报错并 Abort（后续 EXEC 报 EXECABORT）；
/// 同库 SELECT 幂等放行入队。
#[compio::test]
async fn test_txn_select_different_db_aborts_txn() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 0. db0 预置基准值
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ndk\r\n$2\r\nv0\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 1. 事务中异库 SELECT：立即报错并中止事务
  let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ndk\r\n$2\r\nv1\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$1\r\n1\r\n").await?;
  assert_eq!(
    resp,
    b"-ERR SELECT is currently unsupported inside a transaction.\r\n"
  );

  // 2. 中止态后续命令仍回 +QUEUED，EXEC 统一报 EXECABORT（对齐 C# Aborted 路径）
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ndk\r\n$2\r\nv2\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(
    resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  // 3. 中止事务不落库，会话仍处于 db0（异库切换未生效）
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\ndk\r\n").await?;
  assert_eq!(resp, b"$2\r\nv0\r\n");

  // 4. 会话状态复位：重新执行异库 SELECT 与事务均正常
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$1\r\n1\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\ndk\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  info!("事务内异库 SELECT 中止事务验证通过");
  OK
}

/// 事务中同库 SELECT 幂等放行入队（对齐 C# `index == activeDbId` 放行口径），
/// 非法参数同样放行，由 EXEC 执行期统一报错（对齐 C# TryGetInt 失败不中止的口径）
#[compio::test]
async fn test_txn_select_same_db_and_invalid_args() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 同库 SELECT 入队执行，整体事务正常提交
  let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$1\r\n0\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nsk\r\n$2\r\nsv\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*2\r\n+OK\r\n+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nsk\r\n").await?;
  assert_eq!(resp, b"$2\r\nsv\r\n");

  // 2. 非法参数 SELECT 放行入队，EXEC 执行期该条报错但不中止后续命令
  let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$3\r\nabc\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(
    resp,
    b"*2\r\n-ERR value is not an integer or out of range\r\n+PONG\r\n"
  );

  info!("事务内同库与非法参数 SELECT 放行验证通过");
  OK
}

/// 事务内嵌套 MULTI 报错并中止事务（对齐 C# NetworkMULTI：Abort 后 EXEC 统一报 EXECABORT）
#[compio::test]
async fn test_txn_nested_multi_aborts() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nnk\r\n$2\r\nnv\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"MULTI\r\n").await?;
  assert_eq!(resp, b"-ERR MULTI calls can not be nested\r\n");

  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(
    resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  // 中止事务不落库
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nnk\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  info!("嵌套 MULTI 中止事务验证通过");
  OK
}

/// 事务内 WATCH 仅报错不中止事务（对齐 Garnet NetworkSKIP isWatch 分支与 Redis 语义）
///
/// WATCH 在 MULTI 内路由 TransactionManager::watch() 拦截：回泛化错误但不置脏、
/// 不入队，后续排队命令继续 +QUEUED，EXEC 正常提交全部命令。
#[compio::test]
async fn test_watch_inside_multi_error_keeps_txn() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  assert_eq!(send_and_recv(&mut client, b"MULTI\r\n").await?, b"+OK\r\n");

  // 事务内 WATCH：仅报错，事务不中止
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nWATCH\r\n$2\r\nwk\r\n").await?;
  assert_eq!(resp, b"-ERR WATCH inside MULTI is not allowed\r\n");

  // 报错后排队与提交不受影响（EXECABORT 未触发）
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nwk\r\n$2\r\nwv\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*2\r\n+OK\r\n+PONG\r\n");

  // 事务正常落库
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nwk\r\n").await?;
  assert_eq!(resp, b"$2\r\nwv\r\n");

  info!("事务内 WATCH 报错不中止事务验证通过");
  OK
}

/// WATCH 版本键跨库精确失效回归（会话复合键 = 会话前缀 ++ 用户键）
///
/// db0 WATCH 后切换到 db1 写同名键不得使监视失效（修复前裸键哈希保守误判）；
/// 同库单命令写仍必须失效（bump 侧与 watch 侧复合键三方同构）。
#[compio::test]
async fn test_watch_db_scoped_invalidation() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. db0 基线值并以复合键记录 WATCH 基线
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ndk\r\n$2\r\nv0\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nWATCH\r\n$2\r\ndk\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 2. 切库写同名键：db1 的修改不应失效 db0 的监视
  assert_eq!(
    send_and_recv(&mut client, b"SELECT 1\r\n").await?,
    b"+OK\r\n"
  );
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ndk\r\n$2\r\nv1\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  assert_eq!(
    send_and_recv(&mut client, b"SELECT 0\r\n").await?,
    b"+OK\r\n"
  );

  // 3. 事务提交：乐观锁未冲突（修复前裸键哈希在此误报 *-1 冲突）
  assert_eq!(send_and_recv(&mut client, b"MULTI\r\n").await?, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ndk\r\n$2\r\nv2\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");
  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*1\r\n+OK\r\n");

  // 4. 正向失效：同库单命令写必须触发复合键版本递增
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nWATCH\r\n$2\r\ndk\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ndk\r\n$3\r\ndir\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  assert_eq!(send_and_recv(&mut client, b"MULTI\r\n").await?, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ndk\r\n$2\r\nv3\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");
  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*-1\r\n");

  info!("WATCH 跨库精确失效验证通过");
  OK
}

/// 跨会话 WATCH 精确失效回归：异库同名键写不失效，同库事务提交广播必须失效
///
/// 事务提交期版本广播基于入队期按会话复合键登记的锁集合（queue_command
/// 复合哈希），验证其他会话 WATCH 的失效判定精确到 (ns, db, key)。
#[compio::test]
async fn test_watch_cross_session_txn_commit_invalidation() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut watcher = fixture.connect_client().await?;
  let mut writer = fixture.connect_client().await?;

  // 1. 会话 A（db0）写入基准并 WATCH
  let resp = send_and_recv(&mut watcher, b"*3\r\n$3\r\nSET\r\n$2\r\nck\r\n$2\r\nv0\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(&mut watcher, b"*2\r\n$5\r\nWATCH\r\n$2\r\nck\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 2. 会话 B 切到 db1 写同名键：不得失效 A 的监视
  assert_eq!(
    send_and_recv(&mut writer, b"SELECT 1\r\n").await?,
    b"+OK\r\n"
  );
  let resp = send_and_recv(&mut writer, b"*3\r\n$3\r\nSET\r\n$2\r\nck\r\n$2\r\nv1\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 3. A 事务提交：未受跨库写干扰（A 仍在 db0，读回基准值）
  let resp = send_and_recv(&mut watcher, b"*2\r\n$3\r\nGET\r\n$2\r\nck\r\n").await?;
  assert_eq!(resp, b"$2\r\nv0\r\n");
  assert_eq!(send_and_recv(&mut watcher, b"MULTI\r\n").await?, b"+OK\r\n");
  let resp = send_and_recv(&mut watcher, b"*3\r\n$3\r\nSET\r\n$2\r\nck\r\n$2\r\nva\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");
  let resp = send_and_recv(&mut watcher, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*1\r\n+OK\r\n");

  // 4. B 回 db0 并以事务提交同名键：提交期复合键版本广播必须失效 A 的新 WATCH
  assert_eq!(
    send_and_recv(&mut writer, b"SELECT 0\r\n").await?,
    b"+OK\r\n"
  );
  let resp = send_and_recv(&mut watcher, b"*2\r\n$5\r\nWATCH\r\n$2\r\nck\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  assert_eq!(send_and_recv(&mut writer, b"MULTI\r\n").await?, b"+OK\r\n");
  let resp = send_and_recv(&mut writer, b"*3\r\n$3\r\nSET\r\n$2\r\nck\r\n$2\r\nvb\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");
  let resp = send_and_recv(&mut writer, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*1\r\n+OK\r\n");

  let resp = send_and_recv(&mut watcher, b"MULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(&mut watcher, b"*3\r\n$3\r\nSET\r\n$2\r\nck\r\n$2\r\nvc\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");
  let resp = send_and_recv(&mut watcher, b"EXEC\r\n").await?;
  assert_eq!(resp, b"*-1\r\n");

  info!("跨会话事务提交复合键失效验证通过");
  OK
}

/// 入队期 arity 校验回归（对齐 Garnet MultiProcessCommand 入队校验时机）
///
/// 参数个数非法的命令在入队时立即报错并置脏中止（不入队、不回 +QUEUED），
/// EXEC 统一 EXECABORT，事务不落库；会话状态随后复位可正常开启新事务。
#[compio::test]
async fn test_txn_enqueue_arity_rejected() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nar\r\n$2\r\nv0\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 1. GET 恒定 arity=2（3 token）：入队即拒
  assert_eq!(send_and_recv(&mut client, b"MULTI\r\n").await?, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nGET\r\n$2\r\nk1\r\n$2\r\nk2\r\n").await?;
  assert_eq!(
    resp,
    b"-ERR wrong number of arguments for 'get' command\r\n"
  );

  // 中止态后续命令仍回 +QUEUED，EXEC 统一报 EXECABORT
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nar\r\n$2\r\nv1\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");
  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(
    resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  // 事务不落库，会话复位后新事务正常
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nar\r\n").await?;
  assert_eq!(resp, b"$2\r\nv0\r\n");

  // 2. MIGRATE 最低 arity=6（4 token）：入队即拒，杜绝键规格提取越界风险
  assert_eq!(send_and_recv(&mut client, b"MULTI\r\n").await?, b"+OK\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$7\r\nMIGRATE\r\n$1\r\nh\r\n$2\r\n99\r\n$1\r\nk\r\n",
  )
  .await?;
  assert_eq!(
    resp,
    b"-ERR wrong number of arguments for 'migrate' command\r\n"
  );
  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(
    resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  info!("入队期 arity 校验拒绝验证通过");
  OK
}

/// 事务内 SWAPDB 禁入回归（异库操作使排队期键锁与执行期命名空间错位）
///
/// SWAPDB 入队即报错并置脏中止，后续 EXEC 统一 EXECABORT（对齐 C#
/// AllowedInTxn 禁入路径）。
#[compio::test]
async fn test_txn_swapdb_disallowed_aborts() -> Void {
  let fixture = NetworkTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nsk\r\n$2\r\nv0\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  assert_eq!(send_and_recv(&mut client, b"MULTI\r\n").await?, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nsk\r\n$2\r\nv1\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*3\r\n$6\r\nSWAPDB\r\n$1\r\n0\r\n$1\r\n1\r\n").await?;
  assert_eq!(
    resp,
    b"-ERR command 'SWAPDB' is not allowed in transaction\r\n"
  );

  let resp = send_and_recv(&mut client, b"EXEC\r\n").await?;
  assert_eq!(
    resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  // 中止事务不落库
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nsk\r\n").await?;
  assert_eq!(resp, b"$2\r\nv0\r\n");

  info!("事务内 SWAPDB 禁入验证通过");
  OK
}

/// 并发单命令写锁脚手架回归（移交线索：单命令写路径与事务共用同一哈希桶锁线性化）
///
/// 多连接并发 SETNX 同一键：锁脚手架保证"存在性检查-写入"原子闭合，
/// 任意时刻每键恰好一个胜者；并发加锁路径无死锁、无协议错乱。
#[compio::test]
async fn test_concurrent_setnx_single_winner() -> Void {
  const CONN_COUNT: usize = 8;
  const ROUNDS: usize = 64;
  /// 胜者值报文定长：`$2\r\nwN\r\n`
  const WINNER_RESP_LEN: usize = 8;

  let fixture = NetworkTestFixture::setup().await?;
  let addr = fixture.addr;
  let mut handles = Vec::with_capacity(CONN_COUNT);

  for conn in 0..CONN_COUNT {
    handles.push(spawn(async move {
      let mut client = TcpStream::connect(addr).await?;
      // 每连接获胜键数量
      let mut wins = 0u32;
      for round in 0..ROUNDS {
        let (k, v) = (format!("ck{round}"), format!("w{conn}"));
        let cmd = format!(
          "*3\r\n$5\r\nSETNX\r\n${}\r\n{k}\r\n${}\r\n{v}\r\n",
          k.len(),
          v.len()
        );
        let resp = send_and_recv(&mut client, cmd.as_bytes()).await?;
        assert!(resp == b":1\r\n" || resp == b":0\r\n", "非法响应: {resp:?}");
        if resp == b":1\r\n" {
          wins += 1;
        }
        // 轮间让位，增加多连接真实并发重叠窗口
        if round % 8 == 0 {
          sleep(POLL_INTERVAL).await;
        }
      }
      Ok::<_, Error>(wins)
    }));
  }

  let mut total_wins = 0u32;
  for h in handles {
    total_wins += h.await.unwrap()?;
  }

  // 每轮每键恰好一个胜者
  assert_eq!(
    total_wins, ROUNDS as u32,
    "SETNX 每键应恰好一个胜者，实际 {total_wins}"
  );

  // 胜者值完整性：未获胜连接不得污染键值
  let mut client = fixture.connect_client().await?;
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nck0\r\n").await?;
  assert_eq!(resp.len(), WINNER_RESP_LEN, "胜者值报文异常: {resp:?}");

  info!("并发 SETNX 单胜者验证通过");
  OK
}

/// 持锁事务 EXEC 与并发单命令写交叉回归（移交线索：写-写线性化窗口闭合）
///
/// 客户端 1 循环 WATCH+MULTI+EXEC 事务写，客户端 2 循环单命令写同一键：
/// 事务结果只能是成功或 nil 冲突（乐观锁），不得出现协议错乱或服务端错误；
/// 最终键值必为某一方的完整写入值。
#[compio::test]
async fn test_txn_exec_interleaved_with_single_writes() -> Void {
  const ROUNDS: usize = 40;

  let fixture = NetworkTestFixture::setup().await?;
  let addr = fixture.addr;

  let txn_handle = spawn(async move {
    let mut client = TcpStream::connect(addr).await?;
    let mut committed = 0u32;
    let mut conflicts = 0u32;
    for round in 0..ROUNDS {
      let watch = send_and_recv(&mut client, b"*2\r\n$5\r\nWATCH\r\n$3\r\ntik\r\n").await?;
      assert_eq!(watch, b"+OK\r\n");

      assert_eq!(send_and_recv(&mut client, b"MULTI\r\n").await?, b"+OK\r\n");
      let v = format!("tx{round}");
      let set_cmd = format!("*3\r\n$3\r\nSET\r\n$3\r\ntik\r\n${}\r\n{v}\r\n", v.len());
      let queued = send_and_recv(&mut client, set_cmd.as_bytes()).await?;
      assert_eq!(queued, b"+QUEUED\r\n");

      let exec = send_and_recv(&mut client, b"EXEC\r\n").await?;
      match exec.as_slice() {
        b"*1\r\n+OK\r\n" => committed += 1,
        b"*-1\r\n" => conflicts += 1,
        other => panic!("事务 EXEC 非法结果: {other:?}"),
      }
    }
    Ok::<_, Error>((committed, conflicts))
  });

  let single_handle = spawn(async move {
    let mut client = TcpStream::connect(addr).await?;
    for i in 0..ROUNDS * 2 {
      let v = format!("sg{i}");
      let set_cmd = format!("*3\r\n$3\r\nSET\r\n$3\r\ntik\r\n${}\r\n{v}\r\n", v.len());
      let resp = send_and_recv(&mut client, set_cmd.as_bytes()).await?;
      assert_eq!(resp, b"+OK\r\n");
      if i % 8 == 0 {
        sleep(POLL_INTERVAL).await;
      }
    }
    Ok::<_, Error>(())
  });

  let (committed, conflicts) = txn_handle.await.unwrap()?;
  single_handle.await.unwrap()?;

  // 事务结果完整覆盖全部轮次（提交与乐观锁冲突互补，无丢失、无服务端错误）
  assert_eq!(
    committed + conflicts,
    ROUNDS as u32,
    "事务结果应完整覆盖全部轮次"
  );

  // 最终值为某一方的完整写入（事务 tx* 或单写 sg*），无拼接污染
  let mut client = TcpStream::connect(addr).await?;
  sleep(Duration::from_millis(20)).await;
  let mut resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\ntik\r\n").await?;
  // 循环补读直至完整批量回报（防高并发下 TCP 分段导致的半包误判）
  loop {
    let mut complete = false;
    if let Some(cr) = resp.iter().position(|&b| b == b'\r')
      && resp.get(cr + 1) == Some(&b'\n')
      && let Ok(len_str) = from_utf8(&resp[1..cr])
      && let Ok(len) = len_str.parse::<usize>()
    {
      let total = cr + 2 + len + 2;
      if resp.len() >= total {
        complete = true;
        assert!(
          resp[cr + 2..].starts_with(b"tx") || resp[cr + 2..].starts_with(b"sg"),
          "最终值应来自完整写入: {resp:?}"
        );
      }
    }
    if complete {
      break;
    }
    let buf = Vec::with_capacity(DEFAULT_BUF_CAPACITY);
    let BufResult(read_res, returned) = client.read(buf).await;
    let n = read_res?;
    assert!(n > 0, "回报未收齐即遇 EOF: {resp:?}");
    resp.extend_from_slice(&returned[..n]);
  }

  info!("持锁事务与单命令写交叉验证通过");
  OK
}
