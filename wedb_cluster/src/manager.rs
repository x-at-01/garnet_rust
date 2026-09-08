use std::sync::Arc;

use parking_lot::{Mutex, RwLock, RwLockReadGuard};

use crate::{
  config::{ClusterConfig, LOCAL_WORKER_ID},
  error::{Error, Result},
  gossip::{GossipPacket, GossipTracker},
  migration::{route_request_ext, route_slot_ext},
  node::{NodeId, NodeRole},
  protocol::{format_ask_err, format_clusterdown_err, format_moved_err},
  slot::{HashSlot, SlotState, hash_slot, out_of_range},
  worker::Worker,
};

/// 客户端请求路由验证结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RouteResult {
  /// 本地节点可直接处理该请求，返回哈希槽编号
  Ok(u16),
  /// 槽位已分配到其他节点，需要重定向 (-MOVED <slot> <ip:port>)
  Moved { slot: u16, endpoint: String },
  /// 槽位正在迁移中，键在本地不存在，需临时重定向 (-ASK <slot> <ip:port>)
  Ask { slot: u16, endpoint: String },
  /// 请求涉及多个键但跨越不同哈希槽 (-CROSSSLOT)
  CrossSlot,
  /// 集群处于不可用状态或槽位未分配 (-CLUSTERDOWN)
  ClusterDown,
}

/// 集群管理器，负责维护集群拓扑、槽位分配状态与客户端请求重定向
#[derive(Debug, Clone)]
pub struct ClusterManager {
  current_config: Arc<RwLock<ClusterConfig>>,
  /// Gossip 跟踪器 (拓扑合并、禁入名单与故障判定)
  gossip: Arc<Mutex<GossipTracker>>,
  /// 状态机切换与管理操作互斥锁，保证管理操作的强序性与无数据竞争
  pub state_lock: Arc<Mutex<()>>,
}

impl Default for ClusterManager {
  fn default() -> Self {
    Self::new()
  }
}

impl ClusterManager {
  /// 创建集群管理器实例
  pub fn new() -> Self {
    Self {
      current_config: Arc::new(RwLock::new(ClusterConfig::new())),
      gossip: Arc::new(Mutex::new(GossipTracker::new())),
      state_lock: Arc::new(Mutex::new(())),
    }
  }

  /// 获取当前配置快照
  #[inline]
  pub fn current_config(&self) -> ClusterConfig {
    self.current_config.read().clone()
  }

