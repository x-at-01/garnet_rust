use std::{iter::repeat_n, sync::Arc, time::Duration};

use aok::{OK, Void};
use coarsetime::Clock;
use compio::{runtime::Runtime, time::sleep};
use log::info;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_hash::{ExpireOpt as HashExpireOpt, ExpireResult as HashExpireResult};
use wedb_redis::prelude::*;
use wkv::{StorageEncoding, StoreConfig, WedbStore};

/// 十进制补零到 width 位
fn pad(v: impl itoa::Integer, width: usize) -> String {
  let mut buf = itoa::Buffer::new();
  let digits = buf.format(v);
  let mut s = String::with_capacity(width.max(digits.len()));
  s.extend(repeat_n('0', width.saturating_sub(digits.len())));
  s.push_str(digits);
  s
}

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 辅助函数：快速创建测试用 WedbStore
fn create_test_store() -> aok::Result<(tempfile::TempDir, Arc<WedbStore<SegmentedDevice>>)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("flattened.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(2048, 64 * 1024, 16, 0.5)?,
    device,
  )?);
  Ok((dir, store))
}

/// 测试 1: Hash 打平存储基础指令 (hset, hget, hmset, hmget, hdel, hlen, hexists, hstrlen)
#[test]
fn test_flattened_hash_basic_commands() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:test:basic";

    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.hlen(key).await?, 0);
    assert!(!session.hexists(key, b"f1").await?);
    assert_eq!(session.hstrlen(key, b"f1").await?, 0);

    // HSET 单个字段新增与更新
    assert!(session.hset(key, b"f1".to_vec(), b"hello".to_vec()).await?);
    assert_eq!(session.type_of(key).await?, "hash");
    assert_eq!(session.hlen(key).await?, 1);
    assert!(session.hexists(key, b"f1").await?);
    assert_eq!(session.hstrlen(key, b"f1").await?, 5);
    assert_eq!(session.hget(key, b"f1").await?, Some(b"hello".to_vec()));

    // 更新同一个字段：返回 false（不是新增）
    assert!(
      !session
        .hset(key, b"f1".to_vec(), b"world!!".to_vec())
        .await?
    );
    assert_eq!(session.hlen(key).await?, 1);
    assert_eq!(session.hstrlen(key, b"f1").await?, 7);
    assert_eq!(session.hget(key, b"f1").await?, Some(b"world!!".to_vec()));

    // HMSET 批量新增
    let count = session
      .hmset(
        key,
        vec![
          (b"f2".to_vec(), b"foo".to_vec()),
          (b"f3".to_vec(), b"barbaz".to_vec()),
          (b"f1".to_vec(), b"updated".to_vec()),
        ],
      )
      .await?;
    assert_eq!(count, 3); // 成功设置 3 个字段
    assert_eq!(session.hlen(key).await?, 3);

    // HMGET 批量读取
    let fields: Vec<&[u8]> = vec![b"f1", b"f2", b"non_exist", b"f3"];
    let vals = session.hmget(key, &fields).await?;
    assert_eq!(
      vals,
      vec![
        Some(b"updated".to_vec()),
        Some(b"foo".to_vec()),
        None,
        Some(b"barbaz".to_vec()),
      ]
    );

    // HDEL 批量删除
    let del_fields: Vec<&[u8]> = vec![b"f2", b"non_exist", b"f3"];
    let del_count = session.hdel(key, &del_fields).await?;
    assert_eq!(del_count, 2);
    assert_eq!(session.hlen(key).await?, 1);
    assert!(!session.hexists(key, b"f2").await?);
    assert!(!session.hexists(key, b"f3").await?);
    assert!(session.hexists(key, b"f1").await?);

    // 删除剩余的 f1，集合应自动删除 Meta 墓碑自愈
    let del_last = session.hdel(key, &[b"f1"]).await?;
    assert_eq!(del_last, 1);
    assert_eq!(session.hlen(key).await?, 0);
    assert_eq!(session.type_of(key).await?, "none");

    info!("测试 1: Hash 打平存储基础指令全部通过");
    OK
  })
}

