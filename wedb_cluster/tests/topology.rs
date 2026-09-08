use std::{str::FromStr, sync::Arc, thread};

use aok::{OK, Void};
use itoa::Buffer;
use log::info;
use wedb_cluster::{
  ClusterConfig, ClusterManager, GossipHeader, GossipNodeSection, GossipPacket, HashSlot,
  LinkState, NodeId, NodeRole, SlotState, Worker,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 对标 Garnet ClusterManagementTests.cs：集群拓扑内省命令格式 (CLUSTER INFO, CLUSTER NODES, CLUSTER SLOTS)
#[test]
fn test_cluster_topology_introspection() -> Void {
  info!("开始测试：集群拓扑内省命令格式输出");

  let manager = ClusterManager::new();
  let node0_id = "e000000000000000000000000000000000000000";
  let node1_id = "e111111111111111111111111111111111111111";

  manager.init_local(Worker::primary(node0_id, "127.0.0.1", 7000, 1).with_hostname("host-0"));
  manager.try_add_slots(&[0, 1, 2, 3, 4])?;

  // 接入对端节点 1
  manager.try_meet("127.0.0.1", 7001, Some(node1_id))?;

  let conf = manager.current_config();

  // 1. 验证 CLUSTER INFO 格式
  let info_str = conf.get_cluster_info();
  assert!(info_str.contains("cluster_state:ok\r\n"));
  assert!(info_str.contains("cluster_slots_assigned:5\r\n"));
  assert!(info_str.contains("cluster_slots_ok:5\r\n"));
  assert!(info_str.contains("cluster_known_nodes:2\r\n"));
  assert!(info_str.contains("cluster_size:2\r\n"));

  // 2. 验证 CLUSTER NODES 文本格式 (对标 ClusterNodesHostnameTest)
  let nodes_str = conf.get_cluster_nodes();
  assert!(nodes_str.contains("myself,master"));
  assert!(nodes_str.contains("127.0.0.1:7000@17000,host-0"));
  assert!(nodes_str.contains("0-4")); // 连续槽位合并表示

  // 3. 验证 CLUSTER SLOTS RESP 协议报文 (对标 ClusterSlotsTest)
  let slots_resp = conf.get_slots_info();
  assert!(slots_resp.starts_with("*1\r\n")); // 1 个槽位段
  assert!(slots_resp.contains(":0\r\n:4\r\n"));
  assert!(slots_resp.contains("$9\r\n127.0.0.1\r\n:7000\r\n"));
  assert!(slots_resp.contains("hostname\r\n"));

  OK
}

/// 对标 Garnet ClusterManagementTests.cs 中的 ClusterShardsTest：CLUSTER SHARDS RESP 报文
#[test]
fn test_cluster_shards_format() -> Void {
  info!("开始测试：CLUSTER SHARDS 分片报文格式输出");

  let manager = ClusterManager::new();
  let n0 = "e000000000000000000000000000000000000000";
  let n1 = "e111111111111111111111111111111111111111";
  let rep = "e222222222222222222222222222222222222222";

  manager.init_local(Worker::primary(n0, "127.0.0.1", 7000, 1).with_hostname("host-0"));
  manager.try_meet("127.0.0.1", 7001, Some(n1))?;
  manager.try_meet("127.0.0.1", 7002, Some(rep))?;
  manager.try_add_slots(&[0, 1, 2, 3, 4])?;

  // 预置第二个主节点槽位与从节点复制关系
  {
    let mut c = manager.current_config();
    let n1_wid = c.get_worker_id_from_node_id(n1);
    for s in 100..=200 {
      c.update_slot_state(s, n1_wid as u16, SlotState::Stable);
    }
    let rep_wid = c.get_worker_id_from_node_id(rep);
    c.workers[rep_wid].role = NodeRole::Replica;
    c.workers[rep_wid].replica_of_node_id = Some(n1.to_string());
    manager.unsafe_set_config(c);
  }

  let shards = manager.current_config().get_shards_info();
  // 两个分片
  assert!(shards.starts_with("*2\r\n"));
  // 分片 1：本节点 slots 区段 [0..=4] + nodes (仅自身)
  assert!(shards.contains("$5\r\nslots\r\n*2\r\n:0\r\n:4\r\n"));
  assert!(shards.contains("$5\r\nnodes\r\n*1\r\n"));
  // 分片 2：n1 slots 区段 [100..=200] + nodes (自身 + 1 从节点)
  assert!(shards.contains("*2\r\n:100\r\n:200\r\n"));
  assert!(shards.contains("$5\r\nnodes\r\n*2\r\n"));
  // 节点元信息表字段：id / port / ip / endpoint / hostname / role / replication-offset / health
  assert!(shards.contains("$2\r\nid\r\n$40\r\n"));
  assert!(shards.contains("$8\r\nendpoint\r\n$9\r\n127.0.0.1\r\n"));
  assert!(shards.contains("$8\r\nhostname\r\n$6\r\nhost-0\r\n"));
  assert!(shards.contains("$4\r\nrole\r\n$6\r\nmaster\r\n"));
  assert!(shards.contains("$18\r\nreplication-offset\r\n:0\r\n"));
  assert!(shards.contains("$6\r\nhealth\r\n$6\r\nonline\r\n"));
  // 从节点角色标记为 slave
  let rep_entry = shards
    .split("$40\r\n")
    .find(|chunk| chunk.starts_with(rep))
    .map(|chunk| &chunk[41..])
    .unwrap();
  assert!(rep_entry.contains("$4\r\nrole\r\n$5\r\nslave\r\n"));

  // 离线槽位不得进入分片区段，且空档正确断开区段
  {
    let mut c = manager.current_config();
    c.slot_map[2] = HashSlot::default();
    let shards = c.get_shards_info();
    // 空档断开后为两个区段 (2 区段 = 4 个整数端点)
    assert!(shards.contains("$5\r\nslots\r\n*4\r\n:0\r\n:1\r\n:3\r\n:4\r\n"));
    assert!(!shards.contains(":2\r\n"));
  }

  OK
}

/// CLUSTER NODES 端口边界：极大端口 cport 饱和不 panic
#[test]
fn test_cluster_nodes_port_saturation() -> Void {
  info!("开始测试：极大端口下 CLUSTER NODES 输出安全");

  let mut conf = ClusterConfig::new();
  conf.initialize_local_worker(Worker::primary(
    "3000000000000000000000000000000000000000",
    "127.0.0.1",
    u16::MAX,
    0,
  ));
  let nodes = conf.get_cluster_nodes();
  assert!(nodes.contains("3000000000000000000000000000000000000000 127.0.0.1:65535@65535"));

  OK
}

/// CLUSTER SLOTS 非法属主索引安全跳过不 panic
#[test]
fn test_slots_info_skips_unknown_owner() -> Void {
  info!("开始测试：CLUSTER SLOTS 非法属主索引安全跳过");

  let mut conf = ClusterConfig::new();
  conf.initialize_local_worker(Worker::primary(
    "3000000000000000000000000000000000000000",
    "127.0.0.1",
    7000,
    1,
  ));
  // 构造越界属主：worker 9 不在节点表内
  conf.slot_map[7] = HashSlot::new(9, SlotState::Stable);
  let resp = conf.get_slots_info();
  assert_eq!(resp, "*0\r\n");

  OK
}

/// 对标 Garnet ClusterManagementTests.cs 中的 ClusterForgetTest：节点握手与遗忘保护
#[test]
fn test_node_handshake_and_forget() -> Void {
  info!("开始测试：节点握手与成员发现 (MEET & FORGET)");

  let manager = ClusterManager::new();
  let local_id = "f000000000000000000000000000000000000000";
  let peer1_id = "f111111111111111111111111111111111111111";
  let peer2_id = "f222222222222222222222222222222222222222";

  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 1));

  // MEET 加入两个节点
  manager.try_meet("127.0.0.1", 7001, Some(peer1_id))?;
  manager.try_meet("127.0.0.1", 7002, Some(peer2_id))?;

  let conf = manager.current_config();
  assert_eq!(conf.num_workers(), 3);
  assert!(conf.is_known(peer1_id));
  assert!(conf.is_known(peer2_id));

  // FORGET 遗忘 peer1
  manager.try_remove_worker(peer1_id, 30)?;
  let conf_after_forget = manager.current_config();
  assert_eq!(conf_after_forget.num_workers(), 2);
  assert!(!conf_after_forget.is_known(peer1_id));
  assert!(conf_after_forget.is_known(peer2_id));

  // 遗忘不存在的未知节点报错
  let err_unknown = manager.try_remove_worker("unknown_node_id", 30);
  assert!(err_unknown.is_err());

  // 遗忘自身节点报错 (对标 Garnet: ERR I tried to forget myself)
  let err_self = manager.try_remove_worker(local_id, 30);
  assert!(err_self.is_err());

  OK
}