  /// 获取当前配置的只读锁守卫（零拷贝）
  #[inline]
  pub fn config(&self) -> RwLockReadGuard<'_, ClusterConfig> {
    self.current_config.read()
  }

  /// 在只读锁借用范围内零拷贝执行内省或路由查询，避免整表克隆
  #[inline]
  pub fn with_config<R>(&self, f: impl FnOnce(&ClusterConfig) -> R) -> R {
    let conf = self.current_config.read();
    f(&conf)
  }

  /// 仅用于测试与基准评测的不安全配置覆盖
  pub fn unsafe_set_config(&self, config: ClusterConfig) {
    let _guard = self.state_lock.lock();
    *self.current_config.write() = config;
  }

  /// 初始化本地工作节点
  pub fn init_local(&self, worker: Worker) {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    conf.initialize_local_worker(worker);
  }

  // 槽位管理操作 (对标 ClusterManagerSlotState)

  /// 为本地节点添加哈希槽 (CLUSTER ADDSLOTS)
  pub fn try_add_slots(&self, slots: &[u16]) -> Result<usize> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    if conf.num_workers() == 0 {
      return Err(Error::InvalidParam("集群未初始化任何工作节点".into()));
    }

    // 预检：所有槽位必须未越界且当前处于离线状态
    for &slot in slots {
      let s = slot as usize;
      if out_of_range(s) {
        return Err(Error::SlotOutOfRange(slot));
      }
      if conf.slot_map[s].state != SlotState::Offline {
        return Err(Error::InvalidParam(format!("Slot {slot} is already busy")));
      }
    }

    let mut added = 0;
    for &slot in slots {
      let s = slot as usize;
      conf.slot_map[s] = HashSlot::new(LOCAL_WORKER_ID as u16, SlotState::Stable);
      added += 1;
    }
    Ok(added)
  }

  /// 从本地节点移除哈希槽 (CLUSTER DELSLOTS)
  /// 对标 Garnet TryRemoveSlots：仅要求槽位有属主记录（含迁移/导入中的槽位），移除后纪元提升为已知最大值加一
  pub fn try_remove_slots(&self, slots: &[u16]) -> Result<usize> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    if conf.num_workers() == 0 {
      return Err(Error::InvalidParam("集群未初始化任何工作节点".into()));
    }

    // 预检：必须存在属主记录 (离线槽位视为不属于本地)
    for &slot in slots {
      let s = slot as usize;
      if out_of_range(s) {
        return Err(Error::SlotOutOfRange(slot));
      }
      if conf.slot_map[s].worker_id == 0 {
        return Err(Error::NotSlotOwner(slot));
      }
    }

    let mut removed = 0;
    for &slot in slots {
      let s = slot as usize;
      conf.slot_map[s] = HashSlot::new(0, SlotState::Offline);
      removed += 1;
    }
    // 纪元提升为已知最大值加一 (对标 BumpLocalNodeConfigEpoch)，保证 gossip 仲裁时槽位变更可传播
    let max_epoch = conf.get_max_config_epoch();
    if let Some(w) = conf.workers.get_mut(LOCAL_WORKER_ID) {
      w.config_epoch = max_epoch + 1;
    }
    Ok(removed)
  }

  /// 准备迁出槽位 (CLUSTER SETSLOT <slot> MIGRATING <node-id>)
  pub fn try_prepare_slot_for_migration(&self, slot: u16, target_node_id: &str) -> Result<()> {
    let _guard = self.state_lock.lock();
    let s = slot as usize;
    if out_of_range(s) {
      return Err(Error::SlotOutOfRange(slot));
    }

    let mut conf = self.current_config.write();
    let target_worker_id = conf.get_worker_id_from_node_id(target_node_id);
    if target_worker_id == 0 {
      return Err(Error::UnknownNode(target_node_id.to_string()));
    }

    if let Some(local_id) = conf.local_node_id()
      && local_id.eq_ignore_ascii_case(target_node_id)
    {
      return Err(Error::MigrateToMyself);
    }

    if conf.get_node_role_from_node_id(target_node_id) != NodeRole::Primary {
      return Err(Error::TargetNotMaster(target_node_id.to_string()));
    }

    if !conf.is_local(slot, false) {
      return Err(Error::NotSlotOwner(slot));
    }

    if conf.get_state(slot) != SlotState::Stable {
      let cur_owner = conf
        .get_owner_id_from_slot(slot)
        .unwrap_or("unknown")
        .to_string();
      return Err(Error::SlotNotStable(cur_owner));
    }

    conf.update_slot_state(slot, target_worker_id as u16, SlotState::Migrating);
    Ok(())
  }

  /// 准备迁入槽位 (CLUSTER SETSLOT <slot> IMPORTING <node-id>)
  pub fn try_prepare_slot_for_import(&self, slot: u16, source_node_id: &str) -> Result<()> {
    let _guard = self.state_lock.lock();
    let s = slot as usize;
    if out_of_range(s) {
      return Err(Error::SlotOutOfRange(slot));
    }

    let mut conf = self.current_config.write();
    let source_worker_id = conf.get_worker_id_from_node_id(source_node_id);
    if source_worker_id == 0 {
      return Err(Error::UnknownNode(source_node_id.to_string()));
    }

    if conf.local_node_role() != NodeRole::Primary {
      return Err(Error::InvalidParam(format!(
        "Importing node {} is not a master node.",
        conf.local_node_role()
      )));
    }

    // 本地已经拥有的槽位（含迁出中）无需导入 (对标 Garnet IsLocal(slot, readWriteSession: false))
    if conf.is_local(slot, false) {
      return Err(Error::InvalidParam(format!(
        "This is a local hash slot {slot} and is already imported"
      )));
    }

    let not_owned = || Error::InvalidParam(format!("Slot {slot} is not owned by {source_node_id}"));
    let Some(current_owner) = conf.get_node_id_from_slot(slot) else {
      return Err(not_owned());
    };
    if !current_owner.eq_ignore_ascii_case(source_node_id) {
      return Err(not_owned());
    }

    if conf.get_state(slot) != SlotState::Stable {
      return Err(Error::SlotAlreadyImporting(source_node_id.to_string()));
    }

    conf.update_slot_state(slot, source_worker_id as u16, SlotState::Importing);
    Ok(())
  }

  /// 确认槽位最终归属 (CLUSTER SETSLOT <slot> NODE <node-id>)
  pub fn try_prepare_slot_for_ownership_change(&self, slot: u16, new_owner_id: &str) -> Result<()> {
    let _guard = self.state_lock.lock();
    let s = slot as usize;
    if out_of_range(s) {
      return Err(Error::SlotOutOfRange(slot));
    }

    let mut conf = self.current_config.write();
    let worker_id = conf.get_worker_id_from_node_id(new_owner_id);
    if worker_id == 0 {
      return Err(Error::UnknownNode(new_owner_id.to_string()));
    }

    let current_state = conf.get_state(slot);
    match current_state {
      SlotState::Migrating => {
        // 迁出方确认：槽位正式划归目标节点，恢复 STABLE
        conf.update_slot_state(slot, worker_id as u16, SlotState::Stable);
      }
      SlotState::Importing => {
        // 迁入方确认：新所有者必须是本地节点，且纪元提升以通过 gossip 传播新归属
        if let Some(local_id) = conf.local_node_id()
          && !local_id.eq_ignore_ascii_case(new_owner_id)
        {
          return Err(Error::InvalidParam(format!(
            "Input nodeid {new_owner_id} different from local nodeid {local_id}."
          )));
        }
        conf.update_slot_state(slot, LOCAL_WORKER_ID as u16, SlotState::Stable);
        let max_epoch = conf.get_max_config_epoch();
        if let Some(w) = conf.workers.get_mut(LOCAL_WORKER_ID) {
          w.config_epoch = max_epoch + 1;
        }
      }
      _ => {
        // 直接划归指定节点 (强制改派)
        conf.update_slot_state(slot, worker_id as u16, SlotState::Stable);
      }
    }
    Ok(())
  }

  /// 重置槽位状态为稳定运行 (CLUSTER SETSLOT <slot> STABLE)
  /// 对标 Garnet TryResetSlotState：迁出中的槽位回归本地所有者，导入中的槽位保留源属主记录
  pub fn try_prepare_slot_for_stable(&self, slot: u16) -> Result<()> {
    let _guard = self.state_lock.lock();
    let s = slot as usize;
    if out_of_range(s) {
      return Err(Error::SlotOutOfRange(slot));
    }
    let mut conf = self.current_config.write();
    let cur = &mut conf.slot_map[s];
    match cur.state {
      SlotState::Migrating => {
        // 迁移中断恢复：属主指针从目标节点拉回本地，避免槽位被误判为他人所有
        cur.worker_id = LOCAL_WORKER_ID as u16;
        cur.state = SlotState::Stable;
      }
      SlotState::Importing => cur.state = SlotState::Stable,
      _ => {}
    }
    Ok(())
  }

  // 节点成员与重置操作 (对标 ClusterManagerWorkerState)

  /// 握手加入节点 (CLUSTER MEET)
  /// 幂等：同地址+端口的重复 MEET 只更新既有记录，不会累积重复节点
  pub fn try_meet(&self, address: &str, port: u16, node_id: Option<&str>) -> Result<usize> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();

    // 已知节点 ID：更新其地址端口
    if let Some(id) = node_id {
      let existing = conf.get_worker_id_from_node_id(id);
      if existing > 0 {
        let w = &mut conf.workers[existing];
        w.address = address.to_string();
        w.port = port;
        return Ok(existing);
      }
    }
    // 未知节点 ID 但地址端口已存在：仅回填节点 ID
    let addr_idx = conf
      .workers
      .iter()
      .skip(1)
      .position(|w| w.address == address && w.port == port)
      .map(|p| p + 1);
    if let Some(idx) = addr_idx {
      if let Some(id) = node_id {
        conf.workers[idx].node_id = Some(id.to_string());
      }
      return Ok(idx);
    }

    let worker = Worker {
      node_id: node_id.map(|s| s.to_string()),
      address: address.to_string(),
      port,
      config_epoch: 0,
      role: NodeRole::Primary,
      // MEET 种子条目标记为握手中：真实角色/纪元/复制关系等待对端 gossip 首次确认回填
      handshake: true,
      ..Worker::default()
    };
    Ok(conf.add_worker(worker))
  }

  /// 遗忘节点 (CLUSTER FORGET <node-id>)
  /// 节点将进入禁入名单，禁入期内 gossip 报文不会将其重新收录
  pub fn try_remove_worker(&self, node_id: &str, expiry_seconds: u64) -> Result<()> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    if let Some(local_id) = conf.local_node_id()
      && local_id.eq_ignore_ascii_case(node_id)
    {
      return Err(Error::CannotForgetMyself);
    }

    if conf.get_node_role_from_node_id(node_id) == NodeRole::Unassigned {
      return Err(Error::UnknownNode(node_id.to_string()));
    }

    if conf.local_node_role() == NodeRole::Replica
      && let Some(pri_id) = conf.local_node_primary_id()
      && pri_id.eq_ignore_ascii_case(node_id)
    {
      return Err(Error::CannotForgetMyPrimary);
    }

    conf.remove_worker(node_id)?;
    self.gossip.lock().ban(node_id, expiry_seconds);
    Ok(())
  }

  /// 重置集群状态 (CLUSTER RESET SOFT / HARD)
  /// 刻意差异：不保留 C# TryReset 中计算后从未使用的禁入过期参数
  pub fn try_reset(&self, soft: bool, has_keys_in_slots: bool) -> Result<()> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();

    if has_keys_in_slots && conf.has_assigned_slots(LOCAL_WORKER_ID as u16) {
      return Err(Error::ResetWithKeysAssigned);
    }

    let local_id = if soft {
      conf.local_node_id().map(|s| s.to_string())
    } else {
      Some(NodeId::generate().to_string())
    };
    let epoch = if soft {
      conf.local_node_config_epoch()
    } else {
      0
    };
    let addr = conf.local_node_ip().to_string();
    let port = conf.local_node_port();
    let hostname = conf
      .workers
      .get(LOCAL_WORKER_ID)
      .and_then(|w| w.hostname.clone());

    let mut new_conf = ClusterConfig::new();
    let mut local = Worker::primary(local_id.unwrap_or_default(), addr, port, epoch);
    local.hostname = hostname;
    new_conf.initialize_local_worker(local);

    *conf = new_conf;
    Ok(())
  }

  /// 触发从节点故障转移 (CLUSTER FAILOVER)
  /// 将本地从节点提升为主节点，接管对应主节点负责的所有槽位，并自增配置纪元
  /// 刻意差异：FORCE/TAKEOVER 选项的复制流协调由上层 (wedb_repl / wedb_server) 驱动，不在本 crate 建模
  pub fn try_failover(&self) -> Result<i64> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    if conf.local_node_role() != NodeRole::Replica {
      return Err(Error::InvalidParam(
        "Only replica nodes can execute FAILOVER".into(),
      ));
    }
    let old_primary_id = conf
      .local_node_primary_id()
      .map(|s| s.to_string())
      .ok_or_else(|| Error::InvalidParam("No primary assigned to this replica".into()))?;

    let old_primary_wid = conf.get_worker_id_from_node_id(&old_primary_id);

    // 接管原主节点所负责的槽位
    if old_primary_wid > 0 {
      for slot in conf.slot_map.iter_mut() {
        if slot.worker_id == old_primary_wid as u16 {
          slot.worker_id = LOCAL_WORKER_ID as u16;
          slot.state = SlotState::Stable;
        }
      }
    }

    // 晋升为 Primary 并更新配置纪元
    let max_epoch = conf.get_max_config_epoch();
    let new_epoch = max_epoch + 1;
    if let Some(w) = conf.workers.get_mut(LOCAL_WORKER_ID) {
      w.role = NodeRole::Primary;
      w.replica_of_node_id = None;
      w.config_epoch = new_epoch;
    }

    Ok(new_epoch)
  }

  /// 将本地节点变为指定主节点的从副本 (CLUSTER REPLICAOF <node-id>)
  /// 对标 Garnet TryStopWrites：本地持有槽位 (含迁出中) 同步划归新主节点，等待复制流覆盖数据
  pub fn try_replicaof(&self, primary_id: &str) -> Result<()> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    let wid = conf.get_worker_id_from_node_id(primary_id);
    if wid == 0 {
      return Err(Error::UnknownNode(primary_id.to_string()));
    }
    if let Some(local_id) = conf.local_node_id()
      && local_id.eq_ignore_ascii_case(primary_id)
    {
      return Err(Error::CannotReplicateSelf);
    }
    if conf.get_node_role_from_node_id(primary_id) != NodeRole::Primary {
      return Err(Error::TargetNotMaster(primary_id.to_string()));
    }
    let wid = wid as u16;
    // 本地有效持有槽位划归新主节点 (对标 Garnet AssignSlots(GetSlotList(1), workerId, STABLE))
    for slot in conf.slot_map.iter_mut() {
      if slot.effective_worker_id() == LOCAL_WORKER_ID as u16 {
        *slot = HashSlot::new(wid, SlotState::Stable);
      }
    }
    if let Some(w) = conf.workers.get_mut(LOCAL_WORKER_ID) {
      w.role = NodeRole::Replica;
      w.replica_of_node_id = Some(primary_id.to_string());
    }
    Ok(())
  }

  /// 脱离复制关系晋升为主节点 (CLUSTER REPLICAOF NO ONE)
  /// 对标 Garnet TryResetReplica：清除复制源、角色转主并提升纪元；槽位归属保持不变
  /// 非 Redis 语义对齐：主节点调用为无害无操作，直接返回当前纪元
  pub fn try_replicaof_no_one(&self) -> Result<i64> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    if conf.local_node_role() != NodeRole::Replica {
      return Ok(conf.local_node_config_epoch());
    }
    let new_epoch = conf.get_max_config_epoch() + 1;
    if let Some(w) = conf.workers.get_mut(LOCAL_WORKER_ID) {
      w.role = NodeRole::Primary;
      w.replica_of_node_id = None;
      w.config_epoch = new_epoch;
    }
    Ok(new_epoch)
  }

  /// 更新本地配置纪元 (CLUSTER SET-CONFIG-EPOCH)
  /// 仅允许独立实例 (未接入任何对端) 将零纪元一次性提升为正数，保证纪元单调性
  pub fn try_set_local_config_epoch(&self, epoch: i64) -> Result<()> {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    if conf.num_workers() > 1 {
      return Err(Error::InvalidParam(
        "Config epoch can only be set for standalone instances".into(),
      ));
    }
    let current = conf.local_node_config_epoch();
    if current != 0 || epoch <= 0 {
      return Err(Error::EpochCollision);
    }
    if let Some(w) = conf.workers.get_mut(LOCAL_WORKER_ID) {
      w.config_epoch = epoch;
    }
    Ok(())
  }

  /// 自增集群配置纪元 (CLUSTER BUMPEPOCH)
  pub fn try_bump_cluster_epoch(&self) -> i64 {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    let max_epoch = conf.get_max_config_epoch();
    let new_epoch = max_epoch + 1;
    if let Some(w) = conf.workers.get_mut(LOCAL_WORKER_ID) {
      w.config_epoch = new_epoch;
    }
    new_epoch
  }

  // 客户端请求路由验证 (对标 ClusterSlotVerify)

  /// 单键请求槽位验证与路由决策（零拷贝、单只读锁）
  #[inline]
  pub fn verify_key(
    &self,
    key: &[u8],
    key_exists: bool,
    session_asking: bool,
    read_only: bool,
  ) -> RouteResult {
    let conf = self.current_config.read();
    route_request_ext(&conf, key, session_asking, key_exists, read_only)
  }

  /// 多键请求槽位验证与跨槽拦截 (CROSSSLOT)
  pub fn verify_keys(
    &self,
    keys: &[&[u8]],
    key_exists: bool,
    session_asking: bool,
    read_only: bool,
  ) -> RouteResult {
    if keys.is_empty() {
      return RouteResult::Ok(0);
    }
    let first_slot = hash_slot(keys[0]);
    for key in keys.iter().skip(1) {
      if hash_slot(key) != first_slot {
        return RouteResult::CrossSlot;
      }
    }
    let conf = self.current_config.read();
    route_slot_ext(&conf, first_slot, session_asking, key_exists, read_only)
  }

  /// 将路由判定结果转换为标准 RESP 协议错误帧响应
  pub fn to_resp_error(&self, res: &RouteResult) -> Option<String> {
    match res {
      RouteResult::Ok(_) => None,
      RouteResult::Moved { slot, endpoint } => Some(format_moved_err(*slot, endpoint)),
      RouteResult::Ask { slot, endpoint } => Some(format_ask_err(*slot, endpoint)),
      RouteResult::CrossSlot => {
        Some("-CROSSSLOT Keys in request don't hash to the same slot\r\n".to_string())
      }
      RouteResult::ClusterDown => Some(format_clusterdown_err("Hash slot not served")),
    }
  }

  // Gossip 报文收发 (对标 Gossip.cs TryMerge / GossipMainAsync)

  /// 构建待发送的 gossip 数据包 (携带本地稳定槽位与故障判定视图)
  pub fn build_gossip_packet(&self, sample_count: usize) -> GossipPacket {
    let conf = self.current_config.read();
    self.gossip.lock().create_packet(&conf, sample_count)
  }

  /// 合并收到的 gossip 报文，随后处理发送方与本地的纪元冲突仲裁
  /// 返回本地配置是否发生变更
  pub fn try_merge_gossip(&self, packet: &GossipPacket) -> bool {
    let _guard = self.state_lock.lock();
    let mut conf = self.current_config.write();
    let mut tracker = self.gossip.lock();
    let changed = tracker.process_packet(&mut conf, packet);
    let header = &packet.header;
    conf.resolve_epoch_collision(&header.sender_id, header.config_epoch) || changed
  }

  /// 标记本地观察到某节点疑似下线 (PFAIL)
  pub fn mark_node_pfail(&self, node_id: &str) {
    self.gossip.lock().mark_local_pfail(node_id);
  }

  /// 清除本地对某节点的疑似下线标记 (支持故障恢复)
  pub fn clear_node_pfail(&self, node_id: &str) {
    let _guard = self.state_lock.lock();
    self.gossip.lock().clear_local_pfail(node_id);
    let mut conf = self.current_config.write();
    let wid = conf.get_worker_id_from_node_id(node_id);
    if wid > 0 {
      conf.workers[wid].clear_fail();
    }
  }

  /// 执行故障多数派仲裁，返回本轮新确认 FAIL 的节点 ID 列表
  pub fn evaluate_fail_status(&self) -> Vec<String> {
    let _guard = self.state_lock.lock();
    let newly_failed = {
      let conf = self.current_config.read();
      self.gossip.lock().evaluate_fail_status(&conf)
    };
    if !newly_failed.is_empty() {
      let mut conf = self.current_config.write();
      for id in &newly_failed {
        let wid = conf.get_worker_id_from_node_id(id);
        if wid > 0 {
          conf.workers[wid].set_fail();
        }
      }
    }
    newly_failed
  }

  /// 查询节点是否已确认 FAIL
  pub fn is_node_failed(&self, node_id: &str) -> bool {
    self.gossip.lock().fail_nodes.contains(node_id)
  }
}