/// 测试 2: Hash 打平数值自增 (hincrby, hincrbyfloat)
#[test]
fn test_flattened_hash_increments() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:test:incr";

    // HINCRBY 键不存在时自动初始化并自增
    let v1 = session.hincrby(key, b"counter", 10).await?;
    assert_eq!(v1, 10);
    assert_eq!(session.hget(key, b"counter").await?, Some(b"10".to_vec()));

    let v2 = session.hincrby(key, b"counter", -4).await?;
    assert_eq!(v2, 6);
    assert_eq!(session.hget(key, b"counter").await?, Some(b"6".to_vec()));

    // HINCRBYFLOAT 浮点自增
    let f1 = session.hincrbyfloat(key, b"float_cnt", 10.5).await?;
    assert!((f1 - 10.5).abs() < 1e-6);

    let f2 = session.hincrbyfloat(key, b"float_cnt", -2.25).await?;
    assert!((f2 - 8.25).abs() < 1e-6);

    // 对非数值字符串执行数值自增应返回错误
    session.hset(key, b"str".to_vec(), b"abc".to_vec()).await?;
    assert!(session.hincrby(key, b"str", 1).await.is_err());
    assert!(session.hincrbyfloat(key, b"str", 1.5).await.is_err());

    info!("测试 2: Hash 打平数值自增全部通过");
    OK
  })
}

/// 测试 3: Hash 打平字段过期控制 (hexpire, httl, hpersist)
#[test]
fn test_flattened_hash_expire_and_ttl() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:test:expire";

    session.hset(key, b"f1".to_vec(), b"v1".to_vec()).await?;

    // 字段无过期时间，httl 返回 -1
    assert_eq!(session.httl(key, b"f1").await?, -1);
    // 字段不存在，httl 返回 -2
    assert_eq!(session.httl(key, b"non_exist").await?, -2);

    let now = Clock::now_since_epoch().as_millis();

    // 设置过期 10 秒
    let res = session
      .hexpire(key, b"f1", now + 10_000, HashExpireOpt::NONE)
      .await?;
    assert_eq!(res, HashExpireResult::Ok);

    let ttl = session.httl(key, b"f1").await?;
    assert!(ttl > 0 && ttl <= 10_000, "ttl 实际为: {ttl}");

    // PERSIST 移除过期时间
    let p_res = session.hpersist(key, b"f1").await?;
    assert!(p_res);
    assert_eq!(session.httl(key, b"f1").await?, -1);

    // 再次 PERSIST 已无过期时间的字段返回 false
    let p_res2 = session.hpersist(key, b"f1").await?;
    assert!(!p_res2);

    // 设置过期为过去时间（直接过期）
    let res_exp = session
      .hexpire(key, b"f1", now.saturating_sub(1000), HashExpireOpt::NONE)
      .await?;
    assert_eq!(res_exp, HashExpireResult::KeyAlreadyExpired);
    // 已过期的字段读取应返回 None
    assert_eq!(session.hget(key, b"f1").await?, None);

    info!("测试 3: Hash 打平字段过期与 TTL 控制全部通过");
    OK
  })
}

/// 测试 4: Hash 打平多分块大容量遍历 (hgetall, hscan 跨分块)
#[test]
fn test_flattened_hash_multi_chunk_iteration() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:test:multichunk";

    // 写入 250 个字段，跨越默认 128 元素的分块边界
    let count = 250usize;
    let mut pairs = Vec::with_capacity(count);
    for i in 0..count {
      let f = format!("field_{}", pad(i, 4)).into_bytes();
      let v = format!("value_{}", pad(i, 4)).into_bytes();
      pairs.push((f, v));
    }

    let inserted = session.hmset(key, pairs.clone()).await?;
    assert_eq!(inserted, count);
    assert_eq!(session.hlen(key).await?, count);

    // HGETALL 获取全部 250 项
    let all = session.hgetall(key).await?;
    assert_eq!(all.len(), count);

    // HSCAN 游标分页遍历
    let mut scanned_count = 0;
    let mut cursor = 0;
    loop {
      let (next_cur, items) = session.hscan(key, cursor, 40, None).await?;
      scanned_count += items.len();
      cursor = next_cur;
      if cursor == 0 {
        break;
      }
    }
    assert_eq!(scanned_count, count);

    // HSCAN 带通配符模式匹配
    let (c, matched) = session.hscan(key, 0, 300, Some(b"field_001*")).await?;
    assert_eq!(c, 0);
    // field_0010 到 field_0019 共 10 项
    assert_eq!(matched.len(), 10);

    info!("测试 4: Hash 打平多分块大容量遍历全部通过");
    OK
  })
}

