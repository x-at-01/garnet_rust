use std::{
  sync::Arc,
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use aok::{OK, Void};
use compio::{runtime::Runtime, time::sleep};
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_hash::{ExpireOpt as HashExpireOpt, ExpireResult as HashExpireResult};
use wedb_redis::prelude::*;
use wedb_zset::ZAddOpt;
use wkv::{StorageEncoding, StoreConfig, WedbStore};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

fn create_test_store() -> aok::Result<(tempfile::TempDir, Arc<WedbStore<SegmentedDevice>>)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("semantic_fixes.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(4096, 64 * 1024, 128, 0.5)?,
    device,
  )?);
  Ok((dir, store))
}

/// HEXPIRE 选项优先级高于过去时间戳：NX + 过去时间戳绝不能误删已有 TTL 的字段，
/// 两个编码分支 (Compact/Flattened) 均须返回 ExpireConditionNotMet
#[test]
fn test_hexpire_option_precedence_over_past_timestamp_both_encodings() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let past = 1_000u64; // 固定的过去时间戳 (1970-01-01)

    // ---- Compact 分支 (少量小字段) ----
    let key_c = b"hx:compact";
    assert!(session.hset(key_c, b"f".to_vec(), b"v".to_vec()).await?);
    // 先设置一个未来 TTL，确保字段带 curr_expire
    assert_eq!(
      session
        .hexpire(key_c, b"f", 4_102_444_800_000, HashExpireOpt::default())
        .await?,
      HashExpireResult::Ok
    );
    // NX + 过去时间戳：条件不满足，字段必须保留
    assert_eq!(
      session
        .hexpire(
          key_c,
          b"f",
          past,
          HashExpireOpt {
            nx: true,
            ..Default::default()
          }
        )
        .await?,
      HashExpireResult::ExpireConditionNotMet
    );
    assert!(session.hexists(key_c, b"f").await?, "NX 不得误删字段");
    // GT + 过去时间戳 (新值 <= 旧值)：同样不得误删
    assert_eq!(
      session
        .hexpire(
          key_c,
          b"f",
          past,
          HashExpireOpt {
            gt: true,
            ..Default::default()
          }
        )
        .await?,
      HashExpireResult::ExpireConditionNotMet
    );
    assert!(session.hexists(key_c, b"f").await?);

    // ---- Flattened 分支 (> HASH_MAX_COMPACT_ENTRIES 字段) ----
    let key_f = b"hx:flat";
    for i in 0..520usize {
      let f = format!("f{}", i);
      session.hset(key_f, f.into_bytes(), b"v".to_vec()).await?;
    }
    let meta = session.load_meta(key_f).await?.expect("meta exists");
    assert_eq!(
      meta.encoding(),
      StorageEncoding::Flattened,
      "前置条件：应为 Flattened 编码"
    );
    assert_eq!(
      session
        .hexpire(key_f, b"f7", 4_102_444_800_000, HashExpireOpt::default())
        .await?,
      HashExpireResult::Ok
    );
    assert_eq!(
      session
        .hexpire(
          key_f,
          b"f7",
          past,
          HashExpireOpt {
            nx: true,
            ..Default::default()
          }
        )
        .await?,
      HashExpireResult::ExpireConditionNotMet
    );
    assert!(session.hexists(key_f, b"f7").await?, "NX 不得误删字段");

    // 已过期未淘字段：HEXPIRE 按 KeyNotFound 处理 (对齐 hash.rs check_and_purge_expired 口径)
    let key_e = b"hx:expired";
    assert!(session.hset(key_e, b"g".to_vec(), b"v".to_vec()).await?);
    let now_ms = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_millis() as u64;
    assert_eq!(
      session
        .hexpire(key_e, b"g", now_ms + 400, HashExpireOpt::default())
        .await?,
      HashExpireResult::Ok
    );
    sleep(Duration::from_millis(700)).await;
    assert_eq!(
      session
        .hexpire(key_e, b"g", 4_102_444_800_000, HashExpireOpt::default())
        .await?,
      HashExpireResult::KeyNotFound
    );

    OK
  })
}

