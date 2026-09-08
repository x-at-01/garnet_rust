//! 原位读-改-写 (InPlace RMW) 集成测试：INCRBY/SETBIT/SETRANGE 原位与 RCU 扩容路径

use std::sync::Arc;

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use whlog::SECTOR_ALIGNMENT;
use wkv::{StoreConfig, WedbStore};

#[test]
fn test_inplace_rmw_operations() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("store_test7_rmw.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, SECTOR_ALIGNMENT, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    // 1. INCRBY 原位同长度自增（地址不发生变动，0 堆分配追加）
    let cnt_key = b"counter:rmw";
    let addr_init = session.upsert(cnt_key, b"10").await?;
    let val1 = session.incrby(cnt_key, 1).await?;
    assert_eq!(val1, 11);
    let addr_after = session
      .store
      .index
      .find_tag(&session.session_string_key(cnt_key));
    assert_eq!(
      Some(addr_init),
      addr_after,
      "同长度自增必须原位完成，物理地址保持不变"
    );
    assert_eq!(session.read_string(cnt_key).await?, Some(b"11".to_vec()));

    // 2. INCRBY 长度变更自增（11 + 89 = 100，长度从 2 变 3，触发安全 RCU 扩容）
    let val2 = session.incrby(cnt_key, 89).await?;
    assert_eq!(val2, 100);
    let addr_expanded = session
      .store
      .index
      .find_tag(&session.session_string_key(cnt_key));
    assert_ne!(
      Some(addr_init),
      addr_expanded,
      "长度变大时必须安全降级分配新版本记录"
    );
    assert_eq!(session.read_string(cnt_key).await?, Some(b"100".to_vec()));

    // 3. SETBIT 原位翻转验证
    let bm_key = b"bitmap:rmw";
    let bm_addr_init = session.upsert(bm_key, &[0b0000_0000]).await?;
    let old_bit = session.setbit(bm_key, 7, 1).await?;
    assert_eq!(old_bit, 0);
    assert_eq!(
      session
        .store
        .index
        .find_tag(&session.session_string_key(bm_key)),
      Some(bm_addr_init),
      "在现有字节长度内的 SETBIT 必须 100% 原位完成"
    );
    assert_eq!(session.getbit(bm_key, 7).await?, 1);

    // 4. SETRANGE 原位覆写验证
    let str_key = b"string:setrange";
    let str_addr_init = session.upsert(str_key, b"hello world").await?;
    let len = session.setrange(str_key, 6, b"redis").await?;
    assert_eq!(len, 11);
    assert_eq!(
      session
        .store
        .index
        .find_tag(&session.session_string_key(str_key)),
      Some(str_addr_init),
      "在现有长度内的 SETRANGE 必须原位完成"
    );
    assert_eq!(
      session.read_string(str_key).await?,
      Some(b"hello redis".to_vec())
    );

    info!("测试 7: 原位读-改-写 (InPlace RMW) 核心算术与位图操作验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
