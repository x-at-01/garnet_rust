//! key 级 TTL 核心读写路径集成测试
//!
//! 覆盖 EXPIRE 家族返回码契约、NX/XX/GT/LT 分支、惰性过期物理删除、
//! PERSIST、DEL/RENAME/SET 的 TTL 记录一致性，以及 ttl_key 反解往返。

use std::{sync::Arc, time::Duration};

use aok::{OK, Void};
use coarsetime::Clock;
use compio::{runtime::Runtime, time::sleep};
use log::info;
use tempfile::{TempDir, tempdir};
use wdev::SegmentedDevice;
use wedb_redis::{RenameResult, prelude::*};
use wkv::{StoreConfig, StoreSession, TtlOpt, WedbStore};
use wrecord::{KeyTag, NamespaceDbCodec, SessionPrefixBuf};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

fn now_ms() -> u64 {
  Clock::now_since_epoch().as_millis()
}

/// 构造独立临时库与会话（默认 ns=0, db=0）
async fn open(tag: &str) -> aok::Result<(TempDir, StoreSession<SegmentedDevice>)> {
  let dir = tempdir()?;
  let db_path = dir.path().join(format!("ttl_{tag}.db"));
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(1024, 64 * 1024, 16, 0.5)?,
    device,
  )?);
  Ok((dir, store.new_session()?))
}