/// SRANDMEMBER 负数 count：恰好 |count| 个可重复成员，可超过集合基数
#[test]
fn test_srandmember_negative_count_with_repetition() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;
    let key = b"set:srand";
    session.sadd(key, [&b"a"[..], &b"b"[..]]).await?;

    // -5：恰好 5 个元素，每个都属于集合
    let items = session.srandmember(key, -5).await?;
    assert_eq!(items.len(), 5, "负数 count 应返回恰好 |count| 个可重复成员");
    assert!(
      items.iter().all(|m| m == b"a" || m == b"b"),
      "采样元素必须属于集合"
    );

    // -1：恰好 1 个元素的数组
    let one = session.srandmember(key, -1).await?;
    assert_eq!(one.len(), 1);

    // 正数 5：至多 2 个互异成员 (集合基数封顶)
    let distinct = session.srandmember(key, 5).await?;
    assert_eq!(distinct.len(), 2);
    assert_ne!(distinct[0], distinct[1], "正数 count 应互异");

    // 0：空
    assert!(session.srandmember(key, 0).await?.is_empty());

    // 空集合负数：空数组
    let key_empty = b"set:empty";
    session.sadd(key_empty, [&b"x"[..]]).await?;
    session.srem(key_empty, &[&b"x"[..]]).await?;
    assert!(session.srandmember(key_empty, -3).await?.is_empty());

    OK
  })
}

/// KEYS / DBSIZE 用户键语义C# Garnet UnifiedStoreGetDBKeys）：
/// 1. 集合键必须以去标签的用户键名返回（剥离 Meta 前缀），且不得泄漏内部键
/// 2. 已清空 Flattened 集合残留的幽灵元记录不得计入 DBSIZE / KEYS
#[test]
fn test_keys_and_dbsize_user_key_semantics() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    session.upsert(b"str1", b"v").await?;
    session.hset(b"h1", b"f".to_vec(), b"v".to_vec()).await?;
    session.sadd(b"s1", [&b"m"[..]]).await?;

    assert_eq!(session.dbsize().await?, 3);
    let ks = session.keys(b"*").await?;
    assert!(ks.contains(&b"str1".to_vec()));
    assert!(
      ks.contains(&b"h1".to_vec()),
      "集合键必须以用户键名返回（剥离 Meta 标签前缀）"
    );
    assert!(ks.contains(&b"s1".to_vec()));
    assert_eq!(ks.len(), 3, "不得重复或泄漏内部 Meta/子键");

    // 模式匹配作用于用户键名
    assert_eq!(session.keys(b"h*").await?, vec![b"h1".to_vec()]);
    assert!(session.keys(b"zzz*").await?.is_empty());

    // 幽灵键：Flattened 哈希清空后 meta 记录残留（size=0）
    let gk = b"ghost";
    let fields: Vec<Vec<u8>> = (0..520usize)
      .map(|i| format!("f{}", i).into_bytes())
      .collect();
    for f in &fields {
      session.hset(gk, f.clone(), b"v".to_vec()).await?;
    }
    let meta = session.load_meta(gk).await?.expect("meta exists");
    assert_eq!(
      meta.encoding(),
      StorageEncoding::Flattened,
      "前置条件：应为 Flattened 编码"
    );
    let refs: Vec<&[u8]> = fields.iter().map(|f| f.as_slice()).collect();
    assert_eq!(session.hdel(gk, &refs).await?, 520);
    assert_eq!(session.type_of(gk).await?, "none");
    assert!(!session.contains_key(gk).await?, "清空后集合键不应存在");

    assert_eq!(session.dbsize().await?, 3, "幽灵键不得计入 DBSIZE");
    assert!(
      !session.keys(b"*").await?.contains(&gk.to_vec()),
      "幽灵键不得被 KEYS 返回"
    );

    OK
  })
}

/// SINTERCARD LIMIT 提前终止语义：驱动集选取与命中数封顶Redis/Garnet）
#[test]
fn test_sintercard_limit_semantics() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    let a = b"sc:a";
    let b = b"sc:b";
    let c = b"sc:c";
    session
      .sadd(a, [&b"1"[..], &b"2"[..], &b"3"[..], &b"4"[..], &b"5"[..]])
      .await?;
    session.sadd(b, [&b"1"[..], &b"2"[..], &b"3"[..]]).await?;
    session.sadd(c, [&b"2"[..], &b"3"[..], &b"9"[..]]).await?;

    // 完整交集 {2,3}
    assert_eq!(session.sintercard(&[a, b, c], 0).await?, 2);
    assert_eq!(session.sintercard(&[a, b, c], 5).await?, 2);
    // LIMIT 封顶
    assert_eq!(session.sintercard(&[a, b, c], 1).await?, 1);
    // 单键：即其自身
    assert_eq!(session.sintercard(&[a], 0).await?, 5);
    // 含空集
    let e = b"sc:empty";
    session.sadd(e, [&b"x"[..]]).await?;
    session.srem(e, &[&b"x"[..]]).await?;
    assert_eq!(session.sintercard(&[a, e], 0).await?, 0);
    // 空键列表
    assert_eq!(session.sintercard(&[], 0).await?, 0);
    // 不相交
    let d = b"sc:d";
    session.sadd(d, [&b"z"[..]]).await?;
    assert_eq!(session.sintercard(&[a, d], 0).await?, 0);

    OK
  })
}