/// 测试 5: Set 打平存储指令 (sadd, srem, sismember, smismember, scard, smembers, spop, sscan)
#[test]
fn test_flattened_set_commands() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"set:test:basic";

    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.scard(key).await?, 0);
    assert!(!session.sismember(key, b"m1").await?);

    // SADD 新增成员
    let added = session
      .sadd(key, vec![b"m1".to_vec(), b"m2".to_vec(), b"m3".to_vec()])
      .await?;
    assert_eq!(added, 3);
    assert_eq!(session.type_of(key).await?, "set");
    assert_eq!(session.scard(key).await?, 3);

    // 重复添加
    let dup_added = session
      .sadd(key, vec![b"m2".to_vec(), b"m4".to_vec()])
      .await?;
    assert_eq!(dup_added, 1); // 仅 m4 是新的
    assert_eq!(session.scard(key).await?, 4);

    // SISMEMBER & SMISMEMBER
    assert!(session.sismember(key, b"m1").await?);
    assert!(!session.sismember(key, b"non_exist").await?);

    let is_members = session
      .smismember(key, &[b"m1", b"non_exist", b"m3", b"m4"])
      .await?;
    assert_eq!(is_members, vec![true, false, true, true]);

    // SMEMBERS 获取全部
    let members = session.smembers(key).await?;
    assert_eq!(members.len(), 4);

    // SPOP 随机弹出成员
    let popped = session.spop(key, 1).await?;
    assert_eq!(popped.len(), 1);
    assert_eq!(session.scard(key).await?, 3);
    assert!(!session.sismember(key, &popped[0]).await?);

    // SREM 移除指定成员
    let rem_count = session.srem(key, &[b"m1", b"non_exist"]).await?;
    let scard = session.scard(key).await?;
    assert_eq!(scard, 3 - rem_count);

    // 清空剩余全部成员以验证 Meta 自动清除
    let remaining = session.smembers(key).await?;
    let remaining_refs: Vec<&[u8]> = remaining.iter().map(|m| m.as_slice()).collect();
    let rem_all = session.srem(key, &remaining_refs).await?;
    assert_eq!(rem_all, remaining.len());
    assert_eq!(session.scard(key).await?, 0);
    assert_eq!(session.type_of(key).await?, "none");

    info!("测试 5: Set 打平存储指令全部通过");
    OK
  })
}

/// 测试 6: Set 多分块大容量遍历与 SSCAN 游标
#[test]
fn test_flattened_set_multi_chunk_iteration() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"set:test:multichunk";

    // 写入 200 个成员，跨越 128 分块边界
    let count = 200usize;
    let mut members = Vec::with_capacity(count);
    for i in 0..count {
      members.push(format!("member_{}", pad(i, 4)).into_bytes());
    }

    let added = session.sadd(key, members).await?;
    assert_eq!(added, count);
    assert_eq!(session.scard(key).await?, count);

    // SMEMBERS 完整获取
    let all = session.smembers(key).await?;
    assert_eq!(all.len(), count);

    // SSCAN 游标分页
    let mut scanned_count = 0;
    let mut cursor = 0;
    loop {
      let (next_cur, items) = session.sscan(key, cursor, 50, None).await?;
      scanned_count += items.len();
      cursor = next_cur;
      if cursor == 0 {
        break;
      }
    }
    assert_eq!(scanned_count, count);

    // SSCAN 通配符过滤
    let (c, matched) = session.sscan(key, 0, 300, Some(b"member_005*")).await?;
    assert_eq!(c, 0);
    assert_eq!(matched.len(), 10);

    info!("测试 6: Set 多分块大容量遍历与 SSCAN 全部通过");
    OK
  })
}

