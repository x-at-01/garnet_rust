//! 名字空间多租户 / 模块扩展 / 事务语义 / 复制协商 集成测试
//!
//! 覆盖 r2 审查接线清单：
//! 1. NS 0 控制面自动分配（NamespaceAllocator 水位落盘）与租户沙箱隔离
//! 2. 「用户名#空间id」AUTH 凭据点查认证、租户 SETUSER 子账号继承与防提权
//! 3. WATCH inside MULTI 报错不中止；SELECT/SWAPDB 非 0 库切换中止事务
//! 4. MODULE LOAD/UNLOAD/LIST 与未知命令模块路由、COMMAND INFO 聚合
//! 5. PSYNC 同步协商（FULLRESYNC / CONTINUE 增量接续）
//! 6. 阻塞命令 broker 的名字空间全名键隔离

use std::{net::SocketAddr, str::from_utf8, sync::Arc, time::Duration};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
  runtime::spawn,
  time::sleep,
};
use log::info;
use tempfile::{TempDir, tempdir};
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

/// 辅助断言 RESP 批量字符串参数（$len\r\n<bytes>\r\n）
fn resp_bulk(s: &str) -> Vec<u8> {
  let mut out = format!("${}\r\n", s.len()).into_bytes();
  out.extend_from_slice(s.as_bytes());
  out.extend_from_slice(b"\r\n");
  out
}

/// 集成测试脚手架（免密超管视界）
struct NsTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl NsTestFixture {
  async fn setup() -> Result<Self> {
    let dir = tempdir()?;
    let args = ServerArgs {
      port: 0,
      dir: dir.path().to_string_lossy().to_string(),
      quiet: true,
      ..Default::default()
    };
    let server = Arc::new(WedbServer::new(args).await?);
    let addr = server.start().await?;
    Ok(Self {
      server,
      addr,
      _dir: dir,
    })
  }

  async fn connect_client(&self) -> Result<TcpStream> {
    Ok(TcpStream::connect(self.addr).await?)
  }
}

