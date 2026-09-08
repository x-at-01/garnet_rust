use std::fmt::Write;

use itoa::Buffer;

use crate::{
  error::{Error, Result},
  node::{LinkState, NodeRole},
  slot::{HashSlot, SlotBitmap, SlotState, TOTAL_HASH_SLOTS},
  worker::Worker,
};

/// 预留未分配工作节点编号
pub const RESERVED_WORKER_ID: usize = 0;
/// 本地工作节点默认编号
pub const LOCAL_WORKER_ID: usize = 1;
/// 集群配置序列化格式版本号 (1:1 对齐 Garnet ClusterConfigVersion)
pub const CLUSTER_CONFIG_VERSION: u8 = 1;

/// 集群配置定义
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterConfig {
  /// 16384 个哈希槽的状态映射表
  pub slot_map: Box<[HashSlot; TOTAL_HASH_SLOTS]>,
  /// 节点列表 (索引 0 为预留未分配节点，索引 1 为本地节点，后续为对端节点)
  pub workers: Vec<Worker>,
}

impl Default for ClusterConfig {
  fn default() -> Self {
    Self::new()
  }
}

impl ClusterConfig {
  /// 创建默认集群配置 (所有槽离线，包含一个预留节点与默认本地节点)
  pub fn new() -> Self {
    let slot_map = Box::new([HashSlot::default(); TOTAL_HASH_SLOTS]);
    let workers = vec![
      Worker::unassigned(),
      Worker {
        node_id: None,
        address: "127.0.0.1".to_string(),
        port: 0,
        config_epoch: 0,
        role: NodeRole::Unassigned,
        replica_of_node_id: None,
        replication_offset: 0,
        hostname: None,
        ..Default::default()
      },
    ];
    Self { slot_map, workers }
  }

  /// 从给定的槽位数组与节点列表构造配置
  pub fn from_slots_and_workers(
    slot_map: Box<[HashSlot; TOTAL_HASH_SLOTS]>,
    mut workers: Vec<Worker>,
  ) -> Self {
    if workers.is_empty() {
      workers.push(Worker::unassigned());
    } else {
      workers[RESERVED_WORKER_ID] = Worker::unassigned();
    }
    Self { slot_map, workers }
  }

  /// 获取已知的工作节点总数 (排除 0 号预留节点)
  #[inline]
  pub fn num_workers(&self) -> usize {
    if self.workers.is_empty() {
      0
    } else {
      self.workers.len() - 1
    }
  }

  /// 初始化本地工作节点信息
  pub fn initialize_local_worker(&mut self, worker: Worker) -> &mut Self {
    if self.workers.len() < 2 {
      self.workers.resize_with(2, Worker::default);
    }

    self.workers[LOCAL_WORKER_ID] = worker;
    self
  }

  /// 检查指定工作节点编号是否持有已分配的哈希槽
  pub fn has_assigned_slots(&self, worker_id: u16) -> bool {
    self
      .slot_map
      .iter()
      .any(|slot| slot.effective_worker_id() == worker_id)
  }

  /// 检查指定槽位是否为本地节点所管辖
  /// allow_replica_read: 是否处于只读会话 (READONLY)，允许从节点直接读取其主节点管辖的槽位
  pub fn is_local(&self, slot: u16, allow_replica_read: bool) -> bool {
    let s = slot as usize;
    if s >= TOTAL_HASH_SLOTS {
      return false;
    }
    let item = &self.slot_map[s];
    if item.worker_id == LOCAL_WORKER_ID as u16 {
      if self.is_replica() && !allow_replica_read {
        return false;
      }
      return true;
    }
    // 处于迁移中的槽位，当前本地节点仍作为原所有者服务请求
    if item.state == SlotState::Migrating {
      return true;
    }
    // 从节点只读会话：若槽位属于本地节点复制的主节点，则允许本地读取
    if allow_replica_read
      && self.is_replica()
      && item.worker_id > LOCAL_WORKER_ID as u16
      && let Some(local_pri) = self.local_node_primary_id()
      && let Some(owner) = self.workers.get(item.worker_id as usize)
      && let Some(ref owner_id) = owner.node_id
    {
      return owner_id.eq_ignore_ascii_case(local_pri);
    }
    false
  }

  /// 检查指定槽位是否处于正在导入 (Importing) 状态 (1:1 对标 Garnet IsImportingSlot)
  #[inline]
  pub fn is_importing_slot(&self, slot: u16) -> bool {
    self
      .slot_map
      .get(slot as usize)
      .is_some_and(|item| item.state == SlotState::Importing)
  }

  /// 检查指定槽位是否处于正在迁出 (Migrating) 状态 (对标 Garnet IsMigratingSlot)
  #[inline]
  pub fn is_migrating_slot(&self, slot: u16) -> bool {
    self
      .slot_map
      .get(slot as usize)
      .is_some_and(|item| item.state == SlotState::Migrating)
  }

