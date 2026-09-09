//! AOF 追加日志端到端行为测试：
//! 写效果经引擎写监听端口入 AOF，重启后重放恢复；SAVE 后 AOF 只留增量仍可完整恢复。

use std::{net::SocketAddr, sync::Arc};

use aok::{OK, Result, Void};
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
};
use log::info;
use tempfile::tempdir;
use wedb_server::{ServerArgs, WedbServer};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 构造启用 AOF 的最小化测试参数（port 0 内核分配；显式内存预算防巨型自适应配置）
fn aof_args(dir: &std::path::Path) -> ServerArgs {
  ServerArgs {
    port: 0,
    dir: dir.to_string_lossy().to_string(),
    quiet: true,
    aof_enabled: true,
    store_memory_budget: Some(256 * 1024 * 1024),
    ..Default::default()
  }
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

/// 起服务并返回地址
async fn start(server: &Arc<WedbServer>) -> Result<SocketAddr> {
  Ok(server.start().await?)
}

/// AOF 重启恢复闭环：SET/INCR/DEL 写效果入 AOF，重启（无 checkpoint）后全部重放恢复
#[compio::test]
async fn test_aof_restart_recovery() -> Void {
  let dir = tempdir()?;

  // 第一轮：写入并停机（stop 内提交 AOF 并刷盘 hlog）
  {
    let server = Arc::new(WedbServer::new(aof_args(dir.path())).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;

    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$3\r\nfoo\r\n$3\r\nbar\r\n"
      )
      .await?,
      b"+OK\r\n"
    );
    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$4\r\ndead\r\n$2\r\nkv\r\n"
      )
      .await?,
      b"+OK\r\n"
    );
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$4\r\nINCR\r\n$7\r\ncounter\r\n").await?,
      b":1\r\n"
    );
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$4\r\nINCR\r\n$7\r\ncounter\r\n").await?,
      b":2\r\n"
    );
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nDEL\r\n$4\r\ndead\r\n").await?,
      b":1\r\n"
    );

    server.stop().await?;
  }

  // 第二轮：全新开库（无 checkpoint，hlog 索引为空），依赖 AOF 重放恢复
  {
    let server = Arc::new(WedbServer::new(aof_args(dir.path())).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;

    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$3\r\nfoo\r\n").await?,
      b"$3\r\nbar\r\n"
    );
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$7\r\ncounter\r\n").await?,
      b"$1\r\n2\r\n"
    );
    // 已删除键不得复活
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$4\r\ndead\r\n").await?,
      b"$-1\r\n"
    );

    server.stop().await?;
  }

  info!("AOF 重启恢复闭环测试通过");
  OK
}

/// SAVE 联动：checkpoint 后 AOF 截断为增量，重启走 checkpoint + AOF 增量重放完整恢复
#[compio::test]
async fn test_aof_save_truncate_and_incremental_recovery() -> Void {
  let dir = tempdir()?;

  {
    let server = Arc::new(WedbServer::new(aof_args(dir.path())).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;

    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$8\r\nsnap_key\r\n$9\r\nsnap_val1\r\n"
      )
      .await?,
      b"+OK\r\n"
    );
    // checkpoint：快照 [begin, covered) 并物理截断 AOF 前缀
    assert_eq!(
      send_and_recv(&mut client, b"*1\r\n$4\r\nSAVE\r\n").await?,
      b"+OK\r\n"
    );
    // 截断后的增量写
    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$8\r\nsnap_key\r\n$9\r\nsnap_val2\r\n"
      )
      .await?,
      b"+OK\r\n"
    );
    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$9\r\npost_save\r\n$3\r\nyes\r\n"
      )
      .await?,
      b"+OK\r\n"
    );

    server.stop().await?;
  }

  {
    let server = Arc::new(WedbServer::new(aof_args(dir.path())).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;

    // checkpoint 前的值来自快照，其后为 AOF 增量重放
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$8\r\nsnap_key\r\n").await?,
      b"$9\r\nsnap_val2\r\n"
    );
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$9\r\npost_save\r\n").await?,
      b"$3\r\nyes\r\n"
    );

    server.stop().await?;
  }

  info!("AOF SAVE 截断与增量恢复测试通过");
  OK
}

/// 崩溃模拟：不经 stop()（无 flush_all、AOF 环形丢失），仅靠已提交 AOF 帧恢复。
/// 覆盖等长原位覆写快路径（try_update_in_place）——该路径写效果改写 hlog 内存、
/// 不追加日志记录，AOF 帧缺失时无 checkpoint 重启将静默丢写
#[compio::test]
async fn test_aof_crash_recovery_without_graceful_stop() -> Void {
  let dir = tempdir()?;

  {
    let server = Arc::new(WedbServer::new(aof_args(dir.path())).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;

    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$8\r\nsnap_key\r\n$9\r\nsnap_val1\r\n"
      )
      .await?,
      b"+OK\r\n"
    );
    // 同长（9 字节）覆写：命中 hlog 原位更新快路径，帧必须照常入 AOF
    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$8\r\nsnap_key\r\n$9\r\nsnap_val2\r\n"
      )
      .await?,
      b"+OK\r\n"
    );
    // 不调 stop()：直接丢弃服务模拟 kill -9（hlog 内存页与 AOF 环形一并丢失，
    // 已应答写仅存在于已提交的 AOF 磁盘帧中）
  }

  {
    let server = Arc::new(WedbServer::new(aof_args(dir.path())).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;

    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$8\r\nsnap_key\r\n").await?,
      b"$9\r\nsnap_val2\r\n"
    );

    server.stop().await?;
  }

  info!("AOF 崩溃模拟恢复（含原位覆写帧）测试通过");
  OK
}

