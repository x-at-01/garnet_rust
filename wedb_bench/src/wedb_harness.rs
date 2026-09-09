use std::{
  hint::black_box,
  path::PathBuf,
  sync::Arc,
  time::{Duration, Instant},
};

use compio::runtime::Runtime;
use tempfile::{TempDir, tempdir};
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use wedb_zset::ZAddOpt;
use whlog::{DEFAULT_MUTABLE_FRACTION, DEFAULT_NUM_PAGES, DEFAULT_PAGE_SIZE};
use wkv::{DEFAULT_INDEX_SIZE, StoreConfig, StoreSession, WedbStore};

use crate::{
  driver::{
    BatchRun, BenchEngine, RangeEngine, bench_delete_batch, bench_mixed_ops, bench_range_queries,
    bench_read_batch, bench_upsert_batch,
  },
  error::{Error, Result},
  stats::{BenchStats, MixedOp, OpTimer, get_path_size},
  suite::{DEFAULT_MEMORY_BUDGET_MB, MB_BYTES},
};

pub(crate) const ENGINE_NAME: &str = "WeDB";
const DB_FILE_NAME: &str = "wedb_bench.db";
/// 评测专用 ReadCache 页数（256 页 × 64KB = 16MB，约占 256MB 内存预算的 6%）
///
/// 对标依据：C# Garnet 读多写少负载的 ReadCache / CopyFromImmutable 读侧缓存语义——
/// BfTree 自带 LRU 页缓存、RocksDB 有 Block Cache，WeDB 若不开 ReadCache 会使 Zipf
/// 热点读每次穿盘冷读，读侧口径失真。16MB ≈ 16k 条 1KB 记录，可覆盖 Zipf(0.99)
/// 热点集约 70% 的访问概率质量。
/// 刻意不开 copy_reads_to_tail：追加写会改变读负载性质（读尾写放大），只开纯读缓存
/// 语义的 ReadCache（命中回填、环形淘汰），评测口径最干净。
const BENCH_READ_CACHE_NUM_PAGES: usize = 256;
/// 点读流水线分块计时窗口（对齐 wedb_store BATCH_READ_PREFETCH_SIZE = 12 路硬件预取）
const READ_PIPELINE_CHUNK: usize = 12;

/// WeDB (Microsoft Garnet Tsavorite 架构) 评测包装句柄
pub struct WedbHarness {
  pub session: StoreSession<SegmentedDevice>,
  pub store: Arc<WedbStore<SegmentedDevice>>,
  pub dir: TempDir,
  pub db_file: PathBuf,
  pub rt: Runtime,
}

impl WedbHarness {
  /// 基于指定配置创建 WeDB 评测实例
  pub fn new_with_config(config: StoreConfig) -> Result<Self> {
    let dir = tempdir()?;
    let db_file = dir.path().join(DB_FILE_NAME);
    let device = Arc::new(SegmentedDevice::single_file(&db_file)?);

    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;
    let rt = Runtime::new()?;

    Ok(Self {
      session,
      store,
      dir,
      db_file,
      rt,
    })
  }

  /// 创建新的 WeDB 评测实例
  pub fn new(index_size: usize, page_size: usize, num_pages: usize) -> Result<Self> {
    let config = StoreConfig::new(index_size, page_size, num_pages, DEFAULT_MUTABLE_FRACTION)?;
    Self::new_with_config(config)
  }