  /// 检查指定节点 ID 是否为已知节点
  pub fn is_known(&self, node_id: &str) -> bool {
    self.get_worker_id_from_node_id(node_id) != 0
  }

  /// 本地节点是否为主节点
  #[inline]
  pub fn is_primary(&self) -> bool {
    self.local_node_role() == NodeRole::Primary
  }

  /// 本地节点是否为从节点
  #[inline]
  pub fn is_replica(&self) -> bool {
    self.local_node_role() == NodeRole::Replica
  }

  /// 获取本地节点 IP 地址
  #[inline]
  pub fn local_node_ip(&self) -> &str {
    self
      .workers
      .get(LOCAL_WORKER_ID)
      .map(|w| w.address.as_str())
      .unwrap_or("127.0.0.1")
  }

  /// 获取本地节点通信端口
  #[inline]
  pub fn local_node_port(&self) -> u16 {
    self
      .workers
      .get(LOCAL_WORKER_ID)
      .map(|w| w.port)
      .unwrap_or(0)
  }

  /// 获取本地节点唯一 ID
  #[inline]
  pub fn local_node_id(&self) -> Option<&str> {
    self
      .workers
      .get(LOCAL_WORKER_ID)
      .and_then(|w| w.node_id.as_deref())
  }

  /// 获取本地节点角色
  #[inline]
  pub fn local_node_role(&self) -> NodeRole {
    self
      .workers
      .get(LOCAL_WORKER_ID)
      .map(|w| w.role)
      .unwrap_or(NodeRole::Unassigned)
  }

  /// 获取本地节点配置纪元
  #[inline]
  pub fn local_node_config_epoch(&self) -> i64 {
    self
      .workers
      .get(LOCAL_WORKER_ID)
      .map(|w| w.config_epoch)
      .unwrap_or(0)
  }

  /// 获取本地节点复制的主节点 ID
  #[inline]
  pub fn local_node_primary_id(&self) -> Option<&str> {
    self
      .workers
      .get(LOCAL_WORKER_ID)
      .and_then(|w| w.replica_of_node_id.as_deref())
  }

  /// 根据 workerId 获取其地址和端口
  pub fn get_worker_address(&self, worker_id: usize) -> (&str, u16) {
    if let Some(w) = self.workers.get(worker_id) {
      (&w.address, w.port)
    } else {
      ("unassigned", 0)
    }
  }

  /// 根据节点 ID 查找对应 worker 索引 (未找到返回 0)
  pub fn get_worker_id_from_node_id(&self, node_id: &str) -> usize {
    for (i, w) in self.workers.iter().enumerate().skip(1) {
      if let Some(ref id) = w.node_id
        && id.eq_ignore_ascii_case(node_id)
      {
        return i;
      }
    }
    0
  }

  /// 根据节点 ID 获取其角色
  pub fn get_node_role_from_node_id(&self, node_id: &str) -> NodeRole {
    let idx = self.get_worker_id_from_node_id(node_id);
    if idx == 0 {
      NodeRole::Unassigned
    } else {
      self.workers[idx].role
    }
  }

  /// 根据节点 ID 获取其网络终结点
  pub fn get_worker_address_from_node_id(&self, node_id: &str) -> Option<(&str, u16)> {
    let idx = self.get_worker_id_from_node_id(node_id);
    if idx == 0 {
      None
    } else {
      Some((&self.workers[idx].address, self.workers[idx].port))
    }
  }

  /// 获取哈希槽的逻辑所有者节点 ID
  pub fn get_owner_id_from_slot(&self, slot: u16) -> Option<&str> {
    let s = slot as usize;
    if s >= TOTAL_HASH_SLOTS {
      return None;
    }
    let worker_id = self.slot_map[s].effective_worker_id() as usize;
    self
      .workers
      .get(worker_id)
      .and_then(|w| w.node_id.as_deref())
  }

  /// 获取哈希槽底层记录的 worker 对应节点 ID
  pub fn get_node_id_from_slot(&self, slot: u16) -> Option<&str> {
    let s = slot as usize;
    if s >= TOTAL_HASH_SLOTS {
      return None;
    }
    let worker_id = self.slot_map[s].worker_id as usize;
    self
      .workers
      .get(worker_id)
      .and_then(|w| w.node_id.as_deref())
  }

  /// 获取指定槽位的状态
  pub fn get_state(&self, slot: u16) -> SlotState {
    let s = slot as usize;
    if s >= TOTAL_HASH_SLOTS {
      SlotState::Invalid
    } else {
      self.slot_map[s].state
    }
  }

  /// 统计处于特定状态的槽位数量
  pub fn get_slot_count_for_state(&self, state: SlotState) -> usize {
    self.slot_map.iter().filter(|s| s.state == state).count()
  }

