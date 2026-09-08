use std::{sync::Arc, time::Duration};

use aok::{OK, Void};
use compio::{
  BufResult,
  buf::IoBufMut,
  io::{AsyncRead, AsyncWriteExt},
  net::{TcpListener, TcpStream},
  runtime::spawn,
  time::sleep,
};
use log::info;
use wedb_net::{LimitedFixedBufferPool, PooledReceiveBuffer, SendBuffer};

/// 发送缓冲区零分配格式化与内存复用单元测试
#[test]
fn test_send_buffer_recycle() {
  const INITIAL_CAP: usize = 64;
  let mut sbuf = SendBuffer::new(INITIAL_CAP);
  sbuf.write_ok();
  sbuf.write_integer(12345);
  sbuf.write_double_bulk(42.5);
  sbuf.write_null();
  sbuf.write_null_array();

  assert!(!sbuf.is_empty());
  let out = sbuf.take();
  assert!(sbuf.is_empty());

  // 验证序列化格式
  assert_eq!(out, b"+OK\r\n:12345\r\n$4\r\n42.5\r\n$-1\r\n*-1\r\n");

  // 归还复用
  sbuf.recycle(out);
  sbuf.write_pong();
  let out2 = sbuf.take();
  assert_eq!(out2, b"+PONG\r\n");
}

/// LimitedFixedBufferPool 2 的幂次分级定额网络缓冲池测试
#[test]
fn test_limited_fixed_buffer_pool() {
  const MIN_ALLOC: usize = 4096;
  const MAX_PER_LEVEL: usize = 4;
  const NUM_LEVELS: usize = 4;
  const MAX_ALLOC: usize = MIN_ALLOC << (NUM_LEVELS - 1); // 32768

  let pool = LimitedFixedBufferPool::new(MIN_ALLOC, MAX_PER_LEVEL, NUM_LEVELS);
  assert_eq!(pool.min_allocation_size(), MIN_ALLOC);
  assert_eq!(pool.max_allocation_size(), MAX_ALLOC);

  // 1. 首次借出（池空，创建新 buffer）
  let b1 = pool.get(4000);
  assert!(b1.capacity() >= MIN_ALLOC);
  assert_eq!(pool.pool_hits(), 0);

  // 2. 归还并再次借出（命中缓存）
  pool.return_buffer(b1);
  assert_eq!(pool.current_cached_count(), 1);

  let _b2 = pool.get(4000);
  assert_eq!(pool.pool_hits(), 1);
  assert_eq!(pool.current_cached_count(), 0);

  // 3. 多级分配
  let b_8k = pool.get(7000);
  assert!(b_8k.capacity() >= 8192);

  // 4. 定额上限限制（最多缓存 4 个）
  for _ in 0..6 {
    pool.return_buffer(Vec::with_capacity(MIN_ALLOC));
  }
  // 4096 级别最多容纳 4 个
  assert_eq!(pool.current_cached_count(), MAX_PER_LEVEL);

  // 5. 清理全部缓存
  pool.purge();
  assert_eq!(pool.current_cached_count(), 0);
}

