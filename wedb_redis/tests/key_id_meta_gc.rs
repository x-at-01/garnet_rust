//! key_id_versions 元数据紧缩回收测试（紧缩收尾安全回收已死集合条目）
//!
//! 覆盖场景：
//! 1. Flattened 集合 Fast Drop 幽灵 meta（死亡形态 a：size=0 非墓碑改写）落入紧缩区间
//!    → 死条目在收尾时被回收（Lookup 与 Scan 双模式）；
//! 2. 存活集合的条目紧缩后原样保留；非 Flattened 删除路径的 Meta 键墓碑（死亡形态 b）
//!    同样回收；
//! 3. 死亡记录在紧缩范围之外（until_address 小于幽灵 meta 地址）→ 条目保守保留，
//!    二次全量紧缩越过死亡记录后才回收。

use std::sync::Arc;

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;
use wcompact::{CompactionType, LogCompactor};
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use wkv::{StoreConfig, StoreSession, WedbStore};
use wrecord::StorageEncoding;

/// 本地测试存储构造（对齐 compact 支撑模块的默认参数：4096 桶 / 64KB 页 / 16 页 / 0.5 可变比）
fn create_test_store(
  db_name: &str,
) -> aok::Result<(
  tempfile::TempDir,
  Arc<WedbStore<SegmentedDevice>>,
  StoreSession<SegmentedDevice>,
)> {
  let dir = tempfile::tempdir()?;
  let db_path = dir.path().join(db_name);
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let config = StoreConfig::new(4096, 64 * 1024, 16, 0.5)?;
  let store = Arc::new(WedbStore::open(config, device)?);
  let session = store.new_session()?;
  Ok((dir, store, session))
}

/// 大值长度（超过 HASH_MAX_COMPACT_VALUE = 64B，触发 Flattened 直建/晋升）
const BIG_VAL_LEN: usize = 256;
/// 填充记录条数（把死亡记录推进紧缩区间内部并撑起只读区）
const FILLER_COUNT: u32 = 64;

