use aok::{OK, Void};
use itoa::Buffer;
use log::info;
use wedb_cluster::{
  ClusterConfig, ClusterManager, GossipHeader, GossipNodeSection, GossipPacket,
  MAX_GOSSIP_SECTIONS, NodeRole, SlotState, Worker,
};
#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

const N0: &str = "3000000000000000000000000000000000000000";
const N1: &str = "3111111111111111111111111111111111111111";
const N2: &str = "3222222222222222222222222222222222222222";
const N3: &str = "3333333333333333333333333333333333333333";

const NA: &str = "4000000000000000000000000000000000000000";
const NB: &str = "4111111111111111111111111111111111111111";
const NC: &str = "4222222222222222222222222222222222222222";
const ND: &str = "4333333333333333333333333333333333333333";

const RA: &str = "5000000000000000000000000000000000000000";
const RB: &str = "5111111111111111111111111111111111111111";
const RC: &str = "5222222222222222222222222222222222222222";

fn mgr(local_id: &str, port: u16, epoch: i64) -> ClusterManager {
  let m = ClusterManager::new();
  m.init_local(Worker::primary(local_id, "127.0.0.1", port, epoch));
  m
}

fn header(sender_id: &str, port: u16, role: NodeRole, epoch: i64, slots: &[u16]) -> GossipHeader {
  GossipHeader {
    sender_id: sender_id.to_string(),
    sender_addr: "127.0.0.1".to_string(),
    sender_port: port,
    sender_role: role,
    config_epoch: epoch,
    assigned_slots: slots.to_vec(),
    migrating_slots: Vec::new(),
    replica_of: None,
    hostname: None,
  }
}

fn plain_packet(
  sender_id: &str,
  port: u16,
  role: NodeRole,
  epoch: i64,
  slots: &[u16],
) -> GossipPacket {
  GossipPacket {
    header: header(sender_id, port, role, epoch, slots),
    sections: Vec::new(),
  }
}

fn wid(conf: &ClusterConfig, node_id: &str) -> usize {
  conf.get_worker_id_from_node_id(node_id)
}

/// Gossip 合并收录发送方并接受其离线槽位认领
#[test]
fn test_gossip_merge_discovers_and_claims_offline_slots() -> Void {
  info!("开始测试：gossip 合并收录节点与离线槽位认领");

  let manager = mgr(N0, 7000, 0);
  let packet = plain_packet(N1, 7001, NodeRole::Primary, 5, &[1, 2, 3]);
  assert!(manager.try_merge_gossip(&packet));

  let conf = manager.current_config();
  assert_eq!(conf.num_workers(), 2);
  assert!(conf.is_known(N1));
  assert_eq!(conf.get_node_role_from_node_id(N1), NodeRole::Primary);
  for s in [1u16, 2, 3] {
    assert_eq!(conf.get_owner_id_from_slot(s), Some(N1));
    assert_eq!(conf.get_state(s), SlotState::Stable);
  }

  // 幂等：重放同一报文不再产生变更
  assert!(!manager.try_merge_gossip(&packet));

  // 本地节点自身报文不得改写本地条目
  let self_packet = plain_packet(N0, 7000, NodeRole::Primary, 99, &[0, 1, 2, 3]);
  assert!(!manager.try_merge_gossip(&self_packet));
  assert_eq!(manager.current_config().local_node_config_epoch(), 0);
  assert_eq!(manager.current_config().get_state(0), SlotState::Offline);

  OK
}

/// 槽位冲突仲裁：低纪元认领被拒绝，高纪元认领夺槽
#[test]
fn test_merge_slot_conflict_epoch_arbitration() -> Void {
  info!("开始测试：槽位冲突的纪元仲裁");

  let manager = mgr(N0, 7000, 0);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  // 预置：N1 (纪元 5) 持有槽位 7
  {
    let mut conf = manager.current_config();
    let n1 = wid(&conf, N1);
    conf.update_slot_state(7, n1 as u16, SlotState::Stable);
    conf.workers[n1].config_epoch = 5;
    manager.unsafe_set_config(conf);
  }

  // 低纪元 (4) 认领被拒绝
  assert!(!manager.try_merge_gossip(&plain_packet(N1, 7001, NodeRole::Primary, 4, &[7])));
  let conf = manager.current_config();
  assert_eq!(conf.get_owner_id_from_slot(7), Some(N1));

  // 高纪元 (9) 的新节点 N2 认领槽位 7：夺槽成功
  assert!(manager.try_merge_gossip(&plain_packet(N2, 7002, NodeRole::Primary, 9, &[7])));
  let conf = manager.current_config();
  assert_eq!(conf.get_owner_id_from_slot(7), Some(N2));
  assert_eq!(conf.get_state(7), SlotState::Stable);

  OK
}