impl Drop for NsTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 NS 0 自动分配、租户沙箱数据隔离与跨空间 AUTH
#[compio::test]
async fn test_namespace_allocation_and_isolation() -> Void {
  info!("开始测试名字空间自动分配与租户隔离");
  let fixture = NsTestFixture::setup().await?;
  let mut admin = fixture.connect_client().await?;

  // 1. 超管经 NS 0 自动分配两个新租户（水位自 2 起单调授予 2、3）
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$2\r\nu1\r\n$2\r\non\r\n$4\r\n>pw1\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$2\r\nu2\r\n$2\r\non\r\n$4\r\n>pw2\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 2. 租户用户必须以「用户名#空间id」登录（凭据完整携带二元身份）：u1 → ns 2，u2 → ns 3
  let mut c1 = fixture.connect_client().await?;
  let resp = send_and_recv(&mut c1, b"*3\r\n$4\r\nAUTH\r\n$4\r\nu1#2\r\n$3\r\npw1\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut c1, b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$2\r\nu1\r\n").await?;
  assert!(
    from_utf8(&resp)?.contains("ns 2\r\n"),
    "u1 应分配 ns 2: {resp:?}"
  );
  let mut c2 = fixture.connect_client().await?;
  let resp = send_and_recv(&mut c2, b"*3\r\n$4\r\nAUTH\r\n$4\r\nu2#3\r\n$3\r\npw2\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut c2, b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$2\r\nu2\r\n").await?;
  assert!(
    from_utf8(&resp)?.contains("ns 3\r\n"),
    "u2 应分配 ns 3: {resp:?}"
  );

  // 2.1 显式 NS N 推进水位后，后续 NS 0 自动分配不得碰撞（advance_to 语义）
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$2\r\nu3\r\n$2\r\non\r\n$4\r\n>pw3\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$2\r\n50\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$2\r\nu4\r\n$2\r\non\r\n$4\r\n>pw4\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let mut c4 = fixture.connect_client().await?;
  let resp = send_and_recv(&mut c4, b"*3\r\n$4\r\nAUTH\r\n$5\r\nu4#51\r\n$3\r\npw4\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut c4, b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$2\r\nu4\r\n").await?;
  assert!(
    from_utf8(&resp)?.contains("ns 51\r\n"),
    "ns 50 之后自动分配应为 51: {resp:?}"
  );

  // 3. 租户用户必须以「用户名#空间id」登录；省略 #2 的纯用户名仅查全局桶，必然 WRONGPASS
  let mut c1 = fixture.connect_client().await?;
  let resp = send_and_recv(&mut c1, b"*3\r\n$4\r\nAUTH\r\n$2\r\nu1\r\n$3\r\npw1\r\n").await?;
  assert!(
    from_utf8(&resp)?.starts_with("-WRONGPASS"),
    "租户用户省略 #空间id 应被拒绝: {resp:?}"
  );
  let resp = send_and_recv(&mut c1, b"*3\r\n$4\r\nAUTH\r\n$4\r\nu1#2\r\n$3\r\npw1\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut c1, b"*2\r\n$3\r\nACL\r\n$6\r\nWHOAMI\r\n").await?;
  assert_eq!(&resp, b"$2\r\nu1\r\n");

  // 3.1 凭据语法非法：回 ERR 而非 WRONGPASS（与口令错误严格区分）
  let resp = send_and_recv(&mut c1, b"*3\r\n$4\r\nAUTH\r\n$4\r\nu1#b\r\n$3\r\npw1\r\n").await?;
  assert!(
    from_utf8(&resp)?.starts_with("-ERR"),
    "非法空间值应回 ERR: {resp:?}"
  );
  let resp = send_and_recv(&mut c1, b"*3\r\n$4\r\nAUTH\r\n$5\r\nu1#+1\r\n$3\r\npw1\r\n").await?;
  assert!(
    from_utf8(&resp)?.starts_with("-ERR"),
    "带符号空间值应回 ERR: {resp:?}"
  );
  // 3.2 命中用户但口令错误 → WRONGPASS
  let resp = send_and_recv(
    &mut c1,
    b"*3\r\n$4\r\nAUTH\r\n$4\r\nu1#2\r\n$7\r\nwrongpw\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.starts_with("-WRONGPASS"),
    "口令错误应回 WRONGPASS: {resp:?}"
  );
  // 3.3 空间id 写错（u1 绑定 ns 2，请求 ns 3）：该沙箱无此用户 → WRONGPASS
  let resp = send_and_recv(&mut c1, b"*3\r\n$4\r\nAUTH\r\n$4\r\nu1#3\r\n$3\r\npw1\r\n").await?;
  assert!(
    from_utf8(&resp)?.starts_with("-WRONGPASS"),
    "跨沙箱凭据应回 WRONGPASS: {resp:?}"
  );
  // 3.4 HELLO AUTH 携带「用户名#空间id」凭据同样走点查认证
  let mut ch = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut ch,
    b"*5\r\n$5\r\nHELLO\r\n$1\r\n3\r\n$4\r\nAUTH\r\n$4\r\nu1#2\r\n$3\r\npw1\r\n",
  )
  .await?;
  // 该实现 HELLO 成功回 14 项数组（server/redis/version/proto/id/mode/role/modules）
  assert!(
    resp.starts_with(b"*14\r\n$6\r\nserver"),
    "HELLO AUTH 成功应回服务器信息数组: {resp:?}"
  );

  // 4. 租户沙箱数据隔离：u1 (ns 2) 写入，u2 (ns 3) 不可见
  let resp = send_and_recv(
    &mut c1,
    b"*3\r\n$3\r\nSET\r\n$5\r\nnskey\r\n$6\r\nfromu1\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let mut c2 = fixture.connect_client().await?;
  let resp = send_and_recv(&mut c2, b"*3\r\n$4\r\nAUTH\r\n$4\r\nu2#3\r\n$3\r\npw2\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut c2, b"*2\r\n$3\r\nGET\r\n$5\r\nnskey\r\n").await?;
  assert_eq!(&resp, b"$-1\r\n", "ns 3 不应看到 ns 2 的键");
  let resp = send_and_recv(&mut c1, b"*2\r\n$3\r\nGET\r\n$5\r\nnskey\r\n").await?;
  assert_eq!(&resp, b"$6\r\nfromu1\r\n");

  // 5. 租户在自己沙箱内创建子账号（不带 ns 规则，强制继承 Some(2)）
  let resp = send_and_recv(
    &mut c1,
    b"*7\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$3\r\nsub\r\n$2\r\non\r\n$6\r\n>subpw\r\n$2\r\n~*\r\n$5\r\n+@all\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(
    &mut c1,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$3\r\nsub\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ns 2\r\n"),
    "租户子账号应继承 ns 2: {resp:?}"
  );

  // 6. 租户声明 ns none / ns <other> 防垂直提权拦截
  let resp = send_and_recv(
    &mut c1,
    b"*5\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$3\r\nsub\r\n$2\r\nns\r\n$4\r\nnone\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("Permission denied"),
    "租户提权应被拦截: {resp:?}"
  );

  // 7. 子账号以 sub#2 登录，被限定在同一沙箱
  let mut c3 = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut c3,
    b"*3\r\n$4\r\nAUTH\r\n$5\r\nsub#2\r\n$5\r\nsubpw\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut c3, b"*2\r\n$3\r\nACL\r\n$6\r\nWHOAMI\r\n").await?;
  assert_eq!(&resp, b"$3\r\nsub\r\n");
  let resp = send_and_recv(
    &mut c3,
    b"*3\r\n$3\r\nSET\r\n$5\r\nnskey\r\n$6\r\nfromsb\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut c1, b"*2\r\n$3\r\nGET\r\n$5\r\nnskey\r\n").await?;
  assert_eq!(&resp, b"$6\r\nfromsb\r\n", "sub 与 u1 同处 ns 2 沙箱");

  // 8. 租户 ACL LIST 为作用域过滤视图（可见 u1 与 sub，不可见 u2）
  let resp = send_and_recv(&mut c1, b"*2\r\n$3\r\nACL\r\n$4\r\nLIST\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(
    s.contains("u1") && s.contains("sub"),
    "沙箱视图应含 u1/sub: {s}"
  );
  assert!(!s.contains("u2"), "沙箱视图不应越权看到 u2: {s}");

  info!("名字空间自动分配与租户隔离测试通过");
  OK
}

/// 测试 WATCH inside MULTI 不中止与 SELECT/SWAPDB 库切换中止语义
#[compio::test]
async fn test_txn_watch_and_db_switch_semantics() -> Void {
  info!("开始测试事务内 WATCH 与库切换语义");
  let fixture = NsTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. WATCH 在 MULTI 事务内：报错但绝不置脏事务，后续命令照常排队执行
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nWATCH\r\n$2\r\ntk\r\n").await?;
  assert_eq!(
    &resp, b"-ERR WATCH inside MULTI is not allowed\r\n",
    "WATCH inside MULTI 应报错: {resp:?}"
  );
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\ntk\r\n$2\r\ntv\r\n").await?;
  assert_eq!(&resp, b"+QUEUED\r\n", "报错后事务不应被中止");
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await?;
  assert_eq!(&resp, b"*1\r\n+OK\r\n", "EXEC 应正常执行而非 EXECABORT");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\ntk\r\n").await?;
  assert_eq!(&resp, b"$2\r\ntv\r\n");

  // 2. SELECT 目标与当前库相同：视为无操作，正常排队执行
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$1\r\n0\r\n").await?;
  assert_eq!(&resp, b"+QUEUED\r\n");
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await?;
  assert_eq!(&resp, b"*1\r\n+OK\r\n");

  // 3. SELECT 非 0 库切换：报错并置脏事务，EXEC 时 EXECABORT
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$1\r\n1\r\n").await?;
  assert_eq!(
    &resp,
    b"-ERR switching databases inside a transaction is not allowed\r\n"
  );
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await?;
  assert_eq!(
    &resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  // 4. SWAPDB 在事务内必然交换库：一律拦截并中止
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*3\r\n$6\r\nSWAPDB\r\n$1\r\n0\r\n$1\r\n1\r\n").await?;
  assert_eq!(
    &resp,
    b"-ERR switching databases inside a transaction is not allowed\r\n"
  );
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await?;
  assert_eq!(
    &resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  // 5. 非 0 库切换在事务外不受影响
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$1\r\n1\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$1\r\n0\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");

  info!("事务内 WATCH 与库切换语义测试通过");
  OK
}

/// 测试 MODULE 生命周期、未知命令模块路由与 COMMAND INFO 聚合
#[compio::test]
async fn test_module_lifecycle_and_routing() -> Void {
  info!("开始测试模块扩展命令生命周期");
  let fixture = NsTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 空注册表
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nMODULE\r\n$4\r\nLIST\r\n").await?;
  assert_eq!(&resp, b"*0\r\n");

  // 2. 加载内建 example 模块
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nMODULE\r\n$4\r\nLOAD\r\n$7\r\nexample\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 3. 重复加载被拒绝
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nMODULE\r\n$4\r\nLOAD\r\n$7\r\nexample\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("already"),
    "重复加载应报已注册: {resp:?}"
  );

  // 4. LIST 输出 [name, version]
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nMODULE\r\n$4\r\nLIST\r\n").await?;
  let expected = {
    let mut v = b"*1\r\n*2\r\n".to_vec();
    v.extend_from_slice(&resp_bulk("example"));
    v.extend_from_slice(b":1\r\n");
    v
  };
  assert_eq!(&resp, &expected, "MODULE LIST 应输出模块元信息: {resp:?}");

  // 5. COMMAND INFO 聚合模块自定义命令元信息
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$7\r\nCOMMAND\r\n$4\r\nINFO\r\n$12\r\nexample.ping\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("*1\r\n*6\r\n"), "INFO 应输出单条 6 元组: {s}");
  assert!(s.contains("example.ping"), "INFO 应含命令名: {s}");
  assert!(s.contains("readonly"), "只读命令应带 readonly 标志: {s}");

  // 6. 未知命令路由命中注册过程：经生产 ModuleApi 透传内建 PING
  let resp = send_and_recv(&mut client, b"*1\r\n$12\r\nexample.ping\r\n").await?;
  assert_eq!(&resp, b"+PONG\r\n", "模块自定义命令应被路由执行: {resp:?}");

  // 7. 卸载后命令成组移除，回退标准 unknown command
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nMODULE\r\n$6\r\nUNLOAD\r\n$7\r\nexample\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*1\r\n$12\r\nexample.ping\r\n").await?;
  assert!(
    from_utf8(&resp)?.contains("unknown command"),
    "卸载后应回退 unknown command: {resp:?}"
  );
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nMODULE\r\n$6\r\nUNLOAD\r\n$7\r\nexample\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("no such module"),
    "重复卸载应报不存在: {resp:?}"
  );

  // 8. 加载未登记内建模块被拒绝
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nMODULE\r\n$4\r\nLOAD\r\n$7\r\nnosuchm\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("不是已注册的内建模块"),
    "未登记模块应被拒绝: {resp:?}"
  );

  info!("模块扩展命令生命周期测试通过");
  OK
}

/// 测试 PSYNC 同步协商：全量基线与增量接续判定
#[compio::test]
async fn test_psync_negotiation() -> Void {
  info!("开始测试 PSYNC 复制协商");
  let fixture = NsTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 先写入数据推进混合日志尾部位点，确保 wal 区间非空
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\npk\r\n$2\r\npv\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 1. 全新副本 (? -1)：判定全量同步，回 +FULLRESYNC <replid> <snapshot_offset>
  let resp = send_and_recv(&mut client, b"*3\r\n$5\r\nPSYNC\r\n$1\r\n?\r\n$2\r\n-1\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("+FULLRESYNC "), "全新副本应判定全量同步: {s}");
  let body = s
    .strip_prefix("+FULLRESYNC ")
    .and_then(|x| x.strip_suffix("\r\n"))
    .ok_or_else(|| aok::anyhow!("FULLRESYNC 回复格式非法: {s}"))?;
  let (replid, offset_str) = body
    .split_once(' ')
    .ok_or_else(|| aok::anyhow!("FULLRESYNC 缺少位点: {s}"))?;
  assert_eq!(replid.len(), 40, "复制编号应为 40 字符: {replid}");
  let tail: u64 = offset_str.parse()?;
  assert!(tail > 0, "快照基线位点应为日志尾部: {tail}");

  // 2. 持正确 replid 且位点在 wal 区间内：判定增量接续 +CONTINUE
  let mut req = b"*3\r\n$5\r\nPSYNC\r\n$40\r\n".to_vec();
  req.extend_from_slice(replid.as_bytes());
  let offset_arg = format!("${}\r\n{}\r\n", offset_str.len(), offset_str);
  req.extend_from_slice(b"\r\n".as_slice());
  req.extend_from_slice(offset_arg.as_bytes());
  let resp = send_and_recv(&mut client, &req).await?;
  let expected = {
    let mut v = b"+CONTINUE ".to_vec();
    v.extend_from_slice(replid.as_bytes());
    v.extend_from_slice(b"\r\n");
    v
  };
  assert_eq!(&resp, &expected, "增量接续应回 +CONTINUE: {resp:?}");

  // 3. replid 不匹配：回退全量同步
  let mut req = b"*3\r\n$5\r\nPSYNC\r\n$40\r\n".to_vec();
  req.extend_from_slice(b"0000000000000000000000000000000000000000");
  req.extend_from_slice(b"\r\n$1\r\n0\r\n");
  let resp = send_and_recv(&mut client, &req).await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("+FULLRESYNC "), "伪造编号应回退全量: {s}");

  info!("PSYNC 复制协商测试通过");
  OK
}

/// 测试阻塞命令 broker 的名字空间全名键隔离：跨租户推送不唤醒，同租户推送唤醒
#[compio::test]
async fn test_blocking_namespace_isolation() -> Void {
  info!("开始测试阻塞命令名字空间隔离");
  let fixture = NsTestFixture::setup().await?;
  let mut admin = fixture.connect_client().await?;

  // 建立两个隔离租户（复用 NS 0 自动分配 → ns 2 / ns 3；登录凭据须携带 #空间id）
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$2\r\nu1\r\n$2\r\non\r\n$4\r\n>pw1\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$2\r\nu2\r\n$2\r\non\r\n$4\r\n>pw2\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  let mut waiter = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut waiter,
    b"*3\r\n$4\r\nAUTH\r\n$4\r\nu1#2\r\n$3\r\npw1\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let mut alien = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut alien,
    b"*3\r\n$4\r\nAUTH\r\n$4\r\nu2#3\r\n$3\r\npw2\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let mut peer = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut peer,
    b"*3\r\n$4\r\nAUTH\r\n$4\r\nu1#2\r\n$3\r\npw1\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 1. u1 (ns 2) 发起 BLPOP 阻塞等待
  let waiter_handle = spawn(async move {
    let resp = send_and_recv(
      &mut waiter,
      b"*3\r\n$5\r\nBLPOP\r\n$6\r\nbqueue\r\n$1\r\n5\r\n",
    )
    .await?;
    aok::Result::<Vec<u8>>::Ok(resp)
  });
  sleep(Duration::from_millis(100)).await;

  // 2. u2 (ns 3) 推送同名键：全名键不同，绝不唤醒，也不应被跨空间消费
  let resp = send_and_recv(
    &mut alien,
    b"*3\r\n$5\r\nRPUSH\r\n$6\r\nbqueue\r\n$6\r\nalien!\r\n",
  )
  .await?;
  assert_eq!(&resp, b":1\r\n");
  sleep(Duration::from_millis(200)).await;

  // 3. u1 同租户 peer 推送：立即唤醒等待者
  let resp = send_and_recv(
    &mut peer,
    b"*3\r\n$5\r\nRPUSH\r\n$6\r\nbqueue\r\n$4\r\npeer\r\n",
  )
  .await?;
  assert_eq!(&resp, b":1\r\n");

  // 4. 等待者被唤醒且只消费同租户元素
  let waiter_resp = waiter_handle.await.unwrap()?;
  let s = from_utf8(&waiter_resp)?;
  assert!(s.contains("peer"), "BLPOP 应收到同租户元素: {s}");
  assert!(!s.contains("alien!"), "跨租户元素绝不可见: {s}");

  // 5. 跨租户数据仍完整留在 u2 沙箱内
  let resp = send_and_recv(&mut alien, b"*2\r\n$4\r\nLLEN\r\n$6\r\nbqueue\r\n").await?;
  assert_eq!(&resp, b":1\r\n", "u2 沙箱内的元素不应被 u1 消费");

  info!("阻塞命令名字空间隔离测试通过");
  OK
}

/// 测试租户创建用户自动绑定当前名字空间、显式声明自身 ns 不推进水位，以及越权 ns 拦截与水位防护
#[compio::test]
async fn test_tenant_setuser_namespace_protection_and_inheritance() -> Void {
  info!("开始测试租户 SETUSER 名字空间保护与自动继承");
  let fixture = NsTestFixture::setup().await?;
  let mut admin = fixture.connect_client().await?;

  // 0. 超管使用 ns 0 自动分配首个租户（水位变为 2）
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$2\r\nt1\r\n$2\r\non\r\n$4\r\n>pw1\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  let mut t1_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut t1_client,
    b"*3\r\n$4\r\nAUTH\r\n$4\r\nt1#2\r\n$3\r\npw1\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut t1_client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$2\r\nt1\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ns 2\r\n"),
    "t1 必须处于 ns 2 沙箱: {resp:?}"
  );

  // 1. 租户显式声明 ns 2（自身沙箱）成功，且验证分配器水位未被推进
  let resp = send_and_recv(
    &mut t1_client,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$6\r\nsub_ex\r\n$2\r\non\r\n$4\r\n>pwd\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n2\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 探针验证：超管经 ns 0 自动分配，若水位未被推进，分出的应当是 3
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$7\r\nprobe_1\r\n$2\r\non\r\n$4\r\n>pwd\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let mut p1_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut p1_client,
    b"*3\r\n$4\r\nAUTH\r\n$9\r\nprobe_1#3\r\n$3\r\npwd\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(
    &mut p1_client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$7\r\nprobe_1\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ns 3\r\n"),
    "probe_1 必须分配为 ns 3，证明 ns 2 显式声明未推进水位: {resp:?}"
  );

  // 2. 租户声明 ns 3、ns 99、ns none、ns all、ns 0、非法字符串等均被拦截拒绝，且验证分配器水位未被推进
  let deny_rules: &[&[&str]] = &[
    &["ns", "3"],
    &["ns", "99"],
    &["ns", "none"],
    &["ns", "all"],
    &["ns", "0"],
    &["ns", "invalid_ns"],
  ];
  for rule in deny_rules {
    let mut cmd = vec![
      "*5\r\n".to_string(),
      "$3\r\nACL\r\n".to_string(),
      "$7\r\nSETUSER\r\n".to_string(),
      "$8\r\nsub_deny\r\n".to_string(),
    ];
    for part in *rule {
      cmd.push(format!("${}\r\n{}\r\n", part.len(), part));
    }
    let req = cmd.concat().into_bytes();
    let resp = send_and_recv(&mut t1_client, &req).await?;
    let s = from_utf8(&resp)?;
    assert!(
      s.contains("Permission denied: cannot grant namespace outside of the current tenant scope"),
      "租户设置 {rule:?} 应被拦截拒绝: {s}"
    );
  }

  // 探针验证：超管再次 ns 0 自动分配，水位必须紧接着 3 为 4（绝未被 ns 99 推进至 100）
  let resp = send_and_recv(
    &mut admin,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$7\r\nprobe_2\r\n$2\r\non\r\n$4\r\n>pwd\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let mut p2_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut p2_client,
    b"*3\r\n$4\r\nAUTH\r\n$9\r\nprobe_2#4\r\n$3\r\npwd\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(
    &mut p2_client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$7\r\nprobe_2\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ns 4\r\n"),
    "probe_2 必须分配为 ns 4，证明越权 ns 绝对未推进水位: {resp:?}"
  );

  // 3. 租户未声明 ns 创建子账号，验证自动绑定为 ns 2
  let resp = send_and_recv(
    &mut t1_client,
    b"*7\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$8\r\nsub_auto\r\n$2\r\non\r\n$7\r\n>autopw\r\n$2\r\n~*\r\n$5\r\n+@all\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut t1_client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$8\r\nsub_auto\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ns 2\r\n"),
    "sub_auto 必须自动绑定为 ns 2: {resp:?}"
  );

  // 子账号登录校验
  let mut sub_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut sub_client,
    b"*3\r\n$4\r\nAUTH\r\n$10\r\nsub_auto#2\r\n$6\r\nautopw\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  info!("租户 SETUSER 名字空间保护与自动继承测试通过");
  OK
}

