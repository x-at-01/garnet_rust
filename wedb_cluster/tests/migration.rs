use aok::{OK, Void};
use itoa::Buffer;
use log::info;
use wedb_cluster::{
  ClusterConfig, ClusterManager, GossipPacket, RouteResult, SlotState, Worker, hash_slot,
};
#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

const N0: &str = "3000000000000000000000000000000000000000";
const N1: &str = "3111111111111111111111111111111111111111";
const N2: &str = "3222222222222222222222222222222222222222";

fn mgr(local_id: &str, port: u16) -> ClusterManager {
  let m = ClusterManager::new();
  m.init_local(Worker::primary(local_id, "127.0.0.1", port, 0));
  m
}

fn wid(conf: &ClusterConfig, node_id: &str) -> usize {
  conf.get_worker_id_from_node_id(node_id)
}

/// 对标 Garnet ClusterManagementTests.cs 中的 ClusterSetSlotBadOptions：SETSLOT 状态机流转与验证
#[test]
fn test_slot_migration_state_transitions() -> Void {
  info!("开始测试：槽位状态迁移与准备状态机");

  let manager = ClusterManager::new();
  let local_id = "a000000000000000000000000000000000000000";
  let target_id = "a111111111111111111111111111111111111111";

  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 1));
  manager.try_add_slots(&[100, 101])?;
  manager.try_meet("127.0.0.1", 7001, Some(target_id))?;

  // 1. 本地槽位设置为 MIGRATING 迁出状态
  manager.try_prepare_slot_for_migration(100, target_id)?;
  let conf = manager.current_config();
  assert_eq!(conf.get_state(100), SlotState::Migrating);
  // CLUSTER NODES 中应包含迁移标记 [100->-target_id]
  let nodes_str = conf.get_cluster_nodes();
  let mut s = String::from("[100->-");
  s.push_str(target_id);
  s.push(']');
  assert!(nodes_str.contains(&s));

  // 再次迁移已在迁移中的槽位应当失败
  assert!(
    manager
      .try_prepare_slot_for_migration(100, target_id)
      .is_err()
  );

  // 向自身迁移报错
  assert!(
    manager
      .try_prepare_slot_for_migration(101, local_id)
      .is_err()
  );

  // 2. 在目标节点模拟 IMPORTING 迁入状态
  let target_manager = ClusterManager::new();
  target_manager.init_local(Worker::primary(target_id, "127.0.0.1", 7001, 1));
  target_manager.try_meet("127.0.0.1", 7000, Some(local_id))?;

  {
    let mut c = target_manager.current_config();
    let node0_idx = c.get_worker_id_from_node_id(local_id);
    c.update_slot_state(100, node0_idx as u16, SlotState::Stable);
    target_manager.unsafe_set_config(c);
  }

  target_manager.try_prepare_slot_for_import(100, local_id)?;
  let target_conf = target_manager.current_config();
  assert_eq!(target_conf.get_state(100), SlotState::Importing);
  let mut s = String::from("[100-<-");
  s.push_str(local_id);
  s.push(']');
  assert!(target_conf.get_cluster_nodes().contains(&s));

  // 3. 完成迁移：SETSLOT 100 NODE <target_id>
  manager.try_prepare_slot_for_ownership_change(100, target_id)?;
  assert_eq!(manager.current_config().get_state(100), SlotState::Stable);

  target_manager.try_prepare_slot_for_ownership_change(100, target_id)?;
  assert_eq!(
    target_manager.current_config().get_state(100),
    SlotState::Stable
  );

  // 4. SETSLOT 101 STABLE 测试
  manager.try_prepare_slot_for_migration(101, target_id)?;
  manager.try_prepare_slot_for_stable(101)?;
  assert_eq!(manager.current_config().get_state(101), SlotState::Stable);

  OK
}

/// 迁移中断恢复：SETSLOT STABLE 将迁出槽位属主拉回本地
#[test]
fn test_migration_abort_via_stable_restores_local_ownership() -> Void {
  info!("开始测试：迁移中断后 STABLE 恢复本地属主");

  let manager = mgr(N0, 7000);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;

  let key = (0..100000u32)
    .map(|i| {
      let mut buf = Buffer::new();
      let mut s = String::from("mig:");
      s.push_str(buf.format(i));
      s.into_bytes()
    })
    .find(|k| hash_slot(k) == 500)
    .unwrap();

  manager.try_add_slots(&[500])?;
  manager.try_prepare_slot_for_migration(500, N1)?;

  // 迁移中断：SETSLOT 500 STABLE
  manager.try_prepare_slot_for_stable(500)?;
  let conf = manager.current_config();
  assert_eq!(conf.get_state(500), SlotState::Stable);
  // 属主必须拉回本地节点
  assert_eq!(conf.get_owner_id_from_slot(500), Some(N0));
  // 路由恢复本地服务，不再 ASK
  assert_eq!(
    manager.verify_key(&key, false, false, false),
    RouteResult::Ok(500)
  );
  let mut s = String::from("[500->-");
  s.push_str(N1);
  s.push(']');
  assert!(!conf.get_cluster_nodes().contains(&s));

  OK
}

