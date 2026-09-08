use whasher::{HashMap, HashSet};

use crate::{
  config::{ClusterConfig, LOCAL_WORKER_ID, WorkerMeta},
  node::NodeRole,
  slot::SlotState,
  worker::Worker,
};

/// 单个 gossip 报文处理的最大切片数 (恶意包防护：限制第三方节点信息处理上限)
pub const MAX_GOSSIP_SECTIONS: usize = 1024;

/// Gossip 消息头（包含发送方元数据）
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GossipHeader {
  /// 发送方节点 ID
  pub sender_id: String,
  /// 发送方 IP
  pub sender_addr: String,
  /// 业务端口
  pub sender_port: u16,
  /// 节点角色
  pub sender_role: NodeRole,
  /// 配置纪元
  pub config_epoch: i64,
  /// 发送方持有的稳定槽位列表
  pub assigned_slots: Vec<u16>,
  /// 发送方正在迁出的槽位列表 (迁出槽位仍由发送方服务，接收方不得据此弃权回收)
  pub migrating_slots: Vec<u16>,
  /// 发送方复制的源主节点 ID (从节点身份必须随 gossip 传播，对标 Garnet MergeWorkerInfo 携带 ReplicaOfNodeId)
  pub replica_of: Option<String>,
  /// 发送方主机名
  pub hostname: Option<String>,
}

/// Gossip 切片中携带的其他已知节点状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GossipNodeSection {
  /// 节点 ID
  pub node_id: String,
  /// 节点 IP
  pub address: String,
  /// 业务端口
  pub port: u16,
  /// 节点角色
  pub role: NodeRole,
  /// 配置纪元
  pub config_epoch: i64,
  /// 是否疑似下线
  pub is_pfail: bool,
  /// 是否已确认下线
  pub is_fail: bool,
  /// 该节点复制的源主节点 ID (对标 Garnet MergeWorkerInfo 携带 ReplicaOfNodeId)
  pub replica_of: Option<String>,
  /// 节点主机名
  pub hostname: Option<String>,
}

/// Gossip 数据包
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GossipPacket {
  /// 发送方消息头
  pub header: GossipHeader,
  /// 随机挑选的切片集合
  pub sections: Vec<GossipNodeSection>,
}

/// Gossip 协议跟踪器（拓扑合并、禁入名单与 PFAIL/FAIL 故障判定）
#[derive(Debug, Clone, Default)]
pub struct GossipTracker {
  /// 目标节点 ID -> 报告其处于 PFAIL 状态的节点集合
  pub pfail_reports: HashMap<String, HashSet<String>>,
  /// 本地判定处于 PFAIL 状态的节点集合
  pub local_pfails: HashSet<String>,
  /// 确认处于 FAIL 状态的节点集合
  pub fail_nodes: HashSet<String>,
  /// 禁入名单 (FORGET 后防止 gossip 重新收录): 节点 ID -> 过期 Unix 秒
  pub ban_list: HashMap<String, u64>,
}

impl GossipTracker {
  /// 创建新的跟踪器
  pub fn new() -> Self {
    Self::default()
  }

  /// 将节点加入禁入名单 (对标 Garnet workerBanList)
  pub fn ban(&mut self, node_id: &str, ttl_secs: u64) {
    let expiry = coarsetime::Clock::now_since_epoch().as_secs() + ttl_secs;
    self.ban_list.insert(node_id.to_string(), expiry);
  }

  /// 查询节点是否仍在禁入期内 (惰性清理过期项)
  pub fn is_banned(&mut self, node_id: &str) -> bool {
    let now = coarsetime::Clock::now_since_epoch().as_secs();
    match self.ban_list.get(node_id) {
      Some(&expiry) if expiry > now => true,
      Some(_) => {
        self.ban_list.remove(node_id);
        false
      }
      None => false,
    }
  }

  /// 导出仍在禁入期内的节点 ID 列表
  pub fn banned_ids(&mut self) -> Vec<String> {
    let now = coarsetime::Clock::now_since_epoch().as_secs();
    self.ban_list.retain(|_, expiry| *expiry > now);
    self.ban_list.keys().cloned().collect()
  }