  /// 获取当前已知主节点数量
  pub fn get_primary_count(&self) -> usize {
    self
      .workers
      .iter()
      .skip(1)
      .filter(|w| w.role == NodeRole::Primary)
      .count()
  }

  /// 获取已知节点中的最大配置纪元
  pub fn get_max_config_epoch(&self) -> i64 {
    self
      .workers
      .iter()
      .skip(1)
      .map(|w| w.config_epoch)
      .max()
      .unwrap_or(0)
  }

  /// 获取所有复制指定主节点的从节点 ID 列表
  pub fn get_replica_ids(&self, primary_id: &str) -> Vec<String> {
    let mut res = Vec::new();
    for w in self.workers.iter().skip(1) {
      if let Some(ref rep_id) = w.replica_of_node_id
        && rep_id.eq_ignore_ascii_case(primary_id)
        && let Some(ref id) = w.node_id
      {
        res.push(id.clone());
      }
    }
    res
  }

  /// 获取所有复制指定主节点的从节点索引列表 (零字符串拷贝)
  pub fn get_replica_worker_ids(&self, primary_id: &str) -> Vec<usize> {
    let mut res = Vec::new();
    for (idx, w) in self.workers.iter().enumerate().skip(1) {
      if let Some(ref rep_id) = w.replica_of_node_id
        && rep_id.eq_ignore_ascii_case(primary_id)
        && w.node_id.is_some()
      {
        res.push(idx);
      }
    }
    res
  }

  /// 更新单个槽位的目标 worker 与状态
  pub fn update_slot_state(&mut self, slot: u16, worker_id: u16, state: SlotState) {
    let s = slot as usize;
    if s < TOTAL_HASH_SLOTS {
      self.slot_map[s].worker_id = worker_id;
      self.slot_map[s].state = state;
    }
  }

  /// 添加对端已知节点
  pub fn add_worker(&mut self, worker: Worker) -> usize {
    if let Some(ref new_id) = worker.node_id {
      let existing = self.get_worker_id_from_node_id(new_id);
      if existing != 0 {
        self.workers[existing] = worker;
        return existing;
      }
    }
    self.workers.push(worker);
    self.workers.len() - 1
  }

  /// 移除指定节点 (用于 CLUSTER FORGET)
  /// 对标 Garnet RemoveWorker：区分稳定归属 / 迁出目标 / 迁入源三种槽位引用关系
  pub fn remove_worker(&mut self, node_id: &str) -> Result<()> {
    let idx = self.get_worker_id_from_node_id(node_id);
    if idx == 0 {
      return Err(Error::UnknownNode(node_id.to_string()));
    }
    let removed = idx as u16;
    for slot in self.slot_map.iter_mut() {
      if slot.worker_id > removed {
        // 后续节点索引整体前移
        slot.worker_id -= 1;
      } else if slot.worker_id == removed {
        match slot.state {
          SlotState::Migrating => {
            // 被移除节点是迁出目标：槽位回归本地所有者并恢复稳定
            slot.worker_id = LOCAL_WORKER_ID as u16;
            slot.state = SlotState::Stable;
          }
          _ => {
            // 稳定归属被移除节点 / 迁入源被移除：槽位重置为离线
            slot.worker_id = 0;
            slot.state = SlotState::Offline;
          }
        }
      }
    }
    self.workers.remove(idx);
    Ok(())
  }

  /// 检查并解决纪元冲突 (符合 Redis Cluster 规范的仲裁算法)
  /// 当对端主节点与本地主节点纪元相同且持有冲突槽位时，NodeId 较小的节点递增自身纪元以解决冲突
  pub fn resolve_epoch_collision(&mut self, remote_node_id: &str, remote_epoch: i64) -> bool {
    if let Some(local_id) = self.local_node_id()
      && self.is_primary()
      && self.local_node_config_epoch() == remote_epoch
      && remote_epoch > 0
      && local_id < remote_node_id
    {
      let max_epoch = self.get_max_config_epoch();
      let new_epoch = max_epoch + 1;
      if let Some(w) = self.workers.get_mut(LOCAL_WORKER_ID) {
        w.config_epoch = new_epoch;
      }
      return true;
    }
    false
  }

  // Gossip 配置合并 (对标 Merge / MergeWorkerInfo / MergeSlotMap)

  /// 合并对端节点元信息 (对标 Garnet MergeWorkerInfo)
  /// 已知节点仅在其纪元严格更大时更新；未知节点直接收录 (标记为握手中，待直接信源确认)
  /// 返回配置是否发生变更
  pub fn merge_worker(&mut self, worker: Worker) -> bool {
    // 第三方切片信息不参与握手确认：等纪元视图不翻转，保证单调收敛到不动点
    let Worker {
      ref node_id,
      ref address,
      port,
      config_epoch,
      role,
      ref replica_of_node_id,
      ref hostname,
      ..
    } = worker;
    self.merge_worker_ref(
      WorkerMeta {
        node_id: node_id.as_deref(),
        address,
        port,
        config_epoch,
        role,
        replica_of: replica_of_node_id.as_deref(),
        hostname: hostname.as_deref(),
      },
      false,
    )
  }