/// BfTree 帧家族钉子：Flattened ZSET（成员数超 Compact 上限 128）的 score 唯一副本
/// 在共享 BfTree 中，其写效果必须经 BfTreePut 帧进入 AOF。
/// 全程无 SAVE、崩溃式退出（不走 stop），重启后仅靠 AOF 恢复 score。
#[compio::test]
async fn test_aof_bftree_zset_crash_recovery() -> Void {
  let dir = tempdir()?;
  const MEMBERS: usize = 130; // > ZSET_MAX_COMPACT_ENTRIES(128)，触发 Flattened 编码

  // 构造 ZADD 报文（member_i ↔ score i + 0.5）
  fn zadd_req(i: usize) -> Vec<u8> {
    let member = format!("m{i:07}");
    let score = format!("{}", i as f64 + 0.5);
    let mut req = format!(
      "*4\r\n$4\r\nZADD\r\n$4\r\nlead\r\n${}\r\n{}\r\n",
      score.len(),
      score
    )
    .into_bytes();
    req.extend_from_slice(format!("${}\r\n{}\r\n", member.len(), member).as_bytes());
    req
  }

  let mut sample_resp = Vec::new();
  {
    let server = Arc::new(WedbServer::new(aof_args(dir.path())).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;

    for i in 0..MEMBERS {
      let resp = send_and_recv(&mut client, &zadd_req(i)).await?;
      assert_eq!(resp, b":1\r\n", "ZADD m{i} 应成功");
    }
    // 记录崩溃前 ZSCORE 抽样响应（不猜测 score 文本格式，以自一致为准）
    for i in [0, 64, 129] {
      let member = format!("m{i:07}");
      let req = format!(
        "*3\r\n$6\r\nZSCORE\r\n$4\r\nlead\r\n${}\r\n{}\r\n",
        member.len(),
        member
      )
      .into_bytes();
      sample_resp.push(send_and_recv(&mut client, &req).await?);
    }
    assert!(
      sample_resp.iter().all(|r| r.first() == Some(&b'$')),
      "抽样应为 bulk string"
    );
    // 不调 stop()：模拟崩溃，仅 AOF 磁盘帧（含 BfTreePut）可依赖
  }

  {
    let server = Arc::new(WedbServer::new(aof_args(dir.path())).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;

    for (i, expect) in [0usize, 64, 129].iter().zip(&sample_resp) {
      let member = format!("m{i:07}");
      let req = format!(
        "*3\r\n$6\r\nZSCORE\r\n$4\r\nlead\r\n${}\r\n{}\r\n",
        member.len(),
        member
      )
      .into_bytes();
      let got = send_and_recv(&mut client, &req).await?;
      assert_eq!(&got, expect, "ZSCORE m{i} 崩溃后应一致");
    }

    server.stop().await?;
  }

  info!("AOF BfTree 帧家族（Flattened ZSET score）崩溃恢复测试通过");
  OK
}

/// 周期档（aof_commit_ms > 0）：写入后等待周期任务提交，崩溃式退出后仍恢复
#[compio::test]
async fn test_aof_periodic_commit_recovery() -> Void {
  let dir = tempdir()?;
  let args = ServerArgs {
    aof_commit_ms: 200,
    ..aof_args(dir.path())
  };

  {
    let server = Arc::new(WedbServer::new(args.clone()).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;
    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$4\r\nperd\r\n$4\r\nkey1\r\n"
      )
      .await?,
      b"+OK\r\n"
    );
    // 等待周期任务至少完成一次提交（间隔 200ms，取 3 倍余量）
    compio::time::sleep(std::time::Duration::from_millis(600)).await;
    // 崩溃式退出
  }

  {
    let server = Arc::new(WedbServer::new(args).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$4\r\nperd\r\n").await?,
      b"$4\r\nkey1\r\n"
    );
    server.stop().await?;
  }

  info!("AOF 周期档提交恢复测试通过");
  OK
}

/// 手动档（aof_commit_ms = -1）负向验证：无 SAVE、无停机提交时崩溃即丢，
/// 兑现"不保证完整持久性"文档承诺——防止静默违背该取舍
#[compio::test]
async fn test_aof_manual_mode_crash_loses_uncommitted() -> Void {
  let dir = tempdir()?;
  let args = ServerArgs {
    aof_commit_ms: -1,
    ..aof_args(dir.path())
  };

  {
    let server = Arc::new(WedbServer::new(args.clone()).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;
    assert_eq!(
      send_and_recv(
        &mut client,
        b"*3\r\n$3\r\nSET\r\n$6\r\nmanual\r\n$3\r\nval\r\n"
      )
      .await?,
      b"+OK\r\n"
    );
    // 崩溃式退出：未 SAVE、未停机提交 → 环形缓冲帧全部丢失
  }

  {
    let server = Arc::new(WedbServer::new(args).await?);
    let addr = start(&server).await?;
    let mut client = TcpStream::connect(addr).await?;
    assert_eq!(
      send_and_recv(&mut client, b"*2\r\n$3\r\nGET\r\n$6\r\nmanual\r\n").await?,
      b"$-1\r\n"
    );
    server.stop().await?;
  }

  info!("AOF 手动档崩溃丢弃负向验证通过");
  OK
}
