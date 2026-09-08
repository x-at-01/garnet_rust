use aok::{OK, Void, bail};
use bytes::Bytes;
use log::info;
use wedb_resp::RespCommand;
use wedb_txn::{
  ExecResult, LockType, QueuedCommand, TransactionGuard, TransactionManager, TxnKeyEntries,
  TxnKeyEntry, TxnState, WatchVersionMap, WatchedKeyEntry, WatchedKeysContainer,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 冒烟测试 1：事务状态机谓词与枚举判别
#[test]
fn test_smoke_txn_state_predicates() -> Void {
  let mut state = TxnState::None;
  assert!(state.is_none());
  assert!(!state.is_in_txn());
  assert!(!state.is_skipping_operations());

  state = TxnState::Started;
  assert!(state.is_started());
  assert!(state.is_in_txn());
  assert!(state.is_skipping_operations());

  state = TxnState::Running;
  assert!(state.is_running());
  assert!(state.is_in_txn());
  assert!(!state.is_skipping_operations());

  state = TxnState::Aborted;
  assert!(state.is_aborted());
  assert!(state.is_in_txn());
  assert!(state.is_skipping_operations());

  // 对标 Garnet IsSkippingOperations：仅 Started / Aborted 跳过即时执行
  assert_eq!(TxnState::default(), TxnState::None);
  assert_eq!(state as u8, 3);

  info!("冒烟测试：事务状态机谓词测试通过");
  OK
}

/// 冒烟测试 2：事务完整生命周期执行与版本自增
#[test]
fn test_smoke_multi_exec() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();

  mgr.multi()?;
  assert!(mgr.is_in_txn());
  assert_eq!(mgr.state(), TxnState::Started);

  let key = Bytes::from_static(b"user:smoke");
  mgr.queue_command(QueuedCommand::new(RespCommand::GET, vec![key.clone()]))?;
  mgr.queue_command(QueuedCommand::new(
    RespCommand::SET,
    vec![key.clone(), Bytes::from_static(b"value")],
  ))?;

  assert_eq!(mgr.queued_count(), 2);
  assert!(mgr.perform_writes());

  let result = mgr.exec(&vmap, |cmd| format!("executed {:?}", cmd.cmd))?;
  let ExecResult::Success(responses) = result else {
    bail!("期望事务成功执行");
  };
  assert_eq!(responses.len(), 2);
  assert_eq!(responses[0], "executed GET");
  assert_eq!(responses[1], "executed SET");

  // 提交后写操作自增版本
  assert_eq!(vmap.read_version_key(&key), 1);
  assert_eq!(mgr.state(), TxnState::None);
  assert_eq!(mgr.queued_count(), 0);

  info!("冒烟测试：完整事务生命周期与原子提交测试通过");
  OK
}

/// 冒烟测试 3：乐观并发冲突失效与自动清理
#[test]
fn test_smoke_watch_conflict() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  let target_key = Bytes::from_static(b"stock:smoke");

  mgr.watch(target_key.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(
    RespCommand::DECR,
    vec![target_key.clone()],
  ))?;

  // 模拟并发变异
  vmap.bump_version_key(&target_key);

  let result = mgr.exec(&vmap, |_| "ok")?;
  assert_eq!(result, ExecResult::Conflict);
  assert_eq!(mgr.state(), TxnState::None);
  assert_eq!(mgr.queued_count(), 0);
  assert_eq!(mgr.watched_keys().watched_count(), 0);

  info!("冒烟测试：乐观并发冲突失效测试通过");
  OK
}

/// 冒烟测试 4：事务守卫 RAII 自动提交与提升
#[test]
fn test_smoke_txn_guard_raii() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  let k = Bytes::from_static(b"guard:smoke");

  // 单键提升事务
  {
    let mut guard = mgr.promote_to_transaction(&vmap, k.clone(), LockType::Exclusive);
    assert_eq!(guard.state(), Some(TxnState::Running));
    guard.commit();
  }
  assert_eq!(mgr.state(), TxnState::None);
  assert_eq!(vmap.read_version_key(&k), 1);

  // RAII 守卫托管
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(
    RespCommand::SET,
    vec![k.clone(), Bytes::from_static(b"v")],
  ))?;
  let _prep = mgr.exec_prepare(&vmap)?;
  assert_eq!(mgr.state(), TxnState::Running);

  {
    let _guard = TransactionGuard::new(&mut mgr, &vmap);
  }
  assert_eq!(mgr.state(), TxnState::None);
  assert_eq!(vmap.read_version_key(&k), 2);

  info!("冒烟测试：事务守卫 RAII 生命周期测试通过");
  OK
}

/// 冒烟测试 5：bitcode 核心结构编解码往返
#[test]
fn test_smoke_bitcode_roundtrip() -> Void {
  let state = TxnState::Started;
  let decoded_state: TxnState = bitcode::decode(&bitcode::encode(&state))?;
  assert_eq!(decoded_state, state);

  let lock = LockType::Exclusive;
  let decoded_lock: LockType = bitcode::decode(&bitcode::encode(&lock))?;
  assert_eq!(decoded_lock, lock);

  let entry = TxnKeyEntry::new(12345, LockType::Exclusive, Some(Bytes::from_static(b"key")));
  let decoded_entry = TxnKeyEntry::decode_bitcode(&entry.encode_bitcode())?;
  assert_eq!(decoded_entry, entry);

  let mut entries = TxnKeyEntries::new();
  entries.add_key(100, LockType::Shared, Some(Bytes::from_static(b"k1")));
  let decoded_entries = TxnKeyEntries::decode_bitcode(&entries.encode_bitcode())?;
  assert_eq!(decoded_entries, entries);

  let cmd = QueuedCommand::new(
    RespCommand::SET,
    vec![Bytes::from_static(b"k"), Bytes::from_static(b"v")],
  );
  let decoded_cmd = QueuedCommand::decode_bitcode(&cmd.encode_bitcode())?;
  assert_eq!(decoded_cmd, cmd);

  let watched = WatchedKeyEntry {
    key: Bytes::from_static(b"wk"),
    hash: 888,
    version: 1,
    is_watched: true,
  };
  let decoded_watched = WatchedKeyEntry::decode_bitcode(&watched.encode_bitcode())?;
  assert_eq!(decoded_watched, watched);

  let vmap = WatchVersionMap::default();
  let mut container = WatchedKeysContainer::new();
  container.watch(Bytes::from_static(b"wk"), &vmap);
  let decoded_container = WatchedKeysContainer::decode_bitcode(&container.encode_bitcode())?;
  assert_eq!(decoded_container, container);

  info!("冒烟测试：bitcode 编解码序列化往返测试通过");
  OK
}