  /// 借用版合并实现：gossip 稳态合并路径零分配 (仅未知节点收录或字段真变更时才构造 String)
  /// confirm_handshake 控制是否允许确认握手中的种子条目 (直接信源传 true，对标 Redis 直接 PONG)
  pub(crate) fn merge_worker_ref(&mut self, meta: WorkerMeta<'_>, confirm_handshake: bool) -> bool {
    let WorkerMeta {
      node_id,
      address,
      port,
      config_epoch,
      role,
      replica_of,
      hostname,
    } = meta;
    let Some(new_id) = node_id.filter(|id| !id.is_empty()) else {
      return false;
    };
    let Some(idx) = self
      .workers
      .iter()
      .skip(1)
      .position(|w| w.node_id.as_deref() == Some(new_id))
      .map(|pos| pos + 1)
    else {
      // 经切片收录的未知节点尚无直接信源，标记为握手中等待其报文确认 (低频路径)
      self.workers.push(Worker {
        node_id: Some(new_id.to_owned()),
        address: address.to_owned(),
        port,
        config_epoch,
        role,
        replica_of_node_id: replica_of.map(str::to_owned),
        hostname: hostname.map(str::to_owned),
        handshake: !confirm_handshake,
        ..Worker::default()
      });
      return true;
    };

    let stored = &self.workers[idx];
    // 纪元严格更大时更新；直接信源可确认握手中的 MEET 种子 (对端纪元不落后即可)
    let handshake_ok = confirm_handshake && stored.handshake && config_epoch >= stored.config_epoch;
    if !(config_epoch > stored.config_epoch || handshake_ok) {
      return false;
    }
    // 变更探测：gossip 携带字段与存量记录完全一致时仅为握手闭环，不算配置变更
    let changed = config_epoch != stored.config_epoch
      || address != stored.address
      || port != stored.port
      || role != stored.role
      || replica_of != stored.replica_of_node_id.as_deref()
      || hostname != stored.hostname.as_deref();
    // 仅合并 gossip 携带字段，保留复制偏移量与故障/链路等运行时字段 (对标 Garnet MergeWorkerInfo)
    // 节点 ID 已按精确匹配定位，无需回写；字段仅在值变化时重新分配
    let stored = &mut self.workers[idx];
    stored.config_epoch = config_epoch;
    stored.port = port;
    stored.role = role;
    stored.handshake = false;
    if stored.address != address {
      stored.address = address.to_owned();
    }
    if stored.replica_of_node_id.as_deref() != replica_of {
      stored.replica_of_node_id = replica_of.map(str::to_owned);
    }
    if stored.hostname.as_deref() != hostname {
      stored.hostname = hostname.map(str::to_owned);
    }
    changed
  }

  /// 合并对端主节点认领的稳定槽位列表，并对不再持有的槽位执行弃权回收 (对标 Garnet MergeSlotMap)
  /// `sender_slots` 为发送方声明归其所有的稳定槽位
  /// `migrating_slots` 为发送方迁出中的槽位 (仍由发送方服务，豁免弃权回收)
  /// 认领仲裁：发送方纪元非零且 >= 当前属主纪元时跳过；否则槽位划归发送方并置为稳定
  /// 弃权回收：发送方未认领且本地仍记录由其稳定持有 / 正从其导入的槽位重置为离线，
  /// 给真实属主以重新认领的机会，避免纪元碰撞提升后归属永久错位
  /// 返回配置是否发生变更
  pub fn merge_sender_slots(
    &mut self,
    sender_id: &str,
    sender_epoch: i64,
    sender_slots: &[u16],
    migrating_slots: &[u16],
  ) -> bool {
    if sender_id.is_empty() {
      return false;
    }
    let sender_wid = self.get_worker_id_from_node_id(sender_id);
    if sender_wid == 0 {
      return false;
    }
    let sender_wid = sender_wid as u16;

    // 发送方声明保护位图：稳定认领槽位 + 迁出中槽位 (迁出槽位不参与认领，仅豁免弃权)
    let mut claimed = SlotBitmap::new();
    for &slot in sender_slots {
      claimed.set(slot);
    }
    for &slot in migrating_slots {
      claimed.set(slot);
    }

    let mut updated = false;
    for &slot in sender_slots {
      let s = slot as usize;
      if s >= TOTAL_HASH_SLOTS {
        continue;
      }
      let cur = &self.slot_map[s];
      // 纪元仲裁：当前属主纪元不低于发送方时拒绝认领 (0 号属主纪元恒为 0，离线槽位总能被认领)
      let owner_epoch = self
        .workers
        .get(cur.worker_id as usize)
        .map(|w| w.config_epoch)
        .unwrap_or(0);
      if sender_epoch != 0 && owner_epoch >= sender_epoch {
        continue;
      }
      if cur.worker_id != sender_wid || cur.state != SlotState::Stable {
        self.slot_map[s] = HashSlot::new(sender_wid, SlotState::Stable);
        updated = true;
      }
    }

    // 弃权回收扫描：仅命中发送方不再声明、且本地记录为其稳定持有或导入源的槽位
    // (对标 Garnet 中 `senderSlotMap[i]` 非稳定即跳过、MIGRATING 有效属主恒为本地的语义)
    for (s, cur) in self.slot_map.iter_mut().enumerate() {
      if cur.worker_id == sender_wid
        && matches!(cur.state, SlotState::Stable | SlotState::Importing)
        && !claimed.is_set(s as u16)
      {
        *cur = HashSlot::default();
        updated = true;
      }
    }
    updated
  }

