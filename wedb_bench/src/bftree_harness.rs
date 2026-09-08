use std::{
  hint::black_box,
  path::PathBuf,
  sync::Arc,
  time::{Duration, Instant},
};

use tempfile::{TempDir, tempdir};
use wbftree::{BfTreeConfig, BfTreeInsertResult, BfTreeService, ScanReturnField, StorageBackend};

use crate::{
  driver::{
    BatchRun, BenchEngine, RangeEngine, bench_delete_batch, bench_mixed_ops, bench_range_queries,
    bench_read_batch, bench_upsert_batch,
  },
  error::{Error, Result},
  stats::{BenchStats, MixedOp, OpTimer, get_path_size},
  suite::DEFAULT_MEMORY_BUDGET_BYTES,
};

pub(crate) const ENGINE_NAME: &str = "WeDb-BfTree";
const DB_FILE_NAME: &str = "bftree_bench.db";
const DEFAULT_MIN_RECORD_SIZE: usize = 4;
const DEFAULT_MAX_RECORD_SIZE: usize = 4096;
const DEFAULT_MAX_KEY_LEN: usize = 512;
const DEFAULT_LEAF_PAGE_SIZE: usize = 16384;
/// 点读复用的栈上值缓冲区（标准 profile 值长 1000B 内零堆分配）
pub(crate) const READ_BUF_SIZE: usize = 4096;

/// WeDb-BfTree (对标微软 Garnet 实现的有序 B-树存储引擎) 评测包装句柄
pub struct BfTreeHarness {
  pub service: Arc<BfTreeService>,
  pub dir: TempDir,
  pub db_file: PathBuf,
  pub budget_bytes: u64,
}

impl BfTreeHarness {
  /// 创建指定内存预算与页面规格的 WeDb-BfTree 评测实例
  pub fn new_with_budget(budget_bytes: u64) -> Result<Self> {
    let dir = tempdir()?;
    let db_file = dir.path().join(DB_FILE_NAME);

    let mut config = BfTreeConfig::default();
    config.storage_backend(StorageBackend::Std);
    config.file_path(&db_file);
    config.use_snapshot(true);
    if budget_bytes > 0 {
      config.cb_size_byte(budget_bytes as usize);
    }
    config.cb_min_record_size(DEFAULT_MIN_RECORD_SIZE);
    config.cb_max_record_size(DEFAULT_MAX_RECORD_SIZE);
    config.cb_max_key_len(DEFAULT_MAX_KEY_LEN);
    config.leaf_page_size(DEFAULT_LEAF_PAGE_SIZE);

    let service = Arc::new(BfTreeService::new(config)?);
    Ok(Self {
      service,
      dir,
      db_file,
      budget_bytes,
    })
  }

  /// 默认创建对齐物理内存预算的 WeDb-BfTree 实例
  pub fn new() -> Result<Self> {
    Self::new_with_budget(DEFAULT_MEMORY_BUDGET_BYTES)
  }

  /// 校验插入结果（仅 Success 视为有效写入）
  #[inline]
  fn check_insert(&self, res: BfTreeInsertResult) -> Result<()> {
    if res == BfTreeInsertResult::Success {
      Ok(())
    } else {
      Err(Error::BfTreeInsert(res))
    }
  }

  /// 批量写入评测
  pub fn bench_upsert_batch<K: AsRef<[u8]>, V: AsRef<[u8]>>(
    &self,
    name: &str,
    pairs: &[(K, V)],
  ) -> Result<BenchStats> {
    bench_upsert_batch(self, name, pairs)
  }

  /// 批量删除评测
  pub fn bench_delete_batch<K: AsRef<[u8]>>(&self, name: &str, keys: &[K]) -> Result<BenchStats> {
    bench_delete_batch(self, name, keys)
  }

  /// 有序范围扫描评测（scan_with_end_key_callback 扫描 [start_k..=end_k]）
  pub fn bench_range_queries<K: AsRef<[u8]>>(
    &self,
    name: &str,
    ranges: &[(K, K)],
  ) -> Result<BenchStats> {
    bench_range_queries(self, name, ranges)
  }