  /// 基于指定物理内存预算 (MB) 与预估记录数创建 WeDB 评测实例 (严格对标 C# Garnet 官方架构)
  pub fn new_with_budget(memory_budget_mb: usize, max_records: usize) -> Result<Self> {
    let page_size = DEFAULT_PAGE_SIZE;
    let memory_bytes = memory_budget_mb * MB_BYTES;
    // 严格对标 Garnet LogMemorySize 计算: pageCount = LogMemorySize / PageSize (杜绝人为乘2放大)
    let num_pages = (memory_bytes / page_size)
      .next_power_of_two()
      .max(DEFAULT_NUM_PAGES);
    // 严格对标 Garnet 官方标准: 每个 64B Cacheline 哈希桶承载 4 个 distinct keys (indexCacheLines * 4L)
    let index_size = (max_records / 4)
      .next_power_of_two()
      .max(DEFAULT_INDEX_SIZE);
    let config = StoreConfig::new(index_size, page_size, num_pages, DEFAULT_MUTABLE_FRACTION)?
      // 启用 ReadCache 并提升缓存窗口至 16MB，对齐 BfTree 页缓存 / RocksDB Block Cache 的读侧口径
      .with_read_cache(true)
      .with_read_cache_pages(BENCH_READ_CACHE_NUM_PAGES)?;
    Self::new_with_config(config)
  }

  /// 基于默认物理内存预算与预估记录数创建 WeDB 评测实例
  pub fn default_budget(max_records: usize) -> Result<Self> {
    Self::new_with_budget(DEFAULT_MEMORY_BUDGET_MB, max_records)
  }

  /// 批量写入评测（单 block_on 包裹整批，逐操作独立计时）
  pub fn bench_upsert_batch<K: AsRef<[u8]>, V: AsRef<[u8]>>(
    &self,
    name: &str,
    pairs: &[(K, V)],
  ) -> Result<BenchStats> {
    bench_upsert_batch(self, name, pairs)
  }

  /// 批量读取评测（12 路流水线批量读，按分块均摊延迟）
  pub fn bench_read_batch<K: AsRef<[u8]>>(&self, name: &str, keys: &[K]) -> Result<BenchStats> {
    bench_read_batch(self, name, keys)
  }

  /// 范围检索评测（底层 KV 有序范围扫描，与其他三引擎统一口径）
  pub fn bench_range_queries<K: AsRef<[u8]>>(
    &self,
    name: &str,
    ranges: &[(K, K)],
  ) -> Result<BenchStats> {
    bench_range_queries(self, name, ranges)
  }

  /// 底层原始 KV 批量直写（直通内置 BfTree 组件，不经会话前缀/元数据编码）
  ///
  /// 供第 9 项统一纯 KV scan 口径的数据集初始化使用：写入键即扫描键
  pub fn kv_put_raw_batch(&self, pairs: &[(&[u8], &[u8])]) -> Result<()> {
    for (k, v) in pairs {
      let res = self.store.bftree().insert(k.as_ref(), v.as_ref());
      if res != wbftree::BfTreeInsertResult::Success {
        return Err(Error::BfTreeInsert(res));
      }
    }
    Ok(())
  }

  /// 批量删除评测
  pub fn bench_delete_batch<K: AsRef<[u8]>>(&self, name: &str, keys: &[K]) -> Result<BenchStats> {
    bench_delete_batch(self, name, keys)
  }

  /// 混合读写负载评测（对齐 YCSB Task A / Task B / Task D 标准混合任务）
  pub fn bench_mixed_ops<K: AsRef<[u8]>, V: AsRef<[u8]>>(
    &self,
    name: &str,
    ops: &[MixedOp<K, V>],
  ) -> Result<BenchStats> {
    bench_mixed_ops(self, name, ops)
  }

  /// 异步刷盘
  pub fn flush_all(&self) -> Result<()> {
    self.rt.block_on(async {
      self.store.flush_all().await?;
      Ok::<(), Error>(())
    })
  }

  /// 获取当前数据库文件磁盘占用大小
  pub fn disk_usage(&self) -> u64 {
    get_path_size(self.dir.path())
  }

  /// 获取当前数据库常驻内存占用大小（包含 HashIndex、HybridLog 环形缓冲池与 ReadCache）
  pub fn memory_usage(&self) -> u64 {
    let index_mem = (self.store.index.bucket_count()
      + self.store.index.overflow_bucket_count() as usize) as u64
      * 64;
    let hlog_mem = (self.store.hlog.config.num_pages * self.store.hlog.config.page_size) as u64;
    let read_cache_mem = if self.store.read_cache.is_enabled {
      (self.store.read_cache.num_pages * self.store.read_cache.page_size) as u64
    } else {
      0
    };
    index_mem + hlog_mem + read_cache_mem
  }