/// 测试 7: 集合秒删 (O(1) Delete) 与覆盖自愈隔离验证
#[test]
fn test_flattened_delete_and_version_isolation() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"coll:test:isolation";

    // 1. 创建 Hash 集合
    session
      .hset(key, b"field_a".to_vec(), b"val_a".to_vec())
      .await?;
    session
      .hset(key, b"field_b".to_vec(), b"val_b".to_vec())
      .await?;
    assert_eq!(session.hlen(key).await?, 2);
    assert_eq!(session.type_of(key).await?, "hash");

    // 2. 秒删集合 (session.delete)
    assert!(session.delete(key).await?);
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.hlen(key).await?, 0);
    assert_eq!(session.hget(key, b"field_a").await?, None);

    // 3. 重新以 Set 类型复用同一个 key（模拟同名集合重新创建）
    session
      .sadd(key, vec![b"item1".to_vec(), b"item2".to_vec()])
      .await?;
    assert_eq!(session.type_of(key).await?, "set");
    assert_eq!(session.scard(key).await?, 2);
    assert!(session.sismember(key, b"item1").await?);

    // 验证旧 Hash 子键绝不会干扰新的 Set 集合
    assert_eq!(session.hlen(key).await?, 0);
    assert_eq!(session.hget(key, b"field_a").await?, None);

    // 4. 再次秒删
    assert!(session.delete(key).await?);
    assert_eq!(session.type_of(key).await?, "none");
    assert_eq!(session.scard(key).await?, 0);

    // 5. 重新以 Hash 类型复用同一个 key，只写入 field_c
    session
      .hset(key, b"field_c".to_vec(), b"val_c".to_vec())
      .await?;
    assert_eq!(session.type_of(key).await?, "hash");
    assert_eq!(session.hlen(key).await?, 1);
    // 旧的 field_a 绝不能出现
    assert_eq!(session.hget(key, b"field_a").await?, None);
    assert_eq!(
      session.hget(key, b"field_c").await?,
      Some(b"val_c".to_vec())
    );

    info!("测试 7: 集合秒删与版本隔离自愈验证全部通过");
    OK
  })
}

/// 测试 8: 非递归贪心 glob_match 全状态机与防 ReDoS 边界验证
#[test]
fn test_flattened_glob_match_exhaustive() -> Void {
  use wedb_redis::glob_match;

  // 基础精确匹配与空串
  assert!(glob_match(b"", b""));
  assert!(glob_match(b"abc", b"abc"));
  assert!(!glob_match(b"abc", b"abcd"));
  assert!(!glob_match(b"abcd", b"abc"));

  // 单字符通配符 '?'
  assert!(glob_match(b"a?c", b"abc"));
  assert!(glob_match(b"a?c", b"a1c"));
  assert!(!glob_match(b"a?c", b"ac"));
  assert!(!glob_match(b"a?c", b"abbc"));

  // 星号通配符 '*' 与连续星号
  // 对齐 C# GlobUtils：模式非空而目标为空时不匹配
  assert!(!glob_match(b"*", b""));
  assert!(glob_match(b"***", b"hello"));
  assert!(glob_match(b"h*o", b"hello"));
  assert!(glob_match(b"*bar", b"foobar"));
  assert!(glob_match(b"foo*", b"foobar"));
  assert!(glob_match(b"*bar*", b"abcbarxyz"));
  assert!(!glob_match(b"*bar", b"foobaz"));

  // 中括号字符集与范围 '[...]'
  assert!(glob_match(b"h[ae]llo", b"hello"));
  assert!(glob_match(b"h[ae]llo", b"hallo"));
  assert!(!glob_match(b"h[ae]llo", b"hillo"));
  assert!(glob_match(b"item[0-9]", b"item5"));
  assert!(!glob_match(b"item[0-9]", b"itemX"));
  assert!(glob_match(b"[a-zA-Z]", b"G"));

  // 中括号取反 '[^...]' 与 '[!...]'
  assert!(glob_match(b"item[^0-9]", b"itemX"));
  assert!(!glob_match(b"item[^0-9]", b"item5"));
  // 对齐 C# GlobUtils：'!' 为字面成员而非取反
  assert!(!glob_match(b"item[!0-9]", b"itemY"));

  // 极端回溯与对抗模式（贪心状态机单次遍历，杜绝 ReDoS 栈溢出与爆炸）
  assert!(glob_match(b"*a*b*c*d*e*", b"xxxa___b___c___d___e___yyy"));
  assert!(!glob_match(b"*a*b*c*d*e*z", b"xxxa___b___c___d___e___yyy"));

  info!("测试 8: 非递归贪心 glob_match 全状态机与边界测试全部通过");
  OK
}