/// CLUSTER MEET 幂等性测试
#[test]
fn test_meet_is_idempotent() -> Void {
  info!("开始测试：MEET 幂等性");

  let manager = ClusterManager::new();
  let local_id = "3000000000000000000000000000000000000000";
  let peer1_id = "3111111111111111111111111111111111111111";
  let peer2_id = "3222222222222222222222222222222222222222";

  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 0));
  assert_eq!(manager.try_meet("127.0.0.1", 7001, Some(peer1_id))?, 2);
  // 同 ID 重复 MEET：仅更新地址端口
  assert_eq!(manager.try_meet("10.0.0.1", 7001, Some(peer1_id))?, 2);
  // 同地址端口未知 ID：回填不新增
  assert_eq!(manager.try_meet("10.0.0.1", 7001, None)?, 2);
  // 完全新节点：新增
  assert_eq!(manager.try_meet("127.0.0.1", 7002, Some(peer2_id))?, 3);

  let conf = manager.current_config();
  assert_eq!(conf.num_workers(), 3);
  assert_eq!(
    conf.get_worker_address_from_node_id(peer1_id),
    Some(("10.0.0.1", 7001))
  );

  OK
}

/// FORGET 禁入名单阻止 gossip 重新收录
#[test]
fn test_forget_ban_list_blocks_remerge() -> Void {
  info!("开始测试：FORGET 禁入名单与 gossip 重收录防护");

  let manager = ClusterManager::new();
  let local_id = "3000000000000000000000000000000000000000";
  let peer1_id = "3111111111111111111111111111111111111111";

  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 0));
  manager.try_meet("127.0.0.1", 7001, Some(peer1_id))?;
  assert!(manager.current_config().is_known(peer1_id));

  // 遗忘并禁入 60 秒
  manager.try_remove_worker(peer1_id, 60)?;
  assert!(!manager.current_config().is_known(peer1_id));

  // 禁入期内 gossip 报文被拒绝
  let packet = GossipPacket {
    header: GossipHeader {
      sender_id: peer1_id.to_string(),
      sender_addr: "127.0.0.1".to_string(),
      sender_port: 7001,
      sender_role: NodeRole::Primary,
      config_epoch: 9,
      assigned_slots: vec![1, 2],
      migrating_slots: Vec::new(),
      replica_of: None,
      hostname: None,
    },
    sections: Vec::new(),
  };
  assert!(!manager.try_merge_gossip(&packet));
  assert!(!manager.current_config().is_known(peer1_id));

  OK
}

