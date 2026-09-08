//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test.collections/` 有序集合命令测试 (ZADD, ZCARD, ZSCORE, ZRANGE, ZUNION, ZINTER, ZDIFF 等)
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

/// 有序集合测试脚手架
struct ZsetTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl ZsetTestFixture {
  async fn setup() -> Result<Self> {
    let dir = tempdir()?;
    let dir_path = dir.path().to_string_lossy().to_string();
    let args = ServerArgs {
      port: 0,
      dir: dir_path,
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
    let stream = TcpStream::connect(self.addr).await?;
    Ok(stream)
  }
}

impl Drop for ZsetTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试有序集合核心与扩展操作
/// 对应 Garnet 有序集合套件测试规范
#[compio::test]
async fn test_zset_operations() -> Void {
  info!("开始测试有序集合核心命令");
  let fixture = ZsetTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. ZADD myzset 10 one 20 two 30 three 40 four -> :4
  let resp = send_and_recv(
    &mut client,
    b"*10\r\n$4\r\nZADD\r\n$6\r\nmyzset\r\n$2\r\n10\r\n$3\r\none\r\n$2\r\n20\r\n$3\r\ntwo\r\n$2\r\n30\r\n$5\r\nthree\r\n$2\r\n40\r\n$4\r\nfour\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":4\r\n");

  // 2. ZCARD myzset -> :4
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nZCARD\r\n$6\r\nmyzset\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":4\r\n");

  // 3. ZSCORE myzset two -> $4\r\n20.0\r\n
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nZSCORE\r\n$6\r\nmyzset\r\n$3\r\ntwo\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$4\r\n20.0\r\n");

  // 4. ZMSCORE myzset one two nonexisting -> *3\r\n$4\r\n10.0\r\n$4\r\n20.0\r\n$-1\r\n
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$7\r\nZMSCORE\r\n$6\r\nmyzset\r\n$3\r\none\r\n$3\r\ntwo\r\n$11\r\nnonexisting\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "*3\r\n$4\r\n10.0\r\n$4\r\n20.0\r\n$-1\r\n"
  );

  // 5. ZRANGE myzset 0 -1 -> 4 items
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZRANGE\r\n$6\r\nmyzset\r\n$1\r\n0\r\n$2\r\n-1\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*4\r\n"));
  assert!(resp_str.contains("$3\r\none\r\n"));
  assert!(resp_str.contains("$4\r\nfour\r\n"));

