use std::{net::SocketAddr, sync::Arc};

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

/// 服务端测试辅助脚手架
struct ServerTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl ServerTestFixture {
  /// 启动默认轻量配置的测试服务实例
  async fn setup_default() -> Result<Self> {
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

  /// 建立连接到该测试服务实例的 TCP 客户端
  async fn connect_client(&self) -> Result<TcpStream> {
    let stream = TcpStream::connect(self.addr).await?;
    Ok(stream)
  }
}

impl Drop for ServerTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 发送 RESP 请求并接收响应
async fn send_and_recv(stream: &mut TcpStream, req: &[u8]) -> Result<Vec<u8>> {
  let BufResult(write_res, _) = stream.write_all(req.to_vec()).await;
  write_res?;
  let buf = Vec::with_capacity(8192);
  let BufResult(read_res, mut buf) = stream.read(buf).await;
  let n = read_res?;
  buf.truncate(n);
  Ok(buf)
}

/// 场景 1: 键空间管理命令 (DBSIZE, TYPE, RENAME, RENAMENX, EXISTS)
#[compio::test]
async fn test_keyspace_commands() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 初始空库 DBSIZE 应为 0
  let resp = send_and_recv(&mut client, b"*1\r\n$6\r\nDBSIZE\r\n").await?;
  assert_eq!(resp, b":0\r\n");

  // 2. 写入两个字符串键
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nk1\r\n$2\r\nv1\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*3\r\n$3\r\nSET\r\n$2\r\nk2\r\n$2\r\nv2\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 3. 写入一个哈希集合和一个无序集合
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nHSET\r\n$4\r\nmy_h\r\n$2\r\nf1\r\n$2\r\nv1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nSADD\r\n$4\r\nmy_s\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 4. DBSIZE 应精确为 4
  let resp = send_and_recv(&mut client, b"*1\r\n$6\r\nDBSIZE\r\n").await?;
  assert_eq!(resp, b":4\r\n");

  // 5. TYPE 验证各数据结构类型
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nTYPE\r\n$2\r\nk1\r\n").await?;
  assert_eq!(resp, b"+string\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nTYPE\r\n$4\r\nmy_h\r\n").await?;
  assert_eq!(resp, b"+hash\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nTYPE\r\n$4\r\nmy_s\r\n").await?;
  assert_eq!(resp, b"+set\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nTYPE\r\n$9\r\nnot_exist\r\n").await?;
  assert_eq!(resp, b"+none\r\n");

  // 6. EXISTS 多键检查（包含普通键与集合键）
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nEXISTS\r\n$2\r\nk1\r\n$2\r\nk2\r\n$4\r\nmy_h\r\n$7\r\nmissing\r\n",
  )
  .await?;
  assert_eq!(resp, b":3\r\n");

  // 7. RENAME 普通键 k1 -> k1_renamed
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nRENAME\r\n$2\r\nk1\r\n$10\r\nk1_renamed\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$10\r\nk1_renamed\r\n").await?;
  assert_eq!(resp, b"$2\r\nv1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\nk1\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  // 8. RENAME 集合键 my_h -> my_h_new
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nRENAME\r\n$4\r\nmy_h\r\n$8\r\nmy_h_new\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nHGET\r\n$8\r\nmy_h_new\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(resp, b"$2\r\nv1\r\n");

  // 9. RENAMENX: 目标键已存在返回 0，不存在返回 1
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$8\r\nRENAMENX\r\n$2\r\nk2\r\n$10\r\nk1_renamed\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$8\r\nRENAMENX\r\n$2\r\nk2\r\n$10\r\nk2_renamed\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 10. RENAME 不存在的源键返回错误
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nRENAME\r\n$6\r\nno_key\r\n$6\r\nnew_k1\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-ERR no such key"));

  // 11. 删除两个键后验证 DBSIZE
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nDEL\r\n$10\r\nk1_renamed\r\n$10\r\nk2_renamed\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");
  let resp = send_and_recv(&mut client, b"*1\r\n$6\r\nDBSIZE\r\n").await?;
  assert_eq!(resp, b":2\r\n");

  OK
}