/// 测试 9: 零分配入参直传与大 Value 零值拷贝探测 (hset, hmset, sadd, hstrlen, hexists)
#[test]
fn test_flattened_zero_copy_and_large_values() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:test:zerocopy";

    // 1. 直接传入 &[u8] 切片与 &str 字面量，严禁强制要求 .to_vec()
    assert!(session.hset(key, b"f_slice", "v_str").await?);
    assert!(session.hset(key, "f_str", b"v_slice").await?);
    assert_eq!(session.hlen(key).await?, 2);

    // 2. 写入 32KB 大 Value（适配 64KB 单页上限）
    let large_val = vec![b'X'; 32 * 1024];
    session.hset(key, b"big_val", &large_val).await?;

    // 3. hexists 与 hstrlen 零值拷贝探测：直接从子键切片提取长度，不产生全量值拷贝
    assert!(session.hexists(key, b"big_val").await?);
    assert_eq!(session.hstrlen(key, b"big_val").await?, 32 * 1024);

    // 4. sadd 泛型入参测试：直传切片
    let set_key = b"set:test:zerocopy";
    let count = session
      .sadd(set_key, vec!["member1", "member2", "member3"])
      .await?;
    assert_eq!(count, 3);
    assert_eq!(session.scard(set_key).await?, 3);

    info!("测试 9: 零分配入参直传与大 Value 零值拷贝探测测试全部通过");
    OK
  })
}

/// 测试 10: spop 局部 Fisher-Yates 边界与 hkeys/hvals 独立性验证
#[test]
fn test_flattened_spop_and_keys_vals_isolation() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let set_key = b"set:test:spop_edge";

    // 1. 空集合与 count = 0
    assert!(session.spop(set_key, 0).await?.is_empty());
    assert!(session.spop(set_key, 5).await?.is_empty());

    // 2. 插入 3 个元素
    session.sadd(set_key, vec![b"e1", b"e2", b"e3"]).await?;

    // 3. 弹出超过总数（count > scard），应全部弹出并清理 Meta
    let popped = session.spop(set_key, 10).await?;
    assert_eq!(popped.len(), 3);
    assert_eq!(session.scard(set_key).await?, 0);
    assert_eq!(session.type_of(set_key).await?, "none");

    // 4. hkeys 与 hvals 独立提取
    let hash_key = b"hash:test:keys_vals";
    session
      .hmset(
        hash_key,
        vec![(b"k1", b"v1"), (b"k2", b"v2"), (b"k3", b"v3")],
      )
      .await?;
    let mut keys = session.hkeys(hash_key).await?;
    keys.sort();
    assert_eq!(keys, vec![b"k1".to_vec(), b"k2".to_vec(), b"k3".to_vec()]);

    let mut vals = session.hvals(hash_key).await?;
    vals.sort();
    assert_eq!(vals, vec![b"v1".to_vec(), b"v2".to_vec(), b"v3".to_vec()]);

    info!("测试 10: spop 局部 Fisher-Yates 边界与 hkeys/hvals 独立性测试全部通过");
    OK
  })
}