  /// 构建待发送的 Gossip 数据包 (切片附带本地方源的 PFAIL/FAIL 视图)
  pub fn create_packet(&self, config: &ClusterConfig, sample_count: usize) -> GossipPacket {
    let mut assigned_slots = Vec::new();
    let mut migrating_slots = Vec::new();
    for (slot, hs) in config.slot_map.iter().enumerate() {
      if hs.effective_worker_id() == LOCAL_WORKER_ID as u16 && hs.state == SlotState::Stable {
        assigned_slots.push(slot as u16);
      } else if hs.state == SlotState::Migrating {
        migrating_slots.push(slot as u16);
      }
    }

    let header = config
      .workers
      .get(LOCAL_WORKER_ID)
      .map_or_else(GossipHeader::default, |local| GossipHeader {
        sender_id: local.node_id.clone().unwrap_or_default(),
        sender_addr: local.address.clone(),
        sender_port: local.port,
        sender_role: local.role,
        config_epoch: local.config_epoch,
        assigned_slots,
        migrating_slots,
        replica_of: local.replica_of_node_id.clone(),
        hostname: local.hostname.clone(),
      });

    let remote_workers = config.workers.iter().skip(2);
    let known: Vec<&Worker> = remote_workers
      .filter(|w| w.node_id.as_deref().is_some_and(|id| !id.is_empty()))
      .collect();

    let count = sample_count.min(known.len());
    let sections = if count >= known.len() {
      known
        .iter()
        .map(|&w| self.build_section(w))
        .collect::<Vec<_>>()
    } else {
      let mut indices: Vec<usize> = (0..known.len()).collect();
      for i in 0..count {
        let j = fastrand::usize(i..known.len());
        indices.swap(i, j);
      }
      indices[..count]
        .iter()
        .map(|&idx| self.build_section(known[idx]))
        .collect()
    };

    GossipPacket { header, sections }
  }

  /// 由节点信息构建 gossip 切片并注入本地故障判定视图
  fn build_section(&self, w: &Worker) -> GossipNodeSection {
    let id = w.node_id.as_deref().unwrap_or_default();
    GossipNodeSection {
      node_id: id.to_string(),
      address: w.address.clone(),
      port: w.port,
      role: w.role,
      config_epoch: w.config_epoch,
      is_pfail: self.local_pfails.contains(id),
      is_fail: self.fail_nodes.contains(id),
      replica_of: w.replica_of_node_id.clone(),
      hostname: w.hostname.clone(),
    }
  }

