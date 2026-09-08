use std::{
  path::Path,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
};

use coarsetime::Clock;
use parking_lot::{Mutex, RwLock};

use crate::{
  backlog::{DEFAULT_BACKLOG_SIZE, ReplicationBacklog},
  error::{Error, Result},
  history::{ReplId, ReplicationHistory},
  role::{NodeRole, RecoveryStatus},
  sync::{ReplicaSessionInfo, ReplicationSyncManager, SyncDecision},
};

/// 核心主从复制管理器（对应微软 Garnet 复制管理器）
pub struct ReplicationManager {
  /// 当前节点角色（主节点或从节点）
  pub role: RwLock<NodeRole>,
  /// 复制历史与双复制编号拓扑状态
  pub history: RwLock<ReplicationHistory>,
  /// 当前恢复与迁移生命周期状态机
  pub recovery_status: RwLock<RecoveryStatus>,
  /// 拓扑角色与状态机切换互斥锁
  pub state_lock: Mutex<()>,
  /// 全量快照与从节点会话管理中枢
  pub sync_manager: Arc<ReplicationSyncManager>,
  /// 复制积压缓冲区 (Replication Backlog)，用于高速增量追赶
  pub backlog: Arc<RwLock<ReplicationBacklog>>,
  /// 锁定的最小服务截断位点（防止在全量快照生成与从节点追赶期间，下层物理清理关键日志段）
  pub min_served_offset: AtomicU64,
  /// 上次与主节点同步的时间戳（秒）
  pub last_primary_sync_time_secs: AtomicU64,
}

impl ReplicationManager {
  /// 创建新的复制管理器实例
  pub fn new(initial_role: NodeRole, initial_offset: u64) -> Self {
    Self::with_history(initial_role, ReplicationHistory::new(initial_offset))
  }

  /// 使用已有复制历史配置创建复制管理器
  pub fn with_history(initial_role: NodeRole, history: ReplicationHistory) -> Self {
    let mut backlog = ReplicationBacklog::new(DEFAULT_BACKLOG_SIZE);
    backlog.set_master_offset(history.replication_offset);
    Self {
      role: RwLock::new(initial_role),
      history: RwLock::new(history),
      recovery_status: RwLock::new(RecoveryStatus::NoRecovery),
      state_lock: Mutex::new(()),
      sync_manager: Arc::new(ReplicationSyncManager::new()),
      backlog: Arc::new(RwLock::new(backlog)),
      min_served_offset: AtomicU64::new(u64::MAX),
      last_primary_sync_time_secs: AtomicU64::new(0),
    }
  }

  /// 获取当前节点角色
  #[inline]
  pub fn role(&self) -> NodeRole {
    *self.role.read()
  }

  /// 检查当前节点是否为主节点
  #[inline]
  pub fn is_primary(&self) -> bool {
    self.role.read().is_primary()
  }

  /// 检查当前节点是否为从节点
  #[inline]
  pub fn is_replica(&self) -> bool {
    self.role.read().is_replica()
  }

  /// 获取当前恢复状态
  #[inline]
  pub fn recovery_status(&self) -> RecoveryStatus {
    *self.recovery_status.read()
  }

  /// 检查当前是否具备增量日志传输条件
  #[inline]
  pub fn can_stream_aof(&self) -> bool {
    self.recovery_status.read().can_stream_aof()
  }

  /// 检查当前是否处于恢复中状态
  #[inline]
  pub fn is_recovering(&self) -> bool {
    self.recovery_status.read().is_recovering()
  }

  /// 进入指定的恢复状态（原子校验前置状态）
  pub fn begin_recovery(&self, status: RecoveryStatus) -> Result<()> {
    let _guard = self.state_lock.lock();
    let current = *self.recovery_status.read();
    if current.is_recovering() && current != status {
      return Err(Error::InvalidStateTransition {
        current,
        target: status,
      });
    }
    *self.recovery_status.write() = status;
    Ok(())
  }

  /// 退出指定的恢复状态并重置为正常运行态
  pub fn end_recovery(&self, status: RecoveryStatus) -> Result<()> {
    let _guard = self.state_lock.lock();
    let current = *self.recovery_status.read();
    if current != status {
      return Err(Error::InvalidStateTransition {
        current,
        target: RecoveryStatus::NoRecovery,
      });
    }
    *self.recovery_status.write() = RecoveryStatus::NoRecovery;
    Ok(())
  }