/// PooledReceiveBuffer 池化直驱接收缓冲测试
#[compio::test]
async fn test_pooled_receive_buffer() -> Void {
  const INITIAL_SIZE: usize = 4096;
  const MAX_SIZE: usize = 64 * 1024;
  const READ_HINT: usize = 1024;

  let pool = LimitedFixedBufferPool::default_pool();
  let mut prb = PooledReceiveBuffer::new(pool, INITIAL_SIZE, MAX_SIZE);
  assert_eq!(prb.unparsed_len(), 0);
  assert!(prb.is_empty());
  assert!(prb.capacity() >= INITIAL_SIZE);

  // 确保至少有 1024 字节可读空间
  prb.ensure_read_capacity(READ_HINT).unwrap();

  // 模拟从网络读取第一批数据
  let mut v = prb.take_for_read();
  v.extend_from_slice(b"*1\r\n$4\r\nPING\r\n").unwrap();
  prb.put_after_read(v);

  assert_eq!(prb.unparsed_len(), 14);
  assert_eq!(prb.unparsed_slice(), b"*1\r\n$4\r\nPING\r\n");

  // 消费 4 字节
  prb.advance(4);
  assert_eq!(prb.unparsed_slice(), b"$4\r\nPING\r\n");
  assert_eq!(prb.unparsed_len(), 10);

  // 再次准备读取更多数据（验证已有数据不会丢失）
  prb.ensure_read_capacity(READ_HINT).unwrap();
  let mut v = prb.take_for_read();
  v.extend_from_slice(b"*2\r\n$4\r\nECHO\r\n$2\r\nHI\r\n")
    .unwrap();
  prb.put_after_read(v);

  assert_eq!(
    prb.unparsed_slice(),
    b"$4\r\nPING\r\n*2\r\n$4\r\nECHO\r\n$2\r\nHI\r\n"
  );

  // 全部消费完毕并 compact
  prb.advance(prb.unparsed_len());
  assert!(prb.is_empty());
  prb.compact();
  assert_eq!(prb.capacity(), INITIAL_SIZE);

  OK
}

/// PooledReceiveBuffer 真实 TCP 异步直读零拷贝测试
#[compio::test]
async fn test_pooled_receive_buffer_tcp_direct() -> Void {
  const ECHO_CMD: &[u8] = b"*2\r\n$4\r\nECHO\r\n$5\r\nHELLO\r\n";
  const PING_CMD: &[u8] = b"*1\r\n$4\r\nPING\r\n";
  const PACKET_INTERVAL: Duration = Duration::from_millis(5);
  const READ_HINT: usize = 1024;
  const INITIAL_SIZE: usize = 4096;
  const MAX_SIZE: usize = 64 * 1024;

  let listener = TcpListener::bind("127.0.0.1:0").await?;
  let addr = listener.local_addr()?;

  let client_handle = spawn(async move {
    let mut client = TcpStream::connect(addr).await?;
    let (res, _) = client.write_all(ECHO_CMD.to_vec()).await.into();
    res?;
    // 模拟跨包网络延迟
    sleep(PACKET_INTERVAL).await;
    let (res, _) = client.write_all(PING_CMD.to_vec()).await.into();
    res?;
    OK
  });

  let (mut server_stream, _) = listener.accept().await?;
  let pool = LimitedFixedBufferPool::default_pool();
  let mut prb = PooledReceiveBuffer::new(pool, INITIAL_SIZE, MAX_SIZE);

  // 第一批读取
  prb.ensure_read_capacity(READ_HINT).unwrap();
  let raw_buf = prb.take_for_read();
  let BufResult(res, returned_buf) = server_stream.read(raw_buf).await;
  prb.put_after_read(returned_buf);
  let n1 = res?;
  assert!(n1 > 0);
  assert!(prb.unparsed_len() >= n1);
  assert!(prb.unparsed_slice().starts_with(ECHO_CMD));

  // 消费第一条命令
  prb.advance(ECHO_CMD.len());

  // 确定性循环读取第二条命令（消除时序竞态）
  while prb.unparsed_len() < PING_CMD.len() {
    prb.ensure_read_capacity(READ_HINT).unwrap();
    let raw_buf = prb.take_for_read();
    let BufResult(res, returned_buf) = server_stream.read(raw_buf).await;
    prb.put_after_read(returned_buf);
    let n2 = res?;
    if n2 == 0 {
      break;
    }
  }

  assert!(prb.unparsed_slice().starts_with(PING_CMD));
  prb.advance(PING_CMD.len());
  assert!(prb.is_empty());

  let _ = client_handle.await;
  info!("池化接收缓冲 TCP 异步直读测试通过");
  OK
}