  /// 生成 CLUSTER INFO 响应报文
  pub fn get_cluster_info(&self) -> String {
    let mut stable_count = 0;
    let mut fail_count = 0;
    for slot in self.slot_map.iter() {
      match slot.state {
        SlotState::Stable => stable_count += 1,
        SlotState::Fail => fail_count += 1,
        _ => {}
      }
    }
    let state_str = if fail_count > 0 { "fail" } else { "ok" };
    let mut buf = Buffer::new();
    let mut s = String::from("cluster_state:");
    s.push_str(state_str);
    s.push_str("\r\ncluster_slots_assigned:");
    s.push_str(buf.format(stable_count));
    s.push_str("\r\ncluster_slots_ok:");
    s.push_str(buf.format(stable_count));
    s.push_str("\r\ncluster_slots_pfail:");
    s.push_str(buf.format(fail_count));
    s.push_str("\r\ncluster_slots_fail:");
    s.push_str(buf.format(fail_count));
    s.push_str("\r\ncluster_known_nodes:");
    s.push_str(buf.format(self.num_workers()));
    s.push_str("\r\ncluster_size:");
    s.push_str(buf.format(self.get_primary_count()));
    s.push_str("\r\ncluster_current_epoch:");
    s.push_str(buf.format(self.get_max_config_epoch()));
    s.push_str("\r\ncluster_my_epoch:");
    s.push_str(buf.format(self.local_node_config_epoch()));
    s.push_str("\r\ncluster_stats_messages_sent:0\r\ncluster_stats_messages_received:0\r\n");
    s
  }

  /// 生成 CLUSTER NODES 文本报文
  pub fn get_cluster_nodes(&self) -> String {
    let mut out = String::with_capacity(self.workers.len().saturating_mul(256));
    for (i, w) in self.workers.iter().enumerate().skip(1) {
      let node_id = w
        .node_id
        .as_deref()
        .unwrap_or("0000000000000000000000000000000000000000");
      let port = w.port;
      let cport = port.saturating_add(10000);
      let host_suffix = w
        .hostname
        .as_deref()
        .map(|h| {
          let mut s = String::from(",");
          s.push_str(h);
          s
        })
        .unwrap_or_default();

      let myself_flag = if i == LOCAL_WORKER_ID { "myself," } else { "" };
      let role_flag = if w.role == NodeRole::Primary {
        "master"
      } else {
        "slave"
      };
      let fail_flag = if w.is_fail {
        ",fail"
      } else if w.is_pfail {
        ",fail?"
      } else {
        ""
      };
      let primary_id = if w.role == NodeRole::Replica {
        w.replica_of_node_id.as_deref().unwrap_or("-")
      } else {
        "-"
      };
      let link_state = w.link_state.as_str();

      let _ = write!(
        out,
        "{} {}:{}@{}{} {}{}{} {} 0 0 {} {}",
        node_id,
        w.address.as_str(),
        port,
        cport,
        host_suffix.as_str(),
        myself_flag,
        role_flag,
        fail_flag,
        primary_id,
        w.config_epoch,
        link_state
      );

      // 追加该节点连续槽位范围
      self.append_slot_ranges(&mut out, i as u16);

      // 本地节点追加特殊的迁移/导入状态槽标记
      if i == LOCAL_WORKER_ID {
        self.append_special_states(&mut out);
      }

      out.push('\n');
    }
    out
  }

  /// 为节点追加连续哈希槽范围
  pub(crate) fn append_slot_ranges(&self, out: &mut String, worker_id: u16) {
    let mut start = u16::MAX;
    let mut end = 0;
    let mut buf = itoa::Buffer::new();

    for (i, slot) in self.slot_map.iter().enumerate() {
      let idx = i as u16;
      if slot.effective_worker_id() == worker_id && slot.state != SlotState::Offline {
        if start == u16::MAX {
          start = idx;
        }
        end = idx;
      } else if start != u16::MAX {
        push_slot_range(out, &mut buf, start, end);
        start = u16::MAX;
      }
    }
    if start != u16::MAX {
      push_slot_range(out, &mut buf, start, end);
    }
  }