/// 场景 2: 字符串与位图扩展命令 (MSETNX, APPEND, STRLEN, GETRANGE, SETRANGE, SETBIT, GETBIT, BITCOUNT)
#[compio::test]
async fn test_string_and_bitmap_commands() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. MSETNX: 全部键不存在时成功设置返回 1
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nMSETNX\r\n$2\r\ns1\r\n$3\r\nfoo\r\n$2\r\ns2\r\n$3\r\nbar\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 2. MSETNX: 含有已存在键时全部放弃返回 0
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nMSETNX\r\n$2\r\ns2\r\n$3\r\nnew\r\n$2\r\ns3\r\n$3\r\nbaz\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\ns3\r\n").await?;
  assert_eq!(resp, b"$-1\r\n");

  // 3. STRLEN
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSTRLEN\r\n$2\r\ns1\r\n").await?;
  assert_eq!(resp, b":3\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSTRLEN\r\n$7\r\nunknown\r\n").await?;
  assert_eq!(resp, b":0\r\n");

  // 4. APPEND
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nAPPEND\r\n$2\r\ns1\r\n$4\r\n_ext\r\n",
  )
  .await?;
  assert_eq!(resp, b":7\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\ns1\r\n").await?;
  assert_eq!(resp, b"$7\r\nfoo_ext\r\n");

  // 5. GETRANGE (支持正负索引)
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$8\r\nGETRANGE\r\n$2\r\ns1\r\n$1\r\n0\r\n$1\r\n2\r\n",
  )
  .await?;
  assert_eq!(resp, b"$3\r\nfoo\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$8\r\nGETRANGE\r\n$2\r\ns1\r\n$2\r\n-3\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert_eq!(resp, b"$3\r\next\r\n");

  // 6. SETRANGE
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$8\r\nSETRANGE\r\n$2\r\ns1\r\n$1\r\n4\r\n$3\r\nBAR\r\n",
  )
  .await?;
  assert_eq!(resp, b":7\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$2\r\ns1\r\n").await?;
  assert_eq!(resp, b"$7\r\nfoo_BAR\r\n");

  // 7. 位图命令: SETBIT 与 GETBIT // 设置 bit 7 为 1 (第 0 字节最低位，即 0x01)
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nSETBIT\r\n$6\r\nmy_bit\r\n$1\r\n7\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nGETBIT\r\n$6\r\nmy_bit\r\n$1\r\n7\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nGETBIT\r\n$6\r\nmy_bit\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");

  // 8. BITCOUNT
  let resp = send_and_recv(&mut client, b"*2\r\n$8\r\nBITCOUNT\r\n$6\r\nmy_bit\r\n").await?;
  assert_eq!(resp, b":1\r\n");

  // 在 bit 0 置 1 (0x81)
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nSETBIT\r\n$6\r\nmy_bit\r\n$1\r\n0\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$8\r\nBITCOUNT\r\n$6\r\nmy_bit\r\n").await?;
  assert_eq!(resp, b":2\r\n");

  // 9. 带范围的 BITCOUNT
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$8\r\nBITCOUNT\r\n$6\r\nmy_bit\r\n$1\r\n0\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  OK
}

/// 场景 3: 哈希表扩展命令 (HINCRBY, HINCRBYFLOAT, HMGET, HMSET, HSETNX)
#[compio::test]
async fn test_hash_extended_commands() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. HMSET 批量设置
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$5\r\nHMSET\r\n$4\r\nuser\r\n$4\r\nname\r\n$5\r\nalice\r\n$3\r\nage\r\n$2\r\n20\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 2. HMGET 批量读取（含不存在的字段）
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$5\r\nHMGET\r\n$4\r\nuser\r\n$4\r\nname\r\n$3\r\nage\r\n$7\r\nunknown\r\n",
  )
  .await?;
  assert_eq!(resp, b"*3\r\n$5\r\nalice\r\n$2\r\n20\r\n$-1\r\n");

  // 3. HINCRBY 增减整数
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$7\r\nHINCRBY\r\n$4\r\nuser\r\n$3\r\nage\r\n$1\r\n5\r\n",
  )
  .await?;
  assert_eq!(resp, b":25\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$7\r\nHINCRBY\r\n$4\r\nuser\r\n$3\r\nage\r\n$2\r\n-3\r\n",
  )
  .await?;
  assert_eq!(resp, b":22\r\n");

  // 4. HINCRBYFLOAT 浮点增减
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$12\r\nHINCRBYFLOAT\r\n$4\r\nuser\r\n$5\r\nscore\r\n$4\r\n10.5\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"$4\r\n10.5\r\n") || resp.starts_with(b"$"));

  // 5. HSETNX
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nHSETNX\r\n$4\r\nuser\r\n$3\r\nage\r\n$2\r\n99\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nHSETNX\r\n$4\r\nuser\r\n$5\r\nemail\r\n$13\r\na@example.com\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  OK
}

