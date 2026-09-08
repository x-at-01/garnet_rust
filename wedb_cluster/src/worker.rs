use std::fmt;

use crate::node::{LinkState, NodeRole};

/// 集群工作节点定义
#[derive(Debug, Clone, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub struct Worker {
  /// 节点唯一 ID (40 位十六进制字符)
  pub node_id: Option<String>,
  /// 节点监听 IP 地址
  pub address: String,
  /// 节点通信端口
  pub port: u16,
  /// 配置纪元 (Config Epoch)
  pub config_epoch: i64,
  /// 节点角色
  pub role: NodeRole,
  /// 当前节点正在复制的主节点 ID (仅对从节点有效)
  pub replica_of_node_id: Option<String>,
  /// 复制偏移量 (只读信息)
  pub replication_offset: i64,
  /// 节点主机名 (可选)
  pub hostname: Option<String>,
  /// 网络连接状态 (对标 Redis / Garnet link_state)
  pub link_state: LinkState,
  /// 是否疑似下线 (PFAIL)
  pub is_pfail: bool,
  /// 是否已确认下线 (FAIL)
  pub is_fail: bool,
  /// 握手中的未确认节点 (CLUSTER MEET 种子条目，等待对端 gossip 首次确认回填真实角色与纪元；对标 Redis HANDSHAKE)
  pub handshake: bool,
}

impl Default for Worker {
  fn default() -> Self {
    Self {
      node_id: None,
      address: "unassigned".to_string(),
      port: 0,
      config_epoch: 0,
      role: NodeRole::Unassigned,
      replica_of_node_id: None,
      replication_offset: 0,
      hostname: None,
      link_state: LinkState::Connected,
      is_pfail: false,
      is_fail: false,
      handshake: false,
    }
  }
}

impl Worker {
  /// 创建未分配占位节点
  #[inline]
  pub fn unassigned() -> Self {
    Self::default()
  }

  /// 创建工作节点
  pub fn new(
    node_id: impl Into<String>,
    address: impl Into<String>,
    port: u16,
    config_epoch: i64,
    role: NodeRole,
  ) -> Self {
    Self {
      node_id: Some(node_id.into()),
      address: address.into(),
      port,
      config_epoch,
      role,
      replica_of_node_id: None,
      replication_offset: 0,
      hostname: None,
      link_state: LinkState::Connected,
      is_pfail: false,
      is_fail: false,
      handshake: false,
    }
  }

  /// 创建主节点便捷构造函数
  pub fn primary(
    node_id: impl Into<String>,
    address: impl Into<String>,
    port: u16,
    config_epoch: i64,
  ) -> Self {
    Self::new(node_id, address, port, config_epoch, NodeRole::Primary)
  }

  /// 创建从节点便捷构造函数
  pub fn replica(
    node_id: impl Into<String>,
    address: impl Into<String>,
    port: u16,
    config_epoch: i64,
    replica_of: impl Into<String>,
  ) -> Self {
    Self::new(node_id, address, port, config_epoch, NodeRole::Replica).with_replica_of(replica_of)
  }

  /// 设置主节点 ID (用于从节点)
  pub fn with_replica_of(mut self, replica_of: impl Into<String>) -> Self {
    self.replica_of_node_id = Some(replica_of.into());
    self
  }

  /// 设置节点主机名
  pub fn with_hostname(mut self, hostname: impl Into<String>) -> Self {
    let host = hostname.into();
    self.hostname = if host.is_empty() { None } else { Some(host) };
    self
  }

  /// 标记为疑似故障 (PFAIL)
  #[inline]
  pub fn set_pfail(&mut self) {
    if !self.is_fail {
      self.is_pfail = true;
    }
  }

  /// 标记为确认故障 (FAIL)
  #[inline]
  pub fn set_fail(&mut self) {
    self.is_pfail = false;
    self.is_fail = true;
    self.link_state = LinkState::Disconnected;
  }

  /// 清除故障标志恢复正常连接
  #[inline]
  pub fn clear_fail(&mut self) {
    self.is_pfail = false;
    self.is_fail = false;
    self.link_state = LinkState::Connected;
  }
}

impl fmt::Display for Worker {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let id_str = self.node_id.as_deref().unwrap_or("");
    let rep_str = self.replica_of_node_id.as_deref().unwrap_or("");
    write!(
      f,
      "{} {} {} {} {} {}",
      id_str, self.address, self.port, self.config_epoch, self.role, rep_str
    )
  }
}