/// 禁入名单阻断第三方切片重新收录
#[test]
fn test_ban_blocks_third_party_section_reintroduction() -> Void {
  info!("开始测试：禁入名单阻断第三方切片重新收录");

  let manager = ClusterManager::new();
  let n0 = "3000000000000000000000000000000000000000";
  let n1 = "3111111111111111111111111111111111111111";
  let n2 = "3222222222222222222222222222222222222222";

  manager.init_local(Worker::primary(n0, "127.0.0.1", 7000, 0));
  manager.try_meet("127.0.0.1", 7001, Some(n1))?;
  manager.try_meet("127.0.0.1", 7002, Some(n2))?;
  manager.try_remove_worker(n1, 60)?;
  assert!(!manager.current_config().is_known(n1));

  // 第三方 N2 的切片携带已禁入的 N1：不得经由切片重新收录
  let packet = GossipPacket {
    header: GossipHeader {
      sender_id: n2.to_string(),
      sender_addr: "127.0.0.1".to_string(),
      sender_port: 7002,
      sender_role: NodeRole::Primary,
      config_epoch: 0,
      assigned_slots: Vec::new(),
      migrating_slots: Vec::new(),
      replica_of: None,
      hostname: None,
    },
    sections: vec![GossipNodeSection {
      node_id: n1.to_string(),
      address: "127.0.0.1".to_string(),
      port: 7001,
      role: NodeRole::Primary,
      config_epoch: 9,
      is_pfail: false,
      is_fail: false,
      ..Default::default()
    }],
  };
  assert!(!manager.try_merge_gossip(&packet));
  assert!(!manager.current_config().is_known(n1));

  OK
}