  /// 处理接收到的 Gossip 报文：合并发送方与切片节点元信息、槽位认领，并收集 PFAIL 投票
  /// 返回本地配置是否发生变更 (对标 Garnet TryMerge 的变更探测)
  pub fn process_packet(&mut self, config: &mut ClusterConfig, packet: &GossipPacket) -> bool {
    let header = &packet.header;
    let sender_id = header.sender_id.as_str();
    if sender_id.is_empty() || self.is_banned(sender_id) {
      return false;
    }
    // 绝不因 gossip 报文改写本地节点自身条目 (对标 Garnet Merge 跳过本地节点)
    // 借用比较零分配：不可变借用随条件求值结束，不与后续可变合并冲突
    if config.local_node_id() == Some(sender_id) {
      return false;
    }

    let mut changed = false;

    // 1. 发送方自述信息为直接信源：可确认 MEET 握手种子并按纪元更新 (对标 Redis 直接 PONG)
    // 借用合并：稳态（纪元无变更）零字符串分配
    changed |= config.merge_worker_ref(
      WorkerMeta {
        node_id: Some(sender_id),
        address: &header.sender_addr,
        port: header.sender_port,
        config_epoch: header.config_epoch,
        role: header.sender_role,
        replica_of: header.replica_of.as_deref(),
        hostname: header.hostname.as_deref(),
      },
      true,
    );

    // 2. 主节点发送方凭纪元认领稳定槽位，并对不再持有的槽位执行弃权回收 (对标 MergeSlotMap)
    if header.sender_role == NodeRole::Primary {
      changed |= config.merge_sender_slots(
        sender_id,
        header.config_epoch,
        &header.assigned_slots,
        &header.migrating_slots,
      );
    }

    // 3. 合并切片节点元信息并收集 PFAIL/FAIL 投票 (恶意包防护：切片数量截断)
    let sections = if packet.sections.len() > MAX_GOSSIP_SECTIONS {
      &packet.sections[..MAX_GOSSIP_SECTIONS]
    } else {
      &packet.sections[..]
    };
    for sec in sections {
      if sec.node_id.is_empty()
        || sec.node_id == sender_id
        || config.local_node_id() == Some(sec.node_id.as_str())
        // 禁入名单对切片同等生效：FORGET 后的节点不得经由第三方报文重新收录 (对标 Garnet Merge 跳过禁入节点)
        || self.is_banned(&sec.node_id)
      {
        continue;
      }
      // 借用合并：稳态（纪元无变更）零字符串分配；仅未知节点收录或字段真变更时才复制
      changed |= config.merge_worker_ref(
        WorkerMeta {
          node_id: Some(&sec.node_id),
          address: &sec.address,
          port: sec.port,
          config_epoch: sec.config_epoch,
          role: sec.role,
          replica_of: sec.replica_of.as_deref(),
          hostname: sec.hostname.as_deref(),
        },
        false,
      );

      if sec.is_pfail || sec.is_fail {
        self
          .pfail_reports
          .entry(sec.node_id.clone())
          .or_default()
          .insert(sender_id.to_string());
      } else if let Some(reports) = self.pfail_reports.get_mut(&sec.node_id) {
        reports.remove(sender_id);
      }
    }
    changed
  }

  /// 标记本地对某节点的 PFAIL
  pub fn mark_local_pfail(&mut self, node_id: &str) {
    self.local_pfails.insert(node_id.to_string());
  }

  /// 清除本地对某节点的 PFAIL (同时撤销 FAIL 判定以支持恢复)
  pub fn clear_local_pfail(&mut self, node_id: &str) {
    self.local_pfails.remove(node_id);
    self.fail_nodes.remove(node_id);
    self.pfail_reports.remove(node_id);
  }

  /// 多数派仲裁投票：若不少于半数主节点 (含本地) 报告 PFAIL，则升级为 FAIL
  /// 仅统计主节点投票 (对标 Redis Cluster 故障检测)，返回新确认 FAIL 的节点 ID
  pub fn evaluate_fail_status(&mut self, config: &ClusterConfig) -> Vec<String> {
    let majority = config.get_primary_count() / 2 + 1;
    // 本地节点仅在自身为主节点时拥有一票
    let local_vote = config.is_primary();
    let mut newly_failed = Vec::new();

    for w in config.workers.iter().skip(2) {
      let Some(ref id) = w.node_id else {
        continue;
      };
      if self.fail_nodes.contains(id) {
        continue;
      }
      let mut total = usize::from(local_vote && self.local_pfails.contains(id));
      if let Some(voters) = self.pfail_reports.get(id) {
        total += voters
          .iter()
          .filter(|voter| config.get_node_role_from_node_id(voter) == NodeRole::Primary)
          .count();
      }
      if total >= majority {
        self.fail_nodes.insert(id.clone());
        newly_failed.push(id.clone());
      }
    }

    newly_failed
  }
}

/// 空消息头默认值 (本地节点信息缺失时兜底)
impl Default for GossipHeader {
  fn default() -> Self {
    Self {
      sender_id: String::new(),
      sender_addr: String::new(),
      sender_port: 0,
      sender_role: NodeRole::default(),
      config_epoch: 0,
      assigned_slots: Vec::new(),
      migrating_slots: Vec::new(),
      replica_of: None,
      hostname: None,
    }
  }
}

/// 默认切片信息
impl Default for GossipNodeSection {
  fn default() -> Self {
    Self {
      node_id: String::new(),
      address: String::new(),
      port: 0,
      role: NodeRole::default(),
      config_epoch: 0,
      is_pfail: false,
      is_fail: false,
      replica_of: None,
      hostname: None,
    }
  }
}
