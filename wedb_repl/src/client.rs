use core::{mem::take, time::Duration};
use std::sync::Arc;

use compio::{
  BufResult,
  io::{AsyncRead, AsyncWriteExt},
  net::TcpStream,
  time::sleep,
};
use log::{debug, info, warn};
use parking_lot::Mutex;
use waof::WalLog;
use wdev::Device;
use wedb_resp::{Error as RespError, RespCommand, SessionParseState, parse_session_command};
use wedb_redis::prelude::*;
use wkv::{TreeTuning, WedbStore};

use crate::{
  error::{Error, Result},
  history::ReplId,
  manager::ReplicationManager,
  protocol::{
    MasterResponse, encode_auth, encode_ping, encode_psync, encode_replconf_ack,
    encode_replconf_capa, encode_replconf_ip, encode_replconf_port, parse_master_response,
    parse_u64_bytes,
  },
  role::RecoveryStatus,
  sync::SyncDecision,
};

/// 复制回放默认缓存大小 (64KB)
const DEFAULT_REPL_CACHE_SIZE: usize = 64 * 1024;
/// 复制回放默认最小记录长度 (8B)
const DEFAULT_REPL_MIN_RECORD: usize = 8;
/// 复制回放默认最大记录长度 (1024B)
const DEFAULT_REPL_MAX_RECORD: usize = 1024;
/// 复制回放默认最大键长度 (128B)
const DEFAULT_REPL_MAX_KEY_LEN: usize = 128;
/// 复制回放默认叶子页面大小 (4096B)
const DEFAULT_REPL_LEAF_PAGE_SIZE: usize = 4096;
/// 握手阶段响应累积缓冲区上限 (防异常主节点无界发送导致内存放大)
const MAX_HANDSHAKE_BUFFER: usize = 64 * 1024;
/// RI.CREATE 选项关键字最大字节数 (cachesize/minrecord/maxrecord/maxkeylen 均为 9)
const RI_OPT_KW_MAX_LEN: usize = 9;

/// 快速从 ASCII 字节切片解析 usize (零分配、无额外 UTF-8 校验)
#[inline]
fn parse_usize_bytes(bytes: &[u8]) -> usize {
  parse_u64_bytes(bytes).unwrap_or(0) as usize
}

/// 从节点主从复制五步握手状态阶段
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HandshakeStep {
  /// 初始就绪态（等待发送保活探针）
  #[default]
  Initial,
  /// 保活探针已验证，等待发送密码认证
  PingDone,
  /// 鉴权已通过（或无需密码），等待发送端口汇报
  AuthDone,
  /// 端口已汇报并确认，等待发送 IP 汇报
  PortDone,
  /// IP 已汇报并确认，等待发送能力协商
  IpDone,
  /// 特性能力已协商并确认，等待发送同步协商
  CapaDone,
  /// 握手全流程协商完成，已建立主从复制流
  Established,
}

/// 复制握手状态机驱动器（高内聚、零网络耦合，便于单元测试）
#[derive(Debug, Clone)]
pub struct HandshakeDriver {
  /// 当前握手阶段
  pub step: HandshakeStep,
  /// 协商上报的对外服务端口
  pub listening_port: u16,
  /// 协商上报的对外 IP 地址
  pub ip_address: String,
  /// 可选的主节点认证密码
  pub auth_password: Option<String>,
  /// 从节点当前缓存的复制编号（未同步过则为问号编号）
  pub replid: ReplId,
  /// 从节点当前缓存的复制偏移量（未同步过则为 -1）
  pub offset: i64,
}

impl HandshakeDriver {
  /// 创建新的握手驱动器
  pub fn new(
    listening_port: u16,
    ip_address: String,
    auth_password: Option<String>,
    replid: ReplId,
    offset: i64,
  ) -> Self {
    Self {
      step: HandshakeStep::Initial,
      listening_port,
      ip_address,
      auth_password,
      replid,
      offset,
    }
  }

  /// 重置握手驱动器至初始就绪态
  #[inline]
  pub fn reset(&mut self) {
    self.step = HandshakeStep::Initial;
  }

