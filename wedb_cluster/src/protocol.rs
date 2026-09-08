use itoa::Buffer;

use crate::config::ClusterConfig;

/// 格式化 CLUSTER NODES 响应字符串
#[inline]
pub fn format_cluster_nodes(config: &ClusterConfig) -> String {
  config.get_cluster_nodes()
}

/// 格式化 CLUSTER SLOTS RESP 嵌套数组响应报文
#[inline]
pub fn format_cluster_slots(config: &ClusterConfig) -> String {
  config.get_slots_info()
}

/// 格式化 CLUSTER SHARDS RESP 嵌套数组响应报文 (对标 Garnet GetShardsInfo)
#[inline]
pub fn format_cluster_shards(config: &ClusterConfig) -> String {
  config.get_shards_info()
}

/// 格式化 CLUSTER INFO 集群状态报文
#[inline]
pub fn format_cluster_info(config: &ClusterConfig) -> String {
  config.get_cluster_info()
}

/// 格式化 -MOVED 重定向错误响应
#[inline]
pub fn format_moved_err(slot: u16, endpoint: &str) -> String {
  let mut buf = Buffer::new();
  let mut s = String::from("-MOVED ");
  s.push_str(buf.format(slot));
  s.push(' ');
  s.push_str(endpoint);
  s.push_str("\r\n");
  s
}

/// 格式化 -ASK 临时重定向错误响应
#[inline]
pub fn format_ask_err(slot: u16, endpoint: &str) -> String {
  let mut buf = Buffer::new();
  let mut s = String::from("-ASK ");
  s.push_str(buf.format(slot));
  s.push(' ');
  s.push_str(endpoint);
  s.push_str("\r\n");
  s
}

/// 格式化 -CLUSTERDOWN 错误响应
#[inline]
pub fn format_clusterdown_err(reason: &str) -> String {
  let mut s = String::from("-CLUSTERDOWN ");
  s.push_str(reason);
  s.push_str("\r\n");
  s
}