/// 节点角色状态机与故障标志 (Worker / NodeRole / NodeId，对标 Garnet Worker.cs)
#[test]
fn test_node_role_state_machine_and_flags() -> Void {
  info!("开始测试：节点角色状态机与故障标志完备性");

  // 1. NodeRole 字符串解析与显示往返 (对标 Garnet NodeRole)
  assert_eq!(
    NodeRole::from_role_str("master").ok(),
    Some(NodeRole::Primary)
  );
  assert_eq!(
    NodeRole::from_role_str("PRIMARY").ok(),
    Some(NodeRole::Primary)
  );
  assert_eq!(
    NodeRole::from_role_str("slave").ok(),
    Some(NodeRole::Replica)
  );
  assert_eq!(
    NodeRole::from_role_str("replica").ok(),
    Some(NodeRole::Replica)
  );
  assert_eq!(
    NodeRole::from_role_str("unassigned").ok(),
    Some(NodeRole::Unassigned)
  );
  assert!(NodeRole::from_role_str("bogus").is_err());
  assert_eq!(NodeRole::Primary.as_str(), "master");
  assert_eq!(NodeRole::Replica.as_str(), "slave");
  assert_eq!(NodeRole::from_u8(0), NodeRole::Primary);
  assert_eq!(NodeRole::from_u8(1), NodeRole::Replica);
  assert_eq!(NodeRole::from_u8(0xFF), NodeRole::Unassigned);

  // 2. NodeId 解析校验：长度 / 十六进制字符 / 大小写归一
  let parsed: NodeId = "ABCDEF0123456789ABCDEF0123456789ABCDEF01".parse().unwrap();
  assert_eq!(parsed.as_str(), "abcdef0123456789abcdef0123456789abcdef01");
  assert!(NodeId::from_str("short").is_err());
  assert!(NodeId::from_str("zzzz0000000000000000000000000000000000000").is_err());
  let generated = NodeId::generate();
  assert_eq!(generated.as_str().len(), 40);
  assert!(generated.as_str().bytes().all(|b| b.is_ascii_hexdigit()));
  assert!(!generated.is_empty());
  assert!(NodeId::default().is_empty());

  // 3. Worker 故障标志状态机：PFAIL -> FAIL -> 恢复 (对标 Garnet Worker.cs)
  let mut w = Worker::new(
    "3000000000000000000000000000000000000000",
    "127.0.0.1",
    7000,
    0,
    NodeRole::Primary,
  );
  assert!(!w.is_fail && !w.is_pfail);
  assert_eq!(w.link_state, LinkState::Connected);

  w.set_pfail();
  assert!(w.is_pfail);
  // 升级为 FAIL 后清除 PFAIL 并断开链路
  w.set_fail();
  assert!(w.is_fail && !w.is_pfail);
  assert_eq!(w.link_state, LinkState::Disconnected);

  // 已确认 FAIL 时不再降级回 PFAIL
  w.set_pfail();
  assert!(!w.is_pfail && w.is_fail);

  // 清除故障恢复正常连接
  w.clear_fail();
  assert!(!w.is_fail && !w.is_pfail);
  assert_eq!(w.link_state, LinkState::Connected);

  // 4. 从节点便捷构造携带复制源，便捷构建器字段生效
  let master_id = "3111111111111111111111111111111111111111";
  let rep = Worker::replica(
    "3222222222222222222222222222222222222222",
    "127.0.0.1",
    7001,
    1,
    master_id,
  )
  .with_hostname("node-b");
  assert_eq!(rep.role, NodeRole::Replica);
  assert_eq!(rep.replica_of_node_id.as_deref(), Some(master_id));
  assert_eq!(rep.hostname.as_deref(), Some("node-b"));
  // 空主机名归一化为 None
  assert!(
    Worker::primary("0000000000000000000000000000000000000000", "h", 1, 0)
      .with_hostname("")
      .hostname
      .is_none()
  );

  OK
}

