use std::fs;

use aok::{OK, Void};
use log::info;
use tempfile::tempdir;
use wedb_cluster::{
  ClusterConfig, ClusterConfigSerializer, ClusterManager, LOCAL_WORKER_ID, NodeId, NodeRole,
  SlotState, Worker,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

const N0: &str = "3000000000000000000000000000000000000000";
const N1: &str = "3111111111111111111111111111111111111111";
const NA: &str = "4000000000000000000000000000000000000000";
const NB: &str = "4111111111111111111111111111111111111111";
const NC: &str = "4222222222222222222222222222222222222222";

/// 序列化往返保留迁移标记 (文本与 Bitcode 双向往返)
#[test]
fn test_serialization_roundtrip_with_migration_markers() -> Void {
  info!("开始测试：文本与 Bitcode 序列化往返保留迁移标记");

  let manager = ClusterManager::new();
  manager.init_local(Worker::primary(NA, "127.0.0.1", 7000, 3).with_hostname("node-a"));
  manager.try_meet("127.0.0.1", 7001, Some(NB))?;
  manager.try_meet("127.0.0.1", 7002, Some(NC))?;
  manager.try_add_slots(&[10, 11, 12, 20, 21])?;
  {
    let mut conf = manager.current_config();
    let nb = conf.get_worker_id_from_node_id(NB);
    conf.update_slot_state(30, nb as u16, SlotState::Stable);
    manager.unsafe_set_config(conf);
  }
  manager.try_prepare_slot_for_migration(12, NB)?;
  manager.try_prepare_slot_for_import(30, NB)?;

  let conf = manager.current_config();
  assert_eq!(conf.get_state(12), SlotState::Migrating);
  assert_eq!(conf.get_state(30), SlotState::Importing);

  // 1. NestedText 文本往返：to_nested_text -> from_nested_text
  let text = ClusterConfigSerializer::to_nested_text(&conf)?;
  let mut s = String::from("[12->-");
  s.push_str(NB);
  s.push(']');
  assert!(text.contains(&s));
  let mut s = String::from("[30-<-");
  s.push_str(NB);
  s.push(']');
  assert!(text.contains(&s));
  let restored = ClusterConfigSerializer::from_nested_text(&text, Some(NA))?;
  assert_eq!(restored.local_node_id(), Some(NA));
  assert_eq!(restored.local_node_config_epoch(), 3);
  assert_eq!(restored.get_state(10), SlotState::Stable);
  assert_eq!(restored.get_state(11), SlotState::Stable);
  assert_eq!(restored.get_state(12), SlotState::Migrating);
  assert_eq!(restored.get_node_id_from_slot(12), Some(NB));
  assert_eq!(restored.get_state(30), SlotState::Importing);
  assert_eq!(restored.get_node_id_from_slot(30), Some(NB));
  assert_eq!(restored.get_node_role_from_node_id(NC), NodeRole::Primary);
  assert_eq!(restored.workers[1].hostname.as_deref(), Some("node-a"));
  assert_eq!(
    restored.get_worker_address_from_node_id(NC),
    Some(("127.0.0.1", 7002))
  );

  // 2. Bitcode 往返：to_bitcode -> from_bitcode
  let bytes = ClusterConfigSerializer::to_bitcode(&conf);
  let bin = ClusterConfigSerializer::from_bitcode(&bytes)?;
  assert_eq!(bin.get_state(12), SlotState::Migrating);
  assert_eq!(bin.get_node_id_from_slot(12), Some(NB));
  assert_eq!(bin.get_state(30), SlotState::Importing);
  assert_eq!(bin.get_node_id_from_slot(30), Some(NB));
  assert_eq!(bin.get_state(20), SlotState::Stable);
  assert_eq!(bin.local_node_config_epoch(), 3);
  assert_eq!(bin.workers[1].hostname.as_deref(), Some("node-a"));
  assert_eq!(bin.get_node_role_from_node_id(NB), NodeRole::Primary);

  OK
}

/// 断电原子持久化与崩溃自愈恢复深度测试
#[test]
fn test_atomic_persistence_and_crash_self_healing() -> Void {
  info!("开始测试：断电原子持久化与崩溃自愈机制");

  let dir = tempdir()?;
  let conf_path = dir.path().join("cluster").join("nodes.nt");

  let manager = ClusterManager::new();
  let node_id = NodeId::generate();
  manager
    .init_local(Worker::primary(node_id.as_str(), "127.0.0.1", 7000, 1).with_hostname("primary-0"));
  manager.try_add_slots(&[0, 1, 2, 100, 16383])?;
  let config = manager.current_config();

  // 1. 正常原子持久化保存
  ClusterConfigSerializer::save_to_file(&config, &conf_path)?;
  assert!(conf_path.exists());
  let loaded = ClusterConfigSerializer::load_from_file(&conf_path, Some(node_id.as_str()))?;
  assert_eq!(loaded.local_node_id(), Some(node_id.as_str()));
  assert_eq!(loaded.local_node_port(), 7000);
  assert_eq!(loaded.get_state(100), SlotState::Stable);

  // 2. 模拟断电崩溃场景 A：主文件不存在，但存在未完成重命名的 .tmp 临时文件 -> 自愈恢复
  let tmp_path = conf_path.with_file_name("nodes.nt.tmp");
  fs::rename(&conf_path, &tmp_path)?;
  assert!(!conf_path.exists());
  assert!(tmp_path.exists());

  let healed_a = ClusterConfigSerializer::load_from_file(&conf_path, Some(node_id.as_str()))?;
  assert!(conf_path.exists());
  assert_eq!(healed_a.local_node_id(), Some(node_id.as_str()));
  assert_eq!(healed_a.local_node_port(), 7000);

  // 3. 模拟断电崩溃场景 B：主文件因断电截断为 0 字节，但 .tmp 文件有效 -> 自动自愈覆盖
  ClusterConfigSerializer::save_to_file(&config, &tmp_path)?;
  fs::write(&conf_path, b"")?;
  let healed_b = ClusterConfigSerializer::load_from_file(&conf_path, Some(node_id.as_str()))?;
  assert_eq!(healed_b.local_node_id(), Some(node_id.as_str()));
  assert!(!fs::read_to_string(&conf_path)?.is_empty());

  OK
}

/// Bitcode 序列化与反序列化往返及非法载荷防御
#[test]
fn test_bitcode_serialization_and_tamper_proofing() -> Void {
  info!("开始测试：Bitcode 序列化往返与非法载荷防御");

  let mut conf = ClusterConfig::new();
  conf.initialize_local_worker(Worker::primary(N0, "127.0.0.1", 7000, 1).with_hostname("node0"));
  let n1_idx = conf.add_worker(Worker::replica(N1, "127.0.0.1", 7001, 2, N0));
  conf.update_slot_state(0, LOCAL_WORKER_ID as u16, SlotState::Stable);
  conf.update_slot_state(1, LOCAL_WORKER_ID as u16, SlotState::Migrating);
  conf.update_slot_state(2, n1_idx as u16, SlotState::Importing);
  conf.update_slot_state(16383, LOCAL_WORKER_ID as u16, SlotState::Stable);

  // 1. Bitcode 序列化与反序列化往返验证
  let encoded = ClusterConfigSerializer::to_bitcode(&conf);
  assert!(!encoded.is_empty());
  let restored = ClusterConfigSerializer::from_bitcode(&encoded)?;

  assert_eq!(restored.local_node_id(), Some(N0));
  assert_eq!(restored.num_workers(), 2);
  assert_eq!(restored.get_state(0), SlotState::Stable);
  assert_eq!(restored.get_state(1), SlotState::Migrating);
  assert_eq!(restored.get_state(2), SlotState::Importing);
  assert_eq!(restored.get_state(16383), SlotState::Stable);
  assert_eq!(restored.get_state(3), SlotState::Offline);

  // 2. 恶意载荷防护：损坏字节解码报错
  assert!(ClusterConfigSerializer::from_bitcode(b"corrupted bytes").is_err());
  assert!(ClusterConfigSerializer::from_bitcode(&[]).is_err());

  // 3. 版本号不兼容防护
  let mut bad_payload = conf.to_bitcode_payload();
  bad_payload.version = 255;
  let bad_bytes = bitcode::encode(&bad_payload);
  assert!(ClusterConfigSerializer::from_bitcode(&bad_bytes).is_err());

  // 4. 属主索引越界防御
  let mut bad_worker_payload = conf.to_bitcode_payload();
  bad_worker_payload.segments[0].worker_id = 999;
  let bad_worker_bytes = bitcode::encode(&bad_worker_payload);
  assert!(ClusterConfigSerializer::from_bitcode(&bad_worker_bytes).is_err());

  OK
}

/// Bitcode 配置文件原子落盘与崩溃自愈深度验证
#[test]
fn test_bitcode_atomic_persistence_and_crash_self_healing() -> Void {
  info!("开始测试：Bitcode 配置文件原子落盘与崩溃自愈深度验证");

  let dir = tempdir()?;
  let bitcode_path = dir.path().join("cluster").join("nodes.bitcode");

  let manager = ClusterManager::new();
  manager.init_local(Worker::primary(N0, "127.0.0.1", 7000, 0));
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  manager.try_add_slots(&[0, 1, 100, 16383])?;
  let config = manager.current_config();

  // 1. 正常原子持久化
  ClusterConfigSerializer::save_bitcode_to_file(&config, &bitcode_path)?;
  assert!(bitcode_path.exists());
  let loaded = ClusterConfigSerializer::load_bitcode_from_file(&bitcode_path)?;
  assert_eq!(loaded.local_node_id(), Some(N0));
  assert_eq!(loaded.get_state(100), SlotState::Stable);

  // 2. 模拟断电崩溃场景：主文件损坏，从 .tmp 自动恢复自愈
  let tmp_path = bitcode_path.with_file_name("nodes.bitcode.tmp");
  ClusterConfigSerializer::save_bitcode_to_file(&config, &tmp_path)?;
  fs::write(&bitcode_path, b"corrupted garbage")?;
  let healed = ClusterConfigSerializer::load_bitcode_from_file(&bitcode_path)?;
  assert_eq!(healed.local_node_id(), Some(N0));
  assert_eq!(healed.get_state(16383), SlotState::Stable);

  // 3. 模拟主文件完全丢失，但 .tmp 存在：自动 rename 自愈
  fs::remove_file(&bitcode_path)?;
  ClusterConfigSerializer::save_bitcode_to_file(&config, &tmp_path)?;
  assert!(!bitcode_path.exists());
  let healed_from_tmp = ClusterConfigSerializer::load_bitcode_from_file(&bitcode_path)?;
  assert!(bitcode_path.exists());
  assert_eq!(healed_from_tmp.local_node_id(), Some(N0));

  OK
}