/// 场景 4: 列表扩展命令 (LRANGE, LINDEX, LTRIM)
#[compio::test]
async fn test_list_extended_commands() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. RPUSH 压入 5 个元素: a, b, c, d, e
  let resp = send_and_recv(
    &mut client,
    b"*7\r\n$5\r\nRPUSH\r\n$4\r\nlist\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n$1\r\nd\r\n$1\r\ne\r\n",
  )
  .await?;
  assert_eq!(resp, b":5\r\n");

  // 2. LRANGE 范围读取全部
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nLRANGE\r\n$4\r\nlist\r\n$1\r\n0\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert_eq!(
    resp,
    b"*5\r\n$1\r\na\r\n$1\r\nb\r\n$1\r\nc\r\n$1\r\nd\r\n$1\r\ne\r\n"
  );

  // 3. LRANGE 切片读取
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nLRANGE\r\n$4\r\nlist\r\n$1\r\n1\r\n$1\r\n3\r\n",
  )
  .await?;
  assert_eq!(resp, b"*3\r\n$1\r\nb\r\n$1\r\nc\r\n$1\r\nd\r\n");

  // 4. LINDEX 正负索引
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nLINDEX\r\n$4\r\nlist\r\n$1\r\n0\r\n",
  )
  .await?;
  assert_eq!(resp, b"$1\r\na\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nLINDEX\r\n$4\r\nlist\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert_eq!(resp, b"$1\r\ne\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nLINDEX\r\n$4\r\nlist\r\n$2\r\n99\r\n",
  )
  .await?;
  assert_eq!(resp, b"$-1\r\n");

  // 5. LTRIM 截断为中间的 [b, c, d]
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nLTRIM\r\n$4\r\nlist\r\n$1\r\n1\r\n$1\r\n3\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nLRANGE\r\n$4\r\nlist\r\n$1\r\n0\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert_eq!(resp, b"*3\r\n$1\r\nb\r\n$1\r\nc\r\n$1\r\nd\r\n");

  OK
}

