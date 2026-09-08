//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test/RespAdminCommandsTests.cs` (CLIENT, CONFIG, COMMAND, ROLE, HELLO, SAVE 等管理命令)
use std::{net::SocketAddr, str::from_utf8, sync::Arc};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
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

/// 管理命令测试脚手架
struct AdminTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl AdminTestFixture {
  async fn setup() -> Result<Self> {
    let dir = tempdir()?;
    let dir_path = dir.path().to_string_lossy().to_string();
    let args = ServerArgs {
      port: 0,
      dir: dir_path,
      quiet: true,
      // 钉小内存预算：自适应配置按物理内存给 16GB 级页面/索引分配，
      // 全量套件并发时初始化即耗时 30s+ 且命令处理内存抖动饿死（曾致 180s 超时）
      store_memory_budget: Some(64 * 1024 * 1024),
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
    let stream = TcpStream::connect(self.addr).await?;
    Ok(stream)
  }
}

impl Drop for AdminTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 CLIENT, CONFIG, COMMAND 子命令路由、执行与帮助提示
/// 对应 Garnet RespAdminCommandsTests.cs 中的客户端和配置管理测试
#[compio::test]
async fn test_subcommands_routing_and_execution() -> Void {
  info!("开始测试 CLIENT / CONFIG / COMMAND 子命令路由与参数校验");
  let fixture = AdminTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. CLIENT ID
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nCLIENT\r\n$2\r\nID\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with(':'), "CLIENT ID 响应异常: {s}");

  // 2. CLIENT GETNAME
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nCLIENT\r\n$7\r\nGETNAME\r\n").await?;
  assert_eq!(&resp, b"$-1\r\n");

  // 3. CLIENT SETNAME
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nCLIENT\r\n$7\r\nSETNAME\r\n$6\r\nmy-cli\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 4. CLIENT INFO
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nCLIENT\r\n$4\r\nINFO\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(s.contains("id="), "CLIENT INFO 应包含 id 字段: {s}");

  // 5. CONFIG GET *
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nCONFIG\r\n$3\r\nGET\r\n$1\r\n*\r\n",
  )
  .await?;
  assert_eq!(&resp, b"*0\r\n");

  // 6. CONFIG SET maxmemory 100M
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nCONFIG\r\n$3\r\nSET\r\n$9\r\nmaxmemory\r\n$4\r\n100M\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 7. COMMAND, COMMAND COUNT, COMMAND DOCS
  let resp = send_and_recv(&mut client, b"*1\r\n$7\r\nCOMMAND\r\n").await?;
  assert_eq!(&resp, b"*0\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nCOMMAND\r\n$5\r\nCOUNT\r\n").await?;
  assert_eq!(&resp, b":368\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nCOMMAND\r\n$4\r\nDOCS\r\n").await?;
  assert_eq!(&resp, b"*0\r\n");

  // 8. 未知子命令拦截与帮助提示（对齐 Garnet 标准错误响应）
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nCLIENT\r\n$6\r\nFOOBAR\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(
    s.contains("Unknown subcommand") && s.contains("CLIENT HELP"),
    "CLIENT 未知子命令应报错并提示 HELP: {s}"
  );

  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nCONFIG\r\n$6\r\nFOOBAR\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(
    s.contains("Unknown subcommand") && s.contains("CONFIG HELP"),
    "CONFIG 未知子命令应报错并提示 HELP: {s}"
  );

  let resp = send_and_recv(&mut client, b"*2\r\n$7\r\nCOMMAND\r\n$6\r\nFOOBAR\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(
    s.contains("Unknown subcommand") && s.contains("COMMAND HELP"),
    "COMMAND 未知子命令应报错并提示 HELP: {s}"
  );

  // 9. CONFIG SET 奇数参数报错
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nCONFIG\r\n$3\r\nSET\r\n$9\r\nmaxmemory\r\n",
  )
  .await?;
  assert!(
    from_utf8(&resp)?.contains("ERR wrong number of arguments for 'config|set' command"),
    "CONFIG SET 奇数参数未报错: {:?}",
    from_utf8(&resp)
  );

  info!("CLIENT / CONFIG / COMMAND 测试通过");
  OK
}