/// FORGET 迁移目标节点：迁出槽位回归本地
#[test]
fn test_forget_migration_target_restores_slot() -> Void {
  info!("开始测试：遗忘迁出目标节点后槽位回归本地");

  let manager = mgr(N0, 7000);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  manager.try_add_slots(&[501])?;
  manager.try_prepare_slot_for_migration(501, N1)?;

  manager.try_remove_worker(N1, 30)?;
  let conf = manager.current_config();
  assert!(!conf.is_known(N1));
  assert_eq!(conf.get_state(501), SlotState::Stable);
  assert_eq!(conf.get_owner_id_from_slot(501), Some(N0));

  OK
}

/// FORGET 迁入源节点：导入中槽位重置为离线
#[test]
fn test_forget_import_source_resets_slot_offline() -> Void {
  info!("开始测试：遗忘迁入源节点后导入槽位离线");

  let manager = mgr(N0, 7000);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  {
    let mut conf = manager.current_config();
    conf.update_slot_state(502, wid(&conf, N1) as u16, SlotState::Stable);
    manager.unsafe_set_config(conf);
  }
  manager.try_prepare_slot_for_import(502, N1)?;
  assert_eq!(
    manager.current_config().get_state(502),
    SlotState::Importing
  );

  manager.try_remove_worker(N1, 30)?;
  let conf = manager.current_config();
  assert_eq!(conf.get_state(502), SlotState::Offline);

  OK
}

/// SETSLOT NODE 导入确认提升本地纪元以保证归属传播
#[test]
fn test_ownership_change_bumps_epoch_on_import_confirm() -> Void {
  info!("开始测试：导入确认后纪元提升");

  let manager = mgr(N0, 7000);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  manager.try_meet("127.0.0.1", 7002, Some(N2))?;
  {
    let mut conf = manager.current_config();
    let n1 = wid(&conf, N1);
    conf.update_slot_state(600, n1 as u16, SlotState::Stable);
    conf.workers[n1].config_epoch = 5;
    manager.unsafe_set_config(conf);
  }
  manager.try_prepare_slot_for_import(600, N1)?;
  assert_eq!(manager.current_config().local_node_config_epoch(), 0);

  // 确认归属：槽位划归本地且纪元提升为已知最大值加一 (5 + 1 = 6)
  manager.try_prepare_slot_for_ownership_change(600, N0)?;
  let conf = manager.current_config();
  assert_eq!(conf.get_state(600), SlotState::Stable);
  assert_eq!(conf.get_owner_id_from_slot(600), Some(N0));
  assert_eq!(conf.local_node_config_epoch(), 6);

  OK
}

/// DELSLOTS 允许移除迁出中槽位并将纪元提升为最大值加一
#[test]
fn test_delslots_migrating_slot_and_epoch_bump() -> Void {
  info!("开始测试：DELSLOTS 迁出槽位与纪元提升");

  let manager = mgr(N0, 7000);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  manager.try_add_slots(&[503])?;
  manager.try_prepare_slot_for_migration(503, N1)?;

  // 迁出中槽位允许 DELSLOTS
  manager.try_remove_slots(&[503])?;
  let conf = manager.current_config();
  assert_eq!(conf.get_state(503), SlotState::Offline);

  // 完全离线槽位拒绝 DELSLOTS
  assert!(manager.try_remove_slots(&[503]).is_err());

  OK
}

