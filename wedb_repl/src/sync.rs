use std::sync::{
  Arc,
  atomic::{AtomicBool, AtomicU64, Ordering},
};

use coarsetime::Clock;
use waof::{RECORD_HEADER_LEN, WalLog, WalRecord, WalScanIterator};
use wdev::{Device, SegmentedDevice};
use whasher::{GxPapayaMap, new_papaya_map};

use crate::{
  error::Result,
  history::{ReplId, ReplicationHistory},
};

/// 同步判定决策结果
#[derive(Debug, Clone, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub enum SyncDecision {
  /// 允许增量追赶，从指定起始位点流式拉取日志
  PartialResync {
    /// 授权增量的复制编号
    replid: ReplId,
    /// 增量读取的起始字节偏移量
    start_offset: u64,
  },
  /// 需触发全量快照同步
  FullResync {
    /// 主节点当前复制编号
    replid: ReplId,
    /// 全量快照基线对应的预写日志截断位点
    snapshot_offset: u64,
  },
}

/// 活跃从节点会话元数据与在途统计
#[derive(Debug)]
pub struct ReplicaSessionInfo {
  /// 从节点唯一标识 ID
  pub node_id: String,
  /// 从节点汇报的对外服务端口
  pub listening_port: u16,
  /// 从节点汇报的对外 IP
  pub ip_address: String,
  /// 从节点已确认的最新复制位点
  pub ack_offset: AtomicU64,
  /// 上次收到心跳或确认的时间戳（毫秒）
  pub last_ack_timestamp_ms: AtomicU64,
  /// 是否正在进行无盘全量快照同步
  pub is_diskless_syncing: AtomicBool,
}

impl ReplicaSessionInfo {
  /// 创建新的从节点会话记录
  pub fn new(node_id: String, listening_port: u16, ip_address: String) -> Self {
    let now = Clock::now_since_epoch().as_millis();
    Self {
      node_id,
      listening_port,
      ip_address,
      ack_offset: AtomicU64::new(0),
      last_ack_timestamp_ms: AtomicU64::new(now),
      is_diskless_syncing: AtomicBool::new(false),
    }
  }

  /// 更新该从节点的最新确认位点与时间戳
  ///
  /// 位点用 fetch_max 单调推进：乱序/迟到的旧 ACK 绝不回退安全截断水位线
  /// （对标 C# AofSyncTask 单线程消费位点天然单调的语义）；心跳时间戳总是刷新以维持活性判定
  pub fn update_ack(&self, offset: u64) {
    self.ack_offset.fetch_max(offset, Ordering::AcqRel);
    let now = Clock::now_since_epoch().as_millis();
    self.last_ack_timestamp_ms.store(now, Ordering::Release);
  }

  /// 计算从节点的滞后量，即主节点当前尾部位点减去已确认位点
  pub fn lag(&self, current_tail: u64) -> u64 {
    let ack = self.ack_offset.load(Ordering::Acquire);
    current_tail.saturating_sub(ack)
  }

  /// 检查从节点是否已超过指定毫秒数未响应心跳或位点确认
  #[inline]
  pub fn is_timed_out(&self, timeout_ms: u64) -> bool {
    let last = self.last_ack_timestamp_ms.load(Ordering::Acquire);
    let now = Clock::now_since_epoch().as_millis();
    now.saturating_sub(last) > timeout_ms
  }
}

/// 全量与增量复制协调器（管理活跃从节点会话池与汇聚安全水位）
pub struct ReplicationSyncManager {
  /// 活跃从节点连接映射表
  pub sessions: GxPapayaMap<String, Arc<ReplicaSessionInfo>>,
}

impl Default for ReplicationSyncManager {
  fn default() -> Self {
    Self::new()
  }
}

impl ReplicationSyncManager {
  /// 创建复制同步协调器
  pub fn new() -> Self {
    Self {
      sessions: new_papaya_map(),
    }
  }