/// 测试超管未指定 ns 创建用户时，默认就是超级用户（namespace: None，全局视界）
#[compio::test]
async fn test_admin_setuser_default_superuser() -> Void {
  info!("开始测试超管未指定 ns 创建用户默认超级用户");
  let fixture = NsTestFixture::setup().await?;
  let mut admin = fixture.connect_client().await?;

  // 1. 超管未指定 ns 创建用户 super_ops
  let resp = send_and_recv(
    &mut admin,
    b"*7\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$9\r\nsuper_ops\r\n$2\r\non\r\n$8\r\n>superpw\r\n$2\r\n~*\r\n$5\r\n+@all\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 2. 超管 GETUSER super_ops 验证：不含 "ns " 规则标识
  let resp = send_and_recv(
    &mut admin,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$9\r\nsuper_ops\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(
    !resp_str.contains("ns "),
    "超管创建的默认超级用户在 GETUSER 中不应包含 ns 规则: {resp_str}"
  );

  // 3. 使用 super_ops 进行 AUTH 认证，验证具备超管全局视界
  let mut super_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut super_client,
    b"*3\r\n$4\r\nAUTH\r\n$9\r\nsuper_ops\r\n$7\r\nsuperpw\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // WHOAMI 返回 super_ops
  let resp = send_and_recv(&mut super_client, b"*2\r\n$3\r\nACL\r\n$6\r\nWHOAMI\r\n").await?;
  assert_eq!(&resp, b"$9\r\nsuper_ops\r\n");

  // 4. super_client 作为超级用户，能够通过 ACL SETUSER 为其他用户指定任意 ns（具备超管跨租户控制权限）
  let resp = send_and_recv(
    &mut super_client,
    b"*9\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$2\r\nt2\r\n$2\r\non\r\n$4\r\n>pw2\r\n$2\r\n~*\r\n$5\r\n+@all\r\n$2\r\nns\r\n$2\r\n10\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 5. t2 认证登录进入 ns 10 沙箱，校验自身 GETUSER 包含 ns 10
  let mut t2_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut t2_client,
    b"*3\r\n$4\r\nAUTH\r\n$5\r\nt2#10\r\n$3\r\npw2\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut t2_client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$2\r\nt2\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ns 10\r\n"),
    "t2 自视界应查得 ns 10: {resp:?}"
  );

  // 6. 当前用户有 ns 时（t2 在 ns 10），未指定 ns 时默认是当前 ns
  let resp = send_and_recv(
    &mut t2_client,
    b"*7\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$6\r\nt2_sub\r\n$2\r\non\r\n$6\r\n>subpw\r\n$2\r\n~*\r\n$5\r\n+@all\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut t2_client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$6\r\nt2_sub\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ns 10\r\n"),
    "t2 创建未指定 ns 的子账号应默认继承当前 ns 10: {resp:?}"
  );

  // 7. t2_sub 可以正常登录认证
  let mut sub_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut sub_client,
    b"*3\r\n$4\r\nAUTH\r\n$9\r\nt2_sub#10\r\n$5\r\nsubpw\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  info!("超管未指定 ns 默认超级用户与租户未指定 ns 默认当前空间测试通过");
  OK
}