/// 迁入槽位 STABLE 保留源属主指针，路由回源
#[test]
fn test_stable_on_importing_keeps_source_owner() -> Void {
  info!("开始测试：导入槽位 STABLE 后保留源属主");

  let manager = mgr(N0, 7000);
  manager.try_meet("127.0.0.1", 7001, Some(N1))?;
  {
    let mut conf = manager.current_config();
    conf.update_slot_state(504, wid(&conf, N1) as u16, SlotState::Stable);
    manager.unsafe_set_config(conf);
  }
  manager.try_prepare_slot_for_import(504, N1)?;
  manager.try_prepare_slot_for_stable(504)?;

  let conf = manager.current_config();
  assert_eq!(conf.get_state(504), SlotState::Stable);
  // 属主指针仍指向源节点 N1：本地路由返回 -MOVED 至 N1
  assert_eq!(conf.get_owner_id_from_slot(504), Some(N1));

  let key = (0..100000u32)
    .map(|i| {
      let mut buf = Buffer::new();
      let mut s = String::from("imp:");
      s.push_str(buf.format(i));
      s.into_bytes()
    })
    .find(|k| hash_slot(k) == 504)
    .unwrap();
  assert!(matches!(
    manager.verify_key(&key, false, false, false),
    RouteResult::Moved { .. }
  ));

  OK
}

/// 迁出中槽位豁免弃权回收
#[test]
fn test_renounce_protects_migrating_slots() -> Void {
  info!("开始测试：迁出中槽位豁免弃权回收");

  // 接收方 B 视图：N0 稳定持有 10,11,12
  let b = mgr(N1, 7001);
  b.try_meet("127.0.0.1", 7000, Some(N0))?;
  {
    let mut conf = b.current_config();
    let n0 = wid(&conf, N0);
    for s in [10u16, 11, 12] {
      conf.update_slot_state(s, n0 as u16, SlotState::Stable);
    }
    b.unsafe_set_config(conf);
  }

  // 发送方 A：10,11 稳定持有，12 迁出至 N2
  let a = mgr(N0, 7000);
  a.try_meet("127.0.0.1", 7002, Some(N2))?;
  a.try_add_slots(&[10, 11, 12])?;
  a.try_prepare_slot_for_migration(12, N2)?;

  // 携带迁出保护的报文：12 不得被弃权回收
  let packet = GossipPacket {
    header: a.build_gossip_packet(3).header,
    sections: Vec::new(),
  };
  assert_eq!(packet.header.assigned_slots, vec![10, 11]);
  assert_eq!(packet.header.migrating_slots, vec![12]);
  assert!(!b.try_merge_gossip(&packet));
  let conf = b.current_config();
  for s in [10u16, 11, 12] {
    assert_eq!(conf.get_owner_id_from_slot(s), Some(N0));
    assert_eq!(conf.get_state(s), SlotState::Stable);
  }

  // 若发送方未声明迁出保护：槽位 12 被弃权回收为离线
  let mut bare = packet.clone();
  bare.header.migrating_slots.clear();
  assert!(b.try_merge_gossip(&bare));
  assert_eq!(b.current_config().get_state(12), SlotState::Offline);

  OK
}

/// 迁移生命周期与 gossip 最终一致性收敛
#[test]
fn test_migration_lifecycle_with_gossip_convergence() -> Void {
  info!("开始测试：迁移生命周期与 gossip 最终一致");

  let a = mgr(N0, 7000);
  let b = mgr(N1, 7001);
  a.try_meet("127.0.0.1", 7001, Some(N1))?;
  b.try_meet("127.0.0.1", 7000, Some(N0))?;
  a.try_add_slots(&[700])?;

  // A 通告自身持有槽位 700，B 合并收敛
  let announce = a.build_gossip_packet(3);
  assert!(b.try_merge_gossip(&announce));
  assert_eq!(b.current_config().get_owner_id_from_slot(700), Some(N0));

  // B 发起导入后确认归属，纪元提升
  b.try_prepare_slot_for_import(700, N0)?;
  b.try_prepare_slot_for_ownership_change(700, N1)?;
  let b_epoch = b.current_config().local_node_config_epoch();
  assert!(b_epoch > 0);

  // B 以更高纪元重新通告槽位 700，A 合并后归属转移
  let take_over = b.build_gossip_packet(3);
  assert_eq!(take_over.header.config_epoch, b_epoch);
  assert!(a.try_merge_gossip(&take_over));
  let a_conf = a.current_config();
  assert_eq!(a_conf.get_owner_id_from_slot(700), Some(N1));
  assert_eq!(a_conf.get_state(700), SlotState::Stable);

  // A 上的路由请求现在应 -MOVED 至 B
  let key = (0..100000u32)
    .map(|i| {
      let mut buf = Buffer::new();
      let mut s = String::from("cnv:");
      s.push_str(buf.format(i));
      s.into_bytes()
    })
    .find(|k| hash_slot(k) == 700)
    .unwrap();
  assert!(matches!(
    a.verify_key(&key, false, false, false),
    RouteResult::Moved { slot: 700, .. }
  ));

  OK
}