/// 测试 11: HSCAN 与 SSCAN O(limit) 分块跳跃寻址与大规模游标分页测试
#[test]
fn test_flattened_scan_chunk_jumping_and_large_scale_paging() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let hkey = b"hash:test:chunk_jump";
    let skey = b"set:test:chunk_jump";

    // 1. 写入 300 个哈希字段，跨越 3 个分块 (128 + 128 + 44)
    let count = 300usize;
    let mut hpairs = Vec::with_capacity(count);
    let mut smembers = Vec::with_capacity(count);
    for i in 0..count {
      hpairs.push((
        format!("h_field_{}", pad(i, 4)).into_bytes(),
        format!("h_val_{}_{}", pad(i, 4), pad(0, 60)).into_bytes(),
      ));
      smembers.push(format!("s_member_{}", pad(i, 4)).into_bytes());
    }

    assert_eq!(session.hmset(hkey, hpairs).await?, count);
    assert_eq!(session.sadd(skey, smembers).await?, count);

    // 2. 验证 HSCAN 分块跳跃寻址：从游标 128 直接跳到第 1 块（跳过第 0 块的 128 项）
    let (next_cur, items) = session.hscan(hkey, 128, 40, None).await?;
    assert_eq!(items.len(), 40);
    assert_eq!(items[0].0, b"h_field_0128");
    assert_eq!(items[39].0, b"h_field_0167");
    assert_eq!(next_cur, 168);

    // 从 256 开始读取第 2 块（最后一块剩余 44 项）
    let (next_cur, items) = session.hscan(hkey, 256, 100, None).await?;
    assert_eq!(items.len(), 44);
    assert_eq!(items[0].0, b"h_field_0256");
    assert_eq!(items[43].0, b"h_field_0299");
    assert_eq!(next_cur, 0); // 已扫描至集合尾部，自动返回 0

    // 游标超出最大分块边界，安全返回 0 且为空
    let (cur_overflow, overflow_items) = session.hscan(hkey, 1000, 10, None).await?;
    assert_eq!(cur_overflow, 0);
    assert!(overflow_items.is_empty());

    // 3. 验证 SSCAN 分块跳跃寻址：从游标 128 直接跳到第 1 块
    let (next_cur, items) = session.sscan(skey, 128, 50, None).await?;
    assert_eq!(items.len(), 50);
    assert_eq!(items[0], b"s_member_0128");
    assert_eq!(items[49], b"s_member_0177");
    assert_eq!(next_cur, 178);

    // 从 256 开始读取第 2 块
    let (next_cur, items) = session.sscan(skey, 256, 100, None).await?;
    assert_eq!(items.len(), 44);
    assert_eq!(items[0], b"s_member_0256");
    assert_eq!(items[43], b"s_member_0299");
    assert_eq!(next_cur, 0);

    // 超出边界
    let (cur_overflow, overflow_items) = session.sscan(skey, 500, 10, None).await?;
    assert_eq!(cur_overflow, 0);
    assert!(overflow_items.is_empty());

    // 4. 验证删除与跳跃的鲁棒性：删除第 1 块中的前 2 个字段，扫描时能够正确跳过并读取后续
    session
      .hdel(hkey, &[b"h_field_0128", b"h_field_0129"])
      .await?;
    let (next_cur, items) = session.hscan(hkey, 128, 5, None).await?;
    assert_eq!(items.len(), 5);
    assert_eq!(items[0].0, b"h_field_0130");
    assert_eq!(next_cur, 135);

    // 5. 验证恰好跨越分块末尾（index == CHUNK_CAPACITY - 1 == 127）的数学精准度
    // 从游标 0 读取恰好 128 项，最后一项索引恰好为 127，下一游标应精准推进到第 1 块开始 (128)
    let (next_cur_128, items_128) = session.hscan(hkey, 0, 128, None).await?;
    assert_eq!(items_128.len(), 128);
    assert_eq!(items_128[0].0, b"h_field_0000");
    assert_eq!(items_128[127].0, b"h_field_0127");
    assert_eq!(next_cur_128, 128);

    // 6. 验证超大游标防截断与越界保护
    let (max_cur_h, max_items_h) = session.hscan(hkey, usize::MAX, 10, None).await?;
    assert_eq!(max_cur_h, 0);
    assert!(max_items_h.is_empty());

    // 验证 64 位整型巨大游标防截断环绕溢出：避免 (cursor / cap) as u32 发生低位截断环绕回 0 #[cfg(target_pointer_width = "64")]
    {
      let huge_cursor = ((1u64 << 32) as usize).saturating_mul(128);
      let (huge_cur_h, huge_items_h) = session.hscan(hkey, huge_cursor, 10, None).await?;
      assert_eq!(huge_cur_h, 0);
      assert!(huge_items_h.is_empty());

      let (huge_cur_s, huge_items_s) = session.sscan(skey, huge_cursor, 10, None).await?;
      assert_eq!(huge_cur_s, 0);
      assert!(huge_items_s.is_empty());
    }

    info!("测试 11: HSCAN 与 SSCAN O(limit) 分块跳跃寻址与大规模游标分页测试全部通过");
    OK
  })
}