/// PFAIL/FAIL 多数派仲裁：仅主节点投票有效，清除支持恢复
#[test]
fn test_fail_detection_majority_voting() -> Void {
  info!("开始测试：PFAIL/FAIL 多数派故障仲裁");

  let manager = mgr(N0, 7000, 0);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?; // 主节点
  manager.try_meet("127.0.0.1", 7002, Some(N2))?; // 主节点
  manager.try_meet("127.0.0.1", 7003, Some(N3))?; // 从节点
  {
    let mut conf = manager.current_config();
    let n3 = wid(&conf, N3);
    conf.workers[n3].role = NodeRole::Replica;
    manager.unsafe_set_config(conf);
  }

  // 4 节点中 3 主 1 从 -> 多数派为 2 票；本地怀疑 N1 仅为 1 票
  manager.mark_node_pfail(N1);
  assert!(manager.evaluate_fail_status().is_empty());
  assert!(!manager.is_node_failed(N1));

  // 从节点 N3 的投票不计入多数派
  let replica_report = GossipPacket {
    header: header(N3, 7003, NodeRole::Replica, 0, &[]),
    sections: vec![GossipNodeSection {
      node_id: N1.to_string(),
      address: "127.0.0.1".to_string(),
      port: 7001,
      role: NodeRole::Primary,
      config_epoch: 0,
      is_pfail: true,
      is_fail: false,
      ..Default::default()
    }],
  };
  let _ = manager.try_merge_gossip(&replica_report);
  assert!(manager.evaluate_fail_status().is_empty());

  // 主节点 N2 报告 PFAIL -> 达到 2 票多数派 -> 升级 FAIL
  let primary_report = GossipPacket {
    header: header(N2, 7002, NodeRole::Primary, 0, &[]),
    sections: vec![GossipNodeSection {
      node_id: N1.to_string(),
      address: "127.0.0.1".to_string(),
      port: 7001,
      role: NodeRole::Primary,
      config_epoch: 0,
      is_pfail: true,
      is_fail: false,
      ..Default::default()
    }],
  };
  let _ = manager.try_merge_gossip(&primary_report);
  assert_eq!(manager.evaluate_fail_status(), vec![N1.to_string()]);
  assert!(manager.is_node_failed(N1));

  // 清除本地怀疑并恢复：不再处于 FAIL
  manager.clear_node_pfail(N1);
  assert!(!manager.is_node_failed(N1));

  OK
}

/// Gossip 报文构建携带本地 PFAIL 视图与稳定槽位
#[test]
fn test_build_gossip_packet_carries_pfail_view_and_slots() -> Void {
  info!("开始测试：gossip 包构建携带 PFAIL 视图与稳定槽位");

  let manager = mgr(N0, 7000, 0);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  manager.try_meet("127.0.0.1", 7002, Some(N2))?;
  manager.try_add_slots(&[10, 11, 200])?;
  manager.mark_node_pfail(N1);

  let packet = manager.build_gossip_packet(10);
  assert_eq!(packet.header.sender_id, N0);
  assert_eq!(packet.header.config_epoch, 0);
  assert_eq!(packet.header.assigned_slots, vec![10, 11, 200]);
  assert_eq!(packet.sections.len(), 2);
  for sec in &packet.sections {
    if sec.node_id == N1 {
      assert!(sec.is_pfail);
    } else {
      assert!(!sec.is_pfail);
    }
  }

  // 抽样数量限制生效
  let sampled = manager.build_gossip_packet(1);
  assert_eq!(sampled.sections.len(), 1);

  OK
}

