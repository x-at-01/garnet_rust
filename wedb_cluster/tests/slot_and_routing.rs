use std::{sync::Arc, thread};

use aok::{OK, Void};
use itoa::Buffer;
use log::info;
use wedb_cluster::{
  ClusterManager, HashSlot, RouteResult, SlotBitmap, SlotState, TOTAL_HASH_SLOTS, Worker, crc16,
  hash_slot, out_of_range, route_request, route_request_ext, route_slot_ext,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 对标 Garnet ClusterManagementTests.cs 中的 ClusterKeySlotTest：35 组原生哈希槽基准用例
#[test]
fn test_cluster_key_slot_benchmark() -> Void {
  info!("开始测试：Garnet 35 组原生哈希槽计算精确对齐");

  let test_cases: &[(&[u8], u16)] = &[
    (b"6e6bzswz8}", 7038),
    (b"8}jb94e7tf", 4828),
    (b"{}2xc5pbb7", 11672),
    (b"vr{a07}pdt", 12154),
    (b"cx{ldv}wdl", 14261),
    (b"erv805by}u", 15389),
    (b"{ey1pqbij}", 8341),
    (b"2tbjjyn}n8", 5152),
    (b"t}jehlyo06", 1232),
    (b"{u08t}xjal", 2490),
    (b"5g{mkb95a}", 3345),
    (b"x{v}x70nka", 7761),
    (b"g67ikt}q8q", 7694),
    (b"ovi8}mn7t7", 14473),
    (b"p5ljmg{}8s", 11196),
    (b"3wov{fd}8m", 3502),
    (b"bxmcjzi3{}", 10246),
    (b"{b1rrm7rn}", 14105),
    (b"e0{4ylm}78", 5069),
    (b"rkptge5}sx", 3468),
    (b"o6{uyxsy}j", 3278),
    (b"ykd6q{ma8}", 5754),
    (b"w{j5pz3iy}", 6520),
    (b"mhsr{dm}x0", 15077),
    (b"0}dtokfryr", 5134),
    (b"h7}0cj9mwm", 8187),
    (b"w{jhqd}frk", 5369),
    (b"5yzd{6}hzw", 5781),
    (b"w6b4vgtzr}", 6045),
    (b"4{b17h85}l", 5923),
    (b"Hm{W\x13\x1c", 7517),
    (b"zyy8yt1chw", 3081),
    (b"7858tqv03y", 773),
    (b"fdhhuk8yqv", 5763),
    (b"8bfgeino4s", 6257),
  ];

  for &(key, expected_slot) in test_cases {
    let actual_slot = hash_slot(key);
    assert_eq!(
      actual_slot,
      expected_slot,
      "哈希槽不匹配: key={:?}, 预期={}, 实际={}",
      String::from_utf8_lossy(key),
      expected_slot,
      actual_slot
    );
  }

  OK
}

/// HashTag 提取规则与跨命令同槽一致性测试
#[test]
fn test_hashtag_rules_and_consistency() -> Void {
  info!("开始测试：HashTag 规则与跨命令一致性");

  // 相同 HashTag 必定路由到同一个哈希槽
  let slot_profile = hash_slot(b"{user100}:profile");
  let slot_orders = hash_slot(b"{user100}:orders");
  let slot_feed = hash_slot(b"{user100}:feed");
  assert_eq!(slot_profile, slot_orders);
  assert_eq!(slot_orders, slot_feed);

  // 空大括号不作为有效 HashTag，按全键计算
  let slot_empty_tag = hash_slot(b"{}user100");
  let direct_crc = crc16(b"{}user100") & 16383;
  assert_eq!(slot_empty_tag, direct_crc);

  // 多个大括号只捕获第一个合法闭合区间
  let slot_multi_tag = hash_slot(b"{first}{second}");
  let slot_first_tag = hash_slot(b"{first}");
  assert_eq!(slot_multi_tag, slot_first_tag);

  // 未闭合或反向大括号退化为全键计算
  assert_eq!(hash_slot(b"user{100"), crc16(b"user{100") & 16383);
  assert_eq!(hash_slot(b"user}100{"), crc16(b"user}100{") & 16383);

  OK
}

/// HashTag memchr 解析极限边界测试
#[test]
fn test_hashtag_memchr_extreme_boundaries() -> Void {
  info!("开始测试：HashTag memchr 极限边界测试");

  // 正常区间
  let s1 = hash_slot(b"foo{bar}baz");
  let s2 = hash_slot(b"bar");
  assert_eq!(s1, s2);

  // 嵌套大括号：以第一个 '{' 到第一个 '}' 为准
  let nested = hash_slot(b"foo{b{a}r}baz");
  let expected_nested = hash_slot(b"b{a");
  assert_eq!(nested, expected_nested);

  // 只有左括号或只有右括号：退化为全键计算
  assert_eq!(hash_slot(b"{unclosed"), crc16(b"{unclosed") & 16383);
  assert_eq!(hash_slot(b"unopened}"), crc16(b"unopened}") & 16383);
  assert_eq!(hash_slot(b"}inverted{"), crc16(b"}inverted{") & 16383);

  // 二进制键与非 UTF-8 字节切片
  let raw_bytes = [0xFF, 0x00, b'{', 0x12, 0x34, b'}', 0xAA];
  let slot_raw = hash_slot(&raw_bytes);
  let expected_raw = hash_slot(&[0x12, 0x34]);
  assert_eq!(slot_raw, expected_raw);

  OK
}

/// SlotBitmap 位图运算与边界漏洞防护测试
#[test]
fn test_slot_bitmap_operations_and_bounds() -> Void {
  info!("开始测试：SlotBitmap 位图运算与边界防护");

  let mut bitmap = SlotBitmap::new();
  assert!(bitmap.is_empty());
  assert_eq!(bitmap.count(), 0);

  // 设置槽位 0 与最大槽位 16383
  bitmap.set(0);
  bitmap.set(16383);
  assert!(bitmap.is_set(0));
  assert!(bitmap.is_set(16383));
  assert!(!bitmap.is_set(1));
  assert_eq!(bitmap.count(), 2);

  // 越界设置与读取防护 (不 panic 也不破坏数据)
  bitmap.set(16384);
  bitmap.set(u16::MAX);
  assert!(!bitmap.is_set(16384));
  assert!(!bitmap.is_set(u16::MAX));
  assert_eq!(bitmap.count(), 2);

  // 翻转 toggle 与清除 clear
  bitmap.toggle(0);
  assert!(!bitmap.is_set(0));
  assert_eq!(bitmap.count(), 1);

  bitmap.toggle(0);
  assert!(bitmap.is_set(0));
  assert_eq!(bitmap.count(), 2);

  bitmap.clear(0);
  assert!(!bitmap.is_set(0));
  assert_eq!(bitmap.count(), 1);

  // 全满位图测试
  let full_map = SlotBitmap::full();
  assert!(full_map.is_full());
  assert_eq!(full_map.count(), TOTAL_HASH_SLOTS);
  assert_eq!(full_map.ranges(), vec![(0, 16383)]);
  for s in 0..16384 {
    assert!(full_map.is_set(s as u16));
  }

  // 连续区间合并提取 ranges() 测试
  let mut range_map = SlotBitmap::new();
  for s in 10..=20 {
    range_map.set(s);
  }
  range_map.set(100);
  for s in 120..=135 {
    range_map.set(s);
  }
  for s in 16380..=16383 {
    range_map.set(s);
  }

  let ranges = range_map.ranges();
  assert_eq!(
    ranges,
    vec![(10, 20), (100, 100), (120, 135), (16380, 16383)]
  );

  // 跨字边界单位间隙测试
  let mut boundary_gap_map = SlotBitmap::new();
  boundary_gap_map.set(63);
  boundary_gap_map.set(65);
  assert_eq!(boundary_gap_map.ranges(), vec![(63, 63), (65, 65)]);

  boundary_gap_map.set(64);
  assert_eq!(boundary_gap_map.ranges(), vec![(63, 65)]);

  // 二进制 2048 字节序列化与反序列化往返
  let bytes = range_map.to_bytes();
  assert_eq!(bytes.len(), 2048);
  let restored = SlotBitmap::from_bytes(&bytes)?;
  assert_eq!(range_map, restored);

  // 非法长度切片拒绝防护
  assert!(SlotBitmap::from_bytes(&bytes[..2047]).is_err());
  assert!(SlotBitmap::from_bytes(&[]).is_err());

  OK
}

/// SlotBitmap::ranges 差分验证：trailing_zeros 跳跃提取与朴素逐位扫描全量对照
#[test]
fn test_slot_bitmap_ranges_differential() -> Void {
  info!("开始测试：SlotBitmap::ranges 差分验证");

  for seed in 0..128u64 {
    let mut rng = fastrand::Rng::with_seed(seed);
    let mut bm = SlotBitmap::new();
    let density = match seed % 4 {
      0 => 0.02,
      1 => 0.5,
      2 => 0.98,
      _ => 0.25,
    };
    for s in 0..TOTAL_HASH_SLOTS {
      if rng.f64() < density {
        bm.set(s as u16);
      }
    }

    let mut expected = Vec::new();
    let mut s = 0;
    while s < TOTAL_HASH_SLOTS {
      if !bm.is_set(s as u16) {
        s += 1;
        continue;
      }
      let start = s;
      while s < TOTAL_HASH_SLOTS && bm.is_set(s as u16) {
        s += 1;
      }
      expected.push((start as u16, (s - 1) as u16));
    }
    assert_eq!(bm.ranges(), expected, "seed={seed} 差分失败");
    let total: usize = expected.iter().map(|(s, e)| (e - s + 1) as usize).sum();
    assert_eq!(total, bm.count(), "seed={seed} count 与区间长度总和不一致");
  }

  assert!(SlotBitmap::new().ranges().is_empty());
  assert_eq!(SlotBitmap::full().ranges(), vec![(0, 16383)]);

  OK
}

/// 对标 Garnet ClusterManagementTests.cs：哈希槽分配与内省 (ADDSLOTS / DELSLOTS / 越界检查)
#[test]
fn test_slot_allocation_and_introspection() -> Void {
  info!("开始测试：哈希槽分配与内省");

  let manager = ClusterManager::new();
  let local_id = "0123456789abcdef0123456789abcdef01234567";
  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 1));

  // 添加槽位 0..10
  let slots_to_add: Vec<u16> = (0..10).collect();
  let added = manager.try_add_slots(&slots_to_add)?;
  assert_eq!(added, 10);

  let conf = manager.current_config();
  for s in 0..10 {
    assert_eq!(conf.get_state(s), SlotState::Stable);
    assert_eq!(conf.get_owner_id_from_slot(s), Some(local_id));
  }
  assert!(conf.has_assigned_slots(1));

  // 重复添加已分配槽位应当失败
  assert!(manager.try_add_slots(&[5]).is_err());

  // 越界槽位添加应当失败
  assert!(manager.try_add_slots(&[16384]).is_err());
  assert!(out_of_range(16384));
  assert!(!out_of_range(16383));

  // 移除槽位 (DELSLOTS)
  let removed = manager.try_remove_slots(&[0, 1, 2])?;
  assert_eq!(removed, 3);
  let conf_after_remove = manager.current_config();
  assert_eq!(conf_after_remove.get_state(0), SlotState::Offline);
  assert_eq!(conf_after_remove.get_state(1), SlotState::Offline);
  assert_eq!(conf_after_remove.get_state(2), SlotState::Offline);
  assert_eq!(conf_after_remove.get_state(3), SlotState::Stable);

  // 移除未分配槽位应当报错
  assert!(manager.try_remove_slots(&[0]).is_err());

  OK
}