/// 测试 ROLE, HELLO, SAVE, BGSAVE, LASTSAVE 等服务端管理命令
/// 对应 Garnet RespAdminCommandsTests.cs 中的系统级管理命令测试
#[compio::test]
async fn test_admin_system_commands() -> Void {
  info!("开始测试系统管理命令 (ROLE, HELLO, SAVE, BGSAVE, LASTSAVE)");
  let fixture = AdminTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. ROLE
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nROLE\r\n").await?;
  assert_eq!(resp, b"*3\r\n$6\r\nmaster\r\n:0\r\n*0\r\n");

  // 2. HELLO (默认协议协商)
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nHELLO\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(s.contains("server"), "HELLO 响应应包含 server 字段: {s}");
  assert!(s.contains("standalone"), "HELLO 响应应包含 standalone: {s}");
  assert!(s.contains("master"), "HELLO 响应应包含 master: {s}");

  // 3. HELLO 3 (RESP3 协议切换)
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nHELLO\r\n$1\r\n3\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(s.contains(":3\r\n"), "HELLO 3 应返回 proto 3: {s}");

  // 4. SAVE (同步检查点落盘)
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nSAVE\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 5. BGSAVE (异步后台落盘)
  let resp = send_and_recv(&mut client, b"*1\r\n$6\r\nBGSAVE\r\n").await?;
  assert_eq!(resp, b"+Background saving started\r\n");

  // 6. LASTSAVE (上次保存时间戳)
  let resp = send_and_recv(&mut client, b"*1\r\n$8\r\nLASTSAVE\r\n").await?;
  assert!(
    resp.starts_with(b":"),
    "LASTSAVE 应返回整数时间戳: {:?}",
    resp
  );

  info!("系统管理命令测试通过");
  OK
}

/// 测试 ACL 相关指令 (WHOAMI, CAT, GENPASS, SETUSER, GETUSER, SAVE, LOAD, DELUSER)
#[compio::test]
async fn test_acl_commands_integration() -> Void {
  info!("开始测试 ACL 完整生命周期集成");
  let fixture = AdminTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. ACL WHOAMI
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nACL\r\n$6\r\nWHOAMI\r\n").await?;
  assert_eq!(&resp, b"$7\r\ndefault\r\n");

  // 2. ACL CAT (返回全部分类)
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nACL\r\n$3\r\nCAT\r\n").await?;
  let s = from_utf8(&resp)?;
  assert!(s.starts_with("*25\r\n"), "ACL CAT 应返回 25 个分类: {s}");
  assert!(s.contains("string\r\n"));
  assert!(s.contains("vector\r\n"));

  // 3. ACL CAT string (返回 string 分类命令)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nACL\r\n$3\r\nCAT\r\n$6\r\nstring\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.contains("get\r\n"));
  assert!(s.contains("set\r\n"));
  assert!(s.contains("mget\r\n"));

  // 4. ACL GENPASS 默认 256 位 (64 hex 字符)
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nACL\r\n$7\r\nGENPASS\r\n").await?;
  assert_eq!(&resp[..5], b"$64\r\n");

  // 5. ACL GENPASS 0 报错
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGENPASS\r\n$1\r\n0\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR"));

  // 6. ACL SETUSER alice on >alice123 ~* &* +@all
  let resp = send_and_recv(
    &mut client,
    b"*7\r\n$3\r\nACL\r\n$7\r\nSETUSER\r\n$5\r\nalice\r\n$2\r\non\r\n$9\r\n>alice123\r\n$2\r\n~*\r\n$5\r\n+@all\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 7. AUTH alice alice123 切换身份
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nAUTH\r\n$5\r\nalice\r\n$8\r\nalice123\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 8. 验证当前用户已切换为 alice
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nACL\r\n$6\r\nWHOAMI\r\n").await?;
  assert_eq!(&resp, b"$5\r\nalice\r\n");

  // 9. ACL GETUSER alice
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nGETUSER\r\n$5\r\nalice\r\n",
  )
  .await?;
  let s = from_utf8(&resp)?;
  assert!(s.contains("on\r\n"));
  assert!(s.contains("+@all\r\n"));

  // 10. ACL SAVE 与 ACL LOAD
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nACL\r\n$4\r\nSAVE\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nACL\r\n$4\r\nLOAD\r\n").await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 11. ACL DELUSER alice
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nDELUSER\r\n$5\r\nalice\r\n",
  )
  .await?;
  assert_eq!(&resp, b":1\r\n");

  // 认证回默认 default 用户（alice 已被销毁因此无权限再执行 ACL 指令）
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nAUTH\r\n$7\r\ndefault\r\n$0\r\n\r\n",
  )
  .await?;
  assert_eq!(&resp, b"+OK\r\n");

  // 再次删除应返回 :0
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nACL\r\n$7\r\nDELUSER\r\n$5\r\nalice\r\n",
  )
  .await?;
  assert_eq!(&resp, b":0\r\n");

  // 12. AUTH 多参数错误校验
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nAUTH\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n",
  )
  .await?;
  assert!(from_utf8(&resp)?.contains("ERR wrong number of arguments for 'auth' command"));

  info!("ACL 完整生命周期集成测试通过");
  OK
}