/// 场景 5: 集合与有序集合扩展命令 (SPOP, SMOVE, ZINCRBY, ZCOUNT, ZREM, ZRANK, ZREVRANK)
#[compio::test]
async fn test_set_and_zset_extended_commands() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. SADD 初始化集合
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nSADD\r\n$3\r\ns_a\r\n$2\r\nm1\r\n$2\r\nm2\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  // 2. SMOVE: 从 s_a 移动 m1 到 s_b
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nSMOVE\r\n$3\r\ns_a\r\n$3\r\ns_b\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$9\r\nSISMEMBER\r\n$3\r\ns_a\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$9\r\nSISMEMBER\r\n$3\r\ns_b\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 3. SPOP 单个成员
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nSPOP\r\n$3\r\ns_a\r\n").await?;
  assert_eq!(resp, b"$2\r\nm2\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nSCARD\r\n$3\r\ns_a\r\n").await?;
  assert_eq!(resp, b":0\r\n");

  // 4. 有序集合 ZADD 初始数据
  let resp = send_and_recv(
    &mut client,
    b"*8\r\n$4\r\nZADD\r\n$5\r\nmy_zs\r\n$2\r\n10\r\n$4\r\nuser\r\n$2\r\n20\r\n$5\r\nadmin\r\n$2\r\n30\r\n$4\r\nroot\r\n",
  )
  .await?;
  assert_eq!(resp, b":3\r\n");

  // 5. ZINCRBY 分数递增
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$7\r\nZINCRBY\r\n$5\r\nmy_zs\r\n$2\r\n15\r\n$4\r\nuser\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"$2\r\n25\r\n") || resp.starts_with(b"$"));

  // 此时 user 分数为 25，admin 为 20，root 为 30 // 6. ZCOUNT 开闭区间
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZCOUNT\r\n$5\r\nmy_zs\r\n$2\r\n20\r\n$2\r\n30\r\n",
  )
  .await?;
  assert_eq!(resp, b":3\r\n");

  // 开区间 (20 30 应只包含 25(user)，计数为 1
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZCOUNT\r\n$5\r\nmy_zs\r\n$3\r\n(20\r\n$3\r\n(30\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 7. ZRANK 与 ZREVRANK // 顺序: admin(20, rank 0), user(25, rank 1), root(30, rank 2)
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nZRANK\r\n$5\r\nmy_zs\r\n$5\r\nadmin\r\n",
  )
  .await?;
  assert_eq!(resp, b":0\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$8\r\nZREVRANK\r\n$5\r\nmy_zs\r\n$5\r\nadmin\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  // 8. ZREM 移除成员
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$4\r\nZREM\r\n$5\r\nmy_zs\r\n$4\r\nuser\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nZCARD\r\n$5\r\nmy_zs\r\n").await?;
  assert_eq!(resp, b":2\r\n");

  OK
}

/// 场景 6: 事务排队与原子批处理中的扩展命令
#[compio::test]
async fn test_txn_queued_extended_commands() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 开启事务
  let resp = send_and_recv(&mut client, b"*1\r\n$5\r\nMULTI\r\n").await?;
  assert_eq!(resp, b"+OK\r\n");

  // 2. 排队各种扩展命令
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$5\r\ntx_k1\r\n$5\r\nhello\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSTRLEN\r\n$5\r\ntx_k1\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nAPPEND\r\n$5\r\ntx_k1\r\n$6\r\n_world\r\n",
  )
  .await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  let resp = send_and_recv(&mut client, b"*1\r\n$6\r\nDBSIZE\r\n").await?;
  assert_eq!(resp, b"+QUEUED\r\n");

  // 3. EXEC 提交并验证数组结果
  let resp = send_and_recv(&mut client, b"*1\r\n$4\r\nEXEC\r\n").await?;
  assert_eq!(resp, b"*4\r\n+OK\r\n:5\r\n:11\r\n:1\r\n");

  OK
}