  /// 追加特殊槽位状态标记 (如 `[slot->-target_node_id]` 或 `[slot-<-source_node_id]`)
  pub(crate) fn append_special_states(&self, out: &mut String) {
    let mut buf = itoa::Buffer::new();
    for (i, slot) in self.slot_map.iter().enumerate() {
      let target_worker_id = slot.worker_id as usize;
      if let Some(target_worker) = self.workers.get(target_worker_id)
        && let Some(ref target_id) = target_worker.node_id
      {
        // 状态标记 direction：迁出 "->-" / 迁入 "-<-"
        let arrow = match slot.state {
          SlotState::Migrating => "->-",
          SlotState::Importing => "-<-",
          _ => continue,
        };
        out.push_str(" [");
        out.push_str(buf.format(i));
        out.push_str(arrow);
        out.push_str(target_id);
        out.push(']');
      }
    }
  }

  /// 生成 CLUSTER SLOTS RESP 协议数组报文
  pub fn get_slots_info(&self) -> String {
    let mut ranges = Vec::new();
    let mut slot_start = 0;

    while slot_start < TOTAL_HASH_SLOTS {
      if self.slot_map[slot_start].state == SlotState::Offline {
        slot_start += 1;
        continue;
      }
      let curr_worker_id = self.slot_map[slot_start].effective_worker_id();
      let mut slot_end = slot_start;
      while slot_end + 1 < TOTAL_HASH_SLOTS
        && self.slot_map[slot_end + 1].state != SlotState::Offline
        && self.slot_map[slot_end + 1].effective_worker_id() == curr_worker_id
      {
        slot_end += 1;
      }

      // 属主索引不在节点表内 (直接构造的非法配置)：跳过该槽位段以保证不 panic
      let Some(worker) = self.workers.get(curr_worker_id as usize) else {
        slot_start = slot_end + 1;
        continue;
      };
      let node_id = worker.node_id.as_deref().unwrap_or("");
      let replica_wids = self.get_replica_worker_ids(node_id);
      ranges.push((
        slot_start,
        slot_end,
        worker.address.as_str(),
        worker.port,
        node_id,
        worker.hostname.as_deref(),
        replica_wids,
      ));

      slot_start = slot_end + 1;
    }

    let mut buf = Buffer::new();
    let mut sb = String::from("*");
    sb.push_str(buf.format(ranges.len()));
    sb.push_str("\r\n");
    for (start, end, ip, port, node_id, hostname, replica_wids) in ranges {
      let entry_count = 3 + replica_wids.len();
      let _ = write!(sb, "*{}\r\n:{}\r\n:{}\r\n", entry_count, start, end);

      // 主节点网络终结点信息
      append_node_resp_meta(&mut sb, ip, port, node_id, hostname);

      // 从节点网络终结点信息
      for rep_wid in replica_wids {
        if let Some(rep) = self.workers.get(rep_wid)
          && let Some(ref rep_id) = rep.node_id
        {
          append_node_resp_meta(
            &mut sb,
            &rep.address,
            rep.port,
            rep_id,
            rep.hostname.as_deref(),
          );
        }
      }
    }
    sb
  }