/// PFADD 空元素语义：键不存在且未携带元素时不得创建键（对齐 Redis 规范）
#[test]
fn test_pfadd_no_elements_does_not_create_key() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // 不存在 + 无元素：返回 false 且不创建键
    assert!(!session.pfadd(b"pf:empty", &[]).await?);
    assert_eq!(session.type_of(b"pf:empty").await?, "none");
    assert_eq!(session.pfcount(&[b"pf:empty"]).await?, 0);

    // 携带元素：创建并返回 true
    assert!(session.pfadd(b"pf:k", &[b"e1", b"e2"]).await?);
    assert_eq!(session.type_of(b"pf:k").await?, "string");
    assert!(session.pfcount(&[b"pf:k"]).await? >= 1);

    // 已存在 + 无元素：无操作返回 false，基数保持
    let cnt = session.pfcount(&[b"pf:k"]).await?;
    assert!(!session.pfadd(b"pf:k", &[]).await?);
    assert_eq!(session.pfcount(&[b"pf:k"]).await?, cnt);

    OK
  })
}

/// HSET/HSETNX/HINCRBY 对"已过期但尚未惰汰"字段的原位替换语义：
/// 不得重复计数（HLEN 虚高）也不得重复登记分块索引
#[test]
fn test_hset_on_expired_field_no_double_count() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // ---- HSET：Flattened 编码（大值强制打平）----
    let key = b"hx:readd:flat";
    let big_val = vec![b'x'; 80]; // > HASH_MAX_COMPACT_VALUE(64) 强制 Flattened
    assert!(session.hset(key, b"f1".to_vec(), big_val.clone()).await?);
    assert!(session.hset(key, b"f2".to_vec(), big_val).await?);
    assert_eq!(session.hlen(key).await?, 2);

    // 为 f1 设置 60ms 后过期并等待其真实流逝（形成"已过期未惰汰"状态）
    let now_ms = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_millis() as u64;
    assert_eq!(
      session
        .hexpire(key, b"f1", now_ms + 400, HashExpireOpt::default())
        .await?,
      HashExpireResult::Ok
    );
    sleep(Duration::from_millis(700)).await;

    // 过期字段原位覆写：返回 1（视为新增），但 HLEN 必须保持 2 不虚高
    assert!(session.hset(key, b"f1".to_vec(), b"fresh".to_vec()).await?);
    assert_eq!(session.hlen(key).await?, 2, "过期字段原位替换不得重复计数");
    assert_eq!(session.hget(key, b"f1").await?, Some(b"fresh".to_vec()));
    assert_eq!(session.hkeys(key).await?.len(), 2);

    // ---- HINCRBY：过期字段原位替换同样不得重复计数 ----
    let key_i = b"hx:readd:incr";
    assert!(session.hset(key_i, b"n".to_vec(), b"10".to_vec()).await?);
    let now_ms = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_millis() as u64;
    assert_eq!(
      session
        .hexpire(key_i, b"n", now_ms + 400, HashExpireOpt::default())
        .await?,
      HashExpireResult::Ok
    );
    sleep(Duration::from_millis(700)).await;

    assert_eq!(session.hincrby(key_i, b"n", 5).await?, 5);
    assert_eq!(
      session.hlen(key_i).await?,
      1,
      "HINCRBY 过期替换不得重复计数"
    );
    assert_eq!(session.hget(key_i, b"n").await?, Some(b"5".to_vec()));

    // ---- HSETNX：过期字段可写入且不重复计数 ----
    let key_nx = b"hx:readd:nx";
    assert!(session.hset(key_nx, b"n".to_vec(), b"10".to_vec()).await?);
    let now_ms = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_millis() as u64;
    assert_eq!(
      session
        .hexpire(key_nx, b"n", now_ms + 400, HashExpireOpt::default())
        .await?,
      HashExpireResult::Ok
    );
    sleep(Duration::from_millis(700)).await;
    assert!(session.hsetnx(key_nx, b"n", b"99".to_vec()).await?);
    assert_eq!(
      session.hlen(key_nx).await?,
      1,
      "HSETNX 过期替换不得重复计数"
    );
    assert_eq!(session.hget(key_nx, b"n").await?, Some(b"99".to_vec()));

    // ---- 惰汰后真重建：hget 触发物理清理后 HSET 仍收敛一致 ----
    let key_p = b"hx:readd:purge";
    assert!(session.hset(key_p, b"g".to_vec(), b"v1".to_vec()).await?);
    let now_ms = SystemTime::now()
      .duration_since(UNIX_EPOCH)
      .unwrap()
      .as_millis() as u64;
    assert_eq!(
      session
        .hexpire(key_p, b"g", now_ms + 400, HashExpireOpt::default())
        .await?,
      HashExpireResult::Ok
    );
    sleep(Duration::from_millis(700)).await;
    // hget 触发惰汰（物理删除 + size 回退）
    assert_eq!(session.hget(key_p, b"g").await?, None);
    assert_eq!(session.hlen(key_p).await?, 0);
    // 真重建：物理缺席新增路径
    assert!(session.hset(key_p, b"g".to_vec(), b"v2".to_vec()).await?);
    assert_eq!(session.hlen(key_p).await?, 1);
    assert_eq!(session.hget(key_p, b"g").await?, Some(b"v2".to_vec()));

    OK
  })
}