  /// 强制重置恢复状态至就绪态
  pub fn reset_recovery(&self) {
    let _guard = self.state_lock.lock();
    *self.recovery_status.write() = RecoveryStatus::NoRecovery;
  }

  /// 提升为主节点并执行双复制编号轮转（对应微软 Garnet TryUpdateForFailover：
  /// REPLICAOF NO ONE 与故障转移在 C# 中共用同一轮转核心路径）
  ///
  /// 持有 state_lock 全程串行化：轮转历史编号、记录切换断点、对齐积压位点、
  /// 清除恢复态并切换角色一次完成
  fn rotate_and_promote(&self, current_tail: u64) -> ReplId {
    let _guard = self.state_lock.lock();
    let new_id = {
      let mut hist = self.history.write();
      *hist = hist.failover_update(current_tail);
      hist.primary_replid
    };

    self.backlog.write().set_master_offset(current_tail);
    *self.recovery_status.write() = RecoveryStatus::NoRecovery;
    *self.role.write() = NodeRole::Primary;
    new_id
  }

  /// 故障转移并切换为新主节点复制编号（对应微软 Garnet TryUpdateForFailover）
  #[inline]
  pub fn failover_to_primary(&self, current_tail: u64) -> ReplId {
    self.rotate_and_promote(current_tail)
  }

  /// 从节点脱离主节点独立运行（自立为主节点，对应 C# REPLICAOF NO ONE 处理路径）
  #[inline]
  pub fn replica_of_no_one(&self, current_tail: u64) -> ReplId {
    self.rotate_and_promote(current_tail)
  }

  /// 降级为主从拓扑中的从节点：
  /// - 清空原主角色下注册的全部从节点会话（降级后旧会话全部失效）
  /// - 重置恢复状态并切换角色
  pub fn demote_to_replica(&self) {
    let _guard = self.state_lock.lock();
    self.sync_manager.clear_sessions();
    *self.recovery_status.write() = RecoveryStatus::NoRecovery;
    *self.role.write() = NodeRole::Replica;
  }

  /// 评估并判定下游从节点发来的同步协商请求
  pub fn evaluate_psync(
    &self,
    wal_begin: u64,
    wal_tail: u64,
    req_replid: &ReplId,
    req_offset: i64,
  ) -> Result<SyncDecision> {
    let role = *self.role.read();
    if !role.is_primary() {
      return Err(Error::NotPrimary(role));
    }
    let hist = self.history.read();
    Ok(ReplicationSyncManager::decide_sync(
      &hist, wal_begin, wal_tail, req_replid, req_offset,
    ))
  }

  /// 锁定服务从节点的最小日志逻辑位点，防止其被后台截断任务清理
  #[inline]
  pub fn pin_min_served_offset(&self, offset: u64) {
    self.min_served_offset.fetch_min(offset, Ordering::SeqCst);
  }

  /// 解除对最小服务位点的锁定
  #[inline]
  pub fn unpin_min_served_offset(&self) {
    self.min_served_offset.store(u64::MAX, Ordering::Release);
  }

  /// 获取当前保护的最小服务截断位点
  #[inline]
  pub fn min_served_offset(&self) -> u64 {
    self.min_served_offset.load(Ordering::Acquire)
  }

  /// 注册从节点连接
  pub fn register_replica(
    &self,
    node_id: String,
    port: u16,
    ip: String,
  ) -> Arc<ReplicaSessionInfo> {
    self.sync_manager.register_replica(node_id, port, ip)
  }

  /// 获取指定从节点会话句柄
  #[inline]
  pub fn get_replica(&self, node_id: &str) -> Option<Arc<ReplicaSessionInfo>> {
    self.sync_manager.get_replica(node_id)
  }

  /// 更新从节点的心跳位点确认
  pub fn update_replica_ack(&self, node_id: &str, ack_offset: u64) {
    if let Some(replica) = self.sync_manager.get_replica(node_id) {
      replica.update_ack(ack_offset);
    }
  }

