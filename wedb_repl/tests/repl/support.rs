use std::sync::Arc;

use tempfile::tempdir;
use wdev::SegmentedDevice;
use wkv::{StoreConfig, WedbStore};

/// 初始化轻量级测试存储引擎夹具
pub fn init_test_store() -> aok::Result<(Arc<WedbStore<SegmentedDevice>>, tempfile::TempDir)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("test_store.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let config = StoreConfig::new(1024, 64 * 1024, 16, 0.5)?;
  let store = Arc::new(WedbStore::open(config, device)?);
  Ok((store, dir))
}

/// 对标 C# ClusterReplicationDisklessSyncTests.MakeHashFieldBytes:
/// 确定性原地填充大对象哈希字段内容，无需额外堆分配
#[inline]
pub fn fill_hash_field_bytes(buf: &mut [u8], field_index: usize) {
  let seed = ((field_index * 131 + 7) % 256) as u8;
  for (j, b) in buf.iter_mut().enumerate() {
    *b = seed.wrapping_add(j as u8);
  }
}

/// 对标 C# ClusterReplicationDisklessSyncTests.MakeHashFieldBytes:
/// 确定性生成大对象哈希字段内容，无需在内存中维护全量字典
pub fn make_hash_field_bytes(field_index: usize, size: usize) -> Vec<u8> {
  let mut v = vec![0u8; size];
  fill_hash_field_bytes(&mut v, field_index);
  v
}