  /// 获取当前阶段应当发往主节点的命令字节流
  pub fn next_command(&self) -> Result<Vec<u8>> {
    match self.step {
      HandshakeStep::Initial => Ok(encode_ping().to_vec()),
      HandshakeStep::PingDone => {
        if let Some(pwd) = &self.auth_password {
          Ok(encode_auth(pwd))
        } else {
          Ok(encode_replconf_port(self.listening_port))
        }
      }
      HandshakeStep::AuthDone => Ok(encode_replconf_port(self.listening_port)),
      HandshakeStep::PortDone => Ok(encode_replconf_ip(&self.ip_address)),
      HandshakeStep::IpDone => Ok(encode_replconf_capa(&["eof", "psync2"])),
      HandshakeStep::CapaDone => Ok(encode_psync(&self.replid, self.offset)),
      HandshakeStep::Established => Err(Error::HandshakeFailed(
        "握手已完成，无需继续发送协商命令".to_string(),
      )),
    }
  }

  /// 接收主节点响应帧并推进握手状态机
  pub fn feed_response(&mut self, resp: &MasterResponse) -> Result<HandshakeStep> {
    match self.step {
      // 保活阶段：PONG 放行；带密码时收到 NOAUTH 直接转入认证阶段
      HandshakeStep::Initial => match resp {
        MasterResponse::Pong => {
          self.step = if self.auth_password.is_some() {
            HandshakeStep::PingDone
          } else {
            HandshakeStep::AuthDone
          };
          Ok(self.step)
        }
        MasterResponse::Error(msg) if msg.contains("NOAUTH") && self.auth_password.is_some() => {
          self.step = HandshakeStep::PingDone;
          Ok(self.step)
        }
        MasterResponse::Error(msg) => Err(stage_err("保活", msg)),
        _ => Err(Error::HandshakeFailed("保活阶段期望收到 PONG".to_string())),
      },
      // 认证阶段：密码错误直接归因为认证失败
      HandshakeStep::PingDone => match resp {
        MasterResponse::Ok => self.advance(HandshakeStep::AuthDone),
        MasterResponse::Error(msg) => Err(Error::AuthFailed(msg.clone())),
        _ => Err(Error::HandshakeFailed("认证阶段期望收到 OK".to_string())),
      },
      HandshakeStep::AuthDone => self.expect_ok(resp, "汇报端口", HandshakeStep::PortDone),
      HandshakeStep::PortDone => self.expect_ok(resp, "汇报 IP", HandshakeStep::IpDone),
      HandshakeStep::IpDone => self.expect_ok(resp, "能力协商", HandshakeStep::CapaDone),
      // 同步协商阶段：CONTINUE（增量续订）或 FULLRESYNC（全量快照）均完成握手
      HandshakeStep::CapaDone => match resp {
        MasterResponse::Continue { .. } | MasterResponse::FullResync { .. } => {
          self.advance(HandshakeStep::Established)
        }
        MasterResponse::Error(msg) => Err(stage_err("同步协商", msg)),
        _ => Err(Error::HandshakeFailed(
          "同步协商阶段期望收到 CONTINUE 或 FULLRESYNC".to_string(),
        )),
      },
      HandshakeStep::Established => Ok(HandshakeStep::Established),
    }
  }

  /// 跃迁至下一握手阶段
  fn advance(&mut self, next: HandshakeStep) -> Result<HandshakeStep> {
    self.step = next;
    Ok(next)
  }

  /// 期望 OK 响应的通用阶段处理（消除各阶段重复的 match 臂模板）
  fn expect_ok(
    &mut self,
    resp: &MasterResponse,
    stage: &str,
    next: HandshakeStep,
  ) -> Result<HandshakeStep> {
    match resp {
      MasterResponse::Ok => self.advance(next),
      MasterResponse::Error(msg) => Err(stage_err(stage, msg)),
      _ => Err(Error::HandshakeFailed(format!("{stage}阶段期望收到 OK"))),
    }
  }
}

/// 阶段性错误统一构造（消除重复的错误消息拼接模板）
fn stage_err(stage: &str, msg: &str) -> Error {
  Error::HandshakeFailed(format!("{stage}阶段收到错误: {msg}"))
}

/// 从节点数据回放执行器（双写预写日志与本地存储回放）
pub struct ReplicaReplayer<D: Device> {
  /// 本地持久化存储引擎
  pub store: Arc<WedbStore<D>>,
  /// 本地预写日志引擎（用于物理双写对齐）
  pub wal: Option<WalLog<D>>,
  /// 流式半包累积缓冲区（防止 TCP 拆包粘包导致命令丢弃与回放错乱）
  pending_buf: Mutex<Vec<u8>>,
}

impl<D: Device> ReplicaReplayer<D> {
  /// 创建新的从节点回放器
  pub fn new(store: Arc<WedbStore<D>>, wal: Option<WalLog<D>>) -> Self {
    Self {
      store,
      wal,
      pending_buf: Mutex::new(Vec::new()),
    }
  }

