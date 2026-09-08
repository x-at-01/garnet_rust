use std::{
  result,
  sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
  },
  time::Duration,
};

use bytes::Bytes;
use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
  runtime::spawn,
};
use crossfire::{
  AsyncRx, MAsyncTx,
  mpsc::{Array, bounded_async},
};
use log::{debug, error};
#[cfg(unix)]
use socket2::{SockRef, TcpKeepalive};
use wdev::Device;
use wedb_acl::AccessControlList;
use wedb_pubsub::{PubSubMessage, SubscribeBroker, create_session};
use wedb_resp::{
  Error as RespError, RespReadUtils, SessionMruCache, SessionParseState, parse_session_command,
};
use wedb_txn::WatchVersionMap;
use wkv::WedbStore;

use crate::{
  buffer::{PooledReceiveBuffer, SendBuffer},
  config::NetConfig,
  error::Result,
  pool::LimitedFixedBufferPool,
  session::NetSession,
};

/// 单次网络直读前确保的最小空闲空间（字节）
const MIN_READ_SPACE: usize = 1024;
/// 发布订阅推送通道容量
const PUBSUB_CHANNEL_CAPACITY: usize = 1024;
/// 响应写出通道容量（慢客户端背压上限）
const WRITE_CHANNEL_CAPACITY: usize = 256;
/// 写出完毕后旧发送缓冲区回收通道容量
const RECYCLE_CHANNEL_CAPACITY: usize = 16;
/// 单条命令 multibulk 元素数上限（对齐 Redis proto-max-multibulk-len 默认值 1024）
const MAX_MULTIBULK_ARGS: u64 = 1024;
/// multibulk 长度头最大扫描窗口：'*' + 最多 32 位数字 + CRLF（解析器数字上限 32 位）
const MULTIBULK_HEADER_WINDOW: usize = 1 + 32 + 2;
/// 发送缓冲区高水位线：单批解析产生的响应超过该阈值时立即移交写出通道冲刷，
/// 防止慢客户端场景下响应在会话侧随命令批无界积压（对齐 C# RespServerSession
/// 输出缓冲写满即 SendAndReset 的流式冲刷背压语义）
const SEND_FLUSH_HIGH_WATERMARK: usize = 64 * 1024;
/// 写出协程单次聚合冲刷的最大分块数（上限内合批向量化写出，减少系统调用次数）
const WRITE_BATCH_MAX_CHUNKS: usize = 16;
/// TCP 保活探测空闲触发时长（秒）
const TCP_KEEPALIVE_TIME_SECS: u64 = 60;
/// TCP 保活探测重试间隔（秒）
const TCP_KEEPALIVE_INTERVAL_SECS: u64 = 10;

/// 预检 multibulk 数组长度头，拦截恶意超大 `*N` 帧
///
/// 返回 true 表示元素数超出上限须立即拒绝（对齐 Redis "invalid multibulk length"）：
/// 协议解析器会按头部元素数预预留参数数组（每元素一指针），无界 `*N` 将触发
/// 巨量内存分配请求导致分配器中止或地址空间耗尽；须在进入解析器前拦截。
/// 仅在头部完整（已见 CRLF）且数值可判定时介入，半包与非法分隔符交由解析器处理。
fn is_oversized_multibulk(head: &[u8]) -> bool {
  if head.first() != Some(&b'*') {
    return false;
  }
  let window = &head[..head.len().min(MULTIBULK_HEADER_WINDOW)];
  let Some(cr) = window.iter().position(|&b| b == b'\r') else {
    return false; // 头部未完整到达，交由解析器等待半包
  };
  if window.get(cr + 1) != Some(&b'\n') {
    return false; // 非法分隔符交由解析器报协议错误
  }
  let (negative, digits) = match &window[1..cr] {
    [sign @ (b'-' | b'+'), rest @ ..] => (*sign == b'-', rest),
    d => (false, d),
  };
  if digits.is_empty() || !digits.iter().all(|b| b.is_ascii_digit()) {
    return false; // 非数字载荷交由解析器报协议错误
  }
  // 剥离前导零后按真实数值判定（解析器允许前导零：*0003 等价 *3、*-0001 等价 NULL 数组 *-1）
  let Some(nz) = digits.iter().position(|&b| b != b'0') else {
    return false; // 数值为 0（含 -0），不超限
  };
  let digits = &digits[nz..];
  if negative {
    return digits != b"1"; // 负数中仅 -1 合法（NULL 数组），其余全超限拒绝
  }
  if digits.len() > 4 {
    return true; // 剥离前导零后仍 5 位及以上数字 >= 10000 > MAX_MULTIBULK_ARGS(1024)
  }
  // 至多 4 位数字，fold 无溢出风险
  digits
    .iter()
    .fold(0u64, |acc, &b| acc * 10 + u64::from(b - b'0'))
    > MAX_MULTIBULK_ARGS
}