/// 对标 Garnet ClusterRedirectTests.cs：客户端请求路由判定与 -MOVED / -ASK / CROSSSLOT 帧生成
#[test]
fn test_request_routing_and_redirections() -> Void {
  info!("开始测试：客户端请求路由判定与重定向帧生成");

  let manager = ClusterManager::new();
  let local_id = "b000000000000000000000000000000000000000";
  let remote_id = "b111111111111111111111111111111111111111";

  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 1));
  manager.try_meet("127.0.0.1", 7001, Some(remote_id))?;

  let key_slot100 = b"{user100}:test";
  let target_slot = hash_slot(key_slot100);
  manager.try_add_slots(&[target_slot])?;

  // 本地稳定槽位：直接就地处理
  let res_local = manager.verify_key(key_slot100, true, false, false);
  assert_eq!(res_local, RouteResult::Ok(target_slot));
  assert_eq!(manager.to_resp_error(&res_local), None);

  // 本地槽位正在迁出 MIGRATING：
  manager.try_prepare_slot_for_migration(target_slot, remote_id)?;
  // 键在本地存在 -> 本地直接命中
  assert_eq!(
    manager.verify_key(key_slot100, true, false, false),
    RouteResult::Ok(target_slot)
  );
  // 键在本地不存在 -> 触发 -ASK 重定向到目标节点
  let res_ask = manager.verify_key(key_slot100, false, false, false);
  assert_eq!(
    res_ask,
    RouteResult::Ask {
      slot: target_slot,
      endpoint: "127.0.0.1:7001".to_string(),
    }
  );
  let mut buf = Buffer::new();
  let mut ask_msg = String::from("-ASK ");
  ask_msg.push_str(buf.format(target_slot));
  ask_msg.push_str(" 127.0.0.1:7001\r\n");
  assert_eq!(manager.to_resp_error(&res_ask), Some(ask_msg));

  // 非本地稳定槽位：触发 -MOVED 重定向
  let key_unowned = b"{other_slot}:test";
  let unowned_slot = hash_slot(key_unowned);
  {
    let mut c = manager.current_config();
    let remote_idx = c.get_worker_id_from_node_id(remote_id);
    c.update_slot_state(unowned_slot, remote_idx as u16, SlotState::Stable);
    manager.unsafe_set_config(c);
  }
  let res_moved = manager.verify_key(key_unowned, false, false, false);
  assert_eq!(
    res_moved,
    RouteResult::Moved {
      slot: unowned_slot,
      endpoint: "127.0.0.1:7001".to_string(),
    }
  );
  let mut buf = Buffer::new();
  let mut moved_msg = String::from("-MOVED ");
  moved_msg.push_str(buf.format(unowned_slot));
  moved_msg.push_str(" 127.0.0.1:7001\r\n");
  assert_eq!(manager.to_resp_error(&res_moved), Some(moved_msg));

  // 目标节点处于 IMPORTING 状态：未发送 ASKING 返回 -MOVED，携带 ASKING 允许接收
  {
    let mut c = manager.current_config();
    let remote_idx = c.get_worker_id_from_node_id(remote_id);
    c.update_slot_state(unowned_slot, remote_idx as u16, SlotState::Importing);
    manager.unsafe_set_config(c);
  }
  assert!(matches!(
    manager.verify_key(key_unowned, false, false, false),
    RouteResult::Moved { .. }
  ));
  assert_eq!(
    manager.verify_key(key_unowned, false, true, false),
    RouteResult::Ok(unowned_slot)
  );

  // 多键跨槽检测 (CROSSSLOT，对标 Garnet ClusterScatterGatherGetCrossSlotRedirectTest)
  let keys = [b"{user100}:key1".as_slice(), b"{user200}:key2".as_slice()];
  let res_cross = manager.verify_keys(&keys, false, false, false);
  assert_eq!(res_cross, RouteResult::CrossSlot);
  assert_eq!(
    manager.to_resp_error(&res_cross),
    Some("-CROSSSLOT Keys in request don't hash to the same slot\r\n".to_string())
  );

  OK
}