/// 幽灵元记录（打平集合秒删残留）与同名裸键共存的一致性口径：
/// TYPE 必须上报 string，集合写入必须拒绝（WRONGTYPE），杜绝在活字符串之上静默重建同名集合
#[test]
fn test_ghost_meta_with_raw_string_semantics() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    let key = b"ghost:string";

    // 1. 大值哈希（Flattened 编码）后 DEL：留下 size == 0 的幽灵元记录
    let big_val = vec![b'y'; 80];
    assert!(session.hset(key, b"f".to_vec(), big_val).await?);
    assert_eq!(
      session.load_meta(key).await?.map(|m| m.size),
      Some(1),
      "前置条件：活集合 size == 1"
    );
    assert!(session.delete(key).await?);
    assert!(
      session.load_meta(key).await?.is_some(),
      "秒删后残留幽灵元记录"
    );
    assert_eq!(
      session.type_of(key).await?,
      "none",
      "无裸键时幽灵记录按 none 上报"
    );

    // 2. 同名写入裸字符串（模拟秒删后 SET）
    session.upsert(key, b"plain").await?;

    // 3. TYPE 必须上报 string（修复前误报 none）
    assert_eq!(session.type_of(key).await?, "string");
    assert!(session.contains_key(key).await?);
    assert_eq!(session.read_string(key).await?, Some(b"plain".to_vec()));

    // 4. 集合读写在活字符串之上必须 WRONGTYPE（修复前 HSET 会静默重建同名集合）
    let err = session.hset(key, b"f2".to_vec(), b"v".to_vec()).await;
    assert!(
      matches!(err, Err(ref e) if e.is_wrong_type()),
      "HSET 活字符串必须 WRONGTYPE"
    );
    let err = session.hlen(key).await;
    assert!(
      matches!(err, Err(ref e) if e.is_wrong_type()),
      "HLEN 活字符串必须 WRONGTYPE"
    );
    let err = session.hget(key, b"f").await;
    assert!(
      matches!(err, Err(ref e) if e.is_wrong_type()),
      "HGET 活字符串必须 WRONGTYPE"
    );

    // 5. GETSET/GETDEL 语义不受影响
    assert_eq!(
      session.getset(key, b"replaced").await?,
      Some(b"plain".to_vec())
    );
    assert_eq!(session.type_of(key).await?, "string");

    OK
  })
}
/// SMOVE 双键独占锁C# SetMove 事务锁）下成员迁移语义回归
#[test]
fn test_smove_two_key_lock_semantics() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    let src = b"sm:src";
    let dst = b"sm:dst";
    session.sadd(src, [&b"one"[..], &b"two"[..]]).await?;

    // 正常迁移
    assert!(session.smove(src, dst, b"one").await?);
    assert_eq!(session.smembers(src).await?, vec![b"two".to_vec()]);
    assert_eq!(session.smembers(dst).await?, vec![b"one".to_vec()]);

    // 成员不存在：返回 false 且无副作用
    assert!(!session.smove(src, dst, b"missing").await?);
    assert_eq!(session.smembers(dst).await?, vec![b"one".to_vec()]);

    // 源与目标同键：成员存在即返回 true，无迁移
    assert!(session.smove(src, src, b"two").await?);
    assert_eq!(session.scard(src).await?, 1);

    OK
  })
}