  // 6. ZRANGE myzset 0 1 WITHSCORES -> 4 items: one, 10.0, two, 20.0
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nZRANGE\r\n$6\r\nmyzset\r\n$1\r\n0\r\n$1\r\n1\r\n$10\r\nWITHSCORES\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "*4\r\n$3\r\none\r\n$4\r\n10.0\r\n$3\r\ntwo\r\n$4\r\n20.0\r\n"
  );

  // 7. ZREVRANGE myzset 0 1 WITHSCORES -> 4 items: four, 40.0, three, 30.0
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$9\r\nZREVRANGE\r\n$6\r\nmyzset\r\n$1\r\n0\r\n$1\r\n1\r\n$10\r\nWITHSCORES\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "*4\r\n$4\r\nfour\r\n$4\r\n40.0\r\n$5\r\nthree\r\n$4\r\n30.0\r\n"
  );

  // 8. ZRANGEBYSCORE myzset (10 30 -> 2 items: two, three
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$13\r\nZRANGEBYSCORE\r\n$6\r\nmyzset\r\n$3\r\n(10\r\n$2\r\n30\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*2\r\n$3\r\ntwo\r\n$5\r\nthree\r\n");

  // 9. ZRANGESTORE zdest myzset 0 1 -> :2
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$11\r\nZRANGESTORE\r\n$5\r\nzdest\r\n$6\r\nmyzset\r\n$1\r\n0\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");
  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nZCARD\r\n$5\r\nzdest\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 10. ZCOUNT myzset 15 35 -> :2 (two: 20, three: 30)
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$6\r\nZCOUNT\r\n$6\r\nmyzset\r\n$2\r\n15\r\n$2\r\n35\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 11. ZPOPMIN myzset 1 -> one, 10.0
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$7\r\nZPOPMIN\r\n$6\r\nmyzset\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*2\r\n$3\r\none\r\n$4\r\n10.0\r\n");

  // 12. ZPOPMAX myzset 1 -> four, 40.0
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$7\r\nZPOPMAX\r\n$6\r\nmyzset\r\n$1\r\n1\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*2\r\n$4\r\nfour\r\n$4\r\n40.0\r\n");

  // 13. ZLEXCOUNT, ZRANGEBYLEX, ZREMRANGEBYLEX
  let resp = send_and_recv(
    &mut client,
    b"*12\r\n$4\r\nZADD\r\n$4\r\nzlex\r\n$1\r\n0\r\n$1\r\na\r\n$1\r\n0\r\n$1\r\nb\r\n$1\r\n0\r\n$1\r\nc\r\n$1\r\n0\r\n$1\r\nd\r\n$1\r\n0\r\n$1\r\ne\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":5\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$9\r\nZLEXCOUNT\r\n$4\r\nzlex\r\n$2\r\n[b\r\n$2\r\n[d\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":3\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$11\r\nZRANGEBYLEX\r\n$4\r\nzlex\r\n$2\r\n[b\r\n$2\r\n(d\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*2\r\n$1\r\nb\r\n$1\r\nc\r\n");

  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$14\r\nZREMRANGEBYLEX\r\n$4\r\nzlex\r\n$2\r\n[a\r\n$2\r\n[b\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 14. 集合间操作: ZUNION, ZUNIONSTORE, ZINTER, ZINTERSTORE, ZDIFF, ZDIFFSTORE
  let _ = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nZADD\r\n$2\r\nz1\r\n$1\r\n1\r\n$1\r\na\r\n$1\r\n2\r\n$1\r\nb\r\n",
  )
  .await?;
  let _ = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nZADD\r\n$2\r\nz2\r\n$1\r\n2\r\n$1\r\nb\r\n$1\r\n3\r\n$1\r\nc\r\n",
  )
  .await?;

  // ZUNION 2 z1 z2 WITHSCORES -> a:1, b:4, c:3
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nZUNION\r\n$1\r\n2\r\n$2\r\nz1\r\n$2\r\nz2\r\n$10\r\nWITHSCORES\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*6\r\n"));
  assert!(resp_str.contains("$1\r\na\r\n$3\r\n1.0\r\n"));
  assert!(resp_str.contains("$1\r\nb\r\n$3\r\n4.0\r\n"));
  assert!(resp_str.contains("$1\r\nc\r\n$3\r\n3.0\r\n"));

  // ZUNIONSTORE zu 2 z1 z2 -> :3
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$11\r\nZUNIONSTORE\r\n$2\r\nzu\r\n$1\r\n2\r\n$2\r\nz1\r\n$2\r\nz2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":3\r\n");

  // ZINTER 2 z1 z2 WITHSCORES -> b:4.0
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nZINTER\r\n$1\r\n2\r\n$2\r\nz1\r\n$2\r\nz2\r\n$10\r\nWITHSCORES\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*2\r\n$1\r\nb\r\n$3\r\n4.0\r\n");

  // ZINTERSTORE zi 2 z1 z2 -> :1
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$11\r\nZINTERSTORE\r\n$2\r\nzi\r\n$1\r\n2\r\n$2\r\nz1\r\n$2\r\nz2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // ZINTERCARD 2 z1 z2 -> :1
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$10\r\nZINTERCARD\r\n$1\r\n2\r\n$2\r\nz1\r\n$2\r\nz2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // ZDIFF 2 z1 z2 WITHSCORES -> a:1.0
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$5\r\nZDIFF\r\n$1\r\n2\r\n$2\r\nz1\r\n$2\r\nz2\r\n$10\r\nWITHSCORES\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*2\r\n$1\r\na\r\n$3\r\n1.0\r\n");

  // ZDIFFSTORE zd 2 z1 z2 -> :1
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$10\r\nZDIFFSTORE\r\n$2\r\nzd\r\n$1\r\n2\r\n$2\r\nz1\r\n$2\r\nz2\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 15. ZMPOP 2 z1 z2 MIN COUNT 1 -> [z1, [a, 1]]
  let resp = send_and_recv(
    &mut client,
    b"*7\r\n$5\r\nZMPOP\r\n$1\r\n2\r\n$2\r\nz1\r\n$2\r\nz2\r\n$3\r\nMIN\r\n$5\r\nCOUNT\r\n$1\r\n1\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*2\r\n$2\r\nz1\r\n"));

  // 16. BZPOPMIN 空键超时 0.05 秒 -> *-1\r\n
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$8\r\nBZPOPMIN\r\n$7\r\nnon_zst\r\n$4\r\n0.05\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*-1\r\n");

  info!("有序集合测试通过");
  OK
}

/// 测试 ZADD 选项矩阵 (NX / XX / GT / LT / CH / INCR)
/// 对标 C# SortedSetObjectImpl ZAddOptions 语义与 Redis ZADD INCR 回执规范
#[compio::test]
async fn test_zadd_options_suite() -> Void {
  info!("开始测试 ZADD 选项矩阵");
  let fixture = ZsetTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. ZADD key GT 10 a：新键直接写入 -> :1；ZSCORE a -> "10"
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$2\r\nGT\r\n$2\r\n10\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 2. ZADD key GT 5 a：新分值 5 不高于 10 -> :0 且分值不变
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$2\r\nGT\r\n$1\r\n5\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nZSCORE\r\n$2\r\nzo\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$4\r\n10.0\r\n");

  // 3. ZADD key NX 11 a：成员已存在 -> :0；ZADD key XX 12 a：仅更新不计入 -> :0（Redis 口径）
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$2\r\nNX\r\n$2\r\n11\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$2\r\nXX\r\n$2\r\n12\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*3\r\n$6\r\nZSCORE\r\n$2\r\nzo\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$4\r\n12.0\r\n");

  // 4. ZADD key CH 12 a：分值未变化 -> :0；CH 13 a：变更计入 -> :1
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$2\r\nCH\r\n$2\r\n12\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$2\r\nCH\r\n$2\r\n13\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 5. ZADD key INCR 7 a：回新分值 "20" (bulk)
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$4\r\nINCR\r\n$1\r\n7\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$4\r\n20.0\r\n");

  // 6. ZADD key GT INCR 1 a：21 > 20 条件通过 -> "21"；GT INCR -5 a：16 <= 21 不通过 -> $-1
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$2\r\nGT\r\n$4\r\nINCR\r\n$1\r\n1\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$4\r\n21.0\r\n");
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$4\r\nZADD\r\n$2\r\nzo\r\n$2\r\nGT\r\n$4\r\nINCR\r\n$2\r\n-5\r\n$1\r\na\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");

  info!("ZADD 选项矩阵测试通过");
  OK
}