  /// 清理未完成的流式半包残余数据
  #[inline]
  pub fn clear_pending(&self) {
    self.pending_buf.lock().clear();
  }

  /// 回放来自主节点的数据段：
  /// 1. 写入本地预写日志环形缓冲区，持久化对齐主从日志
  /// 2. 解码复制命令并应用到本地存储引擎（自动拼接半包，杜绝数据丢帧）
  pub async fn replay_payload(&self, payload: &[u8]) -> Result<()> {
    if payload.is_empty() {
      return Ok(());
    }

    // 1. 无锁追加到本地预写日志并提交落盘
    if let Some(wal) = &self.wal {
      wal.enqueue(payload)?;
      wal.commit().await?;
    }

    // 2. 解码并回放到本地存储引擎 (在异步执行前立即释放锁，杜绝跨 await 持有锁)
    let pending_taken = {
      let mut pending = self.pending_buf.lock();
      if pending.is_empty() {
        None
      } else {
        pending.extend_from_slice(payload);
        Some(take(&mut *pending))
      }
    };

    let mut cursor: &[u8] = match &pending_taken {
      Some(vec) => &vec[..],
      None => payload,
    };

    let mut state = SessionParseState::new();
    let session = self.store.new_session()?;

    while !cursor.is_empty() {
      state.clear();
      let prev_cursor = cursor;
      let cmd = match parse_session_command(&mut cursor, &mut state) {
        Ok(cmd) => cmd,
        Err(RespError::Incomplete) => {
          // 数据未完整接收，保留未消费数据至 pending_buf 等待后续数据流
          cursor = prev_cursor;
          break;
        }
        Err(e) => {
          // 残缺/非法帧无法通过等待后续数据修复：清空缓冲并上抛，交由上层断线重连
          self.pending_buf.lock().clear();
          let mut msg = String::from("复制流包含非法命令帧: ");
          msg.push_str(&e.to_string());
          return Err(Error::ReplayFailed(msg));
        }
      };

      match cmd {
        RespCommand::SET => {
          if let (Some(key), Some(val)) = (state.get(0), state.get(1)) {
            session.upsert(key, val).await?;
          }
        }
        RespCommand::MSET => {
          for chunk in state.slice_range(0, state.len()).as_chunks::<2>().0 {
            session.upsert(chunk[0], chunk[1]).await?;
          }
        }
        RespCommand::DEL => {
          for &key in state.slice_range(0, state.len()) {
            session.delete(key).await?;
          }
        }
        RespCommand::HSET => {
          if let Some(key) = state.get(0) {
            for chunk in state.slice_range(1, state.len()).as_chunks::<2>().0 {
              session.hset(key, chunk[0], chunk[1]).await?;
            }
          }
        }
        RespCommand::HDEL => {
          if let Some(key) = state.get(0) {
            session.hdel(key, state.slice_range(1, state.len())).await?;
          }
        }
        RespCommand::SADD => {
          if let Some(key) = state.get(0) {
            session
              .sadd(key, state.slice_range(1, state.len()).iter().copied())
              .await?;
          }
        }
        RespCommand::SREM => {
          if let Some(key) = state.get(0) {
            session.srem(key, state.slice_range(1, state.len())).await?;
          }
        }
        RespCommand::PING | RespCommand::SELECT => {
          // 主从保活心跳与库选择，静默消费无需存储写入
        }
        RespCommand::RICREATE => {
          if let Some(key) = state.get(0) {
            // 单次遍历解析调优参数（与 wkv::encode_ri_create 编码一一对应）
            let mut backend = wkv::StorageBackend::Std;
            let mut tuning = TreeTuning {
              cache_size: DEFAULT_REPL_CACHE_SIZE,
              min_record_size: DEFAULT_REPL_MIN_RECORD,
              max_record_size: DEFAULT_REPL_MAX_RECORD,
              max_key_len: DEFAULT_REPL_MAX_KEY_LEN,
              leaf_page_size: DEFAULT_REPL_LEAF_PAGE_SIZE,
            };

            let mut args = state.slice_range(1, state.len()).iter().copied();
            while let Some(opt) = args.next() {
              // 选项关键字协议约定大写，此处栈上归一化为小写以兼容大小写混用帧
              if opt.len() > RI_OPT_KW_MAX_LEN {
                continue;
              }
              let mut kw = [0u8; RI_OPT_KW_MAX_LEN];
              for (dst, src) in kw.iter_mut().zip(opt.iter()) {
                *dst = src.to_ascii_lowercase();
              }

              match &kw[..opt.len()] {
                b"memory" => backend = wkv::StorageBackend::Memory,
                b"disk" => {}
                b"cachesize" => tuning.cache_size = args.next().map(parse_usize_bytes).unwrap_or(0),
                b"minrecord" => {
                  tuning.min_record_size = args.next().map(parse_usize_bytes).unwrap_or(0);
                }
                b"maxrecord" => {
                  tuning.max_record_size = args.next().map(parse_usize_bytes).unwrap_or(0);
                }
                b"maxkeylen" => {
                  tuning.max_key_len = args.next().map(parse_usize_bytes).unwrap_or(0);
                }
                b"pagesize" => {
                  tuning.leaf_page_size = args.next().map(parse_usize_bytes).unwrap_or(0);
                }
                _ => {}
              }
            }

            if let Err(e) = session.range_index_create(key, backend, tuning).await
              && e != wkv::RangeIndexError::AlreadyExists
            {
              log::error!(
                "RI.CREATE 回放失败: key={}, err={:?}",
                String::from_utf8_lossy(key),
                e
              );
            }
          }
        }
        RespCommand::RISET => {
          if let [key, field, val, ..] = state.slice_range(0, state.len())
            && let Err(e) = session.range_index_set(key, field, val).await
          {
            log::error!(
              "RI.SET 回放失败: key={}, err={:?}",
              String::from_utf8_lossy(key),
              e
            );
          }
        }
        RespCommand::RIDEL => {
          if let [key, field, ..] = state.slice_range(0, state.len())
            && let Err(e) = session.range_index_del(key, field).await
          {
            log::error!(
              "RI.DEL 回放失败: key={}, err={:?}",
              String::from_utf8_lossy(key),
              e
            );
          }
        }
        _ => {
          debug!("从节点忽略非标准写命令回放: {:?}", cmd);
        }
      }
    }

    if !cursor.is_empty() {
      self.pending_buf.lock().extend_from_slice(cursor);
    } else if let Some(mut vec) = pending_taken {
      let mut pending = self.pending_buf.lock();
      if pending.is_empty() {
        vec.clear();
        *pending = vec;
      }
    }

    Ok(())
  }
}