  /// 判定从节点的同步协商请求应走增量还是全量
  ///
  /// 核心决策算法（严格对标双复制编号规范）：
  /// 1. 若请求复制编号为问号 '?' 或请求位点为负数，必然走全量同步。
  /// 2. 检查请求位点是否在主节点日志有效物理范围 [wal_begin, wal_tail] 内：
  ///    若超出下界（已被物理截断清理）或超出上界（非法超前），必须走全量同步。
  /// 3. 若请求复制编号与主节点当前主复制编号匹配，走增量同步。
  /// 4. 若请求复制编号与主节点上一代复制编号匹配（空编号视为无上一代，绝不匹配），
  ///    且请求位点不超过切换时刻的边界位点，同样允许走增量同步。
  /// 5. 其余情况均走全量同步。
  pub fn decide_sync(
    history: &ReplicationHistory,
    wal_begin: u64,
    wal_tail: u64,
    req_replid: &ReplId,
    req_offset: i64,
  ) -> SyncDecision {
    if req_replid.is_question_mark() || req_offset < 0 {
      return SyncDecision::FullResync {
        replid: history.primary_replid,
        snapshot_offset: wal_tail,
      };
    }

    let offset = req_offset as u64;

    // 检查位点是否在有效日志范围内
    if offset < wal_begin || offset > wal_tail {
      return SyncDecision::FullResync {
        replid: history.primary_replid,
        snapshot_offset: wal_tail,
      };
    }

    // 1. 匹配当前主节点复制编号
    let match_curr = req_replid == &history.primary_replid;

    // 2. 匹配上一代主节点复制编号（且位点不超过故障转移时的断点边界）；
    //    全 0 空编号代表"无上一代"，必须拒绝，否则默认初始化的从节点可凭伪造编号骗取增量续传
    let match_prev = !history.primary_replid2.is_empty()
      && req_replid == &history.primary_replid2
      && offset <= history.replication_offset2;

    if match_curr || match_prev {
      SyncDecision::PartialResync {
        replid: history.primary_replid,
        start_offset: offset,
      }
    } else {
      SyncDecision::FullResync {
        replid: history.primary_replid,
        snapshot_offset: wal_tail,
      }
    }
  }

  /// 注册并初始化新的从节点会话记录
  pub fn register_replica(
    &self,
    node_id: String,
    port: u16,
    ip: String,
  ) -> Arc<ReplicaSessionInfo> {
    let session = Arc::new(ReplicaSessionInfo::new(node_id.clone(), port, ip));
    self.sessions.pin().insert(node_id, Arc::clone(&session));
    session
  }

  /// 移除断开连接的从节点会话
  pub fn remove_replica(&self, node_id: &str) -> Option<Arc<ReplicaSessionInfo>> {
    self.sessions.pin().remove(node_id).cloned()
  }

  /// 获取指定从节点会话句柄
  pub fn get_replica(&self, node_id: &str) -> Option<Arc<ReplicaSessionInfo>> {
    self.sessions.pin().get(node_id).cloned()
  }

  /// 获取所有活跃从节点会话快照
  pub fn all_replicas(&self) -> Vec<Arc<ReplicaSessionInfo>> {
    self.sessions.pin().values().cloned().collect()
  }

  /// 添加从节点会话（对应微软 Garnet 添加从节点同步会话）
  pub fn add_session(&self, session: Arc<ReplicaSessionInfo>) {
    self.sessions.pin().insert(session.node_id.clone(), session);
  }

  /// 获取活跃会话数（对应微软 Garnet 获取会话数）
  pub fn num_sessions(&self) -> usize {
    self.sessions.pin().len()
  }

  /// 获取所有心跳超时的从节点 ID 列表
  pub fn find_timed_out_replicas(&self, timeout_ms: u64) -> Vec<String> {
    self
      .sessions
      .pin()
      .iter()
      .filter(|(_, r)| r.is_timed_out(timeout_ms))
      .map(|(id, _)| id.clone())
      .collect()
  }

  /// 剔除所有心跳超时的僵死从节点会话并返回其 ID 列表（单次 retain 原子遍历，对应微软 Garnet 心跳超时断开）
  pub fn evict_timed_out_replicas(&self, timeout_ms: u64) -> Vec<String> {
    let mut evicted = Vec::new();
    self.sessions.pin().retain(|id, r| {
      if r.is_timed_out(timeout_ms) {
        evicted.push(id.clone());
        false
      } else {
        true
      }
    });
    evicted
  }

  /// 清空所有活跃从节点会话并返回被清除的数量（角色降级时旧会话全部失效）
  pub fn clear_sessions(&self) -> usize {
    let pinned = self.sessions.pin();
    let count = pinned.len();
    pinned.clear();
    count
  }

  /// 获取所有从节点中最小的确认位点，保护下层日志截断（单次迭代求最小值）
  #[inline]
  pub fn min_ack_offset(&self, default_val: u64) -> u64 {
    self
      .sessions
      .pin()
      .values()
      .map(|r| r.ack_offset.load(Ordering::Acquire))
      .min()
      .unwrap_or(default_val)
  }
}

/// 增量日志流式发送驱动器（对应微软 Garnet 日志同步驱动器）
pub struct AofSyncDriver<D: Device = SegmentedDevice> {
  /// 远端从节点标识（对应微软 Garnet 获取远端节点标识）
  pub remote_node_id: String,
  /// 增量流起始位点（对应微软 Garnet 获取起始地址）
  pub start_address: u64,
  /// 上一次发送位点（对应微软 Garnet 获取上一地址）
  pub previous_address: AtomicU64,
  /// 已确认发送的水位线地址（对应微软 Garnet 获取已发送水位线地址）
  pub shipped_watermark: AtomicU64,
  /// 驱动器连接状态（对应微软 Garnet 获取连接状态）
  pub is_connected: AtomicBool,
  /// 下层预写日志实例（可选）
  wal: Option<WalLog<D>>,
  /// 预写日志扫描迭代器（可选）
  iterator: Option<WalScanIterator<D>>,
  /// 当前消费位点
  cur_offset: u64,
}

