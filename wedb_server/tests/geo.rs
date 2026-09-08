//! 对标 C# 微软 Garnet 源码:
//! `../garnet/test/standalone/Garnet.test.collections/` 地理空间位置命令测试 (GEOADD, GEODIST, GEOPOS, GEOHASH, GEORADIUS, GEOSEARCH)
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

/// 地理空间测试脚手架
struct GeoTestFixture {
  server: Arc<WedbServer>,
  addr: SocketAddr,
  _dir: TempDir,
}

impl GeoTestFixture {
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

impl Drop for GeoTestFixture {
  fn drop(&mut self) {
    self.server.dispose();
  }
}

/// 测试 GEOADD, GEODIST, GEOPOS, GEOHASH, GEORADIUS, GEOSEARCH 等地理空间命令
/// 对应 Garnet 地理空间套件测试规范
#[compio::test]
async fn test_geo_operations() -> Void {
  info!("开始测试地理空间核心命令");
  let fixture = GeoTestFixture::setup().await?;
  let mut client = fixture.connect_client().await?;

  // 1. GEOADD Sicily 13.361267 38.115688 Palermo 15.087833 37.502482 Catania -> :2\r\n
  let resp = send_and_recv(
    &mut client,
    b"*8\r\n$6\r\nGEOADD\r\n$6\r\nSicily\r\n$9\r\n13.361267\r\n$9\r\n38.115688\r\n$7\r\nPalermo\r\n$9\r\n15.087833\r\n$9\r\n37.502482\r\n$7\r\nCatania\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 2. GEOADD with NX: 已存在元素跳过 -> :0\r\n
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$6\r\nGEOADD\r\n$6\r\nSicily\r\n$2\r\nNX\r\n$9\r\n13.361267\r\n$9\r\n38.115688\r\n$7\r\nPalermo\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":0\r\n");

  // 3. GEOADD with XX CH: 更新坐标 -> :1\r\n
  let resp = send_and_recv(
    &mut client,
    b"*7\r\n$6\r\nGEOADD\r\n$6\r\nSicily\r\n$2\r\nXX\r\n$2\r\nCH\r\n$5\r\n13.37\r\n$5\r\n38.12\r\n$7\r\nPalermo\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":1\r\n");

  // 4. GEODIST Sicily Palermo Catania (默认米单位)
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$7\r\nGEODIST\r\n$6\r\nSicily\r\n$7\r\nPalermo\r\n$7\r\nCatania\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with('$'));
  assert!(resp_str.contains("165838"));

  // 5. GEODIST Sicily Palermo Catania km (千米单位)
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$7\r\nGEODIST\r\n$6\r\nSicily\r\n$7\r\nPalermo\r\n$7\r\nCatania\r\n$2\r\nkm\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with('$'));
  assert!(resp_str.contains("165."));