/// 对标 Garnet ClusterManagementTests.cs 中的 ClusterFailoverBadOptions：纪元自增、碰撞仲裁与从节点故障转移
#[test]
fn test_epoch_bump_collision_and_failover() -> Void {
  info!("开始测试：纪元自增、冲突仲裁与从节点故障转移");

  let manager = ClusterManager::new();
  let local_id = "0000000000000000000000000000000000000001";
  let remote_id = "9999999999999999999999999999999999999999";

  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 10));
  manager.try_meet("127.0.0.1", 7001, Some(remote_id))?;

  // 1. CLUSTER BUMPEPOCH 自增
  let new_epoch = manager.try_bump_cluster_epoch();
  assert_eq!(new_epoch, 11);
  assert_eq!(manager.current_config().local_node_config_epoch(), 11);

  // 2. 纪元冲突解决：当对端纪元与本地相同，且 local_id < remote_id 时，本地递增纪元
  let mut conf = manager.current_config();
  let resolved = conf.resolve_epoch_collision(remote_id, 11);
  assert!(resolved);
  assert_eq!(conf.local_node_config_epoch(), 12);

  // 当对端 ID 小于本地 ID 时，本地不自增
  let smaller_remote = "0000000000000000000000000000000000000000";
  let not_resolved = conf.resolve_epoch_collision(smaller_remote, 12);
  assert!(!not_resolved);
  assert_eq!(conf.local_node_config_epoch(), 12);

  // 3. 从节点故障转移 CLUSTER FAILOVER
  let replica_mgr = ClusterManager::new();
  let rep_id = "8888888888888888888888888888888888888888";
  let pri_id = "1111111111111111111111111111111111111111";

  replica_mgr.init_local(Worker::replica(rep_id, "127.0.0.1", 7002, 5, pri_id));
  replica_mgr.try_meet("127.0.0.1", 7000, Some(pri_id))?;

  // 主节点持有槽位 0..10
  {
    let mut c = replica_mgr.current_config();
    let pri_wid = c.get_worker_id_from_node_id(pri_id);
    for s in 0..10 {
      c.update_slot_state(s, pri_wid as u16, SlotState::Stable);
    }
    replica_mgr.unsafe_set_config(c);
  }

  // 执行故障转移
  let failover_epoch = replica_mgr.try_failover()?;
  assert_eq!(failover_epoch, 6);

  let conf_after = replica_mgr.current_config();
  assert_eq!(conf_after.local_node_role(), NodeRole::Primary);
  assert_eq!(conf_after.local_node_primary_id(), None);
  for s in 0..10 {
    assert_eq!(conf_after.get_owner_id_from_slot(s), Some(rep_id));
  }

  OK
}

/// 从节点复制关系随 gossip 传播（携带主机名）
#[test]
fn test_gossip_propagates_replica_relationship() -> Void {
  info!("开始测试：gossip 传播从节点复制关系与主机名");

  let _a = mgr(RA, 7000, 0);
  let b = ClusterManager::new();
  b.init_local(Worker::replica(RB, "127.0.0.1", 7001, 0, RA).with_hostname("node-b"));
  b.try_meet("127.0.0.1", 7000, Some(RA))?;
  let c = mgr(RC, 7002, 0);

  // C 首次收到 B 的报文：应同时得知 A (主) 与 B (从, 复制源为 A)
  assert!(c.try_merge_gossip(&b.build_gossip_packet(8)));
  let conf = c.current_config();
  assert_eq!(conf.get_node_role_from_node_id(RA), NodeRole::Primary);
  assert_eq!(conf.get_node_role_from_node_id(RB), NodeRole::Replica);
  let rb = conf.get_worker_id_from_node_id(RB);
  assert_eq!(conf.workers[rb].replica_of_node_id.as_deref(), Some(RA));
  assert_eq!(conf.workers[rb].hostname.as_deref(), Some("node-b"));
  assert!(conf.get_replica_ids(RA).iter().any(|id| id == RB));

  let nodes = conf.get_cluster_nodes();
  let mut s = String::from(RB);
  s.push_str(" 127.0.0.1:7001@17001,node-b slave ");
  s.push_str(RA);
  s.push(' ');
  assert!(nodes.contains(&s));
  let mut s = String::from(RA);
  s.push_str(" 127.0.0.1:7000@17000 master");
  assert!(nodes.contains(&s));

  OK
}

/// 从节点纪元提升后 gossip 合并保留复制关系
#[test]
fn test_replica_epoch_bump_preserves_replicaof() -> Void {
  info!("开始测试：从节点纪元提升后复制关系保留");

  let a = mgr(RA, 7000, 0);
  let b = ClusterManager::new();
  b.init_local(Worker::replica(RB, "127.0.0.1", 7001, 0, RA).with_hostname("node-b"));
  b.try_meet("127.0.0.1", 7000, Some(RA))?;
  let c = mgr(RC, 7002, 0);
  let _ = a;

  // C 收录 B (纪元 0)
  assert!(c.try_merge_gossip(&b.build_gossip_packet(8)));
  assert_eq!(
    c.current_config().get_node_role_from_node_id(RB),
    NodeRole::Replica
  );

  // B 提升纪元后再次 gossip：复制关系与角色保留
  b.try_bump_cluster_epoch();
  assert_eq!(b.current_config().local_node_config_epoch(), 1);
  assert!(c.try_merge_gossip(&b.build_gossip_packet(8)));
  let conf = c.current_config();
  assert_eq!(conf.get_node_role_from_node_id(RB), NodeRole::Replica);
  let rb = conf.get_worker_id_from_node_id(RB);
  assert_eq!(conf.workers[rb].replica_of_node_id.as_deref(), Some(RA));
  assert_eq!(conf.workers[rb].hostname.as_deref(), Some("node-b"));
  assert_eq!(conf.workers[rb].config_epoch, 1);

  OK
}