/// 扫描并跳过未知命令报文（RESP 数组或内联文本行，零堆分配），返回应消费的字节数
///
/// 对齐 C# `RespCommand.INVALID` 路径：解析器回写错误后按整帧长度推进读头，
/// 连接保持开启继续服务后续命令。
fn skip_unknown_command(mut input: &[u8]) -> result::Result<usize, RespError> {
  if input.is_empty() {
    return Err(RespError::Incomplete);
  }
  let orig_len = input.len();
  if input[0] == b'*' {
    let (array_len, header_len) = RespReadUtils::try_read_signed_array_len(input)?;
    let Some(array_len) = array_len else {
      return Ok(header_len); // *-1\r\n 空数组整帧即头
    };
    input = &input[header_len..];
    for _ in 0..array_len {
      let (_, bytes) = RespReadUtils::try_slice_with_length_header(input)?;
      input = &input[bytes..];
    }
    Ok(orig_len - input.len())
  } else {
    // 内联文本行：单行消费至 CRLF
    RespReadUtils::find_crlf(input)?.map_or(Err(RespError::Incomplete), |pos| Ok(pos + 2))
  }
}

/// 将积攒的批量响应移交写出通道，并无锁回收已写出的旧缓冲区
///
/// 返回 false 表示写出协程已退出（套接字异常），调用方应立即终止会话。
/// 通道满时挂起等待：写出协程持续向套接字排水，构成对慢客户端的天然背压闭环。
#[inline]
async fn flush_send_buf(
  send_buf: &mut SendBuffer,
  recycle_rx: &AsyncRx<Array<Vec<u8>>>,
  writer_tx: &MAsyncTx<Array<OutputChunk>>,
) -> bool {
  while let Ok(buf) = recycle_rx.try_recv() {
    send_buf.recycle(buf);
  }
  let payload = send_buf.take();
  writer_tx.send(OutputChunk::Buffer(payload)).await.is_ok()
}

/// 输出数据块，支持可复用发送缓冲区、直接字节切片与向量化分散-聚合分块
pub enum OutputChunk {
  /// 命令响应批量写出缓冲区（支持零内存分配循环回收）
  Buffer(Vec<u8>),
  /// 发布订阅推送或静态响应切片
  Bytes(Bytes),
}

/// 共享服务端运行时上下文
pub struct ServerContext<D: Device + Send + Sync + 'static> {
  /// 网络配置引用
  pub config: Arc<NetConfig>,
  /// 存储引擎实例引用
  pub store: Arc<WedbStore<D>>,
  /// 访问控制列表用户权限管理器引用
  pub acl: Arc<AccessControlList>,
  /// 高并发发布订阅中继器引用
  pub pubsub_broker: Arc<SubscribeBroker>,
  /// 乐观并发控制键版本映射表引用
  pub version_map: Arc<WatchVersionMap>,
  /// 全局网络缓冲池
  pub network_pool: Arc<LimitedFixedBufferPool>,
  /// 优雅停机运行状态标识
  pub is_running: Arc<AtomicBool>,
}

/// 客户端连接处理结构体，管理单个连接的异步全双工收发生命周期
pub struct NetConnection;