/// 缓冲池边缘尺寸与越界分配回收行为验证
#[test]
fn test_buffer_pool_edge_and_capacity_behavior() {
  const MIN_ALLOC: usize = 2048;
  const MAX_PER_LEVEL: usize = 2;
  const NUM_LEVELS: usize = 3;
  const MAX_ALLOC: usize = MIN_ALLOC << (NUM_LEVELS - 1); // 8192

  let pool = LimitedFixedBufferPool::new(MIN_ALLOC, MAX_PER_LEVEL, NUM_LEVELS);
  assert_eq!(pool.min_allocation_size(), MIN_ALLOC);
  assert_eq!(pool.max_allocation_size(), MAX_ALLOC);

  assert_eq!(pool.num_levels(), NUM_LEVELS);
  assert_eq!(pool.max_entries_per_level(), MAX_PER_LEVEL);

  // 1. 请求小于 min_size，应对齐至 min_size (2048)
  let b_sub = pool.get(512);
  assert!(b_sub.capacity() >= MIN_ALLOC);

  // 2. 借出超出最大受管尺寸 (8192) 的缓冲区：直接走堆分配且记录越界
  let b_huge = pool.get(16384);
  assert!(b_huge.capacity() >= 16384);
  pool.return_buffer(b_huge); // 越界缓冲区不应入池
  assert_eq!(pool.current_cached_count(), 0);
  assert_eq!(pool.total_out_of_bounds(), 1);

  // 3. 正常层级分配与归还
  pool.return_buffer(b_sub);
  assert_eq!(pool.current_cached_count(), 1);

  // 4. 定额上限限制：每级上限为 2
  let b_extra1 = Vec::with_capacity(MIN_ALLOC);
  let b_extra2 = Vec::with_capacity(MIN_ALLOC);
  pool.return_buffer(b_extra1);
  pool.return_buffer(b_extra2); // 超出上限应丢弃
  assert_eq!(pool.current_cached_count(), MAX_PER_LEVEL);

  // 5. 非标容量缓冲区归还：不污染层级池
  let b_odd = Vec::with_capacity(3000);
  pool.return_buffer(b_odd);
  assert_eq!(pool.current_cached_count(), MAX_PER_LEVEL);

  // 6. 清理
  pool.purge();
  assert_eq!(pool.current_cached_count(), 0);

  // 7. PooledReceiveBuffer 自适应缩容：大缓冲区消费完毕后缩回 initial_size
  let pool_arc = Arc::new(pool);
  let mut prb = PooledReceiveBuffer::new(pool_arc, MIN_ALLOC, 64 * 1024);
  // 强制触发大尺寸确保读取空间翻倍
  prb.ensure_read_capacity(MAX_ALLOC).unwrap();
  assert!(prb.capacity() >= MAX_ALLOC);
  // 标记全部消费完毕
  prb.compact();
  // compact 后由于容量超过 initial_size * 2，应缩容回 initial_size
  assert_eq!(prb.capacity(), MIN_ALLOC);
}

#[test]
fn test_send_buffer_dynamic_array() {
  use wedb_net::SendBuffer;

  let mut buf = SendBuffer::new(256);
  // 1. 空数组
  let m0 = buf.start_array();
  buf.finish_array(m0, 0);
  assert_eq!(&buf.take(), b"*0\r\n");

  // 2. 带单项元素
  let m1 = buf.start_array();
  buf.write_bulk_string(b"hello");
  buf.finish_array(m1, 1);
  assert_eq!(&buf.take(), b"*1\r\n$5\r\nhello\r\n");

  // 3. 多项元素（验证前移紧凑对齐）
  let m2 = buf.start_array();
  for i in 0..12 {
    let s = i.to_string();
    buf.write_bulk_string(s.as_bytes());
  }
  buf.finish_array(m2, 12);
  let res = buf.take();
  assert!(res.starts_with(b"*12\r\n$1\r\n0\r\n"));

  // 4. 超占位容量的极端元素数（20 位数字）：头部扩宽后载荷仍精确对齐，不越界
  let m3 = buf.start_array();
  buf.write_bulk_string(b"payload");
  buf.finish_array(m3, usize::MAX);
  let res = buf.take();
  assert_eq!(
    res,
    format!("*{}\r\n$7\r\npayload\r\n", usize::MAX).into_bytes()
  );
}
