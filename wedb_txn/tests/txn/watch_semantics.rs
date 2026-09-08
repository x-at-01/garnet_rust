use aok::{OK, Void};
use log::info;
use wedb_resp::RespCommand;
use wedb_txn::{
  ExecResult, QueuedCommand, TransactionManager, TxnState, WatchVersionMap, WatchedKeysContainer,
};

use super::support::{MockDatabase, b};

/// 对标 Garnet TransactionTests.cs: SimpleWatchTest
/// 验证 WATCH 单键后被外部并发修改导致事务 EXEC 提交冲突 (*-1)，重试后正常提交
#[test]
fn test_simple_watch() -> Void {
  let db = MockDatabase::new();
  let mut client = db.create_client();

  assert_eq!(client.send_command("SET key1 value1"), "+OK\r\n");
  assert_eq!(client.send_command("WATCH key1"), "+OK\r\n");
  assert_eq!(client.send_command("MULTI"), "+OK\r\n");
  assert_eq!(client.send_command("GET key1"), "+QUEUED\r\n");
  assert_eq!(client.send_command("SET key2 value2"), "+QUEUED\r\n");

  // 外部并发修改 key1
  let mut client2 = db.create_client();
  assert_eq!(client2.send_command("SET key1 value1_updated"), "+OK\r\n");

  // 提交应失败并返回空数组
  assert_eq!(client.send_command("EXEC"), "*-1\r\n");

  // 重新发起事务，应当正常提交成功
  client.send_command("MULTI");
  client.send_command("GET key1");
  client.send_command("SET key2 value2");
  let resp = client.send_command("EXEC");
  assert_eq!(resp, "*2\r\n$14\r\nvalue1_updated\r\n+OK\r\n");

  info!("SimpleWatchTest: 单键并发修改冲突与重试测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: LargeTxnWatch
/// 验证大批量条件监视在事务中的正确性
#[test]
fn test_large_txn_watch() -> Void {
  let size = 512;
  let db = MockDatabase::new();
  let val_prefix = "abcdefg";
  let key_prefix = "mykey";

  for i in 0..size {
    db.string_set(
      format!("{}{}", key_prefix, i),
      format!("{}{}", val_prefix, i),
    );
  }

  let mut tran = db.create_transaction()?;
  let mut get_tasks = Vec::with_capacity(size);

  for i in 0..(size * 2) {
    if i % 2 == 0 {
      let k = format!("{}{}", key_prefix, i / 2);
      let t = tran.string_get_async(&k)?;
      get_tasks.push((i / 2, t));
    } else {
      let counter = (i - 1) / 2 + size;
      tran.string_set_async(
        format!("{}{}", key_prefix, counter),
        format!("{}{}", val_prefix, counter),
      )?;
    }
  }

  let committed = tran.execute();
  assert!(committed);

  for (orig_idx, task) in get_tasks {
    assert_eq!(
      task.result().as_deref(),
      Some(format!("{}{}", val_prefix, orig_idx).as_bytes())
    );
  }

  info!("LargeTxnWatch: 大规模事务监视测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: WatchTestWithSetWithEtag
/// 验证带 ETag 版本的条件监视冲突与事务内部 ETag 相关指令执行
#[test]
fn test_watch_with_set_with_etag() -> Void {
  let db = MockDatabase::new();
  let mut client = db.create_client();

  assert_eq!(client.send_command("SETWITHETAG key1 value1"), ":1\r\n");
  assert_eq!(client.send_command("WATCH key1"), "+OK\r\n");
  assert_eq!(client.send_command("MULTI"), "+OK\r\n");
  assert_eq!(client.send_command("GET key1"), "+QUEUED\r\n");
  assert_eq!(client.send_command("SET key2 value2"), "+QUEUED\r\n");

  // 外部并发以 SETWITHETAG 修改 key1
  let mut client2 = db.create_client();
  assert_eq!(
    client2.send_command("SETWITHETAG key1 value1_updated"),
    ":2\r\n"
  );

  // 提交应捕获冲突
  assert_eq!(client.send_command("EXEC"), "*-1\r\n");

  // 重新发起事务，在事务内测试多种 ETag 指令并成功提交
  client.send_command("MULTI");
  client.send_command("GET key1");
  client.send_command("SET key2 value2");
  client.send_command("SETWITHETAG key3 value2");
  client.send_command("GETWITHETAG key3");
  client.send_command("GETIFNOTMATCH key3 1");
  client.send_command("SETIFMATCH key3 anotherVal 1");
  client.send_command("SETWITHETAG key3 arandomval");

  let resp = client.send_command("EXEC");
  let expected = "*7\r\n$14\r\nvalue1_updated\r\n+OK\r\n:1\r\n*2\r\n:1\r\n$6\r\nvalue2\r\n*2\r\n:1\r\n$-1\r\n*2\r\n:2\r\n$-1\r\n:3\r\n";
  assert_eq!(resp, expected);

  // 校验最终 key1 的 ETag 值
  let resp = client.send_command("GETWITHETAG key1");
  assert_eq!(resp, "*2\r\n:2\r\n$14\r\nvalue1_updated\r\n");

  info!("WatchTestWithSetWithEtag: ETag 监视冲突与指令测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: WatchNonExistentKey
/// 验证监视未存在的键被并发创建时触发失效拦截
#[test]
fn test_watch_non_existent_key() -> Void {
  let db = MockDatabase::new();
  let mut client = db.create_client();

  assert_eq!(client.send_command("SET key2 value2"), "+OK\r\n");
  // 监视未存在的键 key1
  assert_eq!(client.send_command("WATCH key1"), "+OK\r\n");

  client.send_command("MULTI");
  client.send_command("GET key2");
  client.send_command("SET key3 value3");

  // 外部并发创建 key1
  let mut client2 = db.create_client();
  client2.send_command("SET key1 value1");

  // 提交应冲突失效
  assert_eq!(client.send_command("EXEC"), "*-1\r\n");

  // 再次重试，正常提交
  client.send_command("MULTI");
  client.send_command("GET key1");
  client.send_command("SET key2 value2");
  assert_eq!(client.send_command("EXEC"), "*2\r\n$6\r\nvalue1\r\n+OK\r\n");

  info!("WatchNonExistentKey: 监视不存在键并发创建冲突测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: WatchKeyFromDisk
/// 验证冷数据（海量键存储中）监视键被外部修改后触发失效拦截
#[test]
fn test_watch_key_from_disk() -> Void {
  let db = MockDatabase::new();
  for i in 0..1000 {
    db.string_set(format!("key{}", i), format!("value{}", i));
  }

  let mut client = db.create_client();
  assert_eq!(client.send_command("WATCH key1"), "+OK\r\n");

  client.send_command("MULTI");
  client.send_command("GET key900");
  client.send_command("SET key901 value901_updated");

  // 外部并发修改冷键 key1
  let mut client2 = db.create_client();
  client2.send_command("SET key1 value1_updated");

  assert_eq!(client.send_command("EXEC"), "*-1\r\n");

  // 重新重试事务正常提交
  client.send_command("MULTI");
  client.send_command("GET key900");
  client.send_command("SET key901 value901_updated");
  assert_eq!(
    client.send_command("EXEC"),
    "*2\r\n$8\r\nvalue900\r\n+OK\r\n"
  );

  info!("WatchKeyFromDisk: 冷数据监视冲突与重试测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: WatchFailsWhenListEmptiedByLPop
/// 验证列表键被 LPOP 弹出清空时版本递增并触发监视失效
#[test]
fn test_watch_fails_when_list_emptied_by_lpop() -> Void {
  let db = MockDatabase::new();
  let mut client = db.create_client();
  let key = "watchlist";

  assert_eq!(
    client.send_command(&format!("LPUSH {} value1", key)),
    ":1\r\n"
  );
  assert_eq!(client.send_command(&format!("WATCH {}", key)), "+OK\r\n");

  // 在事务前同一连接执行 LPOP 清空并删除列表
  assert_eq!(
    client.send_command(&format!("LPOP {}", key)),
    "$6\r\nvalue1\r\n"
  );

  assert_eq!(client.send_command("MULTI"), "+OK\r\n");
  client.send_command(&format!("LPUSH {} value2", key));

  // 提交应因列表清空变异而冲突失败
  assert_eq!(client.send_command("EXEC"), "*-1\r\n");

  info!("WatchFailsWhenListEmptiedByLPop: 列表清空触发监视失效测试通过");
  OK
}

/// 对标 Garnet UNWATCH 规范
/// 验证显式调用 UNWATCH 撤销监视，后续外部修改不再影响事务提交
#[test]
fn test_unwatch() -> Void {
  let db = MockDatabase::new();
  let mut client = db.create_client();

  client.send_command("SET key1 val1");
  client.send_command("WATCH key1");

  // 主动撤销监视
  assert_eq!(client.send_command("UNWATCH"), "+OK\r\n");

  // 外部并发修改 key1
  let mut client2 = db.create_client();
  client2.send_command("SET key1 val1_updated");

  // 事务正常提交
  client.send_command("MULTI");
  client.send_command("GET key1");
  assert_eq!(client.send_command("EXEC"), "*1\r\n$12\r\nval1_updated\r\n");

  info!("UNWATCH: 撤销监视功能测试通过");
  OK
}

/// 对标 Redis/Garnet 规范：事务开启期间 UNWATCH 为空操作
#[test]
fn test_unwatch_noop_inside_multi() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  let k = b("uw_key");

  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  // 事务开启期间 UNWATCH 不生效
  mgr.unwatch();
  assert_eq!(mgr.watched_keys().watched_count(), 1);

  // 回到空闲状态后恢复生效
  mgr.reset();
  mgr.unwatch();
  assert_eq!(mgr.watched_keys().watched_count(), 0);

  info!("UnwatchNoopInsideMulti: 事务期间 UNWATCH 空操作测试通过");
  OK
}

/// 对标 Redis/Garnet 规范：重复 WATCH 刷新版本基线
#[test]
fn test_rewatch_refreshes_version_baseline() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  let k = b("rw_key");

  mgr.watch(k.clone(), &vmap)?;
  // 外部修改后重新 WATCH：应刷新版本基线，避免误报冲突
  vmap.bump_version_key(&k);
  mgr.watch(k.clone(), &vmap)?;
  assert_eq!(mgr.watched_keys().watched_count(), 1);

  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(RespCommand::GET, vec![k]))?;
  let result = mgr.exec(&vmap, |_| "ok")?;
  assert_eq!(result, ExecResult::Success(vec!["ok"]));

  info!("RewatchRefreshesBaseline: 重复 WATCH 刷新版本基线测试通过");
  OK
}

/// 对标 Redis/Garnet 规范：EXEC 成功与冲突后均自动清空监视状态
#[test]
fn test_exec_clears_watch_state() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  let k = b("aw_key");

  // 成功路径
  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(
    RespCommand::SET,
    vec![k.clone(), b("v")],
  ))?;
  assert!(matches!(mgr.exec(&vmap, |_| "ok")?, ExecResult::Success(_)));
  assert_eq!(mgr.watched_keys().watched_count(), 0);
  assert_eq!(vmap.read_version_key(&k), 1);

  // 冲突路径
  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(RespCommand::GET, vec![k.clone()]))?;
  vmap.bump_version_key(&k);
  assert_eq!(mgr.exec(&vmap, |_| "ok")?, ExecResult::Conflict);
  assert_eq!(mgr.watched_keys().watched_count(), 0);

  info!("ExecClearsWatchState: 提交与冲突自动清空监视测试通过");
  OK
}

/// 对标 Redis/Garnet 规范：FLUSHALL / FLUSHDB 全局变异提交时全表广播失效
#[test]
fn test_flush_invalidates_all_watches() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  let k = b("flush_target");

  // 本会话监视后提交含 FLUSHALL 的事务
  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(RespCommand::FLUSHALL, vec![]))?;
  assert!(matches!(mgr.exec(&vmap, |_| "ok")?, ExecResult::Success(_)));
  assert_eq!(vmap.read_version_key(&k), 1);
  assert_eq!(mgr.state(), TxnState::None);

  // 另一会话监视期间，提交含 FLUSHDB 的事务，观察者必须失效
  let mut watcher = TransactionManager::new();
  watcher.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(RespCommand::FLUSHDB, vec![]))?;
  assert!(matches!(mgr.exec(&vmap, |_| "ok")?, ExecResult::Success(_)));
  assert!(!watcher.watched_keys().validate_versions(&vmap));

  // 普通事务不得触发全表广播
  let probe = vmap.read_version_key(b"untouched_key");
  let mut normal = TransactionManager::new();
  normal.multi()?;
  normal.queue_command(QueuedCommand::new(
    RespCommand::SET,
    vec![b("other"), b("v")],
  ))?;
  assert!(matches!(
    normal.exec(&vmap, |_| "ok")?,
    ExecResult::Success(_)
  ));
  assert_eq!(vmap.read_version_key(b"untouched_key"), probe);

  info!("FlushInvalidatesAllWatches: FLUSH 全局变异广播测试通过");
  OK
}

/// 会话监视键容器独立生命周期与失效校验
#[test]
fn test_watched_keys_container() -> Void {
  let vmap = WatchVersionMap::default();
  let mut container = WatchedKeysContainer::new();

  let k1 = b("user:name");
  let k2 = b("user:age");

  container.watch(k1.clone(), &vmap);
  container.watch(k2.clone(), &vmap);

  assert_eq!(container.watched_count(), 2);
  assert!(container.validate_versions(&vmap));

  // 修改 k1 版本
  vmap.bump_version_key(&k1);
  assert!(!container.validate_versions(&vmap));

  // 移除 k1 监视后，仅监视 k2 应恢复有效
  assert!(container.remove_watch(&k1));
  assert!(container.validate_versions(&vmap));

  // 清空监视
  container.reset();
  assert_eq!(container.watched_count(), 0);
  assert!(container.is_empty());

  info!("WatchedKeysContainer: 会话监视容器生命周期测试通过");
  OK
}
