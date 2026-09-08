use std::fmt;

/// 节点当前复制角色
#[derive(Debug, Clone, Copy, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
#[repr(u8)]
pub enum NodeRole {
  /// 主节点 (可读写，产生日志流并向从节点同步)
  Primary = 0,
  /// 从节点 (只读，接收主节点快照与日志流并本地回放)
  Replica = 1,
}

impl NodeRole {
  /// 是否为主节点
  #[inline]
  pub const fn is_primary(self) -> bool {
    matches!(self, Self::Primary)
  }

  /// 是否为从节点
  #[inline]
  pub const fn is_replica(self) -> bool {
    matches!(self, Self::Replica)
  }

  /// 转为复制协议字符串（master 或 slave）
  #[inline]
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Primary => "master",
      Self::Replica => "slave",
    }
  }
}

impl fmt::Display for NodeRole {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

/// 复制与恢复状态机（对应微软 Garnet 恢复状态）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, bitcode::Encode, bitcode::Decode)]
#[repr(u8)]
pub enum RecoveryStatus {
  /// 正常运行无恢复中
  #[default]
  NoRecovery = 0,
  /// 节点启动初始化恢复
  InitializeRecover = 1,
  /// 集群主从挂载同步恢复
  ClusterReplicate = 2,
  /// 主从故障转移恢复
  ClusterFailover = 3,
  /// 脱离主节点自立为主
  ReplicaOfNoOne = 4,
  /// 从节点已成功应用快照检查点
  CheckpointRecoveredAtReplica = 5,
  /// 角色锁定只读中（在提交或检查点期间保护角色不发生突变）
  ReadRole = 6,
}

impl RecoveryStatus {
  /// 是否处于恢复状态（对应微软 Garnet ReplicationManager.IsRecovering：
  /// 非 NoRecovery 且非 ReadRole，ReadRole 只是轻量角色读锁，不算恢复中）
  #[inline]
  pub const fn is_recovering(self) -> bool {
    !matches!(self, Self::NoRecovery | Self::ReadRole)
  }

  /// 是否禁止向从节点推送日志流（对应微软 Garnet ReplicationManager.CannotStreamAOF =
  /// IsRecovering && 状态 != CheckpointRecoveredAtReplica）。
  /// 注意 ReadRole（检查点版本轮转期间的角色锁）不阻断日志流推送，与 C# 轮转期间持续推流一致；
  /// 仅四个真正的恢复中状态（初始化/挂载/故障转移/自立为主）阻断推送
  #[inline]
  pub const fn cannot_stream_aof(self) -> bool {
    matches!(
      self,
      Self::InitializeRecover
        | Self::ClusterReplicate
        | Self::ClusterFailover
        | Self::ReplicaOfNoOne
    )
  }

  /// 是否允许向从节点推送日志流
  #[inline]
  pub const fn can_stream_aof(self) -> bool {
    !self.cannot_stream_aof()
  }
}