/// 极端 count 参数不得触发容量溢出中断（禁 panic 约束）：
/// ZPOPMAX/ZPOPMIN 巨量 count、ZRANDMEMBER 极端负数 count、SCAN 族极端 COUNT 提示，
/// 紧凑与打平双编码分支均须安全收敛
#[test]
fn test_extreme_count_parameters_no_capacity_overflow() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // ---- ZPOPMAX / ZPOPMIN：巨量 count 安全封顶（紧凑分支）----
    let zk = b"zc:extreme";
    for (i, m) in [b"m1", b"m2", b"m3"].iter().enumerate() {
      session
        .zadd(zk, (i + 1) as f64, m.to_vec(), ZAddOpt::default())
        .await?;
    }
    let max_items = session.zpopmax(zk, i64::MAX as usize).await?;
    assert_eq!(
      max_items,
      vec![
        (b"m3".to_vec(), 3.0),
        (b"m2".to_vec(), 2.0),
        (b"m1".to_vec(), 1.0)
      ],
      "ZPOPMAX 巨量 count 应返回全部成员且按分数降序"
    );
    assert_eq!(session.zcard(zk).await?, 0, "弹出的成员必须真实移除");

    for (i, m) in [b"m1", b"m2", b"m3"].iter().enumerate() {
      session
        .zadd(zk, (i + 1) as f64, m.to_vec(), ZAddOpt::default())
        .await?;
    }

    let min_items = session.zpopmin(zk, i64::MAX as usize).await?;
    assert_eq!(min_items.len(), 3, "ZPOPMIN 巨量 count 应返回全部成员");

    // ---- ZPOPMAX：巨量 count（Flattened 分支，>128 成员）----
    let zf = b"zf:extreme";
    let members: Vec<(f64, Vec<u8>)> = (0..130usize)
      .map(|i| (i as f64, format!("m{}", i).into_bytes()))
      .collect();
    session
      .zmadd(zf, members.iter().cloned(), ZAddOpt::default())
      .await?;
    let flat_max = session.zpopmax(zf, i64::MAX as usize).await?;
    assert_eq!(
      flat_max.len(),
      130,
      "Flattened ZPOPMAX 巨量 count 安全返回全部"
    );
    assert_eq!(flat_max[0], (b"m129".to_vec(), 129.0), "降序首项校验");
    assert_eq!(session.zcard(zf).await?, 0);

    // ---- ZRANDMEMBER：极端负数 count（isize::MIN）安全收敛 ----
    let zk2 = b"zc:rand";
    for m in [b"a", b"b", b"c"] {
      session
        .zadd(zk2, 1.0, m.to_vec(), ZAddOpt::default())
        .await?;
    }
    let huge = session.zrandmember(zk2, isize::MIN).await?;
    assert!(!huge.is_empty(), "极端负数 count 必须安全返回且非空");
    assert!(
      huge
        .iter()
        .all(|(m, _)| m == b"a" || m == b"b" || m == b"c"),
      "采样成员必须属于集合"
    );
    let five = session.zrandmember(zk2, -5).await?;
    assert_eq!(five.len(), 5, "负数 count 应返回恰好 |count| 个可重复成员");

    // ---- HSCAN / SSCAN / ZSCAN：极端 COUNT 提示安全收敛（紧凑 + 打平双分支）----
    let hk = b"hx:scan";
    assert!(session.hset(hk, b"f1".to_vec(), b"v".to_vec()).await?);
    assert!(session.hset(hk, b"f2".to_vec(), b"v".to_vec()).await?);
    let (_, items) = session.hscan(hk, 0, i64::MAX as usize, None).await?;
    assert_eq!(items.len(), 2, "紧凑哈希极端 COUNT 安全返回");

    let hf = b"hf:scan";
    for i in 0..520usize {
      session
        .hset(hf, format!("f{}", i).into_bytes(), b"v".to_vec())
        .await?;
    }
    let (_, items) = session.hscan(hf, 0, i64::MAX as usize, None).await?;
    assert_eq!(items.len(), 520, "打平哈希极端 COUNT 安全返回");

    let sk = b"st:scan";
    session.sadd(sk, [&b"x"[..], &b"y"[..]]).await?;
    let (_, items) = session.sscan(sk, 0, i64::MAX as usize, None).await?;
    assert_eq!(items.len(), 2, "紧凑集合极端 COUNT 安全返回");

    let sf = b"sf:scan";
    let batch: Vec<Vec<u8>> = (0..130usize)
      .map(|i| format!("m{}", i).into_bytes())
      .collect();
    let refs: Vec<&[u8]> = batch.iter().map(|b| b.as_slice()).collect();
    session.sadd(sf, refs).await?;
    let (_, items) = session.sscan(sf, 0, i64::MAX as usize, None).await?;
    assert_eq!(items.len(), 130, "打平集合极端 COUNT 安全返回");

    let (_, items) = session.zscan(zk2, 0, i64::MAX as usize, None).await?;
    assert_eq!(items.len(), 3, "紧凑有序集合极端 COUNT 安全返回");

    let (_, items) = session.zscan(zf, 0, i64::MAX as usize, None).await?;
    assert!(
      items.is_empty(),
      "打平有序集合已清空，扫描应返回空（游标终止基准）"
    );

    OK
  })
}