  /// 批量读取评测
  pub fn bench_read_batch<K: AsRef<[u8]>>(&self, name: &str, keys: &[K]) -> Result<BenchStats> {
    bench_read_batch(self, name, keys)
  }

  /// 混合读写负载评测（对齐 YCSB Task A / Task B / Task D 标准混合任务）
  pub fn bench_mixed_ops<K: AsRef<[u8]>, V: AsRef<[u8]>>(
    &self,
    name: &str,
    ops: &[MixedOp<K, V>],
  ) -> Result<BenchStats> {
    bench_mixed_ops(self, name, ops)
  }

  /// 刷盘同步 (CPR 快照持久化)
  pub fn flush_all(&self) -> Result<()> {
    let snap_path = self.dir.path().join("bftree_snapshot.db");
    self.service.cpr_snapshot(&snap_path)?;
    Ok(())
  }

  /// 获取当前数据库目录/文件磁盘实际占用大小
  pub fn disk_usage(&self) -> u64 {
    get_path_size(self.dir.path())
  }

  /// 获取当前数据库常驻内存预算/占用大小
  pub fn memory_usage(&self) -> u64 {
    self.budget_bytes
  }
}

impl BenchEngine for BfTreeHarness {
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
    self.check_insert(self.service.insert(k, v))
  }

  #[inline]
  fn get_len(&self, k: &[u8]) -> Result<usize> {
    // 逐条读取路径：栈上小缓冲即拷即用（混合负载等非批量场景）
    let mut buf = [0u8; READ_BUF_SIZE];
    let (res, len) = self.service.read_into(k, &mut buf);
    black_box((res, len));
    Ok(len)
  }

  #[inline]
  fn del(&self, k: &[u8]) -> Result<()> {
    black_box(self.service.delete(k));
    Ok(())
  }

  /// 覆写批量点读：整批复用同一栈上缓冲区，消除逐条置零开销
  fn get_batch<K: AsRef<[u8]>>(&self, keys: &[K]) -> Result<BatchRun> {
    let mut buf = [0u8; READ_BUF_SIZE];
    let mut run = BatchRun {
      lat: Vec::with_capacity(keys.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start = Instant::now();
    for k in keys {
      let k = k.as_ref();
      let timer = OpTimer::start();
      let (res, len) = self.service.read_into(k, &mut buf);
      run.lat.push(timer.elapsed_ns());
      black_box((res, len));
      run.bytes += (k.len() + len) as u64;
    }
    run.duration = start.elapsed();
    Ok(run)
  }

  /// 覆写混合负载：写前计字节、读后计字节，与默认骨架口径一致；整批复用栈上缓冲
  fn mixed_batch<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, ops: &[MixedOp<K, V>]) -> Result<BatchRun> {
    let mut buf = [0u8; READ_BUF_SIZE];
    let mut run = BatchRun {
      lat: Vec::with_capacity(ops.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start = Instant::now();
    for op in ops {
      match op {
        MixedOp::Read(k) => {
          let k = k.as_ref();
          let timer = OpTimer::start();
          let (res, len) = self.service.read_into(k, &mut buf);
          run.lat.push(timer.elapsed_ns());
          black_box((res, len));
          run.bytes += (k.len() + len) as u64;
        }
        MixedOp::Write(k, v) => {
          let (k, v) = (k.as_ref(), v.as_ref());
          run.bytes += (k.len() + v.len()) as u64;
          let timer = OpTimer::start();
          let res = self.service.insert(k, v);
          run.lat.push(timer.elapsed_ns());
          self.check_insert(res)?;
        }
      }
    }
    run.duration = start.elapsed();
    Ok(run)
  }
}

impl RangeEngine for BfTreeHarness {
  fn scan(&self, start: &[u8], end: &[u8]) -> Result<u64> {
    let mut q_bytes = 0u64;
    let scanned = self.service.scan_with_end_key_callback(
      start,
      end,
      ScanReturnField::KeyAndValue,
      |k, v| {
        q_bytes += (k.len() + v.len()) as u64;
        black_box((k, v));
        true
      },
    )?;
    black_box(scanned);
    Ok(q_bytes)
  }
}