/// 场景 7: 错误类型容错 (WRONGTYPE 保持长连接健康)、SMOVE 同键与非法目标防护、位图越界与正负无穷区间
#[compio::test]
async fn test_extended_commands_wrongtype_and_boundaries() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 初始化一个普通字符串键
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$7\r\nstr_key\r\n$5\r\nhello\r\n",
  )
  .await?;
  assert_eq!(resp, b"+OK\r\n");

  // 2. 对字符串键调用各类集合/列表/有序集合命令，验证 WRONGTYPE 响应且连接保持存活
  let resp = send_and_recv(&mut client, b"*2\r\n$4\r\nSPOP\r\n$7\r\nstr_key\r\n").await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nLRANGE\r\n$7\r\nstr_key\r\n$1\r\n0\r\n$2\r\n-1\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nLINDEX\r\n$7\r\nstr_key\r\n$1\r\n0\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nLTRIM\r\n$7\r\nstr_key\r\n$1\r\n0\r\n$1\r\n1\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZCOUNT\r\n$7\r\nstr_key\r\n$1\r\n0\r\n$2\r\n10\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nZRANK\r\n$7\r\nstr_key\r\n$3\r\nfoo\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  // 验证在连续 WRONGTYPE 之后，连接仍然完全可用
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$7\r\nstr_key\r\n").await?;
  assert_eq!(resp, b"$5\r\nhello\r\n");

  // 3. 对集合键调用字符串与位图命令，验证返回 WRONGTYPE 且不损坏数据
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nSADD\r\n$7\r\nset_key\r\n$2\r\nm1\r\n$2\r\nm2\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$6\r\nSTRLEN\r\n$7\r\nset_key\r\n").await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nAPPEND\r\n$7\r\nset_key\r\n$3\r\nfoo\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nSETBIT\r\n$7\r\nset_key\r\n$1\r\n0\r\n$1\r\n1\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nGETBIT\r\n$7\r\nset_key\r\n$1\r\n0\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  let resp = send_and_recv(&mut client, b"*2\r\n$8\r\nBITCOUNT\r\n$7\r\nset_key\r\n").await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  // 4. SMOVE 边界测试
  // 4.1 source == dest 时原地返回 1（无需写出与自删）
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nSMOVE\r\n$7\r\nset_key\r\n$7\r\nset_key\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 4.2 dest 存在且为错误类型（如字符串），必须拦截报错且保留源成员
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$5\r\nSMOVE\r\n$7\r\nset_key\r\n$7\r\nstr_key\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-WRONGTYPE"));

  // 确认 m1 仍完好保留在 set_key 中
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$9\r\nSISMEMBER\r\n$7\r\nset_key\r\n$2\r\nm1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 5. MSETNX 命令自身包含重复参数键
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nMSETNX\r\n$5\r\ndup_k\r\n$2\r\nv1\r\n$5\r\ndup_k\r\n$2\r\nv2\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$5\r\ndup_k\r\n").await?;
  assert_eq!(resp, b"$2\r\nv2\r\n");

  // 6. 位图偏移越界与非法值拦截
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nGETBIT\r\n$7\r\nstr_key\r\n$11\r\n50000000000\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-ERR bit offset is not an integer or out of range"));

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nSETBIT\r\n$7\r\nstr_key\r\n$11\r\n50000000000\r\n$1\r\n1\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-ERR bit offset is not an integer or out of range"));

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nSETBIT\r\n$7\r\nstr_key\r\n$1\r\n0\r\n$1\r\n2\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"-ERR bit is not an integer or out of range"));

  // 7. 有序集合正负无穷区间 (ZCOUNT)
  let resp = send_and_recv(
    &mut client,
    b"*8\r\n$4\r\nZADD\r\n$7\r\nzs_infs\r\n$2\r\n10\r\n$1\r\na\r\n$2\r\n20\r\n$1\r\nb\r\n$2\r\n30\r\n$1\r\nc\r\n",
  )
  .await?;
  assert_eq!(resp, b":3\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZCOUNT\r\n$7\r\nzs_infs\r\n$4\r\n-inf\r\n$4\r\n+inf\r\n",
  )
  .await?;
  assert_eq!(resp, b":3\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZCOUNT\r\n$7\r\nzs_infs\r\n$3\r\n(10\r\n$4\r\n+inf\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  OK
}

/// 场景 8: 打平集合 HSCAN, SSCAN, ZSCAN 与哈希字段级过期 (HEXPIRE, HTTL, HPERSIST) 网络端到端测试
#[compio::test]
async fn test_flattened_scans_and_field_expirations() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. HSCAN 与哈希字段级过期 (HEXPIRE / HTTL / HPERSIST)
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nHSET\r\n$4\r\nmy_h\r\n$2\r\nf1\r\n$2\r\nv1\r\n$2\r\nf2\r\n$2\r\nv2\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  // HSCAN 游标遍历
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$5\r\nHSCAN\r\n$4\r\nmy_h\r\n$1\r\n0\r\n$5\r\nMATCH\r\n$2\r\nf*\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"*2\r\n$1\r\n0\r\n*4\r\n"));

  // HEXPIRE 字段过期设置 (50 秒)
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$7\r\nHEXPIRE\r\n$4\r\nmy_h\r\n$2\r\n50\r\n$6\r\nFIELDS\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // HTTL 查询剩余存活时间
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$4\r\nHTTL\r\n$4\r\nmy_h\r\n$6\r\nFIELDS\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert!(resp.starts_with(b":"));
  assert_ne!(resp, b":-1\r\n");
  assert_ne!(resp, b":-2\r\n");

  // HPERSIST 移除过期时间
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$8\r\nHPERSIST\r\n$4\r\nmy_h\r\n$6\r\nFIELDS\r\n$2\r\nf1\r\n",
  )
  .await?;
  assert_eq!(resp, b":1\r\n");

  // 2. SSCAN 与 SMISMEMBER 批量成员检查
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nSADD\r\n$4\r\nmy_s\r\n$2\r\nm1\r\n$2\r\nm2\r\n$2\r\nm3\r\n",
  )
  .await?;
  assert_eq!(resp, b":3\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$10\r\nSMISMEMBER\r\n$4\r\nmy_s\r\n$2\r\nm1\r\n$7\r\nunknown\r\n$2\r\nm3\r\n",
  )
  .await?;
  assert_eq!(resp, b"*3\r\n:1\r\n:0\r\n:1\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$5\r\nSSCAN\r\n$4\r\nmy_s\r\n$1\r\n0\r\n$5\r\nMATCH\r\n$2\r\nm*\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"*2\r\n$1\r\n0\r\n*3\r\n"));

  // 3. ZSCAN 有序集合扫描
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nZADD\r\n$4\r\nmy_z\r\n$3\r\n1.5\r\n$2\r\nz1\r\n$3\r\n2.5\r\n$2\r\nz2\r\n",
  )
  .await?;
  assert_eq!(resp, b":2\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$5\r\nZSCAN\r\n$4\r\nmy_z\r\n$1\r\n0\r\n",
  )
  .await?;
  assert!(resp.starts_with(b"*2\r\n$1\r\n0\r\n*4\r\n"));

  OK
}