/// 测试 1: EXPIRE 后未到期可读、到期惰性删除真实生效，pttl/expiretime 正确
#[test]
fn test_expire_lazy_expiry() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("lazy").await?;
    let key = b"sess:token";
    let val = b"payload_v1";
    session.upsert(key, val).await?;

    // 未设 TTL：-1；不存在键：-2
    assert_eq!(session.pttl_ms(key).await?, -1);
    assert_eq!(session.expiretime_ms(key).await?, -1);
    assert_eq!(session.pttl_ms(b"missing").await?, -2);
    assert_eq!(session.expiretime_ms(b"missing").await?, -2);

    let now = now_ms();
    assert_eq!(session.expire_at(key, now + 60_000, TtlOpt::NONE).await?, 1);
    let pttl = session.pttl_ms(key).await?;
    assert!(
      (1..=60_000).contains(&pttl),
      "pttl 应在 (0, 60000] 内: {pttl}"
    );
    let abs = session.expiretime_ms(key).await?;
    assert!(
      abs > now as i64 && abs <= (now + 60_000) as i64,
      "expiretime 应为绝对毫秒时间戳: {abs}"
    );

    // 未到期前 read/contains/mget 均可见
    assert_eq!(session.read(key).await?, Some(val.to_vec()));
    assert!(session.contains_key(key).await?);
    assert_eq!(
      session.mget(&[key.as_slice()]).await?,
      vec![Some(val.to_vec())]
    );

    // 短 TTL：惰性过期真实生效
    assert_eq!(
      session.expire_at(key, now_ms() + 50, TtlOpt::NONE).await?,
      1
    );
    // 对照存活键
    session.upsert(b"alive", b"v").await?;
    sleep(Duration::from_millis(120)).await;
    assert_eq!(session.read(key).await?, None);
    assert!(!session.contains_key(key).await?);
    assert_eq!(session.pttl_ms(key).await?, -2);
    assert_eq!(session.expiretime_ms(key).await?, -2);
    assert_eq!(
      session.pttl_ms(key).await?,
      -2,
      "mget 中过期键必须视为不存在"
    );
    assert_eq!(
      session.mget(&[key.as_slice(), b"alive".as_slice()]).await?,
      vec![None, Some(b"v".to_vec())],
      "mget 中过期键必须视为不存在"
    );

    // 物理删除真实生效：数据记录与 TTL 记录均已墓碑化（raw 路径无 TTL 接线）
    assert_eq!(
      session.read_raw(&session.session_string_key(key)).await?,
      None
    );
    assert_eq!(session.read_raw(&session.ttl_key(key)).await?, None);

    info!("测试 1: EXPIRE 惰性过期与 pttl/expiretime 通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 2: NX/XX/GT/LT 各分支返回码（判序对齐 Redis 7.4：键存活优先，条件校验先于删除）
#[test]
fn test_expire_options() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("opt").await?;
    let key = b"opt:key";
    let now = now_ms();
    let nx = TtlOpt {
      nx: true,
      ..Default::default()
    };
    let xx = TtlOpt {
      xx: true,
      ..Default::default()
    };
    let gt = TtlOpt {
      gt: true,
      ..Default::default()
    };
    let lt = TtlOpt {
      lt: true,
      ..Default::default()
    };

    // 不存在键优先返回 -2（任何选项）
    assert_eq!(session.expire_at(key, now + 1_000, TtlOpt::NONE).await?, -2);
    assert_eq!(session.expire_at(key, now + 1_000, nx).await?, -2);

    session.upsert(key, b"v").await?;
    // NX：未设 TTL 成功；已设 TTL 条件不满足
    assert_eq!(session.expire_at(key, now + 60_000, nx).await?, 1);
    assert_eq!(session.expire_at(key, now + 70_000, nx).await?, 0);
    // GT：不大于当前过期时间 → 0；大于 → 1
    assert_eq!(session.expire_at(key, now + 50_000, gt).await?, 0);
    assert_eq!(session.expire_at(key, now + 70_000, gt).await?, 1);
    // LT：不小于当前过期时间 → 0；小于 → 1
    assert_eq!(session.expire_at(key, now + 80_000, lt).await?, 0);
    assert_eq!(session.expire_at(key, now + 65_000, lt).await?, 1);
    // XX：已设 TTL → 成功
    assert_eq!(session.expire_at(key, now + 70_000, xx).await?, 1);

    // PERSIST 后：XX / GT 必须不满足（未设 TTL），NONE 正常
    assert_eq!(session.persist(key).await?, 1);
    assert_eq!(session.expire_at(key, now + 90_000, xx).await?, 0);
    assert_eq!(session.expire_at(key, now + 90_000, gt).await?, 0);
    assert_eq!(session.expire_at(key, now + 90_000, TtlOpt::NONE).await?, 1);

    info!("测试 2: NX/XX/GT/LT 分支返回码通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 3: 过去时间戳 expire_at 返回 2 且 key 被物理删除；不存在键优先返回 -2
#[test]
fn test_expire_past_deletes() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("past").await?;
    let key = b"past:key";
    session.upsert(key, b"v").await?;

    let now = now_ms();
    assert_eq!(
      session
        .expire_at(key, now.saturating_sub(1_000), TtlOpt::NONE)
        .await?,
      2
    );
    assert!(!session.contains_key(key).await?);
    assert_eq!(session.read(key).await?, None);

    // 物理删除：数据记录与 TTL 记录均已墓碑化
    assert_eq!(
      session.read_raw(&session.session_string_key(key)).await?,
      None
    );
    assert_eq!(session.read_raw(&session.ttl_key(key)).await?, None);

    // 重复对已删除键设过去时间戳：键存活判定优先返回 -2
    assert_eq!(
      session
        .expire_at(key, now.saturating_sub(1_000), TtlOpt::NONE)
        .await?,
      -2
    );

    info!("测试 3: 过去时间戳立即物理删除通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 4: PERSIST 恢复永不过期（返回码 1/0 分支），越过原过期点后仍存活
#[test]
fn test_persist() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("persist").await?;
    let key = b"persist:key";
    session.upsert(key, b"v").await?;

    // 未设 TTL：0
    assert_eq!(session.persist(key).await?, 0);
    assert_eq!(
      session.expire_at(key, now_ms() + 50, TtlOpt::NONE).await?,
      1
    );
    assert_eq!(session.persist(key).await?, 1);
    assert_eq!(session.pttl_ms(key).await?, -1);
    assert_eq!(session.expiretime_ms(key).await?, -1);
    // 再次移除：未设 TTL 返回 0
    assert_eq!(session.persist(key).await?, 0);

    // TTL 已移除：越过原过期点后仍存活
    sleep(Duration::from_millis(120)).await;
    assert_eq!(session.read(key).await?, Some(b"v".to_vec()));
    assert_eq!(session.persist(b"missing").await?, 0);

    info!("测试 4: PERSIST 恢复永不过期通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 5: DEL 清除 TTL 记录；RENAME 随键迁移 TTL（含短 TTL 到期与覆盖带 TTL 目标键）
#[test]
fn test_delete_and_rename_ttl_consistency() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("del_rename").await?;

    // DEL 同步清除 TTL 记录，重建同名键不受旧 TTL 影响
    let a = b"del:key";
    session.upsert(a, b"va").await?;
    assert_eq!(session.expire_at(a, now_ms() + 50, TtlOpt::NONE).await?, 1);
    assert!(session.delete(a).await?);
    assert_eq!(
      session.read_raw(&session.ttl_key(a)).await?,
      None,
      "DEL 必须同步清除 TTL 记录"
    );
    session.upsert(a, b"va2").await?;
    sleep(Duration::from_millis(120)).await;
    assert_eq!(session.read(a).await?, Some(b"va2".to_vec()));

    // RENAME：TTL 跟随迁移到新键，源键 TTL 记录清除
    let (src, dst) = (b"ren:src", b"ren:dst");
    session.upsert(src, b"vs").await?;
    assert_eq!(
      session
        .expire_at(src, now_ms() + 60_000, TtlOpt::NONE)
        .await?,
      1
    );
    assert_eq!(
      session.rename(src, dst, false).await?,
      RenameResult::Success
    );
    assert_eq!(session.read(src).await?, None);
    assert_eq!(session.read(dst).await?, Some(b"vs".to_vec()));
    assert!(session.pttl_ms(dst).await? > 0, "TTL 必须迁移到新键");
    assert_eq!(session.read_raw(&session.ttl_key(src)).await?, None);
    assert!(session.read_raw(&session.ttl_key(dst)).await?.is_some());

    // RENAME 短 TTL：到期后新键随迁消失
    let (s2, d2) = (b"ren:src2", b"ren:dst2");
    session.upsert(s2, b"v2").await?;
    assert_eq!(session.expire_at(s2, now_ms() + 50, TtlOpt::NONE).await?, 1);
    assert_eq!(session.rename(s2, d2, false).await?, RenameResult::Success);
    assert!(session.contains_key(d2).await?);
    sleep(Duration::from_millis(120)).await;
    assert_eq!(session.read(d2).await?, None, "迁移后的 TTL 必须在新键生效");

    // RENAME 覆盖带 TTL 的目标键：目标旧 TTL 清除，采用源键短 TTL
    let (s3, d3) = (b"ren:src3", b"ren:dst3");
    session.upsert(s3, b"v3").await?;
    session.upsert(d3, b"vd3").await?;
    assert_eq!(
      session
        .expire_at(d3, now_ms() + 60_000, TtlOpt::NONE)
        .await?,
      1
    );
    assert_eq!(session.expire_at(s3, now_ms() + 50, TtlOpt::NONE).await?, 1);
    assert_eq!(session.rename(s3, d3, false).await?, RenameResult::Success);
    sleep(Duration::from_millis(120)).await;
    assert_eq!(
      session.read(d3).await?,
      None,
      "覆盖后目标键必须采用源键 TTL，不得残留目标旧 TTL"
    );

    info!("测试 5: DEL/RENAME 的 TTL 记录一致性通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 6: SET（upsert）按 Redis 语义清除既有 TTL（异步路径 + 同步快速路径）
#[test]
fn test_set_clears_ttl() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("set_clear").await?;
    let key = b"set:clear";
    session.upsert(key, b"v1").await?;
    assert_eq!(
      session.expire_at(key, now_ms() + 50, TtlOpt::NONE).await?,
      1
    );

    // 异步 upsert（SET）清除既有 TTL
    session.upsert(key, b"v2").await?;
    assert_eq!(session.pttl_ms(key).await?, -1, "SET 必须清除既有 TTL");
    sleep(Duration::from_millis(120)).await;
    assert_eq!(session.read(key).await?, Some(b"v2".to_vec()));

    // 同步快速路径 try_upsert_sync（对标 Garnet NetworkSET）同样清除
    assert_eq!(
      session.expire_at(key, now_ms() + 50, TtlOpt::NONE).await?,
      1
    );
    assert!(session.try_upsert_sync(key, b"v3")?.is_ok());
    assert_eq!(session.pttl_ms(key).await?, -1);
    sleep(Duration::from_millis(120)).await;
    assert_eq!(session.read(key).await?, Some(b"v3".to_vec()));

    info!("测试 6: SET 清除既有 TTL 通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 7: ttl_key ↔ user_key_from_ttl_key 反解往返（含多字节变长前缀与超长键）
#[test]
fn test_ttl_key_roundtrip() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("roundtrip").await?;
    let user = b"user:1001:profile";

    // 默认前缀 (ns=0, db=0)：[0, 0, 0x09] + 用户键
    let k = session.ttl_key(user);
    let mut expect = vec![0u8, 0, KeyTag::Ttl as u8];
    expect.extend_from_slice(user);
    assert_eq!(k.as_slice(), &expect[..]);
    assert_eq!(
      StoreSession::<SegmentedDevice>::user_key_from_ttl_key(&k),
      Some((0, 0, user.as_slice()))
    );

    // 非 TTL 标签键安全反解为 None
    assert_eq!(
      StoreSession::<SegmentedDevice>::user_key_from_ttl_key(&session.session_meta_key(user)),
      None
    );
    assert_eq!(
      StoreSession::<SegmentedDevice>::user_key_from_ttl_key(&session.session_string_key(user)),
      None
    );

    // 多字节变长前缀 (ns=200, db=300) 往返，且与会话上下文构造一致
    let prefix = SessionPrefixBuf::new(200, 300);
    let (ns, db) = prefix.decode()?;
    let k2 = StoreSession::<SegmentedDevice>::ttl_key_with_prefix(prefix.as_slice(), user);
    assert_eq!(
      StoreSession::<SegmentedDevice>::user_key_from_ttl_key(&k2),
      Some((ns, db, user.as_slice()))
    );

    // 超长用户键（> 62B 栈容量回退堆）往返
    let long = vec![b'k'; 100];
    let k3 = session.ttl_key(&long);
    assert_eq!(
      StoreSession::<SegmentedDevice>::user_key_from_ttl_key(&k3),
      Some((0, 0, long.as_slice()))
    );

    // NamespaceDbCodec 通用解码口径一致
    let (_, _, tag, payload) = NamespaceDbCodec::decode_tagged_key(&k)?;
    assert_eq!(tag, KeyTag::Ttl);
    assert_eq!(payload, user);

    info!("测试 7: ttl_key 反解往返通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 8: 集合键惰性过期经 load_meta 守卫——读视同不存在，写按不存在重建（Redis 语义）
#[test]
fn test_collection_lazy_expiry() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("collection").await?;
    let key = b"hash:ttl";
    session.hset(key, b"f1".to_vec(), b"v1".to_vec()).await?;
    assert_eq!(
      session.expire_at(key, now_ms() + 50, TtlOpt::NONE).await?,
      1,
      "集合键同样适用 key 级 TTL"
    );
    assert_eq!(session.type_of(key).await?, "hash");

    sleep(Duration::from_millis(120)).await;
    // 读路径：过期集合视同不存在
    assert_eq!(session.hget(key, b"f1").await?, None);
    assert_eq!(session.type_of(key).await?, "none");
    assert!(!session.contains_key(key).await?);
    assert_eq!(session.pttl_ms(key).await?, -2);

    // 写路径：对已过期集合按不存在重建
    assert!(session.hset(key, b"f2".to_vec(), b"v2").await?);
    assert_eq!(session.hget(key, b"f2").await?, Some(b"v2".to_vec()));
    assert_eq!(session.pttl_ms(key).await?, -1, "重建后不得残留旧 TTL");

    info!("测试 8: 集合键惰性过期与重建通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 9: read_with 的 TTL 裁决必须先于读闭包执行——过期键绝不触碰闭包
/// （读闭包直写响应缓冲场景的双写回归）
#[test]
fn test_read_with_adjudicates_ttl_before_closure() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("read_with").await?;
    let key = b"rw:key";
    session.upsert(key, b"v").await?;
    assert_eq!(
      session.expire_at(key, now_ms() + 50, TtlOpt::NONE).await?,
      1
    );

    // 未到期：闭包恰执行一次
    let mut invocations = 0usize;
    let hit = session
      .read_with(key, |v| {
        invocations += 1;
        v.to_vec()
      })
      .await?;
    assert_eq!(hit, Some(b"v".to_vec()));
    assert_eq!(invocations, 1);

    sleep(Duration::from_millis(120)).await;
    // 已过期：裁决前移，闭包零执行（若闭包直写缓冲，此处绝不产生双写）
    let mut invocations = 0usize;
    let hit = session
      .read_with(key, |v| {
        invocations += 1;
        v.to_vec()
      })
      .await?;
    assert_eq!(hit, None);
    assert_eq!(invocations, 0, "过期键的读闭包不得执行（裁决必须前移）");

    info!("测试 9: read_with TTL 裁决先于闭包执行通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 10: mget_each 延迟裁决（TTL 记录落盘触发 Deferred）不得破坏按请求序回调契约
/// （MGET 线上协议按回调序对位写响应，乱序即错位响应的回归）
#[test]
fn test_mget_each_deferred_ttl_ordering() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("mget_order").await?;
    // 冷键先写入低位地址：数据与 TTL 记录都将滑出内存头
    let cold = b"ord:cold";
    session.upsert(cold, b"cold_val").await?;
    assert_eq!(
      session
        .expire_at(cold, now_ms() + 60_000, TtlOpt::NONE)
        .await?,
      1
    );
    // 写入超过环形缓冲容量（16 页 × 64KB = 1MB）强制换页驱逐，head 推过冷键
    let bulk = vec![b'x'; 1024];
    for i in 0..1600u32 {
      session
        .upsert(format!("ord:bulk:{i}").as_bytes(), &bulk)
        .await?;
    }
    // 前置确认：冷键 TTL 记录已落盘（内存探针返回磁盘候选 → Ok(None)），
    // 保证 Deferred 延迟裁决路径被真实触发
    let probe = session.try_read_raw_in_memory(&session.ttl_key(cold), |v| {
      <[u8; 8]>::try_from(v).map(u64::from_be_bytes)
    })?;
    assert!(
      probe.is_none(),
      "前置条件失败：冷键 TTL 记录应已落盘（内存探针应返回磁盘候选）"
    );

    // 热键纯内存驻留；请求序 [冷(TTL 落盘), 热(内存)]，回调必须严格按此序
    let hot = b"ord:hot";
    session.upsert(hot, b"hot_val").await?;
    let mut delivered: Vec<Vec<u8>> = Vec::new();
    session
      .mget_each(&[cold.as_slice(), hot.as_slice()], |v| {
        delivered.push(v.unwrap_or_default().to_vec());
      })
      .await?;
    assert_eq!(
      delivered,
      [b"cold_val".to_vec(), b"hot_val".to_vec()],
      "Deferred 裁决键不得插队破坏请求序"
    );

    info!("测试 10: mget_each 延迟裁决保序通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}

/// 测试 11: 原位 RMW 命令对已过期键视同不存在——不得复活过期值，且不残留旧 TTL
#[test]
fn test_rmw_commands_treat_expired_as_missing() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, session) = open("rmw").await?;

    // INCRBY 过期键：视同 0 基准，结果为增量本身
    let incr_key = b"rmw:incr";
    session.upsert(incr_key, b"100").await?;
    assert_eq!(
      session
        .expire_at(incr_key, now_ms() + 50, TtlOpt::NONE)
        .await?,
      1
    );
    sleep(Duration::from_millis(120)).await;
    assert_eq!(session.incrby(incr_key, 5).await?, 5);
    assert_eq!(session.read(incr_key).await?, Some(b"5".to_vec()));
    assert_eq!(session.pttl_ms(incr_key).await?, -1, "重建后不得残留旧 TTL");

    // SETBIT 过期键：old_bit 视同 0，按空串重建
    let bit_key = b"rmw:bit";
    session.upsert(bit_key, &[0u8]).await?;
    assert_eq!(
      session
        .expire_at(bit_key, now_ms() + 50, TtlOpt::NONE)
        .await?,
      1
    );
    sleep(Duration::from_millis(120)).await;
    assert_eq!(session.setbit(bit_key, 3, 1).await?, 0);
    assert_eq!(session.read(bit_key).await?, Some(vec![0b0001_0000u8]));

    info!("测试 11: 原位 RMW 过期视同不存在通过");
    aok::Result::<()>::Ok(())
  })?;
  OK
}