  /// 查询从节点的复制延迟滞后量
  pub fn get_replica_lag(&self, node_id: &str, current_tail: u64) -> Option<u64> {
    self
      .sync_manager
      .get_replica(node_id)
      .map(|rep| rep.lag(current_tail))
  }

  /// 持久化当前复制历史到指定配置文件
  pub fn save_history(&self, path: impl AsRef<Path>) -> Result<()> {
    let hist = self.history.read();
    hist.save_to_file(path)
  }

  /// 从指定配置文件加载复制历史
  pub fn load_history(&self, path: impl AsRef<Path>) -> Result<()> {
    let hist = ReplicationHistory::load_from_file(path)?;
    *self.history.write() = hist;
    Ok(())
  }

  /// 获取当前主复制编号（对应微软 Garnet 获取主复制编号方法）
  #[inline]
  pub fn get_primary_repl_id(&self) -> ReplId {
    self.history.read().primary_replid
  }

  /// 获取上一代主复制编号（对应微软 Garnet 获取上一代主复制编号方法）
  #[inline]
  pub fn get_primary_repl_id2(&self) -> ReplId {
    self.history.read().primary_replid2
  }

  /// 获取最新复制偏移量（对应微软 Garnet 获取复制偏移量方法）
  #[inline]
  pub fn get_replication_offset(&self) -> u64 {
    self.history.read().replication_offset
  }

  /// 获取上一代复制编号的有效截断边界位点（对应微软 Garnet 获取上一代复制偏移量方法）
  #[inline]
  pub fn get_replication_offset2(&self) -> u64 {
    self.history.read().replication_offset2
  }

  /// 主节点统一复制流入口：向积压缓冲区追加数据并同步推进全局复制位点，返回追加后的尾部位点
  ///
  /// 在同一临界区内嵌套持有 backlog 与 history 写锁（固定 backlog -> history 顺序防死锁），
  /// 保证 backlog.master_offset 与 history.replication_offset 单调用原子对齐，
  /// 杜绝并发调用下位点回退（history 落后于 backlog 尾部）的错位窗口。
  pub fn append_stream(&self, data: &[u8]) -> u64 {
    let mut backlog = self.backlog.write();
    backlog.feed(data);
    let new_tail = backlog.master_offset();
    self.history.write().set_offset(new_tail);
    new_tail
  }

  /// 从复制积压缓冲区中读取指定位点的数据
  #[inline]
  pub fn read_backlog(&self, offset: u64, max_bytes: usize) -> Vec<u8> {
    self.backlog.read().read_bytes(offset, max_bytes)
  }

  /// 零拷贝借用积压缓冲区中的连续切片（用于高性能网络直接发送）
  #[inline]
  pub fn with_backlog_slices<R>(
    &self,
    offset: u64,
    max_bytes: usize,
    f: impl FnOnce(&[u8], &[u8]) -> R,
  ) -> R {
    let b = self.backlog.read();
    let (s1, s2) = b.slices(offset, max_bytes);
    f(s1, s2)
  }

  /// 判断指定位点是否处于复制积压缓冲区有效区间内
  #[inline]
  pub fn is_backlog_in_range(&self, offset: u64) -> bool {
    self.backlog.read().is_offset_in_range(offset)
  }

  /// 获取复制积压缓冲区最早与最新有效位点区间 (最早有效位点, 最新主位点)
  #[inline]
  pub fn backlog_offsets(&self) -> (u64, u64) {
    let b = self.backlog.read();
    (b.first_byte_offset(), b.master_offset())
  }

  /// 更新最新复制偏移量（history 与 backlog 同临界区原子对齐，防止并发调用位点错位）
  pub fn set_replication_offset(&self, offset: u64) {
    let mut backlog = self.backlog.write();
    backlog.set_master_offset(offset);
    self.history.write().set_offset(offset);
  }

  /// 评估并处理同步协商请求
  #[inline]
  pub fn handle_psync(
    &self,
    req_replid: impl AsRef<str>,
    req_offset: i64,
    wal_begin: u64,
    wal_tail: u64,
  ) -> Result<SyncDecision> {
    let replid = ReplId::from_str_val(req_replid.as_ref())?;
    self.evaluate_psync(wal_begin, wal_tail, &replid, req_offset)
  }