  /// 生成 CLUSTER SHARDS RESP 协议数组报文 (对标 Garnet GetShardsInfo / GetShardRanges / GetWorkerReplicas)
  /// 每个分片为 4 元素数组：slots 区段整数对 + nodes 元信息表
  /// 刻意差异：健康状态取本地记录的链路状态 (本节点恒在线)，无连接级探测
  pub fn get_shards_info(&self) -> String {
    // 单次扫描槽位表聚合各属主的连续区段 (wid, start, end)，避免逐主节点全表重扫
    // 离线槽位以哨兵值归组，确保空档正确断开区段
    let offline_wid = u16::MAX;
    let mut runs: Vec<(u16, u16, u16)> = Vec::new();
    let mut run_wid = offline_wid;
    let mut run_start = 0u16;
    let mut run_open = false;
    for (i, slot) in self.slot_map.iter().enumerate() {
      let wid = if slot.state == SlotState::Offline {
        offline_wid
      } else {
        slot.effective_worker_id()
      };
      if run_open && wid != run_wid {
        if run_wid != offline_wid {
          runs.push((run_wid, run_start, (i - 1) as u16));
        }
        run_open = false;
      }
      if !run_open {
        run_wid = wid;
        run_start = i as u16;
        run_open = wid != offline_wid;
      }
    }
    if run_open {
      runs.push((run_wid, run_start, (TOTAL_HASH_SLOTS - 1) as u16));
    }

    let primary_count = self.get_primary_count();
    let mut buf = Buffer::new();
    let mut sb = String::with_capacity(128 + primary_count * 192);
    sb.push('*');
    sb.push_str(buf.format(primary_count));
    sb.push_str("\r\n");

    for (i, w) in self.workers.iter().enumerate().skip(1) {
      if w.role != NodeRole::Primary {
        continue;
      }
      let wid = i as u16;
      // 该主节点名下的连续槽位区段 (区段天然按槽位升序)
      let range_count = runs.iter().filter(|r| r.0 == wid).count();

      sb.push_str("*4\r\n$5\r\nslots\r\n*");
      sb.push_str(buf.format(range_count * 2));
      sb.push_str("\r\n");
      for &(run_wid, start, end) in &runs {
        if run_wid == wid {
          sb.push(':');
          sb.push_str(buf.format(start));
          sb.push_str("\r\n:");
          sb.push_str(buf.format(end));
          sb.push_str("\r\n");
        }
      }

      let node_id = w.node_id.as_deref().unwrap_or("");
      let replica_wids = self.get_replica_worker_ids(node_id);
      sb.push_str("$5\r\nnodes\r\n*");
      sb.push_str(buf.format(1 + replica_wids.len()));
      sb.push_str("\r\n");
      // 本地节点恒在线 (对标 Garnet 对 workerId==1 强制 connected)
      let connected = i == LOCAL_WORKER_ID || w.link_state == LinkState::Connected;
      append_shard_node_entry(&mut sb, w, node_id, connected);
      for rep_wid in replica_wids {
        if let Some(rep) = self.workers.get(rep_wid)
          && let Some(ref rep_id) = rep.node_id
        {
          append_shard_node_entry(&mut sb, rep, rep_id, rep.link_state == LinkState::Connected);
        }
      }
    }
    sb
  }

  // 序列化与反序列化实现 (Garnet 1:1 对齐)

  /// 从二进制数据切片窥探序列化版本号
  #[inline]
  pub fn try_peek_version(data: &[u8]) -> Option<u8> {
    data.first().copied()
  }

  // Bitcode 高性能序列化与反序列化实现

  /// 导出为紧凑的 Bitcode 序列化中间结构
  pub fn to_bitcode_payload(&self) -> ClusterConfigBitcode {
    let mut segments = Vec::new();
    let mut count: u16 = 1;
    let mut worker_id = self.slot_map[0].worker_id;
    let mut state = self.slot_map[0].state;

    for slot in self.slot_map.iter().skip(1) {
      if slot.worker_id != worker_id || slot.state != state {
        segments.push(ClusterConfigSegment {
          count,
          worker_id,
          state,
        });
        count = 1;
        worker_id = slot.worker_id;
        state = slot.state;
      } else {
        count += 1;
      }
    }
    segments.push(ClusterConfigSegment {
      count,
      worker_id,
      state,
    });

    ClusterConfigBitcode {
      version: CLUSTER_CONFIG_VERSION,
      segments,
      workers: self.workers.clone(),
    }
  }

  /// 从 Bitcode 序列化中间结构恢复 ClusterConfig
  pub fn from_bitcode_payload(payload: ClusterConfigBitcode) -> Result<Self> {
    if payload.version != CLUSTER_CONFIG_VERSION {
      return Err(Error::InvalidPayload(format!(
        "Incompatible ClusterConfig bitcode version: expected {CLUSTER_CONFIG_VERSION}, got {}",
        payload.version
      )));
    }
    let num_workers = payload.workers.len();
    if num_workers == 0 || num_workers > TOTAL_HASH_SLOTS {
      return Err(Error::InvalidPayload(format!(
        "工作节点数量非法: {num_workers}"
      )));
    }

    let mut slot_map = Box::new([HashSlot::default(); TOTAL_HASH_SLOTS]);
    let mut slot_offset = 0;

    for seg in payload.segments {
      if seg.worker_id as usize >= num_workers {
        return Err(Error::InvalidPayload(format!(
          "Bitcode SlotMap 属主索引越界: {}",
          seg.worker_id
        )));
      }
      let end = (slot_offset + seg.count as usize).min(TOTAL_HASH_SLOTS);
      if slot_offset < end {
        slot_map[slot_offset..end].fill(HashSlot::new(seg.worker_id, seg.state));
        slot_offset = end;
      }
    }

    Ok(Self {
      slot_map,
      workers: payload.workers,
    })
  }

  /// 序列化为 Bitcode 格式字节数组
  #[inline]
  pub fn to_bitcode(&self) -> Vec<u8> {
    bitcode::encode(&self.to_bitcode_payload())
  }

  /// 从 Bitcode 格式字节数组还原配置
  pub fn from_bitcode(bytes: &[u8]) -> Result<Self> {
    let payload: ClusterConfigBitcode = bitcode::decode(bytes)
      .map_err(|e| Error::InvalidPayload(format!("Bitcode 解码失败: {e}")))?;
    Self::from_bitcode_payload(payload)
  }
}

