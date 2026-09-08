use std::{
  cmp::Ordering,
  sync::{Arc, Barrier},
  thread,
};

use aok::{OK, Void, anyhow, bail};
use log::info;
use wedb_resp::RespCommand;
use wedb_txn::{
  ExecPreparation, ExecResult, LockType, QueuedCommand, TransactionManager, TxnKeyComparison,
  TxnKeyEntries, TxnState, WatchVersionMap,
};

use super::support::{MockDatabase, b};

/// 对标 Garnet 事务多键排序 2PL 加锁防死锁验证
/// 验证多线程分别以相反顺序 A->B 与 B->A 排队操作，通过全局哈希全序排序加锁避免死锁
#[test]
fn test_deadlock_prevention() -> Void {
  let vmap = Arc::new(WatchVersionMap::default());
  let barrier = Arc::new(Barrier::new(2));

  let vmap1 = Arc::clone(&vmap);
  let barrier1 = Arc::clone(&barrier);
  let h1 = thread::spawn(move || -> aok::Result<()> {
    let mut mgr = TransactionManager::new();
    mgr.multi()?;
    // 线程 1 顺序：key_A -> key_B
    mgr.queue_command(QueuedCommand::new(
      RespCommand::SET,
      vec![b("key_A"), b("1")],
    ))?;
    mgr.queue_command(QueuedCommand::new(
      RespCommand::SET,
      vec![b("key_B"), b("1")],
    ))?;

    barrier1.wait();
    let res = mgr.exec(&vmap1, |_| "ok")?;
    assert!(matches!(res, ExecResult::Success(_)));
    OK
  });

  let vmap2 = Arc::clone(&vmap);
  let barrier2 = Arc::clone(&barrier);
  let h2 = thread::spawn(move || -> aok::Result<()> {
    let mut mgr = TransactionManager::new();
    mgr.multi()?;
    // 线程 2 逆序：key_B -> key_A
    mgr.queue_command(QueuedCommand::new(
      RespCommand::SET,
      vec![b("key_B"), b("2")],
    ))?;
    mgr.queue_command(QueuedCommand::new(
      RespCommand::SET,
      vec![b("key_A"), b("2")],
    ))?;

    barrier2.wait();
    let res = mgr.exec(&vmap2, |_| "ok")?;
    assert!(matches!(res, ExecResult::Success(_)));
    OK
  });

  h1.join()
    .map_err(|e| anyhow!("线程 1 join 失败: {e:?}"))??;
  h2.join()
    .map_err(|e| anyhow!("线程 2 join 失败: {e:?}"))??;

  info!("DeadlockPrevention: 多键相反顺序入队防死锁测试通过");
  OK
}

/// 对标 Garnet 2PL 死锁防御键锁排序与锁升级合并算法
#[test]
fn test_key_entries_deadlock_prevention_sort() -> Void {
  let mut entries = TxnKeyEntries::new();

  // 乱序插入多个键（含重复哈希及锁级别冲突）
  entries.add_key(300, LockType::Shared, Some(b("k3")));
  entries.add_key(100, LockType::Shared, Some(b("k1")));
  entries.add_key(200, LockType::Shared, Some(b("k2")));
  // 插入相同哈希的写锁，应升级为排他写锁
  entries.add_key(100, LockType::Exclusive, Some(b("k1")));
  entries.add_key(200, LockType::Shared, Some(b("k2")));

  assert_eq!(entries.len(), 5);
  assert!(!entries.is_read_only());

  // 执行死锁防御排序
  entries.sort_by_key_hash();

  // 校验去重合并后总数
  assert_eq!(entries.len(), 3);

  let slice = entries.as_slice();
  assert_eq!(slice[0].key_hash, 100);
  assert_eq!(slice[0].lock_type, LockType::Exclusive);

  assert_eq!(slice[1].key_hash, 200);
  assert_eq!(slice[1].lock_type, LockType::Shared);

  assert_eq!(slice[2].key_hash, 300);
  assert_eq!(slice[2].lock_type, LockType::Shared);

  let cmp = TxnKeyComparison::compare(&slice[0], &slice[1]);
  assert_eq!(cmp, Ordering::Less);

  let lockset = entries.get_lockset(1);
  assert!(lockset.contains("phase: lock"));

  info!("KeyEntriesDeadlockSort: 死锁防御键锁排序与合并测试通过");
  OK
}

