use std::{io, result};

use thiserror::Error;

/// 集群错误类型
#[derive(Error, Debug)]
pub enum Error {
  /// 无效配置载荷
  #[error("无效配置载荷: {0}")]
  InvalidPayload(String),

  /// 未知节点 ID
  #[error("ERR I don't know about node {0}.")]
  UnknownNode(String),

  /// 目标节点不是主节点
  #[error("ERR Target node {0} is not a master node.")]
  TargetNotMaster(String),

  /// 不能向自身迁移
  #[error("ERR Can't MIGRATE to myself.")]
  MigrateToMyself,

  /// 非本地哈希槽所有者
  #[error("ERR I'm not the owner of hash slot {0}")]
  NotSlotOwner(u16),

  /// 哈希槽已处于非稳定状态
  #[error("ERR Slot already scheduled for migration from {0}")]
  SlotNotStable(String),

  /// 导入槽位状态冲突
  #[error("ERR Slot already scheduled for import from {0}")]
  SlotAlreadyImporting(String),

  /// 不能遗忘自身节点
  #[error("ERR I tried to forget myself...")]
  CannotForgetMyself,

  /// 从节点不能遗忘其主节点
  #[error("ERR I can't forget my master!")]
  CannotForgetMyPrimary,

  /// 存在已分配数据的哈希槽，重置失败
  #[error("ERR It is not possible to reset a node with keys in slots assigned.")]
  ResetWithKeysAssigned,

  /// 越界哈希槽编号
  #[error("ERR Invalid slot: {0}")]
  SlotOutOfRange(u16),

  /// 通用无效参数错误
  #[error("ERR {0}")]
  InvalidParam(String),

  /// 配置序列化错误
  #[error("配置序列化错误: {0}")]
  ConfigSerialization(String),

  /// 不能将自身设为复制源 (对标 Garnet RESP_ERR_GENERIC_CANNOT_REPLICATE_SELF)
  #[error("ERR Can't replicate myself")]
  CannotReplicateSelf,

  /// 纪元冲突 (对标 Garnet RESP_ERR_GENERIC_CONFIG_EPOCH_NOT_SET)
  #[error("ERR Node config epoch was not set due to invalid epoch specified")]
  EpochCollision,

  /// 无效节点 ID
  #[error("无效节点 ID: {0}")]
  InvalidNodeId(String),

  /// 无效节点角色
  #[error("无效节点角色: {0}")]
  InvalidNodeRole(String),

  /// 槽位位图错误
  #[error("槽位位图错误: {0}")]
  SlotBitmapError(String),

  /// 损坏的配置文件
  #[error("损坏的配置文件: {0}")]
  CorruptedConfig(String),

  /// IO 错误
  #[error(transparent)]
  Io(#[from] io::Error),
}

/// 集群标准 Result 类型别名
pub type Result<T> = result::Result<T, Error>;