/// 从节点长连接客户端驱动器
pub struct ReplicaClient<D: Device> {
  /// 主节点地址
  pub primary_addr: String,
  /// 本节点对外暴露的监听端口
  pub listening_port: u16,
  /// 本节点对外暴露的 IP 地址
  pub local_ip: String,
  /// 主节点密码
  pub auth_password: Option<String>,
  /// 本地复制管理器引用
  pub manager: Arc<ReplicationManager>,
  /// 从节点数据回放执行器
  pub replayer: Arc<ReplicaReplayer<D>>,
}

impl<D: Device> ReplicaClient<D> {
  /// 创建新的从节点后台客户端
  pub fn new(
    primary_addr: String,
    listening_port: u16,
    local_ip: String,
    auth_password: Option<String>,
    manager: Arc<ReplicationManager>,
    store: Arc<WedbStore<D>>,
    wal: Option<WalLog<D>>,
  ) -> Self {
    let replayer = Arc::new(ReplicaReplayer::new(store, wal));
    Self {
      primary_addr,
      listening_port,
      local_ip,
      auth_password,
      manager,
      replayer,
    }
  }

  /// 与主节点建立 TCP 连接并驱动五步握手状态机
  pub async fn connect_and_handshake(&self) -> Result<(TcpStream, SyncDecision)> {
    self
      .manager
      .begin_recovery(RecoveryStatus::ClusterReplicate)?;

    let handshake_res = async {
      let mut stream = TcpStream::connect(&self.primary_addr).await?;
      info!("已成功连接至主节点 {}", self.primary_addr);

      let (replid, offset) = {
        let hist = self.manager.history.read();
        if hist.primary_replid.is_empty() {
          (ReplId::question_mark(), -1i64)
        } else {
          (hist.primary_replid, hist.replication_offset as i64)
        }
      };

      let mut driver = HandshakeDriver::new(
        self.listening_port,
        self.local_ip.clone(),
        self.auth_password.clone(),
        replid,
        offset,
      );

      let mut recv_buf = Vec::with_capacity(4096);
      let mut decision: Option<SyncDecision> = None;
      let mut chunk_buf = vec![0u8; 1024];

      while driver.step != HandshakeStep::Established {
        let cmd_bytes = driver.next_command()?;
        let BufResult(res, _) = stream.write_all(cmd_bytes).await;
        res?;

        // 持续读取直到累积缓冲区中出现至少一条完整主节点响应帧（完备处理网络半包）
        let (resp, consumed) = loop {
          if let Some((resp, consumed)) = parse_master_response(&recv_buf)? {
            break (resp, consumed);
          }
          if recv_buf.len() > MAX_HANDSHAKE_BUFFER {
            return Err(Error::Protocol("握手响应帧超长".to_string()));
          }
          let BufResult(read_res, returned_buf) = stream.read(chunk_buf).await;
          chunk_buf = returned_buf;
          let n = read_res?;
          if n == 0 {
            return Err(Error::ConnectionClosed);
          }
          recv_buf.extend_from_slice(&chunk_buf[..n]);
        };

        // 消耗并丢弃已完成当前步骤的响应数据，保留后续粘包数据供下轮握手（完备处理网络粘包）
        recv_buf.drain(..consumed);

        match &resp {
          MasterResponse::Continue { replid } => {
            // 增量接续的起始位点必须为本次 PSYNC 请求所携带的位点（applied 位点），
            // 而非事后重读的 history（语义上主节点授权续传的基准就是请求位点）；
            // 负数位点（初次协商）被异常主节点批准续传时，防御性归零从日志头部追赶
            decision = Some(SyncDecision::PartialResync {
              replid: *replid,
              start_offset: driver.offset.max(0) as u64,
            });
            // PSYNC2 规范：裸 +CONTINUE（无 replid 参数，解析为空编号）表示沿用从节点当前缓存的复制编号；
            // 非空编号经 try_update_my_primary_repl_id 持 state_lock 采纳，防止与本节点并发角色切换竞争
            if !replid.is_empty() {
              self.manager.try_update_my_primary_repl_id(*replid);
            }
          }
          MasterResponse::FullResync { replid, offset } => {
            decision = Some(SyncDecision::FullResync {
              replid: *replid,
              snapshot_offset: *offset,
            });
            // 持 state_lock 原子采纳主节点下发的复制身份（编号 + 基线位点 + 积压对齐清空旧纪元历史）
            self.manager.adopt_primary_identity(*replid, *offset);
          }
          _ => {}
        }

        driver.feed_response(&resp)?;
      }

      let decision = decision
        .ok_or_else(|| Error::HandshakeFailed("握手成功完成但未取得同步决策结果".to_string()))?;

      // 增量接续：主节点紧随 +CONTINUE 直接流式下发 RESP 命令流，残余粘包数据立即回放；
      // 全量同步：后续载荷为检查点快照流，必须走检查点接收通道，绝不允许在握手通道上直接回放
      if !recv_buf.is_empty() {
        match decision {
          SyncDecision::PartialResync { .. } => self.replayer.replay_payload(&recv_buf).await?,
          SyncDecision::FullResync { .. } => warn!(
            "全量同步握手后握手通道出现 {} 字节残余载荷，已忽略（载荷应走检查点接收通道）",
            recv_buf.len()
          ),
        }
      }

      Ok((stream, decision))
    }
    .await;

    match handshake_res {
      Ok(res) => {
        self
          .manager
          .end_recovery(RecoveryStatus::ClusterReplicate)?;
        Ok(res)
      }
      Err(err) => {
        self.manager.reset_recovery();
        Err(err)
      }
    }
  }