/// 对标 Garnet 高并发 WATCH 乐观锁对抗压力测试
/// 8 线程高频并发发起事务修改同一共享受监视键，严格验证状态守恒
#[test]
fn test_high_concurrency_watch_contention() -> Void {
  let db = MockDatabase::new();
  let thread_count = 8;
  let iterations = 25;
  let shared_key = "account_balance";

  db.string_set(shared_key, "1000");

  let mut handles = Vec::new();
  for _ in 0..thread_count {
    let db_clone = db.clone();

    handles.push(thread::spawn(move || -> aok::Result<(usize, usize)> {
      let mut client = db_clone.create_client();
      let mut success = 0;
      let mut conflict = 0;

      for _ in 0..iterations {
        client.send_command("WATCH account_balance");
        client.send_command("MULTI");
        client.send_command("SET account_balance 999");
        let resp = client.send_command("EXEC");
        if resp.starts_with('*') && !resp.starts_with("*-1") {
          success += 1;
        } else if resp.starts_with("*-1") {
          conflict += 1;
        }
      }
      Ok((success, conflict))
    }));
  }

  let mut total_success = 0;
  let mut total_conflict = 0;
  for h in handles {
    let (s, c) = h.join().map_err(|e| anyhow!("线程 join 失败: {e:?}"))??;
    total_success += s;
    total_conflict += c;
  }

  assert_eq!(total_success + total_conflict, thread_count * iterations);
  info!(
    "HighConcurrencyWatchContention: 高并发监视乐观锁竞争测试通过（成功：{total_success}，冲突：{total_conflict}）"
  );
  OK
}

/// 对标 Garnet 全局版本映射表多会话并发竞争原子性
/// 验证版本号单调递增且最终版本恰好等于所有提交成功的事务总数
#[test]
fn test_concurrent_watch_version_map_atomicity() -> Void {
  let vmap = Arc::new(WatchVersionMap::default());
  let target_key = b("shared_resource");

  let mut handles = Vec::new();
  let thread_count = 8;
  let iterations_per_thread = 50;

  for _ in 0..thread_count {
    let vmap_clone = Arc::clone(&vmap);
    let key = target_key.clone();

    handles.push(thread::spawn(move || -> aok::Result<(usize, usize)> {
      let mut local_mgr = TransactionManager::new();
      let mut success_count = 0;
      let mut conflict_count = 0;

      for _ in 0..iterations_per_thread {
        local_mgr.watch(key.clone(), &vmap_clone)?;
        local_mgr.multi()?;
        local_mgr.queue_command(QueuedCommand::new(
          RespCommand::SET,
          vec![key.clone(), b("val")],
        ))?;

        match local_mgr.exec(&vmap_clone, |_| "ok")? {
          ExecResult::Success(_) => {
            success_count += 1;
          }
          ExecResult::Conflict => {
            conflict_count += 1;
          }
          ExecResult::Aborted => {
            bail!("并发竞争中不应发生语法中止");
          }
        }
      }

      Ok((success_count, conflict_count))
    }));
  }

  let mut total_success = 0;
  let mut total_conflict = 0;

  for h in handles {
    let (s, c) = h.join().map_err(|e| anyhow!("线程 join 失败: {e:?}"))??;
    total_success += s;
    total_conflict += c;
  }

  assert_eq!(
    total_success + total_conflict,
    thread_count * iterations_per_thread
  );
  let final_version = vmap.read_version_key(&target_key);
  assert_eq!(final_version, total_success as u64);

  info!(
    "ConcurrentVersionMapAtomicity: 并发版本原子性校验通过（成功 {total_success}，冲突 {total_conflict}，版本 {final_version}）"
  );
  OK
}

/// 对标 Garnet 拆分式准备与锁后校验（闭合加锁竞态窗口）
#[test]
fn test_split_phase_prepare_and_validate() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  let k = b("split_key");

  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(
    RespCommand::SET,
    vec![k.clone(), b("v")],
  ))?;

  // 阶段一：构建锁集合
  assert!(mgr.prepare_lockset()?);
  assert_eq!(mgr.state(), TxnState::Started);
  assert!(!mgr.key_entries().is_empty());

  // 外部并发修改模拟加锁窗口冲突
  vmap.bump_version_key(&k);
  // 阶段二：持锁后校验必须拦截冲突
  assert!(!mgr.validate_watches(&vmap));
  assert_eq!(mgr.state(), TxnState::None);
  assert_eq!(mgr.watched_keys().watched_count(), 0);

  // 成功流程
  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(
    RespCommand::SET,
    vec![k.clone(), b("v2")],
  ))?;
  assert!(mgr.prepare_lockset()?);
  assert!(mgr.validate_watches(&vmap));
  mgr.begin_run();
  assert_eq!(mgr.state(), TxnState::Running);
  let commands = mgr.take_queue();
  assert_eq!(commands.len(), 1);
  mgr.commit(&vmap);
  assert_eq!(mgr.state(), TxnState::None);
  assert_eq!(vmap.read_version_key(&k), 2);

  info!("SplitPhasePrepareAndValidate: 拆分准备与锁后校验测试通过");
  OK
}