/// 两级锁架构并发隔离与死锁排查 (state_lock + config RwLock)
#[test]
fn test_two_level_lock_hierarchy_and_concurrency() -> Void {
  info!("开始测试：两级锁架构隔离与无死锁排查");

  let manager = Arc::new(ClusterManager::new());
  let local_id = "1000000000000000000000000000000000000001";
  let remote_id = "2000000000000000000000000000000000000002";

  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 1));
  manager.try_meet("127.0.0.1", 7001, Some(remote_id))?;

  // 零拷贝锁守卫访问 config()
  {
    let guard = manager.config();
    assert_eq!(guard.local_node_id(), Some(local_id));
    assert_eq!(guard.num_workers(), 2);
  }

  let mut handles = Vec::new();

  // 4 个只读并发线程
  for _ in 0..4 {
    let mgr = Arc::clone(&manager);
    handles.push(thread::spawn(move || {
      for i in 0..1000 {
        let guard = mgr.config();
        let _ = guard.has_assigned_slots(1);
        drop(guard);

        let mut buf = Buffer::new();
        let mut k = String::from("test_key_");
        k.push_str(buf.format(i));
        let k = k.into_bytes();
        let _ = mgr.verify_key(&k, false, false, false);
      }
    }));
  }

  // 2 个写并发线程
  for tid in 0..2 {
    let mgr = Arc::clone(&manager);
    handles.push(thread::spawn(move || {
      for step in 0..100 {
        let slot = (tid * 100 + step) as u16;
        let _ = mgr.try_add_slots(&[slot]);
        let _ = mgr.try_bump_cluster_epoch();
        let _ = mgr.try_remove_slots(&[slot]);
      }
    }));
  }

  for h in handles {
    h.join().expect("两级锁并发测试线程异常中断");
  }

  assert_eq!(manager.current_config().num_workers(), 2);

  OK
}

/// 对标 Garnet ClusterManagementTests.cs 中的 ClusterResetTest 与 ClusterResetFailsForMasterWithKeysInSlotsTest：集群重置操作
#[test]
fn test_cluster_reset() -> Void {
  info!("开始测试：集群重置操作 (RESET SOFT / HARD)");

  let manager = ClusterManager::new();
  let local_id = "c000000000000000000000000000000000000000";
  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 5));
  manager.try_add_slots(&[0, 1, 2])?;
  manager.try_meet(
    "127.0.0.1",
    7001,
    Some("c111111111111111111111111111111111111111"),
  )?;

  // 1. 当存在键时拒绝重置 (对标 ClusterResetFailsForMasterWithKeysInSlotsTest)
  let err_keys = manager.try_reset(true, true);
  assert!(err_keys.is_err());

  // 2. SOFT 重置：保留节点 ID 与纪元，但清空槽位与对端节点 (对标 ClusterResetTest SOFT)
  manager.try_reset(true, false)?;
  let conf_soft = manager.current_config();
  assert_eq!(conf_soft.local_node_id(), Some(local_id));
  assert_eq!(conf_soft.local_node_config_epoch(), 5);
  assert_eq!(conf_soft.num_workers(), 1);
  assert_eq!(conf_soft.get_slot_count_for_state(SlotState::Stable), 0);

  // 3. HARD 重置：生成全新 NodeId，纪元归零，清空槽位与对端 (对标 ClusterResetTest HARD)
  manager.try_reset(false, false)?;
  let conf_hard = manager.current_config();
  assert_ne!(conf_hard.local_node_id(), Some(local_id));
  assert_eq!(conf_hard.local_node_config_epoch(), 0);
  assert_eq!(conf_hard.num_workers(), 1);
  assert_eq!(conf_hard.get_slot_count_for_state(SlotState::Stable), 0);

  OK
}