/// 重定向决策矩阵与未分配槽位安全性 (CLUSTERDOWN / READONLY 从节点只读)
#[test]
fn test_migration_redirection_matrix_and_unowned_slot() -> Void {
  info!("开始测试：重定向决策矩阵与未分配槽位安全性");

  let manager = ClusterManager::new();
  let local_id = "1111111111111111111111111111111111111111";
  let target_id = "2222222222222222222222222222222222222222";

  manager.init_local(Worker::primary(local_id, "127.0.0.1", 7000, 1));
  manager.try_meet("127.0.0.1", 7001, Some(target_id))?;

  // 完全未分配槽位：访问必须返回 CLUSTERDOWN
  let unassigned_key = b"unassigned_key";
  let res_unassigned = manager.verify_key(unassigned_key, false, false, false);
  assert_eq!(res_unassigned, RouteResult::ClusterDown);
  assert_eq!(
    manager.to_resp_error(&res_unassigned),
    Some("-CLUSTERDOWN Hash slot not served\r\n".to_string())
  );

  manager.try_add_slots(&[500])?;
  let mut conf = manager.current_config();
  conf.slot_map[500] = HashSlot::new(1, SlotState::Stable);

  // 槽位迁出状态
  manager.try_prepare_slot_for_migration(500, target_id)?;
  let conf_migrating = manager.current_config();
  let mut fake_key_slot500 = Vec::new();
  for candidate in 0..100000 {
    let mut buf = Buffer::new();
    let mut k = String::from("test:");
    k.push_str(buf.format(candidate));
    let k = k.into_bytes();
    if hash_slot(&k) == 500 {
      fake_key_slot500 = k;
      break;
    }
  }
  assert!(!fake_key_slot500.is_empty());

  let route_exist = route_request(&conf_migrating, &fake_key_slot500, false, true);
  assert_eq!(route_exist, RouteResult::Ok(500));

  let route_not_exist = route_request(&conf_migrating, &fake_key_slot500, false, false);
  assert_eq!(
    route_not_exist,
    RouteResult::Ask {
      slot: 500,
      endpoint: "127.0.0.1:7001".to_string(),
    }
  );

  // 模拟从节点 (READONLY)
  let replica_manager = ClusterManager::new();
  let replica_id = "3333333333333333333333333333333333333333";
  replica_manager.init_local(Worker::replica(replica_id, "127.0.0.1", 7002, 1, local_id));
  replica_manager.try_meet("127.0.0.1", 7000, Some(local_id))?;
  {
    let mut c = replica_manager.current_config();
    let primary_wid = c.get_worker_id_from_node_id(local_id);
    c.update_slot_state(500, primary_wid as u16, SlotState::Stable);
    replica_manager.unsafe_set_config(c);
  }

  let conf_replica = replica_manager.current_config();
  // 普通写请求 -> 返回 -MOVED 到主节点
  let res_write = route_request_ext(&conf_replica, &fake_key_slot500, false, false, false);
  assert_eq!(
    res_write,
    RouteResult::Moved {
      slot: 500,
      endpoint: "127.0.0.1:7000".to_string(),
    }
  );

  // 只读请求且槽位属于其主节点 -> 允许从节点本地读取 Ok
  let res_readonly = route_request_ext(&conf_replica, &fake_key_slot500, false, false, true);
  assert_eq!(res_readonly, RouteResult::Ok(500));

  // 基于槽位编号的 route_slot_ext 验证
  let res_slot_write = route_slot_ext(&conf_replica, 500, false, false, false);
  assert_eq!(
    res_slot_write,
    RouteResult::Moved {
      slot: 500,
      endpoint: "127.0.0.1:7000".to_string(),
    }
  );
  let res_slot_readonly = route_slot_ext(&conf_replica, 500, false, false, true);
  assert_eq!(res_slot_readonly, RouteResult::Ok(500));

  OK
}

