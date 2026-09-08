use std::{
  hint::black_box,
  path::PathBuf,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
};

use rocksdb::{BlockBasedOptions, Cache, DB, Options};
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

pub(crate) const ENGINE_NAME: &str = "RocksDB";

/// RocksDB (Facebook 工业级 C++ LSM-Tree 存储引擎) 评测包装句柄
pub struct RocksDbHarness {
  pub db: Arc<DB>,
  pub cache: Cache,
  pub dir: TempDir,
  pub db_dir: PathBuf,
  pub cache_size_bytes: u64,
  pub memtable_size_bytes: u64,
  /// 运行期动态内存峰值采样
  pub peak_memory: AtomicU64,
}

impl RocksDbHarness {
  /// 创建指定物理内存预算的 RocksDB 评测实例
  pub fn new_with_budget(cache_size_bytes: u64, memtable_size_bytes: u64) -> Result<Self> {
    let dir = tempdir()?;
    let db_dir = dir.path().to_path_buf();

    let mut opts = Options::default();
    opts.create_if_missing(true);
    opts.set_write_buffer_size(memtable_size_bytes as usize);

    let mut block_opts = BlockBasedOptions::default();
    let cache = Cache::new_lru_cache(cache_size_bytes as usize);
    block_opts.set_block_cache(&cache);
    opts.set_block_based_table_factory(&block_opts);

    let db = Arc::new(DB::open(&opts, &db_dir)?);

    Ok(Self {
      db,
      cache,
      dir,
      db_dir,
      cache_size_bytes,
      memtable_size_bytes,
      peak_memory: AtomicU64::new(0),
    })
  }

  /// 创建默认配置的 RocksDB 实例 (默认对齐物理内存预算: Block Cache + Memtable)
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

  /// 有序范围扫描评测（raw_iterator 零拷贝扫描 [start_k..=end_k]）
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
    self.db.flush()?;
    Ok(())
  }

  /// 获取当前数据库目录磁盘占用大小
  pub fn disk_usage(&self) -> u64 {
    get_path_size(&self.db_dir)
  }

  /// 获取当前实时动态活跃内存（MemTable + Block Cache + Table Readers）
  fn dynamic_memory_usage(&self) -> u64 {
    let memtable = self
      .db
      .property_int_value("rocksdb.cur-size-all-mem-tables")
      .ok()
      .flatten()
      .unwrap_or(0);
    let block_cache = self
      .db
      .property_int_value("rocksdb.block-cache-usage")
      .ok()
      .flatten()
      .unwrap_or(0);
    let table_readers = self
      .db
      .property_int_value("rocksdb.estimate-table-readers-mem")
      .ok()
      .flatten()
      .unwrap_or(0);
    memtable + block_cache + table_readers
  }

  /// 采样并记录运行期动态内存峰值
  #[inline]
  fn record_memory_sample(&self) -> u64 {
    let current = self.dynamic_memory_usage();
    self.peak_memory.fetch_max(current, Ordering::Relaxed);
    current
  }

  /// 获取当前数据库常驻工作集内存占用大小
  pub fn memory_usage(&self) -> u64 {
    let dynamic = self.record_memory_sample();
    let peak = self.peak_memory.load(Ordering::Relaxed);
    let max_observed = dynamic.max(peak);
    let budget_total = self.cache_size_bytes + self.memtable_size_bytes;
    // 若实际动态使用量超出了配置预算（如高并发下写缓冲或未释放迭代器膨胀），反映真实超出的物理内存开销；
    // 否则反映为其配置分配的常驻工作集预算（与 WeDB、BfTree、Fjall 统一口径，避免 flush 后写缓冲清零仅剩 40KB 元数据造成虚假读数）
    if max_observed > budget_total {
      max_observed
    } else {
      budget_total
    }
  }
}

impl BenchEngine for RocksDbHarness {
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
    self.db.put(k, v)?;
    Ok(())
  }

  #[inline]
  fn get_len(&self, k: &[u8]) -> Result<usize> {
    let val = self.db.get_pinned(k)?;
    let len = val.as_deref().map_or(0, <[u8]>::len);
    black_box(val);
    Ok(len)
  }

  #[inline]
  fn del(&self, k: &[u8]) -> Result<()> {
    self.db.delete(k)?;
    Ok(())
  }
}

impl RangeEngine for RocksDbHarness {
  fn scan(&self, start: &[u8], end: &[u8]) -> Result<u64> {
    let mut iter = self.db.raw_iterator();
    iter.seek(start);
    let mut q_bytes = 0u64;
    while iter.valid() {
      let Some(k) = iter.key() else {
        break;
      };
      if k > end {
        break;
      }
      let v = iter.value().unwrap_or_default();
      q_bytes += (k.len() + v.len()) as u64;
      black_box((k, v));
      iter.next();
    }
    iter.status()?;
    Ok(q_bytes)
  }
}