  /// 初始化写入有序集合元素
  pub fn zadd_elements<M: AsRef<[u8]>>(&self, zset_key: &[u8], items: &[(f64, M)]) -> Result<()> {
    self.rt.block_on(async {
      self
        .session
        .zmadd(
          zset_key,
          items.iter().map(|&(score, ref m)| (score, m.as_ref())),
          ZAddOpt::default(),
        )
        .await?;
      Ok(())
    })
  }

  /// 有序范围切片查询评测（基于跳表索引获取 [start..stop] 区间切片）
  pub fn bench_zrange_queries(
    &self,
    name: &str,
    zset_key: &[u8],
    queries: &[(isize, isize)],
  ) -> Result<BenchStats> {
    let mut run = BatchRun {
      lat: Vec::with_capacity(queries.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start_all = Instant::now();
    self.rt.block_on(async {
      for &(start, stop) in queries {
        let timer = OpTimer::start();
        let mut q_bytes = 0u64;
        self
          .session
          .zrange_cb(zset_key, start, stop, false, |member, score| {
            // bytes 口径与其他引擎一致：member 长度 + 8 字节分数
            q_bytes += (member.len() + 8) as u64;
            black_box((member, score));
            true
          })
          .await?;
        run.lat.push(timer.elapsed_ns());
        run.bytes += q_bytes;
      }
      Ok::<(), Error>(())
    })?;
    run.duration = start_all.elapsed();
    Ok(self.stats(name, run))
  }
}

/// 范围检索评测：底层 KV 有序范围扫描（与其他三引擎统一纯 KV scan 口径，
/// 直接驱动 store 内置 BfTree 原始 KV，不经 ZSet 上层路径）
impl RangeEngine for WedbHarness {
  fn scan(&self, start: &[u8], end: &[u8]) -> Result<u64> {
    let mut q_bytes = 0u64;
    let scanned = self.store.scan_range_callback(start, end, |k, v| {
      q_bytes += (k.len() + v.len()) as u64;
      black_box((k, v));
      true
    })?;
    black_box(scanned);
    Ok(q_bytes)
  }
}

impl BenchEngine for WedbHarness {
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
    self
      .rt
      .block_on(async { self.session.upsert(k, v).await })?;
    Ok(())
  }

  #[inline]
  fn get_len(&self, k: &[u8]) -> Result<usize> {
    let len = self
      .rt
      .block_on(async { self.session.read_with(k, |v| v.len()).await })?
      .unwrap_or(0);
    Ok(len)
  }

  #[inline]
  fn del(&self, k: &[u8]) -> Result<()> {
    let deleted = self.rt.block_on(async { self.session.delete(k).await })?;
    black_box(deleted);
    Ok(())
  }

  /// 覆写批量点写：批处理纪元保护（对标 C# UnsafeContext）下连续执行同步快路径，
  /// 遭遇 PageNotReady / TTL 异步闭环时退出批处理降级全异步，再重回批处理继续
  fn put_batch<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, pairs: &[(K, V)]) -> Result<BatchRun> {
    let mut run = BatchRun {
      lat: Vec::with_capacity(pairs.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start_all = Instant::now();
    self.rt.block_on(async {
      let mut batch = self.session.enter_batch();
      for (k, v) in pairs {
        let (k, v) = (k.as_ref(), v.as_ref());
        run.bytes += (k.len() + v.len()) as u64;
        let timer = OpTimer::start();
        // 纪元守卫绝不跨越任何可能磁盘 I/O 的 await：降级前必须先退出批处理
        if batch.try_upsert_sync(k, v)?.is_err() {
          drop(batch);
          self.session.upsert(k, v).await?;
          batch = self.session.enter_batch();
        }
        run.lat.push(timer.elapsed_ns());
      }
      Ok::<(), Error>(())
    })?;
    run.duration = start_all.elapsed();
    Ok(run)
  }

  /// 覆写批量点读：12 路流水线批量读，分块均摊单条延迟（对齐底层预取窗口）
  fn get_batch<K: AsRef<[u8]>>(&self, keys: &[K]) -> Result<BatchRun> {
    let count = keys.len();
    let mut run = BatchRun {
      lat: Vec::with_capacity(count),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start_all = Instant::now();
    self.rt.block_on(async {
      let mut timer = OpTimer::start();
      let mut batch_start = 0usize;
      self
        .session
        .read_batch_with(keys, |idx, val_opt| {
          let k = unsafe { keys.get_unchecked(idx) }.as_ref();
          let val_len = val_opt.map_or(0, <[u8]>::len);
          black_box(val_len);
          run.bytes += (k.len() + val_len) as u64;

          if (idx + 1) % READ_PIPELINE_CHUNK == 0 || idx + 1 == count {
            let elapsed = timer.elapsed_ns();
            let batch_len = (idx + 1) - batch_start;
            // 流水线批量读无法逐条计时，按分块均摊且下限 1ns
            let per_op = (elapsed / batch_len as u64).max(1);
            run.lat.resize(run.lat.len() + batch_len, per_op);
            batch_start = idx + 1;
            timer = OpTimer::start();
          }
        })
        .await?;
      Ok::<(), Error>(())
    })?;
    run.duration = start_all.elapsed();
    Ok(run)
  }

  /// 覆写批量删除：整批共享单次 block_on
  fn del_batch<K: AsRef<[u8]>>(&self, keys: &[K]) -> Result<BatchRun> {
    let mut run = BatchRun {
      lat: Vec::with_capacity(keys.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start_all = Instant::now();
    self.rt.block_on(async {
      for k in keys {
        let k = k.as_ref();
        run.bytes += k.len() as u64;
        let timer = OpTimer::start();
        let deleted = self.session.delete(k).await?;
        run.lat.push(timer.elapsed_ns());
        black_box(deleted);
      }
      Ok::<(), Error>(())
    })?;
    run.duration = start_all.elapsed();
    Ok(run)
  }

  /// 覆写混合负载：批处理纪元保护下写走同步快路径、读走 TTL 门控同步直读，
  /// 需磁盘 I/O / TTL 异步裁决时退出批处理降级全异步，再重回批处理继续
  fn mixed_batch<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, ops: &[MixedOp<K, V>]) -> Result<BatchRun> {
    let mut run = BatchRun {
      lat: Vec::with_capacity(ops.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start_all = Instant::now();
    self.rt.block_on(async {
      let mut batch = self.session.enter_batch();
      for op in ops {
        match op {
          MixedOp::Read(k) => {
            let k = k.as_ref();
            let timer = OpTimer::start();
            // Ok(None) ⇒ 磁盘候选或 TTL 需异步裁决，先退批处理再走全异步
            let val_len = match batch.try_read_sync(k, |v| v.len())? {
              Some(len_opt) => len_opt.unwrap_or(0),
              None => {
                drop(batch);
                let len = self.session.read_with(k, |v| v.len()).await?.unwrap_or(0);
                batch = self.session.enter_batch();
                len
              }
            };
            run.lat.push(timer.elapsed_ns());
            black_box(val_len);
            run.bytes += (k.len() + val_len) as u64;
          }
          MixedOp::Write(k, v) => {
            let (k, v) = (k.as_ref(), v.as_ref());
            run.bytes += (k.len() + v.len()) as u64;
            let timer = OpTimer::start();
            if batch.try_upsert_sync(k, v)?.is_err() {
              drop(batch);
              self.session.upsert(k, v).await?;
              batch = self.session.enter_batch();
            }
            run.lat.push(timer.elapsed_ns());
          }
        }
      }
      Ok::<(), Error>(())
    })?;
    run.duration = start_all.elapsed();
    Ok(run)
  }
}
