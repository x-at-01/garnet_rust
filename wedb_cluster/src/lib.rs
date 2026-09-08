#![cfg_attr(docsrs, feature(doc_cfg))]

mod config;
mod error;
mod gossip;
mod manager;
mod migration;
mod node;
mod protocol;
mod serializer;
mod slot;
mod worker;

pub use config::{
  CLUSTER_CONFIG_VERSION, ClusterConfig, ClusterConfigBitcode, ClusterConfigSegment,
  LOCAL_WORKER_ID, RESERVED_WORKER_ID,
};
pub use error::{Error, Result};
pub use gossip::{
  GossipHeader, GossipNodeSection, GossipPacket, GossipTracker, MAX_GOSSIP_SECTIONS,
};
pub use manager::{ClusterManager, RouteResult};
pub use migration::{route_request, route_request_ext, route_slot_ext};
pub use node::{LinkState, NodeId, NodeRole};
pub use protocol::{
  format_ask_err, format_cluster_info, format_cluster_nodes, format_cluster_shards,
  format_cluster_slots, format_clusterdown_err, format_moved_err,
};
pub use serializer::ClusterConfigSerializer;
pub use slot::{
  HashSlot, MAX_HASH_SLOT_VALUE, MIN_HASH_SLOT_VALUE, SlotBitmap, SlotState, TOTAL_HASH_SLOTS,
  crc16, hash_slot, out_of_range,
};
pub use worker::Worker;