/// 场景 1：Flattened 集合 Fast Drop 后紧缩，key_id_versions 死条目必须被回收（双模式）
#[test]
fn key_id_meta_gc_after_fast_drop_compaction() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    for comp_type in [CompactionType::Lookup, CompactionType::Scan] {
      let (_dir, store, session) = create_test_store("key_id_gc_fast_drop.db")?;

      let key = b"hash:gc:fast_drop";
      let big_val = vec![b'v'; BIG_VAL_LEN];

      // 大值首写直接以 Flattened 编码创建集合
      assert!(session.hset(key, b"f1", &big_val).await?);
      assert!(session.hset(key, b"f2", &big_val).await?);

      // 记录集合 key_id（load_meta 内部会同步登记 key_id_versions）
      let meta = session.load_meta(key).await?.expect("集合元数据必须存在");
      assert_eq!(
        meta.encoding(),
        StorageEncoding::Flattened,
        "大值必须触发打平存储"
      );
      let key_id = meta.key_id;
      assert_eq!(
        store.get_key_id_meta(key_id),
        Some((meta.version, true)),
        "存活集合的 key_id_versions 条目必须存在"
      );

      // Fast Drop 删除：bump_version + size=0 幽灵 meta 重写 + update_key_id_meta(false)
      assert!(session.delete(key).await?);
      let dead_ver = meta.version + 1;
      assert_eq!(
        store.get_key_id_meta(key_id),
        Some((dead_ver, false)),
        "删除后条目必须标记为死"
      );

      // 追加填充数据，把幽灵 meta 推入紧缩区间内部
      let filler = vec![b'p'; BIG_VAL_LEN];
      for i in 0..FILLER_COUNT {
        let k = format!("filler:{i:03}");
        session.upsert_raw(k.as_bytes(), &filler).await?;
      }

      store.flush_and_evict_all().await?;
      let compact_until = store.tail_address();

      let compactor = LogCompactor::new(Arc::clone(&store));
      let stats = compactor.compact(compact_until, comp_type).await?;
      assert!(stats.scanned_records > 0);

      // 死亡记录（幽灵 meta）已完整落入紧缩区间：死条目必须在收尾时被回收
      assert_eq!(
        store.get_key_id_meta(key_id),
        None,
        "已死集合的 key_id_versions 条目紧缩后必须被回收"
      );
      info!("Flattened Fast Drop 死条目回收验证通过: {comp_type:?}");
    }
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 场景 2：存活集合条目紧缩后原样保留，Compact 编码集合删除留下的 Meta 键墓碑（死亡形态 b）被回收
#[test]
fn key_id_meta_gc_live_entry_kept_and_tombstone_form_collected() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store, session) = create_test_store("key_id_gc_mixed.db")?;

    // 存活集合：小值保持 Compact 编码
    let live_key = b"hash:gc:live";
    assert!(session.hset(live_key, b"f1", b"small_v1").await?);
    let live_meta = session
      .load_meta(live_key)
      .await?
      .expect("存活集合元数据必须存在");
    let live_id = live_meta.key_id;

    // 死亡集合：Compact 编码删除走非 Flattened 路径，留下 Meta 物理键墓碑（死亡形态 b）
    let dead_key = b"hash:gc:tombstoned";
    assert!(session.hset(dead_key, b"f1", b"small_v1").await?);
    let dead_meta = session
      .load_meta(dead_key)
      .await?
      .expect("死亡集合元数据必须存在");
    assert_eq!(dead_meta.encoding(), StorageEncoding::Compact);
    let dead_id = dead_meta.key_id;
    assert_ne!(live_id, dead_id, "两个集合必须分配不同 key_id");
    assert!(session.delete(dead_key).await?);
    assert_eq!(
      store.get_key_id_meta(dead_id),
      Some((dead_meta.version, false)),
      "删除后条目必须标记为死"
    );

    // 填充数据推进只读区，把 Meta 墓碑包进紧缩区间
    let filler = vec![b'p'; BIG_VAL_LEN];
    for i in 0..FILLER_COUNT {
      let k = format!("filler:{i:03}");
      session.upsert_raw(k.as_bytes(), &filler).await?;
    }

    store.flush_and_evict_all().await?;
    let compact_until = store.tail_address();

    let compactor = LogCompactor::new(Arc::clone(&store));
    compactor
      .compact(compact_until, CompactionType::Lookup)
      .await?;

    // Meta 键墓碑死亡形态条目回收；存活集合条目原样保留
    assert_eq!(
      store.get_key_id_meta(dead_id),
      None,
      "Meta 墓碑死亡形态条目必须被回收"
    );
    assert_eq!(
      store.get_key_id_meta(live_id),
      Some((live_meta.version, true)),
      "存活集合的条目紧缩后必须原样保留"
    );

    // 存活集合数据在紧缩后完整可读
    assert_eq!(
      session.hget(live_key, b"f1").await?,
      Some(b"small_v1".to_vec()),
      "存活集合字段紧缩后必须可读"
    );

    info!("存活条目保留与 Meta 墓碑死条目回收验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 场景 3：死亡记录在紧缩范围之外（until_address 小于幽灵 meta 地址）→ 条目保守保留，二次全量紧缩后才回收
///
/// 注：先推进只读区再删除——可变区内同键同长写入会原位覆写（幽灵 meta 会顶替 v1 meta
/// 的物理地址），只有只读区的幽灵 meta 才会追加到日志尾部，从而落在截断点之外。
#[test]
fn key_id_meta_gc_death_outside_compaction_scope_kept() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store, session) = create_test_store("key_id_gc_outside_scope.db")?;

    let key = b"hash:gc:outside";
    let big_val = vec![b'v'; BIG_VAL_LEN];
    assert!(session.hset(key, b"f1", &big_val).await?);
    let meta = session.load_meta(key).await?.expect("集合元数据必须存在");
    let key_id = meta.key_id;

    // 分界填充记录 + 推进只读区：此后 Fast Drop 的幽灵 meta 只能追加在日志尾部
    session.upsert_raw(b"marker", b"m").await?;
    store.flush_and_evict_all().await?;
    let compact_until = store.tail_address();

    // Fast Drop 删除：幽灵 meta 地址 >= compact_until（紧缩范围之外）
    assert!(session.delete(key).await?);
    let dead_ver = meta.version + 1;
    assert_eq!(store.get_key_id_meta(key_id), Some((dead_ver, false)));
    assert!(
      store.tail_address() >= compact_until,
      "幽灵 meta 必须追加在截断点之后"
    );

    // 填充推进只读区，保证紧缩合法执行
    let filler = vec![b'p'; BIG_VAL_LEN];
    for i in 0..FILLER_COUNT {
      let k = format!("pad:{i:03}");
      session.upsert_raw(k.as_bytes(), &filler).await?;
    }

    store.flush_and_evict_all().await?;

    let compactor = LogCompactor::new(Arc::clone(&store));
    let stats = compactor
      .compact(compact_until, CompactionType::Lookup)
      .await?;
    assert!(stats.scanned_records > 0);

    // 死亡记录在紧缩范围之外：条目必须保守保留（宁可泄漏，绝不冒险）
    assert_eq!(
      store.get_key_id_meta(key_id),
      Some((dead_ver, false)),
      "死亡记录未落入紧缩区间时条目必须保守保留"
    );

    // 二次全量紧缩越过幽灵 meta：条目此时才被回收
    // （第一轮紧缩的存活迁移已推进尾部，先排空只读区再取截断点）
    store.flush_and_evict_all().await?;
    let full_until = store.tail_address();
    compactor.compact(full_until, CompactionType::Scan).await?;
    assert_eq!(
      store.get_key_id_meta(key_id),
      None,
      "全量紧缩越过死亡记录后条目必须被回收"
    );

    info!("紧缩范围外死条目保守保留与二次回收验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
