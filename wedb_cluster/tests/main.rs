use aok::{OK, Void};
use itoa::Buffer;
use log::info;
use wedb_cluster::{ClusterManager, RouteResult, SlotState, Worker, hash_slot, route_request};
#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 集群端到端完整生命周期轻量集成验证
#[test]
fn test_cluster_end_to_end_lifecycle() -> Void {
  info!("开始测试：集群端到端完整生命周期轻量集成");

  let n0_id = "1000000000000000000000000000000000000000";
  let n1_id = "2000000000000000000000000000000000000000";

  let mgr0 = ClusterManager::new();
  mgr0.init_local(Worker::primary(n0_id, "127.0.0.1", 7000, 1).with_hostname("node-0"));

  let mgr1 = ClusterManager::new();
  mgr1.init_local(Worker::primary(n1_id, "127.0.0.1", 7001, 1).with_hostname("node-1"));

  // 1. 节点互连握手 (MEET)
  mgr0.try_meet("127.0.0.1", 7001, Some(n1_id))?;
  mgr1.try_meet("127.0.0.1", 7000, Some(n0_id))?;

  // 2. 槽位分配：N0 分配 0..1000，N1 分配 1000..2000
  let slots_n0: Vec<u16> = (0..1000).collect();
  mgr0.try_add_slots(&slots_n0)?;

  let slots_n1: Vec<u16> = (1000..2000).collect();
  mgr1.try_add_slots(&slots_n1)?;

  // 3. Gossip 单轮同步，使 N0 知晓 N1 的槽位，N1 知晓 N0 的槽位
  let pkt0 = mgr0.build_gossip_packet(4);
  let pkt1 = mgr1.build_gossip_packet(4);
  assert!(mgr0.try_merge_gossip(&pkt1));
  assert!(mgr1.try_merge_gossip(&pkt0));

  // 4. 路由判定验证
  // 查找一个 slot 在 0..1000 内的 key
  let local_key = (0..10000u32)
    .map(|i| {
      let mut buf = Buffer::new();
      let mut k = String::from("k:");
      k.push_str(buf.format(i));
      k.into_bytes()
    })
    .find(|k| hash_slot(k) < 1000)
    .expect("未找到槽位在 0..1000 内的测试键");
  let local_slot = hash_slot(&local_key);

  let res_local = mgr0.verify_key(&local_key, true, false, false);
  assert_eq!(res_local, RouteResult::Ok(local_slot));

  // 查找一个 slot 在 1000..2000 内的 key
  let remote_key = (0..10000u32)
    .map(|i| {
      let mut buf = Buffer::new();
      let mut k = String::from("rem:");
      k.push_str(buf.format(i));
      k.into_bytes()
    })
    .find(|k| (1000..2000).contains(&hash_slot(k)))
    .expect("未找到槽位在 1000..2000 内的测试键");
  let remote_slot = hash_slot(&remote_key);

  let res_moved = mgr0.verify_key(&remote_key, false, false, false);
  assert_eq!(
    res_moved,
    RouteResult::Moved {
      slot: remote_slot,
      endpoint: "127.0.0.1:7001".to_string(),
    }
  );

  // 5. 槽位迁移生命周期：将 local_slot 从 N0 迁往 N1
  mgr0.try_prepare_slot_for_migration(local_slot, n1_id)?;
  mgr1.try_prepare_slot_for_import(local_slot, n0_id)?;

  // 迁移中本地不存在的键触发 -ASK
  let res_ask = mgr0.verify_key(&local_key, false, false, false);
  assert_eq!(
    res_ask,
    RouteResult::Ask {
      slot: local_slot,
      endpoint: "127.0.0.1:7001".to_string(),
    }
  );

  // 迁入节点携带 ASKING 时允许服务
  let conf_importing = mgr1.current_config();
  assert_eq!(
    route_request(&conf_importing, &local_key, true, false),
    RouteResult::Ok(local_slot)
  );

  // 确认归属转移
  mgr0.try_prepare_slot_for_ownership_change(local_slot, n1_id)?;
  mgr1.try_prepare_slot_for_ownership_change(local_slot, n1_id)?;

  assert_eq!(
    mgr0.current_config().get_state(local_slot),
    SlotState::Stable
  );
  assert_eq!(
    mgr1.current_config().get_state(local_slot),
    SlotState::Stable
  );

  // N0 路由转为 -MOVED
  let res_now_moved = mgr0.verify_key(&local_key, false, false, false);
  assert_eq!(
    res_now_moved,
    RouteResult::Moved {
      slot: local_slot,
      endpoint: "127.0.0.1:7001".to_string(),
    }
  );

  // 6. 拓扑内省信息验证
  let info_str = mgr0.current_config().get_cluster_info();
  assert!(info_str.contains("cluster_state:ok\r\n"));
  assert!(info_str.contains("cluster_known_nodes:2\r\n"));

  OK
}
