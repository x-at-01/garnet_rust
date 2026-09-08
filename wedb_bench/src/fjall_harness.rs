use std::{hint::black_box, path::PathBuf, sync::Arc};

use fjall::{Database, Keyspace, KeyspaceCreateOptions, PersistMode};
use tempfile::{TempDir, tempdir};

use crate::{
  driver::{
    BenchEngine, RangeEngine, bench_delete_batch, bench_mixed_ops, bench_range_queries,
    bench_read_batch, bench_upsert_batch,
  },
  error::Result,
  stats::{BenchStats, MixedOp, get_path_size},
  suite::{DEFAULT_LSM_CACHE_BYTES, DEFAULT_LSM_MEMTABLE_BYTES},
};

pub(crate) const ENGINE_NAME: &str = "Fjall";
const KEYSPACE_NAME: &str = "bench_kv";

/// Fjall (LSM-Tree 存储引擎) 评测包装句柄
pub struct FjallHarness {
  pub keyspace: Keyspace,
  pub db: Arc<Database>,
  pub dir: TempDir,
  pub db_dir: PathBuf,
  pub cache_size_bytes: u64,
  pub memtable_size_bytes: u64,
}

impl FjallHarness {
  /// 创建新的 Fjall 评测实例（支持严格对齐的内存预算）
  pub fn new_with_budget(cache_size_bytes: u64, memtable_size_bytes: u64) -> Result<Self> {
    let dir = tempdir()?;
    let db_dir = dir.path().to_path_buf();
    let db = Arc::new(
      Database::builder(&db_dir)
        .cache_size(cache_size_bytes)
        .open()?,
    );
    let keyspace = db.keyspace(KEYSPACE_NAME, move || {
      KeyspaceCreateOptions::default().max_memtable_size(memtable_size_bytes)
    })?;

    Ok(Self {
      keyspace,
      db,
      dir,
      db_dir,
      cache_size_bytes,
      memtable_size_bytes,
    })
  }

  /// 创建默认配置的 Fjall 评测实例 (默认对齐物理内存预算: Block Cache + Memtable)
  pub fn new() -> Result<Self> {
    Self::new_with_budget(DEFAULT_LSM_CACHE_BYTES, DEFAULT_LSM_MEMTABLE_BYTES)
  }

  /// 批量写入评测
  pub fn bench_upsert_batch<K: AsRef<[u8]>, V: AsRef<[u8]>>(
    &self,
    name: &str,
    pairs: &[(K, V)],
  ) -> Result<BenchStats> {
    bench_upsert_batch(self, name, pairs)
  }

  /// 批量读取评测
  pub fn bench_read_batch<K: AsRef<[u8]>>(&self, name: &str, keys: &[K]) -> Result<BenchStats> {
    bench_read_batch(self, name, keys)
  }

  /// 批量删除评测
  pub fn bench_delete_batch<K: AsRef<[u8]>>(&self, name: &str, keys: &[K]) -> Result<BenchStats> {
    bench_delete_batch(self, name, keys)
  }

  /// 有序范围切片查询评测（LSM-Tree SSTable 范围迭代器获取 [start_k..=end_k] 记录）
  pub fn bench_range_queries<K: AsRef<[u8]>>(
    &self,
    name: &str,
    ranges: &[(K, K)],
  ) -> Result<BenchStats> {
    bench_range_queries(self, name, ranges)
  }

  /// 混合读写负载评测（对齐 YCSB Task A / Task B / Task D 标准混合任务）
  pub fn bench_mixed_ops<K: AsRef<[u8]>, V: AsRef<[u8]>>(
    &self,
    name: &str,
    ops: &[MixedOp<K, V>],
  ) -> Result<BenchStats> {
    bench_mixed_ops(self, name, ops)
  }

  /// 刷盘同步
  pub fn flush_all(&self) -> Result<()> {
    self.db.persist(PersistMode::SyncAll)?;
    Ok(())
  }

  /// 获取数据库目录实际磁盘占用大小
  pub fn disk_usage(&self) -> u64 {
    get_path_size(&self.db_dir)
  }

  /// 获取数据库常驻内存占用大小（基于配置的物理预算）
  pub fn memory_usage(&self) -> u64 {
    self.cache_size_bytes + self.memtable_size_bytes
  }
}

impl BenchEngine for FjallHarness {
  #[inline]
  fn name(&self) -> &'static str {
    ENGINE_NAME
  }

  #[inline]
  fn disk(&self) -> u64 {
    self.disk_usage()
  }

  #[inline]
  fn mem(&self) -> u64 {
    self.memory_usage()
  }

  #[inline]
  fn put(&self, k: &[u8], v: &[u8]) -> Result<()> {
    self.keyspace.insert(k, v)?;
    Ok(())
  }

  #[inline]
  fn get_len(&self, k: &[u8]) -> Result<usize> {
    let val = self.keyspace.get(k)?;
    let len = val.as_deref().map_or(0, <[u8]>::len);
    black_box(val);
    Ok(len)
  }

  #[inline]
  fn del(&self, k: &[u8]) -> Result<()> {
    self.keyspace.remove(k)?;
    Ok(())
  }
}

impl RangeEngine for FjallHarness {
  fn scan(&self, start: &[u8], end: &[u8]) -> Result<u64> {
    let mut q_bytes = 0u64;
    for item in self.keyspace.range(start..=end) {
      let (k, v) = item.into_inner()?;
      q_bytes += (k.len() + v.len()) as u64;
      black_box((k, v));
    }
    Ok(q_bytes)
  }
}