impl NetConnection {
  /// 启动单个客户端连接的异步处理事件循环
  pub async fn handle<D: Device + Send + Sync + 'static>(
    stream: TcpStream,
    session_id: u64,
    ctx: Arc<ServerContext<D>>,
  ) -> Result<()> {
    if ctx.config.tcp_nodelay {
      let _ = stream.set_nodelay(true);
    }
    // 启用 TCP 保活探测：最终由内核回收对端宕机/链路中断产生的半开连接
    // （compio 0.19 的 TcpStream 未暴露 keepalive 接口，经 socket2 SockRef 借用 fd 设置）
    if ctx.config.tcp_keepalive {
      #[cfg(unix)]
      {
        let keepalive = TcpKeepalive::new()
          .with_time(Duration::from_secs(TCP_KEEPALIVE_TIME_SECS))
          .with_interval(Duration::from_secs(TCP_KEEPALIVE_INTERVAL_SECS));
        let _ = SockRef::from(&stream).set_tcp_keepalive(&keepalive);
      }
    }

    let (pubsub_session, pubsub_rx) = create_session(session_id, PUBSUB_CHANNEL_CAPACITY);
    let store_session = Arc::new(ctx.store.new_session()?);
    // 会话初始逻辑数据库：写入 StoreSession 原子变量，键命名空间前缀按需重算即时生效
    store_session.set_active_db(u64::from(ctx.config.default_db));
    let mut session = NetSession::new(
      session_id,
      Arc::clone(&ctx.acl),
      store_session,
      Arc::clone(&ctx.version_map),
      Arc::clone(&ctx.pubsub_broker),
      pubsub_session,
    );

    let (mut read_stream, mut write_stream) = stream.into_split();
    let (writer_tx, writer_rx) = bounded_async::<OutputChunk>(WRITE_CHANNEL_CAPACITY);
    let (recycle_tx, recycle_rx) = bounded_async::<Vec<u8>>(RECYCLE_CHANNEL_CAPACITY);

    // 1. 启动专用写出协程，负责将所有的命令响应及推送消息下发至套接字
    let write_handle = spawn(async move {
      while let Ok(first_chunk) = writer_rx.recv().await {
        // 上限内尽力聚批：把通道中已就绪的分块一次性向量化写出，摊薄系统调用开销
        let mut chunks = Vec::with_capacity(WRITE_BATCH_MAX_CHUNKS);
        chunks.push(first_chunk);
        while chunks.len() < WRITE_BATCH_MAX_CHUNKS
          && let Ok(next_chunk) = writer_rx.try_recv()
        {
          chunks.push(next_chunk);
        }

        if chunks.len() == 1 {
          match chunks.pop().unwrap() {
            OutputChunk::Buffer(buf) => {
              let BufResult(res, returned_buf) = write_stream.write_all(buf).await;
              if res.is_err() {
                break;
              }
              let _ = recycle_tx.try_send(returned_buf);
            }
            OutputChunk::Bytes(bytes) => {
              let BufResult(res, _) = write_stream.write_all(bytes).await;
              if res.is_err() {
                break;
              }
            }
          }
        } else {
          let mut vectored_payloads = Vec::with_capacity(chunks.len());
          for c in chunks {
            match c {
              OutputChunk::Buffer(b) => {
                vectored_payloads.push(Bytes::from(b));
              }
              OutputChunk::Bytes(b) => {
                vectored_payloads.push(b);
              }
            }
          }
          let BufResult(res, _) = write_stream.write_vectored_all(vectored_payloads).await;
          if res.is_err() {
            break;
          }
        }
      }
    });

    // 2. 启动发布订阅消息推送转发协程
    let pubsub_writer_tx = writer_tx.clone();
    let pubsub_handle = spawn(async move {
      while let Ok(msg) = pubsub_rx.recv().await {
        if matches!(msg, PubSubMessage::Close) {
          break;
        }
        let bytes = msg.to_resp2_bytes();
        if pubsub_writer_tx
          .send(OutputChunk::Bytes(bytes))
          .await
          .is_err()
        {
          break;
        }
      }
    });

    // 3. 读循环与命令解析执行
    let mut recv_buf = PooledReceiveBuffer::new(
      Arc::clone(&ctx.network_pool),
      ctx.config.initial_recv_buf_size,
      ctx.config.max_recv_buf_size,
    );
    let mut send_buf = SendBuffer::new(ctx.config.send_buf_size);
    let mut mru_cache = SessionMruCache::default();

    // 说明：compio 为完成式 IO，无多路复用 select；停机采用协同式退出——
    // 空闲阻塞在 read 上的连接将在下一个网络事件（新数据到达或对端断开）时
    // 检测停机标志后立即关闭，与 thread-per-core 模型下的零开销语义一致。
    loop {
      if !ctx.is_running.load(Ordering::Relaxed) {
        debug!("接收到停机广播信号, 优雅关闭连接 session_id={}", session_id);
        break;
      }

      if let Err(e) = recv_buf.ensure_read_capacity(MIN_READ_SPACE) {
        error!("接收缓冲区错误, session_id={}, 错误: {}", session_id, e);
        break;
      }
      let raw_buf = recv_buf.take_for_read();
      let BufResult(res, returned_buf) = read_stream.read(raw_buf).await;
      recv_buf.put_after_read(returned_buf);

      match res {
        Ok(0) => {
          debug!("客户端连接已正常断开 (EOF), session_id={}", session_id);
          break;
        }
        Ok(_n) => {
          // 读后就近二次检查停机标志：停机瞬间的在途数据不再执行，尽快释放会话资源
          if !ctx.is_running.load(Ordering::Relaxed) {
            debug!("停机窗口内收到在途数据, 主动断开 session_id={}", session_id);
            break;
          }
        }
        Err(e) => {
          debug!("客户端读取异常, session_id={}, 错误: {}", session_id, e);
          break;
        }
      };

      let mut should_close = false;
      loop {
        let mut slice = recv_buf.unparsed_slice();
        if slice.is_empty() {
          break;
        }
        // multibulk 长度头预检：拦截恶意超大 *N，防止解析器按元素数巨量预留内存
        if is_oversized_multibulk(slice) {
          send_buf.write_error(b"ERR Protocol error: invalid multibulk length");
          should_close = true;
          break;
        }
        let orig_len = slice.len();
        let mut parse_state = SessionParseState::with_mru(mru_cache);
        let cmd = match parse_session_command(&mut slice, &mut parse_state) {
          Ok(c) => {
            mru_cache = parse_state.mru();
            c
          }
          Err(RespError::Incomplete) => {
            // 半包未接收完整，等待下一次网络读取
            break;
          }
          Err(RespError::UnknownCommand(name)) => {
            // 未知命令不关闭连接（对齐 C# RespCommand.INVALID 跳过推进与 Redis 语义）：
            // 回写错误后整帧跳过继续服务；事务开启中则置脏中止，EXEC 统一报 EXECABORT
            match skip_unknown_command(recv_buf.unparsed_slice()) {
              Ok(0) | Err(RespError::Incomplete) => break, // 零消费防御或报文跨包未完整
              Ok(skip) => {
                drop(parse_state);
                send_buf.write_error_fmt(format_args!("ERR unknown command '{name}'"));
                session.on_unknown_command();
                recv_buf.advance(skip);
              }
              Err(e) => {
                send_buf.write_error_fmt(format_args!("ERR Protocol error: {e:?}"));
                should_close = true;
                break;
              }
            }
            continue;
          }
          Err(e) => {
            send_buf.write_error_fmt(format_args!("ERR Protocol error: {e:?}"));
            should_close = true;
            break;
          }
        };

        let consumed = orig_len - slice.len();
        if consumed == 0 {
          break;
        }
        let exec_res = session.execute(cmd, &parse_state, &mut send_buf).await;
        drop(parse_state);
        recv_buf.advance(consumed);

        match exec_res {
          Ok(closed) => {
            if closed {
              should_close = true;
              break;
            }
          }
          Err(e) => {
            send_buf.write_error_fmt(format_args!("ERR Server error: {e:?}"));
            should_close = true;
            break;
          }
        }

        // 高水位冲刷：响应积压超阈值立即移交写出通道；单条命令响应不受影响，
        // 阈值内的跨命令聚合仍可继续享受向量化聚合写出（对齐 C# SendAndReset 流式冲刷）
        if send_buf.len() >= SEND_FLUSH_HIGH_WATERMARK
          && !flush_send_buf(&mut send_buf, &recycle_rx, &writer_tx).await
        {
          break;
        }
      }

      recv_buf.compact();

      // 将本批剩余积攒的响应送入写出通道
      if !send_buf.is_empty() && !flush_send_buf(&mut send_buf, &recycle_rx, &writer_tx).await {
        break;
      }

      if should_close || !ctx.is_running.load(Ordering::Relaxed) {
        break;
      }
    }

    // 4. 优雅清理资源：发送停机信号、退订频道、释放会话与发送端引用
    session.pubsub_session.close();
    ctx.pubsub_broker.remove_subscription(session_id);
    drop(session);
    drop(writer_tx);
    drop(recycle_rx);

    // 等待后台转发协程与写出协程完全退出
    let _ = pubsub_handle.await;
    let _ = write_handle.await;

    Ok(())
  }
}