  /// 自动重试连接与握手协商（支持网络抖动与主节点重启时的断线重连与指数退避增量续订）
  pub async fn connect_with_retry(
    &self,
    max_retries: usize,
    retry_delay_ms: u64,
  ) -> Result<(TcpStream, SyncDecision)> {
    let mut last_err = Error::ConnectionClosed;
    let mut current_delay = retry_delay_ms;
    for attempt in 0..=max_retries {
      match self.connect_and_handshake().await {
        Ok(res) => return Ok(res),
        Err(e) => {
          self.reset();
          last_err = e;
          if attempt < max_retries && current_delay > 0 {
            sleep(Duration::from_millis(current_delay)).await;
            // 指数退避：每次失败后延迟加倍（上限 30 秒防止无限膨胀）
            current_delay = current_delay.saturating_mul(2).min(30_000);
          }
        }
      }
    }
    Err(last_err)
  }

  /// 通过套接字向主节点发送心跳位点确认
  pub async fn send_ack(&self, stream: &mut TcpStream, offset: u64) -> Result<()> {
    let ack_bytes = encode_replconf_ack(offset);
    let BufResult(res, _) = stream.write_all(ack_bytes).await;
    res?;
    Ok(())
  }

  /// 异常断线时的优雅清理与状态重置
  #[inline]
  pub fn reset(&self) {
    self.replayer.clear_pending();
    self.manager.reset_recovery();
  }
}