  // 6. GEODIST 不存在成员 -> $-1\r\n
  let resp = send_and_recv(
    &mut client,
    b"*4\r\n$7\r\nGEODIST\r\n$6\r\nSicily\r\n$7\r\nPalermo\r\n$3\r\nFoo\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "$-1\r\n");

  // 7. GEOPOS Sicily Palermo Foo Catania
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$6\r\nGEOPOS\r\n$6\r\nSicily\r\n$7\r\nPalermo\r\n$3\r\nFoo\r\n$7\r\nCatania\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*3\r\n"));
  assert!(resp_str.contains("*-1\r\n")); // Foo 为空
  assert!(resp_str.contains("13.3699"));

  // 8. GEOHASH Sicily Palermo Foo Catania
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$7\r\nGEOHASH\r\n$6\r\nSicily\r\n$7\r\nPalermo\r\n$3\r\nFoo\r\n$7\r\nCatania\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*3\r\n"));
  assert!(resp_str.contains("$-1\r\n"));
  assert!(resp_str.contains("$11\r\nsq"));

  // 9. GEORADIUS Sicily 15.0 37.5 200 km
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$9\r\nGEORADIUS\r\n$6\r\nSicily\r\n$4\r\n15.0\r\n$4\r\n37.5\r\n$3\r\n200\r\n$2\r\nkm\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert_eq!(resp_str.lines().next().unwrap(), "*2");
  assert!(resp_str.contains("Palermo"));
  assert!(resp_str.contains("Catania"));

  // 10. GEORADIUS Sicily 15.0 37.5 100 km (仅包含 Catania)
  let resp = send_and_recv(
    &mut client,
    b"*6\r\n$9\r\nGEORADIUS\r\n$6\r\nSicily\r\n$4\r\n15.0\r\n$4\r\n37.5\r\n$3\r\n100\r\n$2\r\nkm\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, "*1\r\n$7\r\nCatania\r\n");

  // 11. GEORADIUS Sicily 15.0 37.5 200 km WITHDIST WITHCOORD
  let resp = send_and_recv(
    &mut client,
    b"*8\r\n$9\r\nGEORADIUS\r\n$6\r\nSicily\r\n$4\r\n15.0\r\n$4\r\n37.5\r\n$3\r\n200\r\n$2\r\nkm\r\n$8\r\nWITHDIST\r\n$9\r\nWITHCOORD\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert!(resp_str.starts_with("*2\r\n"));
  assert!(resp_str.contains("*3\r\n"));

  // 12. GEORADIUS Sicily 15.0 37.5 200 km STORE dest_geo
  let resp = send_and_recv(
    &mut client,
    b"*8\r\n$9\r\nGEORADIUS\r\n$6\r\nSicily\r\n$4\r\n15.0\r\n$4\r\n37.5\r\n$3\r\n200\r\n$2\r\nkm\r\n$5\r\nSTORE\r\n$8\r\ndest_geo\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  let resp = send_and_recv(&mut client, b"*2\r\n$5\r\nZCARD\r\n$8\r\ndest_geo\r\n").await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  // 13. GEORADIUSBYMEMBER Sicily Palermo 200 km
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$17\r\nGEORADIUSBYMEMBER\r\n$6\r\nSicily\r\n$7\r\nPalermo\r\n$3\r\n200\r\n$2\r\nkm\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert_eq!(resp_str.lines().next().unwrap(), "*2");

  // 14. GEORADIUSBYMEMBER Sicily NonExistent 100 km -> 报错
  let resp = send_and_recv(
    &mut client,
    b"*5\r\n$17\r\nGEORADIUSBYMEMBER\r\n$6\r\nSicily\r\n$3\r\nFoo\r\n$3\r\n100\r\n$2\r\nkm\r\n",
  )
  .await?;
  assert_eq!(
    from_utf8(&resp)?,
    "-ERR could not decode requested zset member\r\n"
  );

  // 15. GEOSEARCH Sicily FROMMEMBER Palermo BYRADIUS 200 km ASC
  let resp = send_and_recv(
    &mut client,
    b"*7\r\n$9\r\nGEOSEARCH\r\n$6\r\nSicily\r\n$10\r\nFROMMEMBER\r\n$7\r\nPalermo\r\n$8\r\nBYRADIUS\r\n$3\r\n200\r\n$2\r\nkm\r\n$3\r\nASC\r\n",
  )
  .await?;
  let resp_str = from_utf8(&resp)?;
  assert_eq!(resp_str.lines().next().unwrap(), "*2");
  let items: Vec<&str> = resp_str.lines().collect();
  assert_eq!(items[2], "Palermo");
  assert_eq!(items[4], "Catania");

  // 16. GEOSEARCHSTORE dest_search Sicily FROMLONLAT 15.0 37.5 BYBOX 400 400 km STOREDIST
  let resp = send_and_recv(
    &mut client,
    b"*11\r\n$14\r\nGEOSEARCHSTORE\r\n$11\r\ndest_search\r\n$6\r\nSicily\r\n$10\r\nFROMLONLAT\r\n$4\r\n15.0\r\n$4\r\n37.5\r\n$5\r\nBYBOX\r\n$3\r\n400\r\n$3\r\n400\r\n$2\r\nkm\r\n$9\r\nSTOREDIST\r\n",
  )
  .await?;
  assert_eq!(from_utf8(&resp)?, ":2\r\n");

  info!("地理空间核心命令测试通过");
  OK
}