/// 多节点全互连 gossip 交换收敛到不动点与幂等性
#[test]
fn test_gossip_multi_node_fixed_point_convergence() -> Void {
  info!("开始测试：多节点 gossip 合并不动点收敛");

  let ids = [NA, NB, NC, ND];
  let mut nodes = Vec::new();
  let a = mgr(NA, 7000, 1);
  a.try_add_slots(&(0..100u16).collect::<Vec<_>>())?;
  nodes.push(a);
  let b = mgr(NB, 7001, 2);
  b.try_add_slots(&(100..200u16).collect::<Vec<_>>())?;
  nodes.push(b);
  let c = ClusterManager::new();
  c.init_local(Worker::replica(NC, "127.0.0.1", 7002, 0, NA));
  nodes.push(c);
  let d = mgr(ND, 7003, 3);
  d.try_add_slots(&(200..300u16).collect::<Vec<_>>())?;
  nodes.push(d);

  // 全互连 MEET
  for (i, node) in nodes.iter().enumerate() {
    for (j, id) in ids.iter().enumerate() {
      if i != j {
        node.try_meet("127.0.0.1", 7000 + j as u16, Some(id))?;
      }
    }
  }

  // 逐轮交换直至收敛
  let mut converged_rounds = None;
  for round in 1..=10u32 {
    let packets: Vec<(usize, _)> = nodes
      .iter()
      .enumerate()
      .map(|(i, n)| (i, n.build_gossip_packet(8)))
      .collect();
    let mut changes = 0usize;
    for (i, packet) in &packets {
      for (j, node) in nodes.iter().enumerate() {
        if *i != j && node.try_merge_gossip(packet) {
          changes += 1;
        }
      }
    }
    if changes == 0 {
      converged_rounds = Some(round);
      break;
    }
  }
  assert!(
    converged_rounds.is_some_and(|r| r <= 3),
    "gossip 交换应在 3 轮内收敛: {converged_rounds:?}"
  );

  // 不动点重放幂等
  for (i, node) in nodes.iter().enumerate() {
    let packet = node.build_gossip_packet(8);
    for (j, peer) in nodes.iter().enumerate() {
      if i != j {
        assert!(!peer.try_merge_gossip(&packet), "重放报文不应产生变更");
      }
    }
  }

  // 全局视图一致性
  for node in &nodes {
    let conf = node.current_config();
    assert_eq!(conf.get_node_id_from_slot(50), Some(NA));
    assert_eq!(conf.get_node_id_from_slot(150), Some(NB));
    assert_eq!(conf.get_node_id_from_slot(250), Some(ND));
    assert_eq!(conf.get_node_role_from_node_id(NC), NodeRole::Replica);
    let nc = conf.get_worker_id_from_node_id(NC);
    assert_eq!(conf.workers[nc].replica_of_node_id.as_deref(), Some(NA));
  }

  OK
}

/// SET-CONFIG-EPOCH 纪元单调性约束
#[test]
fn test_set_config_epoch_monotonic() -> Void {
  info!("开始测试：SET-CONFIG-EPOCH 单调性约束");

  let manager = mgr(N0, 7000, 0);
  // 零纪元设定正数成功
  manager.try_set_local_config_epoch(5)?;
  assert_eq!(manager.current_config().local_node_config_epoch(), 5);
  // 非零纪元拒绝覆写
  assert!(manager.try_set_local_config_epoch(9).is_err());
  assert_eq!(manager.current_config().local_node_config_epoch(), 5);
  // 非正数拒绝
  manager.try_reset(true, false)?;
  assert!(manager.try_set_local_config_epoch(0).is_err());

  // 接入对端后拒绝设定
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  assert!(manager.try_set_local_config_epoch(7).is_err());

  OK
}