/// 测试 12: Flattened hash 字段过期路径锁重入回归 (HEXPIRE/HPERSIST 持锁后必须复用无锁内核)
///
/// 回归背景：HEXPIRE/HPERSIST 入口已持本键独占桶锁，其 Flattened 编码分支曾重入加锁版
/// hdel，对同键同桶二次加锁自旋 1024 次失败后向调用方返回 LockTimeout 错误
#[test]
fn test_flattened_hash_expire_lock_reentrancy() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"hash:test:expire_reentrant";

    // 256B 大值字段超过紧凑内联 64B 门限，强制打平为 Flattened 编码
    let big_val = vec![b'V'; 256];
    assert!(
      session
        .hset(key, b"f_expire".to_vec(), big_val.clone())
        .await?
    );
    assert!(session.hset(key, b"f_persist".to_vec(), big_val).await?);
    let meta = session.load_meta(key).await?.expect("meta 存在");
    assert_eq!(meta.encoding(), StorageEncoding::Flattened);

    let now = Clock::now_since_epoch().as_millis();

    // 1. HEXPIRE 过去时间戳：过期即删，应返回业务语义 KeyAlreadyExpired，绝不能是 LockTimeout
    let res = session
      .hexpire(
        key,
        b"f_expire",
        now.saturating_sub(1000),
        HashExpireOpt::NONE,
      )
      .await?;
    assert_eq!(res, HashExpireResult::KeyAlreadyExpired);
    assert_eq!(session.hget(key, b"f_expire").await?, None);
    assert_eq!(session.hlen(key).await?, 1);

    // 2. HPERSIST 已过期字段：物理清理后返回 false，同样不得 LockTimeout
    let now2 = Clock::now_since_epoch().as_millis();
    assert_eq!(
      session
        .hexpire(key, b"f_persist", now2 + 10, HashExpireOpt::NONE)
        .await?,
      HashExpireResult::Ok
    );
    sleep(Duration::from_millis(25)).await;
    assert!(!session.hpersist(key, b"f_persist").await?);
    assert_eq!(session.hlen(key).await?, 0);

    // 3. HEXPIRE 未来时间戳正常 Ok，随后 HGET/HTTL 走 TTL 判活路径不炸
    assert!(
      session
        .hset(key, b"f_alive".to_vec(), vec![b'A'; 256])
        .await?
    );
    let now3 = Clock::now_since_epoch().as_millis();
    assert_eq!(
      session
        .hexpire(key, b"f_alive", now3 + 60_000, HashExpireOpt::NONE)
        .await?,
      HashExpireResult::Ok
    );
    assert_eq!(session.hget(key, b"f_alive").await?, Some(vec![b'A'; 256]));
    let ttl = session.httl(key, b"f_alive").await?;
    assert!(ttl > 0 && ttl <= 60_000, "ttl 实际为: {ttl}");

    info!("测试 12: Flattened hash 字段过期路径锁重入回归全部通过");
    OK
  })
}