/// 验证多数据库 (SELECT <db>) 与多命名空间 (Namespace) 物理隔离与动态切换：
/// 1. 不同 DB 中同名集合元数据完全物理隔离，各自独立修改与加载；
/// 2. SessionPrefixBuf 单次计算并在 set_active_db 后立即生效；
/// 3. DBSIZE 与 KEYS 严格按当前会话 (ns, db) 前缀过滤，绝不发生跨租户/跨库键泄漏；
/// 4. 会话在切换 db 后即时接入对应数据库视图。
#[test]
fn test_multi_database_and_namespace_isolation() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    let session = store.new_session()?;

    // 1. 在默认 DB 0 写入集合与数据
    session
      .hset(b"hash_key", b"field_0".to_vec(), b"val_db0".to_vec())
      .await?;
    assert_eq!(
      session.hget(b"hash_key", b"field_0").await?,
      Some(b"val_db0".to_vec())
    );

    // 2. 切换到 DB 1 (SELECT 1)
    session.set_active_db(1);
    assert_eq!(session.active_db(), 1);
    // 在 DB 1 中，DB 0 的同名哈希不存在
    assert_eq!(session.hget(b"hash_key", b"field_0").await?, None);

    // 在 DB 1 中写入同名哈希但不同字段与值
    session
      .hset(b"hash_key", b"field_1".to_vec(), b"val_db1".to_vec())
      .await?;
    assert_eq!(
      session.hget(b"hash_key", b"field_1").await?,
      Some(b"val_db1".to_vec())
    );
    assert_eq!(session.hget(b"hash_key", b"field_0").await?, None);

    // 3. 验证 DBSIZE 严格隔离：DB 1 仅有 1 个键
    assert_eq!(session.dbsize().await?, 1);
    let keys_db1 = session.keys(b"*").await?;
    assert_eq!(keys_db1, vec![b"hash_key".to_vec()]);

    // 4. 切回 DB 0，验证 DB 0 数据完好无损，且无 DB 1 字段
    session.set_active_db(0);
    assert_eq!(session.active_db(), 0);
    assert_eq!(
      session.hget(b"hash_key", b"field_0").await?,
      Some(b"val_db0".to_vec())
    );
    assert_eq!(session.hget(b"hash_key", b"field_1").await?, None);

    // 5. 租户命名空间隔离测试 (ns = 100)
    session.set_namespace(100);
    assert_eq!(session.namespace(), 100);
    assert_eq!(session.hget(b"hash_key", b"field_0").await?, None);
    assert_eq!(session.dbsize().await?, 0);
    assert!(session.keys(b"*").await?.is_empty());

    // 在 ns 100 写入数据
    session
      .hset(
        b"hash_key",
        b"field_tenant".to_vec(),
        b"val_tenant".to_vec(),
      )
      .await?;
    assert_eq!(session.dbsize().await?, 1);
    assert_eq!(
      session.hget(b"hash_key", b"field_tenant").await?,
      Some(b"val_tenant".to_vec())
    );

    // 切回默认沙箱 (ns=0, db=0)
    session.set_namespace(0);
    session.set_active_db(0);
    assert_eq!(
      session.hget(b"hash_key", b"field_0").await?,
      Some(b"val_db0".to_vec())
    );
    assert_eq!(session.hget(b"hash_key", b"field_tenant").await?, None);

    OK
  })
}