/// 发送方弃权回收不再持有的槽位，真实属主得以重新认领
#[test]
fn test_merge_sender_renounce_resets_lost_slots() -> Void {
  info!("开始测试：发送方弃权回收与真实属主重新认领");

  let manager = mgr(N0, 7000, 0);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  // 预置：N1 (纪元 5) 稳定持有槽位 1,2,3
  {
    let mut conf = manager.current_config();
    let n1 = wid(&conf, N1);
    conf.workers[n1].config_epoch = 5;
    for s in [1u16, 2, 3] {
      conf.update_slot_state(s, n1 as u16, SlotState::Stable);
    }
    manager.unsafe_set_config(conf);
  }

  // N1 只认领 1,2：槽位 3 弃权回收为离线
  let packet = plain_packet(N1, 7001, NodeRole::Primary, 5, &[1, 2]);
  assert!(manager.try_merge_gossip(&packet));
  let conf = manager.current_config();
  assert_eq!(conf.get_owner_id_from_slot(1), Some(N1));
  assert_eq!(conf.get_owner_id_from_slot(2), Some(N1));
  assert_eq!(conf.get_state(3), SlotState::Offline);

  // 真实属主 N2 (纪元 9) 认领槽位 3：离线槽位成功认领
  assert!(manager.try_merge_gossip(&plain_packet(N2, 7002, NodeRole::Primary, 9, &[3])));
  assert_eq!(manager.current_config().get_owner_id_from_slot(3), Some(N2));

  OK
}

/// 对标 Garnet ClusterManagementTests.cs 中的 ClusterRoleCommand：REPLICAOF 复制关系切换路径与 REPLICAOF NO ONE
#[test]
fn test_replicaof_and_no_one_paths() -> Void {
  info!("开始测试：REPLICAOF 复制关系切换路径");

  let manager = mgr(N0, 7000, 0);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  manager.try_meet("127.0.0.1", 7002, Some(N2))?;
  manager.try_add_slots(&[5, 6])?;
  {
    let mut conf = manager.current_config();
    let n2 = wid(&conf, N2);
    conf.workers[n2].role = NodeRole::Replica;
    manager.unsafe_set_config(conf);
  }

  // 非法复制源：未知节点 / 自身 / 从节点
  assert!(
    manager
      .try_replicaof("4044444444444444444444444444444444444444")
      .is_err()
  );
  assert!(manager.try_replicaof(N0).is_err());
  assert!(manager.try_replicaof(N2).is_err());

  // 正常切换：本地转从、记录复制源、本地槽位划归新主
  manager.try_replicaof(N1)?;
  let conf = manager.current_config();
  assert_eq!(conf.local_node_role(), NodeRole::Replica);
  assert_eq!(conf.local_node_primary_id(), Some(N1));
  for s in [5u16, 6] {
    assert_eq!(conf.get_owner_id_from_slot(s), Some(N1));
    assert_eq!(conf.get_state(s), SlotState::Stable);
  }

  // REPLICAOF NO ONE：晋升主节点并提升纪元
  let epoch = manager.try_replicaof_no_one()?;
  assert!(epoch > 0);
  let conf = manager.current_config();
  assert_eq!(conf.local_node_role(), NodeRole::Primary);
  assert_eq!(conf.local_node_primary_id(), None);
  // 主节点重复调用为幂等无操作
  assert_eq!(manager.try_replicaof_no_one()?, epoch);

  OK
}

/// 恶意 gossip 报文切片数量限流
#[test]
fn test_gossip_malicious_sections_capped() -> Void {
  info!("开始测试：恶意 gossip 报文切片数量限流");

  let manager = mgr(N0, 7000, 0);
  let sections = (0..MAX_GOSSIP_SECTIONS + 8)
    .map(|i| {
      let mut buf = Buffer::new();
      let digits = buf.format(i);
      let mut node_id = String::with_capacity(40);
      for _ in 0..(40 - digits.len()) {
        node_id.push('0');
      }
      node_id.push_str(digits);
      GossipNodeSection {
        node_id,
        address: "10.0.0.1".to_string(),
        port: 1000 + i as u16,
        role: NodeRole::Primary,
        config_epoch: 0,
        is_pfail: false,
        is_fail: false,
        ..Default::default()
      }
    })
    .collect();
  let packet = GossipPacket {
    header: header(N1, 7001, NodeRole::Primary, 0, &[]),
    sections,
  };
  assert!(manager.try_merge_gossip(&packet));
  // 超出上限的切片被截断
  assert_eq!(
    manager.current_config().num_workers(),
    MAX_GOSSIP_SECTIONS + 2
  );

  OK
}