  /// 快照版本轮转开始（对应微软 Garnet CheckpointVersionShiftStart）
  ///
  /// 与 C# 一致：从节点角色直接旁路，仅主节点锁定角色为只读态
  /// （ReadRole 仅锁角色突变，不阻断日志流推送，检查点轮转期间持续推流）
  pub fn checkpoint_version_shift_start(&self) {
    if !self.is_primary() {
      return;
    }
    let _guard = self.state_lock.lock();
    *self.recovery_status.write() = RecoveryStatus::ReadRole;
  }

  /// 快照版本轮转结束（对应微软 Garnet CheckpointVersionShiftEnd）
  ///
  /// 与 C# 一致：从节点角色直接旁路
  pub fn checkpoint_version_shift_end(&self) {
    if !self.is_primary() {
      return;
    }
    let _guard = self.state_lock.lock();
    *self.recovery_status.write() = RecoveryStatus::NoRecovery;
  }

  /// 获取当前安全日志位点（对应微软 Garnet 获取当前安全日志地址，综合保护检查点与活跃从节点追赶水位）
  #[inline]
  pub fn get_current_safe_aof_address(&self) -> u64 {
    let pinned = self.min_served_offset();
    let min_ack = self.sync_manager.min_ack_offset(u64::MAX);
    pinned.min(min_ack)
  }

  /// 设置安全日志位点
  #[inline]
  pub fn set_safe_aof_address(&self, addr: u64) {
    self.pin_min_served_offset(addr);
  }

  /// 更新主节点复制编号（对应微软 Garnet 尝试更新主复制编号）
  pub fn try_update_my_primary_repl_id(&self, new_id: ReplId) {
    let _guard = self.state_lock.lock();
    let mut hist = self.history.write();
    hist.update_primary_replid(new_id);
  }

  /// 从节点原子采纳主节点下发的复制身份（复制编号 + 全量快照基线位点）
  ///
  /// 全程持 state_lock，防止握手采纳期间与本节点角色切换（failover / demote）并发竞争覆盖
  /// 复制历史；backlog 与 history 在同一临界区内对齐，并清空旧纪元的积压历史，
  /// 保证全量同步基线位点不出现跨纪元错位窗口
  pub fn adopt_primary_identity(&self, replid: ReplId, offset: u64) {
    let _guard = self.state_lock.lock();
    {
      let mut hist = self.history.write();
      hist.update_primary_replid(replid);
      hist.set_offset(offset);
    }
    let mut backlog = self.backlog.write();
    backlog.set_master_offset(offset);
    backlog.clear();
  }

  /// 更新上次主节点同步时间戳（对应微软 Garnet 更新主同步时间）
  #[inline]
  pub fn update_last_primary_sync_time(&self) {
    let now = Clock::now_since_epoch().as_secs();
    self
      .last_primary_sync_time_secs
      .store(now, Ordering::Release);
  }

  /// 获取自上次主节点同步以来经过的秒数（对应微软 Garnet 获取主同步经过秒数）
  #[inline]
  pub fn get_last_primary_sync_seconds(&self) -> u64 {
    let last = self.last_primary_sync_time_secs.load(Ordering::Acquire);
    if last == 0 {
      0
    } else {
      Clock::now_since_epoch().as_secs().saturating_sub(last)
    }
  }

  /// 获取已连接的从节点数量（对应微软 Garnet 获取已连接从节点数）
  #[inline]
  pub fn get_connected_replicas_count(&self) -> usize {
    self.sync_manager.num_sessions()
  }

  /// 获取当前所有从节点会话信息（对应微软 Garnet 获取从节点信息）
  #[inline]
  pub fn get_replica_info(&self) -> Vec<Arc<ReplicaSessionInfo>> {
    self.sync_manager.all_replicas()
  }

  /// 获取所有心跳超时的从节点 ID 列表
  #[inline]
  pub fn get_timed_out_replicas(&self, timeout_ms: u64) -> Vec<String> {
    self.sync_manager.find_timed_out_replicas(timeout_ms)
  }

  /// 剔除所有心跳超时的僵死从节点会话并返回其 ID 列表（对应微软 Garnet 心跳超时断开）
  #[inline]
  pub fn evict_timed_out_replicas(&self, timeout_ms: u64) -> Vec<String> {
    self.sync_manager.evict_timed_out_replicas(timeout_ms)
  }
}