/// 高并发路由与动态槽位迁移竞争 (多线程安全与无死锁)
#[test]
fn test_concurrent_routing_and_migration_stress() -> Void {
  info!("开始测试：高并发路由压测与槽位迁移动态竞争");

  let manager = Arc::new(ClusterManager::new());
  let node0_id = "0000000000000000000000000000000000000001";
  let node1_id = "0000000000000000000000000000000000000002";

  manager.init_local(Worker::primary(node0_id, "127.0.0.1", 7000, 1));
  manager.try_meet("127.0.0.1", 7001, Some(node1_id))?;

  let slots: Vec<u16> = (0..1000).collect();
  manager.try_add_slots(&slots)?;

  let mut handles = Vec::new();

  // 8 个并发读线程持续路由验证
  for thread_idx in 0..8 {
    let mgr = Arc::clone(&manager);
    let h = thread::spawn(move || {
      for i in 0..2000 {
        let slot = (i + thread_idx * 100) % 1000;
        let mut buf = Buffer::new();
        let mut key = String::from("user:");
        key.push_str(buf.format(slot));
        let key = key.into_bytes();
        let res = mgr.verify_key(&key, true, false, false);
        match res {
          RouteResult::Ok(s) => assert_eq!(s, hash_slot(&key)),
          RouteResult::Ask { .. } | RouteResult::Moved { .. } => {}
          RouteResult::ClusterDown | RouteResult::CrossSlot => {}
        }

        mgr.with_config(|c| {
          assert!(c.num_workers() >= 1);
        });
      }
    });
    handles.push(h);
  }

  // 2 个写线程并发触发槽位迁移与状态变更
  for _ in 0..2 {
    let mgr = Arc::clone(&manager);
    let h = thread::spawn(move || {
      for s in 100..120 {
        let _ = mgr.try_prepare_slot_for_migration(s, node1_id);
        thread::yield_now();
        let _ = mgr.try_prepare_slot_for_stable(s);
      }
    });
    handles.push(h);
  }

  for h in handles {
    h.join().expect("并发线程执行异常");
  }

  OK
}
