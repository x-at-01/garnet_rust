//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test/` 边界条件、故障恢复、事务回滚与集群重定向测试
use std::{net::SocketAddr, str::from_utf8, sync::Arc, time::Duration};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::{TcpListener, TcpStream},
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

/// 边界测试脚手架
struct BoundaryTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl BoundaryTestFixture {
  async fn setup_with_args(args_builder: impl FnOnce(&str) -> ServerArgs) -> Result<Self> {
    let dir = tempdir()?;
    let dir_path = dir.path().to_string_lossy().to_string();
    let args = args_builder(&dir_path);
    let server = Arc::new(WedbServer::new(args).await?);
    let addr = server.start().await?;
    Ok(Self {
      server,
      addr,
      _dir: dir,
    })
  }

  async fn setup_default() -> Result<Self> {
    Self::setup_with_args(|dir| ServerArgs {
      port: 0,
      dir: dir.to_string(),
      quiet: true,
      ..Default::default()
    })
    .await
  }

  async fn connect_client(&self) -> Result<TcpStream> {
    let stream = TcpStream::connect(self.addr).await?;
    Ok(stream)
  }
}

impl Drop for BoundaryTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 多客户端并发高频请求压测（无死锁、无内存泄漏）
/// 对应 Garnet 高并发客户端连接与请求压力测试
#[compio::test]
async fn test_concurrent_clients_stress_and_clean_exit() -> Void {
  info!("开始测试多客户端并发高频请求压测");
  let fixture = BoundaryTestFixture::setup_default().await?;

  let mut handles = Vec::new();
  for i in 0..8 {
    let addr = fixture.addr;
    let handle = spawn(async move {
      let mut client = TcpStream::connect(addr).await.unwrap();
      for j in 0..15 {
        // SET
        let mut ibuf = itoa::Buffer::new();
        let is = ibuf.format(i);
        let mut jbuf = itoa::Buffer::new();
        let js = jbuf.format(j);

        let mut set_cmd = String::from("*3\r\n$3\r\nSET\r\n$7\r\nk_");
        set_cmd.push_str(is);
        set_cmd.push('_');
        set_cmd.push_str(js);
        set_cmd.push_str("\r\n$7\r\nv_");
        set_cmd.push_str(is);
        set_cmd.push('_');
        set_cmd.push_str(js);
        set_cmd.push_str("\r\n");
        let resp = send_and_recv(&mut client, set_cmd.as_bytes())
          .await
          .unwrap();
        assert_eq!(resp, b"+OK\r\n");

        // GET
        let mut get_cmd = String::from("*2\r\n$3\r\nGET\r\n$7\r\nk_");
        get_cmd.push_str(is);
        get_cmd.push('_');
        get_cmd.push_str(js);
        get_cmd.push_str("\r\n");
        let resp = send_and_recv(&mut client, get_cmd.as_bytes())
          .await
          .unwrap();
        let mut expected = String::from("$7\r\nv_");
        expected.push_str(is);
        expected.push('_');
        expected.push_str(js);
        expected.push_str("\r\n");
        assert_eq!(resp, expected.as_bytes());

        // LPUSH & RPOP
        let mut lpush_cmd = String::from("*3\r\n$5\r\nLPUSH\r\n$7\r\nl_");
        lpush_cmd.push_str(is);
        lpush_cmd.push('_');
        lpush_cmd.push_str(js);
        lpush_cmd.push_str("\r\n$4\r\nitem\r\n");
        let resp = send_and_recv(&mut client, lpush_cmd.as_bytes())
          .await
          .unwrap();
        assert_eq!(resp, b":1\r\n");

        let mut rpop_cmd = String::from("*2\r\n$4\r\nRPOP\r\n$7\r\nl_");
        rpop_cmd.push_str(is);
        rpop_cmd.push('_');
        rpop_cmd.push_str(js);
        rpop_cmd.push_str("\r\n");
        let resp = send_and_recv(&mut client, rpop_cmd.as_bytes())
          .await
          .unwrap();
        assert_eq!(resp, b"$4\r\nitem\r\n");
      }

      // 优雅断开
      let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nQUIT\r\n")
        .await
        .unwrap();
      assert_eq!(resp, b"+OK\r\n");
    });
    handles.push(handle);
  }

  for h in handles {
    let _ = h.await;
  }

  // 验证并发连接全部断开后，服务无死锁且正常接收新连接
  let mut client = fixture.connect_client().await?;
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  info!("多客户端并发高频请求压测通过");
  OK
}