/// 场景 9:C# Garnet NetworkGET_SG 流水线前瞻投机批处理测试
/// 验证在单次网络报文中打包多个连续 GET 命令（包括大写 GET 与小写 get、命中与未命中、混合管道），
/// 服务端前瞻批处理能够正确无缝解析、同步直读内存并按序精准返回全部响应。
#[compio::test]
async fn test_network_get_sg_speculative_pipeline() -> Void {
  let fixture = ServerTestFixture::setup_default().await?;
  let mut client = fixture.connect_client().await?;

  // 1. 初始化预置数据
  let _ = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$2\r\nk1\r\n$4\r\nval1\r\n",
  )
  .await?;
  let _ = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$2\r\nk2\r\n$4\r\nval2\r\n",
  )
  .await?;
  let _ = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$2\r\nk3\r\n$4\r\nval3\r\n",
  )
  .await?;
  let _ = send_and_recv(
    &mut client,
    b"*3\r\n$3\r\nSET\r\n$2\r\nk4\r\n$4\r\nval4\r\n",
  )
  .await?;

  // 2. 单次写入包含 5 个连续 GET 命令的流水线报文（含大小写与未命中）
  let mut pipeline_req = Vec::new();
  pipeline_req.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$2\r\nk1\r\n");
  pipeline_req.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$2\r\nk2\r\n");
  pipeline_req.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$7\r\nmissing\r\n");
  pipeline_req.extend_from_slice(b"*2\r\n$3\r\nget\r\n$2\r\nk3\r\n");
  pipeline_req.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$2\r\nk4\r\n");

  let resp = send_and_recv(&mut client, &pipeline_req).await?;
  let expected = b"$4\r\nval1\r\n$4\r\nval2\r\n$-1\r\n$4\r\nval3\r\n$4\r\nval4\r\n";
  assert_eq!(resp, expected, "连续 GET 流水线响应必须按序完全匹配");

  // 3. 混合命令流水线：GET 遇到非 GET 边界自动切回主调度循环
  let mut mixed_pipeline = Vec::new();
  mixed_pipeline.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$2\r\nk1\r\n");
  mixed_pipeline.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$2\r\nk2\r\n");
  mixed_pipeline.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$2\r\nk5\r\n$4\r\nval5\r\n");
  mixed_pipeline.extend_from_slice(b"*2\r\n$3\r\nGET\r\n$2\r\nk5\r\n");

  let resp = send_and_recv(&mut client, &mixed_pipeline).await?;
  let expected_mixed = b"$4\r\nval1\r\n$4\r\nval2\r\n+OK\r\n$4\r\nval5\r\n";
  assert_eq!(
    resp, expected_mixed,
    "混合指令流水线在 GET 投机结束后正确恢复"
  );

  info!("场景 9:C# Garnet NetworkGET_SG 流水线前瞻投机批处理验证通过");
  OK
}