impl AofSyncDriver<SegmentedDevice> {
  /// 创建轻量级元数据流驱动器（主要用于位点追踪与状态校验）
  pub fn new(remote_node_id: String, start_address: u64) -> Self {
    Self {
      remote_node_id,
      start_address,
      previous_address: AtomicU64::new(start_address),
      shipped_watermark: AtomicU64::new(start_address),
      is_connected: AtomicBool::new(true),
      wal: None,
      iterator: None,
      cur_offset: start_address,
    }
  }
}

impl<D: Device> AofSyncDriver<D> {
  /// 基于底层物理预写日志创建流驱动器
  pub fn with_wal(remote_node_id: String, wal: WalLog<D>, start_offset: u64) -> Self {
    let committed = wal.committed_until_address();
    let iterator = wal.scan(start_offset, committed);
    Self {
      remote_node_id,
      start_address: start_offset,
      previous_address: AtomicU64::new(start_offset),
      shipped_watermark: AtomicU64::new(start_offset),
      is_connected: AtomicBool::new(true),
      wal: Some(wal),
      iterator: Some(iterator),
      cur_offset: start_offset,
    }
  }

  /// 获取初始位点（对应微软 Garnet 获取起始地址）
  #[inline]
  pub const fn get_start_address(&self) -> u64 {
    self.start_address
  }

  /// 获取上一位点（对应微软 Garnet 获取上一地址）
  #[inline]
  pub fn get_previous_address(&self) -> u64 {
    self.previous_address.load(Ordering::Acquire)
  }

  /// 获取已发送水位线位点（对应微软 Garnet 获取已发送水位线地址）
  #[inline]
  pub fn get_shipped_watermark_address(&self) -> u64 {
    self.shipped_watermark.load(Ordering::Acquire)
  }

  /// 推进发送水位线（对应微软 Garnet 推进水位线）
  ///
  /// 双水位线全部用 fetch_max 单调推进：AOF 位点只增不减，并发推进时较旧的地址
  /// 绝不允许覆盖较新的地址导致水位线回退（swap 无条件写入会使 shipped_watermark 倒退）
  #[inline]
  pub fn advance_watermark(&self, new_address: u64) {
    let prev = self
      .shipped_watermark
      .fetch_max(new_address, Ordering::AcqRel);
    self.previous_address.fetch_max(prev, Ordering::AcqRel);
  }

  /// 检查是否保持连接（对应微软 Garnet 获取连接状态）
  #[inline]
  pub fn is_connected(&self) -> bool {
    self.is_connected.load(Ordering::Acquire)
  }

  /// 关闭连接并断开
  #[inline]
  pub fn close(&self) {
    self.is_connected.store(false, Ordering::Release);
  }

  /// 当前已消费扫描到的位点
  #[inline]
  pub fn current_offset(&self) -> u64 {
    self.cur_offset
  }

  /// 检查当前流式位点是否已被底层日志截断清理
  #[inline]
  pub fn is_truncated(&self) -> bool {
    if let Some(wal) = &self.wal {
      self.cur_offset < wal.begin_address()
    } else {
      false
    }
  }

  /// 异步读取下一条已提交的预写日志记录
  pub async fn next_record(&mut self) -> Result<Option<WalRecord>> {
    let (Some(wal), Some(iterator)) = (&self.wal, &mut self.iterator) else {
      return Ok(None);
    };

    if let Some(rec) = iterator.next().await? {
      self.cur_offset = rec.next_address;
      return Ok(Some(rec));
    }

    let committed = wal.committed_until_address();
    if committed > iterator.end_address() {
      iterator.set_end_address(committed);
      if let Some(rec) = iterator.next().await? {
        self.cur_offset = rec.next_address;
        return Ok(Some(rec));
      }
    }

    Ok(None)
  }

  /// 批量获取一批待发送的记录（限制最大字节数与最大记录数）
  pub async fn next_batch(
    &mut self,
    max_bytes: usize,
    max_records: usize,
  ) -> Result<Vec<WalRecord>> {
    let mut batch = Vec::with_capacity(max_records.min(64));
    let mut total_bytes = 0;

    while batch.len() < max_records && total_bytes < max_bytes {
      match self.next_record().await? {
        Some(rec) => {
          total_bytes += rec.payload.len() + RECORD_HEADER_LEN;
          batch.push(rec);
        }
        None => break,
      }
    }

    Ok(batch)
  }
}
