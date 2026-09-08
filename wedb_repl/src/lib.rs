#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(clippy::absolute_paths)]

//! WeDB 高可靠全量与增量主从复制引擎 (`wedb_repl`)
//!
//! 对标微软 Garnet 复制系统核心架构与复制协议规范：
//! - 统一双复制编号架构与无缝故障转移增量续订 (`ReplicationHistory`)
//! - 复制协议帧编解码与五步握手状态机 (`protocol`, `HandshakeDriver`)
//! - 同步决策状态机与无盘全量快照协调 (`ReplicationSyncManager`, `SyncDecision`)
//! - 增量预写日志流式驱动器与消费水位线保护 (`AofSyncDriver`)
//! - 从节点物理双写与本地存储引擎即时回放 (`ReplicaReplayer`, `ReplicaClient`)

mod backlog;
mod client;
mod error;
mod history;
mod manager;
mod protocol;
mod range_index;
mod role;
mod sync;

pub use backlog::{DEFAULT_BACKLOG_SIZE, ReplicationBacklog};
pub use client::{HandshakeDriver, HandshakeStep, ReplicaClient, ReplicaReplayer};
pub use error::{Error, Result};
pub use history::{
  REPL_ID_LEN, REPLICATION_HISTORY_BYTES_LEN, REPLICATION_HISTORY_VERSION, ReplId,
  ReplicationHistory,
};
pub use manager::ReplicationManager;
pub use protocol::{
  MasterResponse, OK_FRAME, PING_FRAME, PONG_FRAME, REPLCONF_GETACK_FRAME, ReplConfSubCmd,
  ReplicaCommand, encode_auth, encode_continue, encode_fullresync, encode_ok, encode_ping,
  encode_pong, encode_psync, encode_replconf_ack, encode_replconf_capa, encode_replconf_getack,
  encode_replconf_ip, encode_replconf_port, parse_i64_bytes, parse_master_response,
  parse_replica_command, parse_u16_bytes, parse_u64_bytes,
};
pub use range_index::{
  CheckpointFileType, CooperativeDisposeGuard, DEFAULT_CHUNK_SIZE, DisposeResult,
  FLUSH_METADATA_LEN, KEY_HASH_LEN, RangeIndexFileDataSink, RangeIndexFileDataSource,
  RangeIndexMigrationReceiveState, RangeIndexSnapshotReader, RangeIndexStreamReassembler,
  ReceiveStatus,
};
pub use role::{NodeRole, RecoveryStatus};
pub use sync::{AofSyncDriver, ReplicaSessionInfo, ReplicationSyncManager, SyncDecision};