/// Gossip 携带的节点元信息借用视图 (稳态合并零分配)
pub(crate) struct WorkerMeta<'a> {
  /// 节点 ID (None 或空串直接忽略合并)
  pub node_id: Option<&'a str>,
  /// 节点 IP 地址
  pub address: &'a str,
  /// 业务端口
  pub port: u16,
  /// 配置纪元
  pub config_epoch: i64,
  /// 节点角色
  pub role: NodeRole,
  /// 复制的源主节点 ID
  pub replica_of: Option<&'a str>,
  /// 主机名
  pub hostname: Option<&'a str>,
}

/// 集群配置的槽位连续区间行程段 (用于高效 Bitcode 紧凑持久化)
#[derive(Debug, Clone, Copy, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub struct ClusterConfigSegment {
  /// 连续相同状态与所有者的槽位数量
  pub count: u16,
  /// 工作节点编号
  pub worker_id: u16,
  /// 槽位状态
  pub state: SlotState,
}

/// 集群配置的紧凑 Bitcode 序列化中间体
#[derive(Debug, Clone, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub struct ClusterConfigBitcode {
  /// 序列化协议格式版本号
  pub version: u8,
  /// 行程编码后的哈希槽状态映射段
  pub segments: Vec<ClusterConfigSegment>,
  /// 节点列表
  pub workers: Vec<Worker>,
}

/// 追加节点网络与元数据信息 (CLUSTER SLOTS 响应元素)
fn append_node_resp_meta(
  sb: &mut String,
  ip: &str,
  port: u16,
  node_id: &str,
  hostname: Option<&str>,
) {
  let mut buf = Buffer::new();
  sb.push_str("*4\r\n$");
  sb.push_str(buf.format(ip.len()));
  sb.push_str("\r\n");
  sb.push_str(ip);
  sb.push_str("\r\n:");
  sb.push_str(buf.format(port));
  sb.push_str("\r\n$");
  sb.push_str(buf.format(node_id.len()));
  sb.push_str("\r\n");
  sb.push_str(node_id);
  sb.push_str("\r\n");

  if let Some(host) = hostname {
    sb.push_str("*2\r\n$8\r\nhostname\r\n$");
    sb.push_str(buf.format(host.len()));
    sb.push_str("\r\n");
    sb.push_str(host);
    sb.push_str("\r\n");
  } else {
    sb.push_str("*0\r\n");
  }
}

/// 追加 CLUSTER SHARDS 单节点元信息表 (14 字段，含主机名时 16 字段，对标 Garnet AppendFormattedNodeInfo)
fn append_shard_node_entry(sb: &mut String, w: &Worker, node_id: &str, connected: bool) {
  let mut buf = Buffer::new();
  // 基础 7 字段对 (id/port/ip/endpoint/role/replication-offset/health)，可选 hostname 增加一对
  let field_count = if w.hostname.is_some() { 16 } else { 14 };
  let _ = write!(sb, "*{field_count}\r\n");

  sb.push_str("$2\r\nid\r\n$");
  sb.push_str(buf.format(node_id.len()));
  sb.push_str("\r\n");
  sb.push_str(node_id);
  sb.push_str("\r\n");

  sb.push_str("$4\r\nport\r\n:");
  sb.push_str(buf.format(w.port));
  sb.push_str("\r\n");

  sb.push_str("$2\r\nip\r\n$");
  sb.push_str(buf.format(w.address.len()));
  sb.push_str("\r\n");
  sb.push_str(&w.address);
  sb.push_str("\r\n");

  // 终结点偏好固定为 IP (Rust 端未建模 ClusterPreferredEndpointType)
  sb.push_str("$8\r\nendpoint\r\n$");
  sb.push_str(buf.format(w.address.len()));
  sb.push_str("\r\n");
  sb.push_str(&w.address);
  sb.push_str("\r\n");

  if let Some(host) = w.hostname.as_deref() {
    sb.push_str("$8\r\nhostname\r\n$");
    sb.push_str(buf.format(host.len()));
    sb.push_str("\r\n");
    sb.push_str(host);
    sb.push_str("\r\n");
  }

  let role_str = w.role.as_str();
  let _ = write!(
    sb,
    "$4\r\nrole\r\n${}\r\n{role_str}\r\n$18\r\nreplication-offset\r\n:{}\r\n$6\r\nhealth\r\n",
    role_str.len(),
    w.replication_offset
  );
  if connected {
    sb.push_str("$6\r\nonline\r\n");
  } else {
    sb.push_str("$7\r\noffline\r\n");
  }
}

/// 追加单个槽位连续区段 " <start>" 或 " <start>-<end>"
fn push_slot_range(out: &mut String, buf: &mut Buffer, start: u16, end: u16) {
  out.push(' ');
  out.push_str(buf.format(start));
  if start != end {
    out.push('-');
    out.push_str(buf.format(end));
  }
}