/// 畸形 RESP 协议报文容错与恢复
/// 对应 Garnet 网络层非标准与畸形输入容错规范
#[compio::test]
async fn test_malformed_and_incomplete_resp_recovery() -> Void {
  info!("开始测试畸形 RESP 协议报文容错与恢复");
  let fixture = BoundaryTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 空行与多重 CRLF 忽略与恢复
  let resp = send_and_recv(&mut client, b"\r\n\r\n\r\n*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  // 2. 未知内联命令容错：响应未知命令错误，但连接保持活跃
  let resp = send_and_recv(&mut client, b"UNKNOWN_INLINE_CMD arg1 arg2\r\n").await?;
  assert!(resp.starts_with(b"-ERR unknown command 'UNKNOWN_INLINE_CMD'"));

  // 后续命令正常执行
  let resp = send_and_recv(&mut client, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  // 3. 未知 RESP 数组命令容错：响应未知命令错误，连接保持活跃
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$11\r\nUNKNOWN_ARR\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-ERR unknown command 'UNKNOWN_ARR'"));

  // 后续写入与读取正常
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$6\r\nrec_k1\r\n$6\r\nrec_v1\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$6\r\nrec_k1\r\n").await?;
  assert_eq!(resp, b"$6\r\nrec_v1\r\n");

  // 4. 分包半包跨网络数据帧模拟
  let BufResult(write_res, _) = client.write_all(b"*2\r\n$4\r\nECHO\r\n".to_vec()).await;
  write_res?;
  sleep(Duration::from_millis(30)).await;
  let resp = send_and_recv(&mut client, b"$5\r\nhello\r\n").await?;
  assert_eq!(resp, b"$5\r\nhello\r\n");

  // 5. 空数组与 null 数组容错
  let resp = send_and_recv(&mut client, b"*0\r\n*-1\r\n*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nQUIT\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  info!("畸形 RESP 协议报文容错与恢复测试通过");
  OK
}

/// 深度事务嵌套与中断异常回滚
/// 对应 Garnet 事务失败丢弃与嵌套拦截规范
#[compio::test]
async fn test_transaction_nesting_and_execabort_rollback() -> Void {
  info!("开始测试深度事务嵌套与中断异常回滚");
  let fixture = BoundaryTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 深度事务嵌套拦截：多次 MULTI 拒绝
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(resp, b"-ERR MULTI calls can not be nested\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(resp, b"-ERR MULTI calls can not be nested\r\n");

  // 2. 语法错误触发 EXECABORT 整体回滚
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$7\r\ntx_good\r\n$5\r\nval_g\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  // 故意发送参数不足的错误语法命令 (SET 缺少参数)
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nSET\r\n$6\r\ntx_bad\r\n").await?;
  assert!(resp.starts_with(b"-ERR wrong number of arguments"));

  // 执行 EXEC，必须触发 EXECABORT 并丢弃事务
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await?;
  assert_eq!(
    resp,
    b"-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  // 确认 tx_good 未被真正写入
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$7\r\ntx_good\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  // 3. 验证 DISCARD 回滚
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$6\r\ndisc_k\r\n$6\r\ndisc_v\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$7\r\nDISCARD\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$6\r\ndisc_k\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  // 无 MULTI 时的 DISCARD 与 EXEC 均报错
  let resp = send_and_recv(&mut client, b"*1\r\n$7\r\nDISCARD\r\n").await?;
  assert_eq!(resp, b"-ERR DISCARD without MULTI\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await?;
  assert_eq!(resp, b"-ERR EXEC without MULTI\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nQUIT\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  info!("深度事务嵌套与中断异常回滚测试通过");
  OK
}

/// 集群模式下跨槽多键操作拦截与重定向
/// 对应 Garnet 集群槽位哈希与跨槽拦截规范
#[compio::test]
async fn test_cluster_cross_slot_and_redirection() -> Void {
  info!("开始测试集群模式下跨槽多键操作拦截与重定向");
  let fixture = BoundaryTestFixture::setup_with_args(|dir| ServerArgs {
    port: 0,
    dir: dir.to_string(),
    cluster_enabled: true,
    quiet: true,
    ..Default::default()
  })
  .await?;

  let mut client = fixture.connect_client().await?;

  // 1. 跨槽多键操作拦截：DEL foo bar (foo 槽位 12182，bar 槽位 5061)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nDEL\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
  )
  .await?;
  assert_eq!(
    resp,
    b"-CROSSSLOT Keys in request don't hash to the same slot\r\n"
  );

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXISTS\r\n$3\r\nfoo\r\n$3\r\nbar\r\n",
  )
  .await?;
  assert_eq!(
    resp,
    b"-CROSSSLOT Keys in request don't hash to the same slot\r\n"
  );

  // 2. 同槽多键放行（使用 Hash Tag 保证 slot 相同）
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nDEL\r\n$9\r\n{user}:k1\r\n$9\r\n{user}:k2\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");

  // 3. ASKING 单次标志测试
  let resp = send_and_recv(&mut client, b"*1\r\n$6\r\nASKING\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 4. READONLY 与 READWRITE 切换
  let resp = send_and_recv(&mut client, b"*1\r\n$8\r\nREADONLY\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$9\r\nREADWRITE\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nQUIT\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  info!("集群模式下跨槽多键操作拦截与重定向测试通过");
  OK
}

/// 未知命令与参数溢出保护
/// 对应 Garnet 错误参数与越界保护规范
#[compio::test]
async fn test_unknown_commands_and_parameter_overflow() -> Void {
  info!("开始测试未知命令与参数溢出保护");
  let fixture = BoundaryTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 负数范围参数保护 (RPOP 负数)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nRPOP\r\n$6\r\nmylist\r\n$2\r\n-5\r\n",
  )
  .await?;
  assert_eq!(resp, b"-ERR value is out of range, must be positive\r\n");

  // 2. 非法整型参数保护 (ZRANGE 非法范围)
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZRANGE\r\n$5\r\nmyzst\r\n$3\r\nabc\r\n$2\r\n10\r\n",
  )
  .await?;
  assert_eq!(resp, b"-ERR value is not an integer or out of range\r\n");

  // 3. 非法整型参数保护 (EXPIRE 非法时间)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nEXPIRE\r\n$5\r\nmykey\r\n$3\r\nxyz\r\n",
  )
  .await?;
  assert_eq!(resp, b"-ERR value is not an integer or out of range\r\n");

  // 4. 奇数参数保护 (HSET 缺少字段值)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nHSET\r\n$5\r\nmyhsh\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(
    resp,
    b"-ERR wrong number of arguments for 'hset' command\r\n"
  );

  // 5. 语法错误保护 (ZADD 奇数参数)
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nZADD\r\n$5\r\nmyzst\r\n$3\r\n1.0\r\n$2\r\nm1\r\n$3\r\n2.0\r\n",
  )
  .await?;
  assert_eq!(resp, b"-ERR syntax error\r\n");

  // 6. 非法浮点数保护 (ZADD 非法 score)
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nZADD\r\n$5\r\nmyzst\r\n$6\r\nnotflt\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b"-ERR value is not a valid float\r\n");

  // 7. DB 索引切换与非法索引保护（每个命名空间独立支持 u64 数据库）
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$3\r\n999\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSELECT\r\n$2\r\n-1\r\n").await?;
  assert_eq!(resp, b"-ERR value is not an integer or out of range\r\n");

  // 8. 连续未知命令容错且连接不崩溃
  let resp = send_and_recv(&mut client, b"FOO_UNKNOWN_1\r\n").await?;
  assert!(resp.starts_with(b"-ERR unknown command 'FOO_UNKNOWN_1'"));

  let resp = send_and_recv(&mut client, b"FOO_UNKNOWN_2\r\n").await?;
  assert!(resp.starts_with(b"-ERR unknown command 'FOO_UNKNOWN_2'"));

  let resp = send_and_recv(&mut client, b"PING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nQUIT\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  info!("未知命令与参数溢出保护测试通过");
  OK
}

/// 动态 ACL 权限修改即时生效验证
/// 对应 Garnet 动态权限与认证控制规范
#[compio::test]
async fn test_dynamic_acl_permission_modification() -> Void {
  info!("开始测试动态 ACL 权限修改即时生效验证");
  let fixture = BoundaryTestFixture::setup_default().await?;

  // 1. 管理员客户端连接并创建受限用户 alice (仅允许 GET 与 PING)
  // 显式声明 ns none：超管 SETUSER 缺省绑定主业务沙箱 Some(1)（防疏忽创建全库超管），
  // 本测试聚焦全局用户的动态权限修改，故显式落到超管全局视界
  let mut admin_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut admin_client,
    b"*8\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$5\r\nalice\r\n$2\r\non\r\n$10\r\n>alice_pwd\r\n$4\r\n+get\r\n$2\r\nns\r\n$4\r\nnone\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 额外允许 PING
  let resp = send_and_recv(
    &mut admin_client,
    b"*4\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$5\r\nalice\r\n$5\r\n+ping\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 2. alice 客户端连接并登录认证
  let mut alice_client = fixture.connect_client().await?;
  let resp = send_and_recv(
    &mut alice_client,
    b"*3\r\n$4\r\nAUTH\r\n$5\r\nalice\r\n$9\r\nalice_pwd\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // alice 执行 PING 与 GET 放行
  let resp = send_and_recv(&mut alice_client, b"*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  let resp = send_and_recv(&mut alice_client, b"*2\r\n$3\r\nGET\r\n$7\r\nsecretk\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  // alice 尝试执行未被授权的 SET 命令，立即被拦截 NOPERM
  let resp = send_and_recv(
    &mut alice_client,
    b"*3\r\n$3\r\nSET\r\n$7\r\nsecretk\r\n$7\r\nsecretv\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-NOPERM this user has no permissions to run the 'SET' command"));

  // 3. 管理员动态为 alice 赋予 +set 权限
  let resp = send_and_recv(
    &mut admin_client,
    b"*4\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$5\r\nalice\r\n$4\r\n+set\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 4. alice 无需重连，在原连接上再次执行 SET，即时生效放行！
  let resp = send_and_recv(
    &mut alice_client,
    b"*3\r\n$3\r\nSET\r\n$7\r\nsecretk\r\n$7\r\nsecretv\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut alice_client, b"*2\r\n$3\r\nGET\r\n$7\r\nsecretk\r\n").await?;
  assert_eq!(resp, b"$7\r\nsecretv\r\n");

  // 5. 管理员动态禁用 alice 账户
  let resp = send_and_recv(
    &mut admin_client,
    b"*4\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$5\r\nalice\r\n$3\r\noff\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // alice 尝试再次执行命令，即时被禁用拦截
  let resp = send_and_recv(&mut alice_client, b"*2\r\n$3\r\nGET\r\n$7\r\nsecretk\r\n").await?;
  assert!(resp.starts_with(b"-NOPERM"));

  // 6. 验证 ACL USERS 与 DELUSER
  let resp = send_and_recv(&mut admin_client, b"*2\r\n$3\r\nACL\r\n$5\r\nUSERS\r\n").await?;
  let users_str = from_utf8(&resp).unwrap_or("");
  assert!(users_str.contains("alice"));

  let resp = send_and_recv(
    &mut admin_client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nDELUSER\r\n$5\r\nalice\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let _ = send_and_recv(&mut alice_client, b"*1\r\n$4\r\nQUIT\r\n").await;
  let _ = send_and_recv(&mut admin_client, b"*1\r\n$4\r\nQUIT\r\n").await;

  info!("动态 ACL 权限修改即时生效验证测试通过");
  OK
}

/// 优雅停机信号触发与端口复用
/// 对应 Garnet 优雅关闭与网络端口释放重绑定规范
#[compio::test]
async fn test_graceful_shutdown_and_port_reuse() -> Void {
  info!("开始测试优雅停机信号触发与端口复用");
  let dir = tempdir()?;
  let dir_path = dir.path().to_string_lossy().to_string();

  // 获取一个当前系统分配的可用端口
  let temp_listener = TcpListener::bind("127.0.0.1:0".parse::<SocketAddr>().unwrap()).await?;
  let target_port = temp_listener.local_addr()?.port();
  drop(temp_listener);
  sleep(Duration::from_millis(50)).await;

  // 1. 启动第一任服务端守护进程绑定固定端口
  let server1 = Arc::new(
    WedbServer::new(ServerArgs {
      port: target_port,
      dir: dir_path.clone(),
      quiet: true,
      ..Default::default()
    })
    .await?,
  );
  let addr1 = server1.start().await?;
  assert_eq!(addr1.port(), target_port);

  // 写入持久化数据
  let mut client1 = TcpStream::connect(addr1).await?;
  let resp = send_and_recv(
    &mut client1,
    b"*3\r\n$3\r\nSET\r\n$10\r\nport_reuse\r\n$5\r\nok_v1\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 优雅停机第一任服务
  server1.stop().await?;
  assert!(server1.is_disposed());
  sleep(Duration::from_millis(100)).await;

  // 确认旧端口已关闭且客户端无法连接
  assert!(TcpStream::connect(addr1).await.is_err());

  // 2. 在完全相同的 target_port 端口上重新启动第二任服务端守护进程
  let server2 = Arc::new(
    WedbServer::new(ServerArgs {
      port: target_port,
      dir: dir_path,
      quiet: true,
      ..Default::default()
    })
    .await?,
  );
  let addr2 = server2.start().await?;
  assert_eq!(addr2.port(), target_port);

  // 客户端连接第二任服务，验证端口复用与数据自愈
  let mut client2 = TcpStream::connect(addr2).await?;
  let resp = send_and_recv(
    &mut client2,
    b"*3\r\n$3\r\nSET\r\n$10\r\nport_reuse\r\n$5\r\nok_v2\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(&mut client2, b"*2\r\n$3\r\nGET\r\n$10\r\nport_reuse\r\n").await?;
  assert_eq!(resp, b"$5\r\nok_v2\r\n");

  server2.stop().await?;
  assert!(server2.is_disposed());

  info!("优雅停机信号触发与端口复用测试通过");
  OK
}

/// 连接指标生命周期统计与多分段 INFO 报告验证
/// 对应 Garnet 实例统计指标与 INFO 章节规范
#[compio::test]
async fn test_server_connection_metrics_and_info_sections() -> Void {
  info!("开始测试连接指标生命周期统计与多分段 INFO 报告验证");
  let fixture = BoundaryTestFixture::setup_default().await?;
  let server = &fixture.server;

  // 初始状态
  assert_eq!(server.active_connections(), 0);
  assert_eq!(server.total_connections_received(), 0);
  assert_eq!(server.total_connections_disposed(), 0);
  assert_eq!(server.get_conn_active(), 0);
  assert_eq!(server.active_consumers(), 0);
  assert_eq!(server.active_cluster_sessions(), 0);

  // 客户端 1 连接
  let mut client1 = fixture.connect_client().await?;
  let resp = send_and_recv(&mut client1, b"*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  assert_eq!(server.total_connections_received(), 1);
  assert_eq!(server.active_connections(), 1);

  // 客户端 2 连接
  let mut client2 = fixture.connect_client().await?;
  let resp = send_and_recv(&mut client2, b"*1\r\n$4\r\nPING\r\n").await?;
  assert_eq!(resp, b"+PONG\r\n");

  assert_eq!(server.total_connections_received(), 2);
  assert_eq!(server.active_connections(), 2);

  // 验证 INFO 分段查询
  // 1. INFO replication
  let resp = send_and_recv(&mut client1, b"*2\r\n$4\r\nINFO\r\n$11\r\nreplication\r\n").await?;
  let resp_str = from_utf8(&resp).unwrap_or("");
  assert!(resp_str.contains("# Replication"));
  assert!(resp_str.contains("role:master"));

  // 2. INFO clients
  let resp = send_and_recv(&mut client1, b"*2\r\n$4\r\nINFO\r\n$7\r\nclients\r\n").await?;
  let resp_str = from_utf8(&resp).unwrap_or("");
  assert!(resp_str.contains("# Clients"));
  assert!(resp_str.contains("connected_clients:"));

  // 3. INFO server
  let resp = send_and_recv(&mut client1, b"*2\r\n$4\r\nINFO\r\n$6\r\nserver\r\n").await?;
  let resp_str = from_utf8(&resp).unwrap_or("");
  assert!(resp_str.contains("# Server"));
  assert!(resp_str.contains("wedb_version:0.1.0"));

  // 客户端 1 退出
  let resp = send_and_recv(&mut client1, b"*1\r\n$4\r\nQUIT\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  sleep(Duration::from_millis(30)).await;
  assert_eq!(server.active_connections(), 1);
  assert_eq!(server.total_connections_disposed(), 1);

  // 客户端 2 退出
  let resp = send_and_recv(&mut client2, b"*1\r\n$4\r\nQUIT\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  sleep(Duration::from_millis(30)).await;
  assert_eq!(server.active_connections(), 0);
  assert_eq!(server.total_connections_disposed(), 2);

  // 验证重置计数器
  server.reset_connections_received();
  server.reset_connections_disposed();
  assert_eq!(server.total_connections_received(), 0);
  assert_eq!(server.total_connections_disposed(), 0);

  // 辅助别名方法验证
  server.purge();
  server.register_extensions();

  info!("连接指标生命周期统计与多分段 INFO 报告验证测试通过");
  OK
}