/// 对标 Garnet 拆分流程冲突后保留锁集合供释放物理锁
#[test]
fn test_split_conflict_preserves_lockset() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  let k = b("lock_keep");

  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(
    RespCommand::SET,
    vec![k.clone(), b("v")],
  ))?;

  assert!(mgr.prepare_lockset()?);
  let locked = mgr.key_entries().len();
  assert!(locked >= 1);

  // 持锁期间外部变异
  vmap.bump_version_key(&k);
  assert!(!mgr.validate_watches(&vmap));
  // 冲突后锁条目保留以便释放物理锁
  assert_eq!(mgr.key_entries().len(), locked);
  assert_eq!(mgr.state(), TxnState::None);
  mgr.key_entries_mut().unlock_all_keys();
  assert_eq!(mgr.key_entries().len(), 0);

  // 一站式 exec_prepare 自动清空锁条目
  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(
    RespCommand::SET,
    vec![k.clone(), b("v2")],
  ))?;
  vmap.bump_version_key(&k);
  assert_eq!(mgr.exec_prepare(&vmap)?, ExecPreparation::Conflict);
  assert_eq!(mgr.key_entries().len(), 0);
  assert_eq!(mgr.state(), TxnState::None);

  info!("SplitConflictPreservesLockset: 拆分冲突保留锁条目测试通过");
  OK
}

/// 对标 64 位哈希碰撞时的全序排序防死锁防御
#[test]
fn test_hash_collision_total_order() -> Void {
  let mut entries = TxnKeyEntries::new();
  let fake_hash = 777_888_999;

  entries.add_key(fake_hash, LockType::Shared, Some(b("hash_collision_beta")));
  entries.add_key(
    fake_hash,
    LockType::Exclusive,
    Some(b("hash_collision_alpha")),
  );

  entries.sort_by_key_hash();

  // 碰撞键不合并：按键字节全序排序，保证物理加锁顺序全局一致
  assert_eq!(entries.len(), 2);
  let slice = entries.as_slice();
  assert_eq!(slice[0].key_hash, fake_hash);
  assert_eq!(slice[1].key_hash, fake_hash);
  assert_eq!(
    slice[0].key.as_deref(),
    Some(b("hash_collision_alpha").as_ref())
  );
  assert_eq!(
    slice[1].key.as_deref(),
    Some(b("hash_collision_beta").as_ref())
  );

  // 同哈希且键数据等价（或缺失）时必须合并并升级为最高锁级别
  let mut merged = TxnKeyEntries::new();
  merged.add_key(fake_hash, LockType::Shared, Some(b("hash_collision_alpha")));
  merged.add_key(fake_hash, LockType::Exclusive, None);
  merged.add_key(fake_hash, LockType::Shared, Some(b("hash_collision_alpha")));
  merged.sort_by_key_hash();
  assert_eq!(merged.len(), 1);
  assert_eq!(merged.as_slice()[0].lock_type, LockType::Exclusive);

  let cmp1 = TxnKeyComparison::compare(&slice[0], &slice[1]);
  let cmp2 = TxnKeyComparison::compare(&slice[1], &slice[0]);
  assert_eq!(cmp1, Ordering::Less);
  assert_eq!(cmp2, Ordering::Greater);

  info!("HashCollisionTotalOrder: 哈希碰撞全序排序测试通过");
  OK
}

/// 对标 VersionMap 全表自增广播、清空与对齐
#[test]
fn test_version_map_bump_all_and_clear() -> Void {
  let vmap = WatchVersionMap::default();
  let k1 = b"bk1";
  let k2 = b"bk2";

  vmap.bump_version_key(k1);
  vmap.bump_version_key(k1);
  vmap.bump_version_key(k2);
  assert_eq!(vmap.read_version_key(k1), 2);
  assert_eq!(vmap.read_version_key(k2), 1);

  let pre_k3 = vmap.read_version_key(b"bk3");
  vmap.bump_all();
  assert_eq!(vmap.read_version_key(k1), 3);
  assert_eq!(vmap.read_version_key(k2), 2);
  assert_eq!(vmap.read_version_key(b"bk3"), pre_k3 + 1);

  vmap.clear();
  assert_eq!(vmap.read_version_key(k1), 0);
  assert_eq!(vmap.read_version_key(k2), 0);
  assert_eq!(vmap.read_version_key(b"bk3"), 0);

  let vmap2 = WatchVersionMap::new(3);
  assert_eq!(vmap2.slot_count(), 4);
  assert_eq!(vmap2.mask(), 3);

  info!("VersionMapBumpAllAndClear: 版本表全表自增与对齐测试通过");
  OK
}

/// 对标 Garnet 版本表与键锁条目生命周期
#[test]
fn test_version_map_and_lock_lifecycle() -> Void {
  let vmap = WatchVersionMap::new(512);
  assert_eq!(vmap.mask(), 511);

  let key = b"compat_key";
  assert_eq!(vmap.read_version_key(key), 0);
  assert_eq!(vmap.bump_version_key(key), 1);
  assert_eq!(vmap.read_version_key(key), 1);

  let mut entries = TxnKeyEntries::new();
  entries.add_key_bytes(b("k1"), LockType::Shared);
  entries.add_key_bytes(b("k2"), LockType::Exclusive);
  assert_eq!(entries.len(), 2);
  entries.unlock_all_keys();
  assert_eq!(entries.len(), 0);

  info!("VersionMapAndLockLifecycle: 版本表与键锁条目生命周期测试通过");
  OK
}
