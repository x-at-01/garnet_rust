use aok::{OK, Void, bail};
use bytes::Bytes;
use log::info;
use wedb_resp::RespCommand;
use wedb_txn::{
  Error, ExecPreparation, ExecResult, LockType, QueuedCommand, TransactionManager, TxnState,
  WatchVersionMap,
};

use super::support::{MockDatabase, b};

/// 对标 Garnet TransactionTests.cs: TxnSetTest
/// 验证事务内异步排队 SET 命令，EXEC 提交后键值生效
#[test]
fn test_txn_set() -> Void {
  let db = MockDatabase::new();
  let mut tran = db.create_transaction()?;

  let val1 = "abcdefg1";
  let val2 = "abcdefg2";

  tran.string_set_async("mykey1", val1)?;
  tran.string_set_async("mykey2", val2)?;
  let committed = tran.execute();

  assert!(committed);
  assert_eq!(db.string_get("mykey1").as_deref(), Some(val1.as_bytes()));
  assert_eq!(db.string_get("mykey2").as_deref(), Some(val2.as_bytes()));

  info!("TxnSetTest: 事务 SET 执行测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: TxnExecuteTest
/// 验证通过通用 Execute 接口排队命令并原子提交
#[test]
fn test_txn_execute() -> Void {
  let db = MockDatabase::new();
  let mut tran = db.create_transaction()?;

  let val1 = "abcdefg1";
  let val2 = "abcdefg2";

  tran.execute_async("SET", &["mykey1", val1])?;
  tran.execute_async("SET", &["mykey2", val2])?;
  let committed = tran.execute();

  assert!(committed);
  assert_eq!(db.string_get("mykey1").as_deref(), Some(val1.as_bytes()));
  assert_eq!(db.string_get("mykey2").as_deref(), Some(val2.as_bytes()));

  info!("TxnExecuteTest: 通用命令排队执行测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: TxnGetTest
/// 验证事务内排队读取已存在键，提交后任务句柄获取对应数据
#[test]
fn test_txn_get() -> Void {
  let db = MockDatabase::new();
  let val1 = "abcdefg1";
  let val2 = "abcdefg2";

  db.string_set("mykey1", val1);
  db.string_set("mykey2", val2);

  let mut tran = db.create_transaction()?;
  let t1 = tran.string_get_async("mykey1")?;
  let t2 = tran.string_get_async("mykey2")?;
  let committed = tran.execute();

  assert!(committed);
  assert_eq!(t1.result().as_deref(), Some(val1.as_bytes()));
  assert_eq!(t2.result().as_deref(), Some(val2.as_bytes()));

  info!("TxnGetTest: 事务 GET 读取测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: TxnGetSetTest
/// 验证事务内混合执行 GET 与 SET 操作
#[test]
fn test_txn_get_set() -> Void {
  let db = MockDatabase::new();
  let val1 = "abcdefg1";
  let val2 = "abcdefg2";

  db.string_set("mykey1", val1);

  let mut tran = db.create_transaction()?;
  let t1 = tran.string_get_async("mykey1")?;
  tran.string_set_async("mykey2", val2)?;
  let committed = tran.execute();

  assert!(committed);
  assert_eq!(t1.result().as_deref(), Some(val1.as_bytes()));
  assert_eq!(db.string_get("mykey2").as_deref(), Some(val2.as_bytes()));

  info!("TxnGetSetTest: 事务 GET 与 SET 混合执行测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: TxnHExpireTest
/// 验证哈希字段设置与过期控制指令在事务内的组合执行
#[test]
fn test_txn_hexpire() -> Void {
  let db = MockDatabase::new();
  let key = "test";
  let he = [("a", "1"), ("b", "2"), ("c", "3")];

  let mut tran = db.create_transaction()?;
  tran.hash_set_async(key, &he)?;
  tran.execute_async("HEXPIRE", &[key, "1000", "FIELDS", "1", "b"])?;
  let committed = tran.execute();

  assert!(committed);
  let hashes = db.hashes.pin();
  let Some(sub) = hashes.get(key.as_bytes()) else {
    bail!("未找到哈希键");
  };
  let sub_pin = sub.pin();
  assert_eq!(
    sub_pin.get(b"a".as_slice()).map(Bytes::as_ref),
    Some(b"1".as_slice())
  );
  assert_eq!(
    sub_pin.get(b"b".as_slice()).map(Bytes::as_ref),
    Some(b"2".as_slice())
  );
  assert_eq!(
    sub_pin.get(b"c".as_slice()).map(Bytes::as_ref),
    Some(b"3".as_slice())
  );

  info!("TxnHExpireTest: 哈希字段及过期命令事务测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: LargeTxn
/// 验证大规模批量读写在事务中的原子执行稳定性
#[test]
fn test_large_txn() -> Void {
  let sizes = [512, 1024];
  for &size in &sizes {
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
        let t = tran.string_get_async(format!("{}{}", key_prefix, i / 2))?;
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
      let counter = orig_idx + size;
      let res = db.string_get(format!("{}{}", key_prefix, counter));
      assert_eq!(
        res.as_deref(),
        Some(format!("{}{}", val_prefix, counter).as_bytes())
      );
    }
  }

  info!("LargeTxn: 大规模批量读写事务测试通过");
  OK
}

/// 对标 Garnet 事务 DISCARD 规范
/// 验证 DISCARD 重置事务状态机、清空排队命令及受监视键
#[test]
fn test_txn_discard() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();

  // 未开启事务时 DISCARD 报错
  let Err(err) = mgr.discard() else {
    bail!("未开启事务时执行 discard 应报错");
  };
  assert_eq!(err, Error::DiscardWithoutMulti);

  // 监视键并开启事务
  let k = Bytes::from_static(b"counter");
  mgr.watch(k.clone(), &vmap)?;
  mgr.multi()?;
  mgr.queue_command(QueuedCommand::new(RespCommand::INCR, vec![k]))?;

  assert_eq!(mgr.queued_count(), 1);
  assert_eq!(mgr.watched_keys().watched_count(), 1);

  // 执行 DISCARD 撤销事务
  mgr.discard()?;

  // 状态机、队列与监视全部重置
  assert_eq!(mgr.state(), TxnState::None);
  assert_eq!(mgr.queued_count(), 0);
  assert_eq!(mgr.watched_keys().watched_count(), 0);

  info!("DISCARD: 取消事务与状态重置测试通过");
  OK
}

/// 对标 Garnet 事务嵌套 MULTI 拦截规范
/// 验证多次调用 MULTI 报错并置状态为 Aborted，EXECABORT 统一拒绝
#[test]
fn test_nested_multi_interception() -> Void {
  let db = MockDatabase::new();
  let mut client = db.create_client();

  assert_eq!(client.send_command("MULTI"), "+OK\r\n");
  assert_eq!(
    client.send_command("MULTI"),
    "-ERR MULTI calls can not be nested\r\n"
  );
  assert_eq!(
    client.send_command("EXEC"),
    "-EXECABORT Transaction discarded because of previous errors.\r\n"
  );

  info!("NestedMulti: 嵌套 MULTI 拦截测试通过");
  OK
}

/// 对标 Garnet 事务前置约束规范
/// 验证未开启事务时调用 EXEC / DISCARD 报错拦截
#[test]
fn test_exec_discard_without_multi() -> Void {
  let db = MockDatabase::new();
  let mut client = db.create_client();

  assert_eq!(client.send_command("EXEC"), "-ERR EXEC without MULTI\r\n");
  assert_eq!(
    client.send_command("DISCARD"),
    "-ERR DISCARD without MULTI\r\n"
  );

  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();
  assert_eq!(mgr.exec_prepare(&vmap), Err(Error::ExecWithoutMulti));
  assert_eq!(mgr.discard(), Err(Error::DiscardWithoutMulti));
  assert_eq!(
    mgr.queue_command(QueuedCommand::new(RespCommand::GET, vec![b("k")])),
    Err(Error::QueueWithoutMulti)
  );

  info!("ExecDiscardWithoutMulti: 前置约束校验测试通过");
  OK
}

/// 对标 Garnet 事务语法错误中止规范 (EXECABORT)
/// 验证命令排队失败（或禁入命令）使事务中止，后续命令仍可入队放行，最终由 EXEC 返回 EXECABORT
#[test]
fn test_txn_aborted_execabort() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();

  mgr.multi()?;

  // 入队禁入事务命令导致中止
  let Err(err) = mgr.queue_command(QueuedCommand::new(RespCommand::SWAPDB, vec![])) else {
    bail!("入队禁入事务命令应报错");
  };
  assert!(matches!(err, Error::CommandNotAllowedInTxn(_)));
  assert_eq!(mgr.state(), TxnState::Aborted);

  // 中止状态下普通命令仍可入队放行（NetworkSKIP 规范）
  mgr.queue_command(QueuedCommand::new(RespCommand::GET, vec![b("k")]))?;
  assert_eq!(mgr.queued_count(), 1);

  // EXEC 时统一返回 Aborted，并清理事务
  assert_eq!(mgr.exec(&vmap, |_| "ok")?, ExecResult::Aborted);
  assert_eq!(mgr.state(), TxnState::None);
  assert_eq!(mgr.queued_count(), 0);

  // 中止状态下尝试监视键应被拦截
  mgr.multi()?;
  mgr.abort();
  let Err(err) = mgr.watch(b("k"), &vmap) else {
    bail!("中止状态下 WATCH 应被拦截");
  };
  assert_eq!(err, Error::WatchInsideMulti);

  // 阶段式准备返回 Aborted
  let prep = mgr.exec_prepare(&vmap)?;
  assert_eq!(prep, ExecPreparation::Aborted);

  info!("TxnAbortedExecabort: 语法错误中止与 EXECABORT 处理测试通过");
  OK
}

/// 对标 Garnet 空事务执行规范
/// 验证空事务成功提交返回空结果集
#[test]
fn test_empty_transaction() -> Void {
  let vmap = WatchVersionMap::default();
  let mut mgr = TransactionManager::new();

  mgr.multi()?;
  let result = mgr.exec(&vmap, |_| "ok")?;
  assert_eq!(result, ExecResult::Success(Vec::<&str>::new()));
  assert_eq!(mgr.state(), TxnState::None);

  info!("EmptyTransaction: 空事务提交测试通过");
  OK
}

/// 对标 Garnet TransactionTests.cs: TxnCommandCoverage
/// 验证排队命令禁入判定与多键提取规则（EVAL, BITOP, MPOP, SDIFF, PF 等）
#[test]
fn test_command_coverage_and_key_extraction() -> Void {
  // 1. 事务控制及禁入命令验证
  for cmd in [
    RespCommand::MULTI,
    RespCommand::EXEC,
    RespCommand::DISCARD,
    RespCommand::WATCH,
    RespCommand::UNWATCH,
    RespCommand::SWAPDB,
    RespCommand::RUNTXP,
    RespCommand::COMMITAOF,
    RespCommand::ASYNC,
    RespCommand::CLUSTER_INFO,
  ] {
    assert!(
      !QueuedCommand::is_allowed_in_txn(cmd),
      "{cmd:?} 不应允许在事务内入队"
    );
  }

  // 2. 键提取规则验证
  // BITOP AND dest src1 src2：目标键排他 + 源键共享
  let cmd = QueuedCommand::new(
    RespCommand::BitopAnd,
    vec![b("AND"), b("dest"), b("src1"), b("src2")],
  );
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("dest"), LockType::Exclusive),
      (b("src1"), LockType::Shared),
      (b("src2"), LockType::Shared),
    ]
  );

  // EVAL script 2 k1 k2 arg：按 numkeys 提取，排他写锁
  let cmd = QueuedCommand::new(
    RespCommand::Eval,
    vec![b("return 1"), b("2"), b("k1"), b("k2"), b("arg")],
  );
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("k1"), LockType::Exclusive),
      (b("k2"), LockType::Exclusive),
    ]
  );

  // BLPOP k1 k2 0：除末尾超时参数外的全部键
  let cmd = QueuedCommand::new(RespCommand::Blpop, vec![b("k1"), b("k2"), b("0")]);
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("k1"), LockType::Exclusive),
      (b("k2"), LockType::Exclusive),
    ]
  );

  // LMPOP 2 k1 k2 LEFT：按 numkeys 提取全部键
  let cmd = QueuedCommand::new(
    RespCommand::Lmpop,
    vec![b("2"), b("k1"), b("k2"), b("LEFT")],
  );
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("k1"), LockType::Exclusive),
      (b("k2"), LockType::Exclusive),
    ]
  );

  // BLMPOP 0 1 k1 LEFT：timeout 之后再按 numkeys 提取
  let cmd = QueuedCommand::new(
    RespCommand::Blmpop,
    vec![b("0"), b("1"), b("k1"), b("LEFT")],
  );
  assert_eq!(cmd.extract_keys(), vec![(b("k1"), LockType::Exclusive)]);

  // ZMPOP 2 k1 k2 MIN：按 numkeys 提取全部键
  let cmd = QueuedCommand::new(RespCommand::Zmpop, vec![b("2"), b("k1"), b("k2"), b("MIN")]);
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("k1"), LockType::Exclusive),
      (b("k2"), LockType::Exclusive),
    ]
  );

  // SDIFF k1 k2 k3：多键只读差集，全部共享读锁
  let cmd = QueuedCommand::new(RespCommand::Sdiff, vec![b("k1"), b("k2"), b("k3")]);
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("k1"), LockType::Shared),
      (b("k2"), LockType::Shared),
      (b("k3"), LockType::Shared),
    ]
  );

  // PFCOUNT k1 k2：多键基数估计，内部编码可能变异（对标 Garnet 键规格 RW, Access），
  // 全部排他写锁
  let cmd = QueuedCommand::new(RespCommand::Pfcount, vec![b("k1"), b("k2")]);
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("k1"), LockType::Exclusive),
      (b("k2"), LockType::Exclusive)
    ]
  );

  // PFMERGE dest src1 src2：目标排他 + 源共享
  let cmd = QueuedCommand::new(RespCommand::Pfmerge, vec![b("dest"), b("src1"), b("src2")]);
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("dest"), LockType::Exclusive),
      (b("src1"), LockType::Shared),
      (b("src2"), LockType::Shared),
    ]
  );

  // ZINTER 2 k1 k2 WEIGHTS 1 1：numkeys 多键读，全部共享
  let cmd = QueuedCommand::new(
    RespCommand::Zinter,
    vec![b("2"), b("k1"), b("k2"), b("WEIGHTS"), b("1"), b("1")],
  );
  assert_eq!(
    cmd.extract_keys(),
    vec![(b("k1"), LockType::Shared), (b("k2"), LockType::Shared)]
  );

  // 非法 numkeys 回退为空键集合
  let cmd = QueuedCommand::new(RespCommand::Zinter, vec![b("x"), b("k1")]);
  assert!(cmd.extract_keys().is_empty());

  // LCS k1 k2：双键共享读
  let cmd = QueuedCommand::new(RespCommand::Lcs, vec![b("k1"), b("k2")]);
  assert_eq!(
    cmd.extract_keys(),
    vec![(b("k1"), LockType::Shared), (b("k2"), LockType::Shared)]
  );

  // ZRANGESTORE dst src 0 -1：目标排他 + 源共享
  let cmd = QueuedCommand::new(
    RespCommand::Zrangestore,
    vec![b("dst"), b("src"), b("0"), b("-1")],
  );
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("dst"), LockType::Exclusive),
      (b("src"), LockType::Shared),
    ]
  );

  // MIGRATE host port key db timeout：迁移键排他写锁
  let cmd = QueuedCommand::new(
    RespCommand::MIGRATE,
    vec![b("127.0.0.1"), b("6379"), b("mig_key"), b("0"), b("1000")],
  );
  assert_eq!(
    cmd.extract_keys(),
    vec![(b("mig_key"), LockType::Exclusive)]
  );

  // MIGRATE host port key db timeout KEYS k1 k2：KEYS 关键字变体多键排他写锁
  // （对标 Garnet BeginSearchKeyword "KEYS" 键规格 RW）
  let cmd = QueuedCommand::new(
    RespCommand::MIGRATE,
    vec![
      b("127.0.0.1"),
      b("6379"),
      b("mig_key"),
      b("0"),
      b("1000"),
      b("KEYS"),
      b("k1"),
      b("k2"),
    ],
  );
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("mig_key"), LockType::Exclusive),
      (b("k1"), LockType::Exclusive),
      (b("k2"), LockType::Exclusive),
    ]
  );

  // MIGRATE 残缺参数（缺 key/db/timeout）不得 panic，且不登记任何键
  let cmd = QueuedCommand::new(RespCommand::MIGRATE, vec![b("127.0.0.1"), b("6379")]);
  assert!(cmd.extract_keys().is_empty());
  let cmd = QueuedCommand::new(RespCommand::MIGRATE, vec![b("127.0.0.1")]);
  assert!(cmd.extract_keys().is_empty());

  // GEORADIUS key lon lat radius unit STORE dst：主键共享读 + 存储键排他写
  // （对标 Garnet 键规格：主键 RO，STORE / STOREDIST 关键字目标键 OW）
  let cmd = QueuedCommand::new(
    RespCommand::GEORADIUS,
    vec![
      b("geo_key"),
      b("13.36"),
      b("38.86"),
      b("100"),
      b("km"),
      b("storedist"),
      b("geo_dst"),
    ],
  );
  assert_eq!(
    cmd.extract_keys(),
    vec![
      (b("geo_key"), LockType::Shared),
      (b("geo_dst"), LockType::Exclusive)
    ]
  );

  // 无 STORE 选项时仅主键共享读
  let cmd = QueuedCommand::new(
    RespCommand::GEORADIUSBYMEMBER,
    vec![b("geo_key"), b("member"), b("100"), b("km"), b("ASC")],
  );
  assert_eq!(cmd.extract_keys(), vec![(b("geo_key"), LockType::Shared)]);

  // SSUBSCRIBE ch1 ch2：分片订阅频道全部共享锁（对标 Garnet 键规格 Index=1 LastKey=-1 RO）
  let cmd = QueuedCommand::new(RespCommand::SSUBSCRIBE, vec![b("ch1"), b("ch2")]);
  assert_eq!(
    cmd.extract_keys(),
    vec![(b("ch1"), LockType::Shared), (b("ch2"), LockType::Shared)]
  );

  // OBJECT ENCODING key：存储键位于第 2 参数共享读锁（对标 Garnet 键规格 Index=2 RO），
  // 首参子命令 token 不得误登记为幻影键
  let cmd = QueuedCommand::new(
    RespCommand::OBJECT_ENCODING,
    vec![b("ENCODING"), b("obj_key")],
  );
  assert_eq!(cmd.extract_keys(), vec![(b("obj_key"), LockType::Shared)]);

  let cmd = QueuedCommand::new(RespCommand::OBJECT_FREQ, vec![b("FREQ"), b("obj_key")]);
  assert_eq!(cmd.extract_keys(), vec![(b("obj_key"), LockType::Shared)]);

  // 残缺 OBJECT（缺存储键）不登记任何键
  let cmd = QueuedCommand::new(RespCommand::OBJECT_ENCODING, vec![b("ENCODING")]);
  assert!(cmd.extract_keys().is_empty());

  // CUSTOMOBJECTSCAN 正则参数非存储键（对标 Garnet 自定义命令无键规格）：不登记任何键
  let cmd = QueuedCommand::new(RespCommand::COSCAN, vec![b(".*"), b("10")]);
  assert!(cmd.extract_keys().is_empty());

  // 非数据命令（SELECT / PUBLISH / ACL 等，对标 Garnet 无键规格）：不登记任何键，
  // 避免把库索引、频道名等非键参数误登记为存储键
  for (cmd, args) in [
    (RespCommand::SELECT, vec![b("3")]),
    (RespCommand::PUBLISH, vec![b("channel"), b("msg")]),
    (RespCommand::SUBSCRIBE, vec![b("channel"), b("channel2")]),
    (RespCommand::CLIENT_SETNAME, vec![b("conn-name")]),
    (RespCommand::SCRIPT_LOAD, vec![b("return 1")]),
  ] {
    assert!(
      QueuedCommand::new(cmd, args).extract_keys().is_empty(),
      "{cmd:?} 不应登记键锁"
    );
  }

  // FLUSHALL / FLUSHDB 全局变异：不登记键（提交时经版本表全表广播失效）
  assert!(
    QueuedCommand::new(RespCommand::FLUSHALL, vec![])
      .extract_keys()
      .is_empty()
  );

  info!("TxnCommandCoverage: 命令禁入与键提取规则测试通过");
  OK
}
