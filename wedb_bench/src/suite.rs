//! 对比评测编排：workload 生成、四引擎调度与多线程并发骨架

use std::{
  hint::black_box,
  panic::{AssertUnwindSafe, catch_unwind},
  sync::{Arc, Barrier},
  thread,
  time::{Duration, Instant},
};

use compio::runtime::Runtime;
use fastrand::Rng;

use crate::{
  bftree_harness::{BfTreeHarness, ENGINE_NAME as BFTREE, READ_BUF_SIZE},
  data_gen::CompressibleDataPool,
  error::{Error, Result},
  fjall_harness::{ENGINE_NAME as FJALL, FjallHarness},
  report::format_ops,
  rocksdb_harness::{ENGINE_NAME as ROCKSDB, RocksDbHarness},
  stats::{BenchStats, MixedOp, NANOS_PER_MICRO, get_process_rss_bytes},
  wedb_harness::{ENGINE_NAME as WEDB, WedbHarness},
};

pub const SEQ_KEY_LEN: usize = 14;
pub const RAND_KEY_LEN: usize = 16;
pub const ORDER_KEY_LEN: usize = 12;
pub const CONC_KEY_LEN: usize = 9;

const NAME_SEQ_INSERT: &str = "seq_insert";
const NAME_RAND_INSERT: &str = "rand_insert";
const NAME_SEQ_READ: &str = "seq_read";
const NAME_RAND_READ: &str = "rand_read";
const NAME_TASK_A: &str = "task_a";
const NAME_TASK_B: &str = "task_b";
const NAME_TASK_C: &str = "task_c";
const NAME_TASK_D: &str = "task_d";
const NAME_RANGE: &str = "range_query";
const NAME_CONCURRENCY: &str = "concurrency";

/// 任务 E 范围检索数据集规模
const RANGE_DATASET_SIZE: usize = 1_000;
/// 并发测试键空间规模（独立于主数据集的 1 万热键）
const CONC_KEYSPACE: usize = 10_000;
/// 任务 D 追加键数量下限（防极小读取量下无追加键）
const MIN_APPEND_COUNT: usize = 1_000;
/// 并发测试逐操作延迟抽样间隔（1/N 采样，统计上足以稳定估计 P50/P99）
const CONC_LAT_SAMPLE_EVERY: usize = 16;

/// 单兆字节对应的字节数 (1MB = 1024 * 1024 字节)
pub const MB_BYTES: usize = 1024 * 1024;

/// 默认各存储引擎对齐的物理内存预算 (MB)
pub const DEFAULT_MEMORY_BUDGET_MB: usize = 256;

/// 默认各存储引擎对齐的物理内存预算 (字节)
pub const DEFAULT_MEMORY_BUDGET_BYTES: u64 = (DEFAULT_MEMORY_BUDGET_MB * MB_BYTES) as u64;

/// LSM-Tree 存储引擎 Memtable 预算占总物理内存预算的分母比例 (即 1/4 = 25%)
pub const LSM_MEMTABLE_DIVISOR: u64 = 4;

/// 将指定总物理内存预算字节切分为 LSM-Tree 架构的 (Block Cache, Memtable) 预算字节
#[inline]
pub const fn calc_lsm_budget_split(total_bytes: u64) -> (u64, u64) {
  let memtable = total_bytes / LSM_MEMTABLE_DIVISOR;
  let cache = total_bytes - memtable;
  (cache, memtable)
}

/// 默认 LSM-Tree Block Cache 预算 (字节，3/4 占比)
pub const DEFAULT_LSM_CACHE_BYTES: u64 = calc_lsm_budget_split(DEFAULT_MEMORY_BUDGET_BYTES).0;

/// 默认 LSM-Tree Memtable 预算 (字节，1/4 占比)
pub const DEFAULT_LSM_MEMTABLE_BYTES: u64 = calc_lsm_budget_split(DEFAULT_MEMORY_BUDGET_BYTES).1;

/// 基准测试运行规格与参数配置
#[derive(Debug, Clone)]
pub struct BenchOpt {
  pub seq_count: usize,
  pub rand_count: usize,
  pub read_count: usize,
  pub val_len: usize,
  pub memory_budget_mb: usize,
}

impl Default for BenchOpt {
  fn default() -> Self {
    Self::from_profile("standard")
  }
}

impl BenchOpt {
  pub fn from_profile(profile: &str) -> Self {
    match profile.to_ascii_lowercase().as_str() {
      "quick" => Self {
        seq_count: 30_000,
        rand_count: 30_000,
        read_count: 50_000,
        val_len: 100,
        memory_budget_mb: DEFAULT_MEMORY_BUDGET_MB,
      },
      _ => Self {
        seq_count: 500_000,
        rand_count: 500_000,
        read_count: 500_000,
        val_len: 1000,
        memory_budget_mb: DEFAULT_MEMORY_BUDGET_MB,
      },
    }
  }
}

/// 4个存储引擎在同一项测试中的对比数据
#[derive(Debug, Clone)]
pub struct ComparisonItem {
  pub wedb: BenchStats,
  pub bftree: BenchStats,
  pub rocksdb: BenchStats,
  pub fjall: BenchStats,
}

impl ComparisonItem {
  #[inline]
  pub fn calc_speedup(numerator: f64, denominator: f64) -> f64 {
    if denominator <= 0.0 || numerator <= 0.0 || denominator.is_nan() || numerator.is_nan() {
      0.0
    } else {
      numerator / denominator
    }
  }

  pub fn speedup_vs_rocksdb(&self) -> f64 {
    Self::calc_speedup(self.wedb.gb_per_sec, self.rocksdb.gb_per_sec)
  }

  pub fn speedup_vs_fjall(&self) -> f64 {
    Self::calc_speedup(self.wedb.gb_per_sec, self.fjall.gb_per_sec)
  }

  pub fn speedup_vs_bftree(&self) -> f64 {
    Self::calc_speedup(self.wedb.gb_per_sec, self.bftree.gb_per_sec)
  }
}

/// 十进制序号右对齐填充的栈上定长键构造（prefix + 固定位数字，超宽高位截断）
#[inline]
pub fn num_key<const N: usize>(prefix: &[u8], i: usize) -> [u8; N] {
  let mut buf = [0u8; N];
  buf[..prefix.len()].copy_from_slice(prefix);
  let mut v = i;
  for b in buf[prefix.len()..].iter_mut().rev() {
    *b = b'0' + (v % 10) as u8;
    v /= 10;
  }
  buf
}

/// 任务 D 追加键数量（读取量的 1/10，下限保底）
#[inline]
fn append_keys_len(read_ops: usize) -> usize {
  (read_ops / 10).max(MIN_APPEND_COUNT)
}

/// 并发测试共享口径（全量操作数与逻辑传输字节，吞吐以此为准，与延迟抽样解耦）
struct ConcWorkload {
  total_ops: usize,
  total_bytes: u64,
}

impl ConcWorkload {
  /// 汇总单引擎并发评测统计（百分比延迟基于 1/N 抽样真实计算）
  fn stats(
    &self,
    engine: &'static str,
    duration: Duration,
    latencies_ns: Vec<u64>,
    disk_bytes: u64,
    mem_bytes: u64,
  ) -> BenchStats {
    let secs = duration.as_secs_f64();
    let mut stat = BenchStats::compute(
      NAME_CONCURRENCY,
      engine,
      latencies_ns,
      duration,
      self.total_bytes,
      disk_bytes,
      mem_bytes,
    );
    // 吞吐口径以全量 total_ops 为准；均值取墙钟口径（含排队），与抽样分位数互补
    stat.total_ops = self.total_ops;
    stat.ops_per_sec = if secs > 0.0 {
      (self.total_ops as f64) / secs
    } else {
      0.0
    };
    stat.avg_us = if self.total_ops > 0 {
      (duration.as_nanos() as f64) / (self.total_ops as f64) / NANOS_PER_MICRO
    } else {
      0.0
    };
    stat
  }
}

/// 多线程并发评测统一骨架：准备（setup）与测量（run）两阶段分离，
/// 就绪/发令双屏障对齐计时窗口，消除引擎间线程就绪相位差；
/// 准备阶段失败（含 panic 兜底）的线程同样到达双屏障后再返回错误，保证主线程永不死等
fn run_threaded<C>(
  thread_count: usize,
  setup: impl Fn(usize) -> Result<C> + Send + Sync + 'static,
  run: impl Fn(usize, C) -> Result<Vec<u64>> + Send + Sync + 'static,
) -> Result<(Duration, Vec<u64>)> {
  let setup = Arc::new(setup);
  let run = Arc::new(run);
  let ready = Arc::new(Barrier::new(thread_count + 1));
  let go = Arc::new(Barrier::new(thread_count + 1));
  let mut handles = Vec::with_capacity(thread_count);
  for t in 0..thread_count {
    let (setup, run, ready, go) = (
      Arc::clone(&setup),
      Arc::clone(&run),
      Arc::clone(&ready),
      Arc::clone(&go),
    );
    handles.push(thread::spawn(move || {
      // 准备阶段失败（含 panic）也要到齐双屏障，避免主线程死等
      let ctx = match catch_unwind(AssertUnwindSafe(|| setup(t))) {
        Ok(Ok(ctx)) => ctx,
        Ok(Err(e)) => {
          ready.wait();
          go.wait();
          return Err(e);
        }
        Err(_) => {
          ready.wait();
          go.wait();
          return Err(Error::ThreadPanic);
        }
      };
      ready.wait();
      go.wait();
      run(t, ctx)
    }));
  }
  ready.wait();
  let start = Instant::now();
  go.wait();
  let mut lat = Vec::new();
  for h in handles {
    lat.extend(h.join().map_err(|_| Error::ThreadPanic)??);
  }
  Ok((start.elapsed(), lat))
}

/// 对齐 YCSB / Gray et al. 规范的 Zipfian 分布生成器 (零第三方依赖，基于 fastrand)
struct ZipfianGenerator {
  n: usize,
  alpha: f64,
  zetan: f64,
  eta: f64,
  zeta_half: f64,
}

impl ZipfianGenerator {
  fn new(n: usize, theta: f64) -> Self {
    let n = n.max(1);
    let alpha = 1.0 / (1.0 - theta);
    let mut zetan = 0.0;
    for i in 1..=n {
      zetan += (i as f64).powf(-theta);
    }
    let zeta2 = 1.0 + 2.0_f64.powf(-theta);
    let eta = (1.0 - (2.0 / n as f64).powf(1.0 - theta)) / (1.0 - zeta2 / zetan);
    let zeta_half = 1.0 + 0.5_f64.powf(theta);
    Self {
      n,
      alpha,
      zetan,
      eta,
      zeta_half,
    }
  }

  #[inline]
  fn sample(&self, rng: &mut Rng) -> usize {
    if self.n <= 1 {
      return 0;
    }
    let u = rng.f64();
    let uz = u * self.zetan;
    if uz < 1.0 {
      return 0;
    }
    // 预计算常量避免内循环重复浮点幂次运算 (powf)
    if uz < self.zeta_half {
      return 1.min(self.n - 1);
    }
    let idx = (self.n as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as usize;
    idx.min(self.n - 1)
  }
}

/// 生成对齐 rust-storage-bench / YCSB 标准的 Zipfian 0.99 倾斜混合操作序列
fn generate_zipf_ops<'a>(
  zipf: &ZipfianGenerator,
  keys: &'a [[u8; SEQ_KEY_LEN]],
  pool: &'a CompressibleDataPool,
  val_len: usize,
  ops_count: usize,
  write_ratio: f32,
  seed: u64,
) -> Vec<MixedOp<&'a [u8], &'a [u8]>> {
  let mut rng = Rng::with_seed(seed);
  let mut ops = Vec::with_capacity(ops_count);
  for idx in 0..ops_count {
    let k_idx = zipf.sample(&mut rng);
    let k = keys[k_idx].as_slice();
    if rng.f32() < write_ratio {
      let val = pool.get_slice((idx ^ k_idx) * val_len, val_len);
      ops.push(MixedOp::Write(k, val));
    } else {
      ops.push(MixedOp::Read(k));
    }
  }
  ops
}

/// 生成对齐 YCSB Task D (Read Latest: 95% 读最新 / 5% 追加写入) 的操作序列
fn generate_task_d_ops<'a>(
  initial_keys: &'a [[u8; SEQ_KEY_LEN]],
  append_keys: &'a [[u8; SEQ_KEY_LEN]],
  pool: &'a CompressibleDataPool,
  val_len: usize,
  ops_count: usize,
  seed: u64,
) -> Vec<MixedOp<&'a [u8], &'a [u8]>> {
  let mut rng = Rng::with_seed(seed);
  let mut ops = Vec::with_capacity(ops_count);
  let mut append_idx = 0usize;
  for idx in 0..ops_count {
    if rng.u32(0..100) < 5 && append_idx < append_keys.len() {
      let k = append_keys[append_idx].as_slice();
      let val = pool.get_slice((idx ^ append_idx) * val_len, val_len);
      append_idx += 1;
      ops.push(MixedOp::Write(k, val));
    } else if append_idx > 0 {
      let k = append_keys[append_idx - 1].as_slice();
      ops.push(MixedOp::Read(k));
    } else {
      let k = initial_keys
        .last()
        .map_or(&b"seq:0000000000"[..], |k| k.as_slice());
      ops.push(MixedOp::Read(k));
    }
  }
  ops
}

pub fn run_comparative_benchmarks(options: &BenchOpt) -> Result<BenchmarkReport> {
  let mut results = Vec::new();
  let mut rng = Rng::with_seed(42);

  let seq_count = options.seq_count;
  let rand_count = options.rand_count;
  let read_count = options.read_count;
  let val_len = options.val_len;

  // 离线预构建真实可压缩数据池 (严格融合 RocksDB CompressibleString 与 YCSB 字段规范)
  let data_pool = Arc::new(CompressibleDataPool::default_pool());

  let seq_keys: Vec<[u8; SEQ_KEY_LEN]> = (0..seq_count)
    .map(|i| num_key::<SEQ_KEY_LEN>(b"seq:", i))
    .collect();
  let seq_pairs: Vec<(&[u8], &[u8])> = seq_keys
    .iter()
    .enumerate()
    .map(|(idx, k)| (k.as_slice(), data_pool.get_slice(idx * val_len, val_len)))
    .collect();

  let rand_keys: Vec<[u8; RAND_KEY_LEN]> = (0..rand_count)
    .map(|_| {
      let mut k = [0u8; RAND_KEY_LEN];
      rng.fill(&mut k);
      k
    })
    .collect();
  let rand_pairs: Vec<(&[u8], &[u8])> = rand_keys
    .iter()
    .enumerate()
    .map(|(idx, k)| {
      (
        k.as_slice(),
        data_pool.get_slice((idx + 137) * val_len, val_len),
      )
    })
    .collect();

  let wedb = WedbHarness::new_with_budget(options.memory_budget_mb, seq_count.max(rand_count))?;
  let actual_budget_bytes = (options.memory_budget_mb * MB_BYTES) as u64;
  let (cache_budget, memtable_budget) = calc_lsm_budget_split(actual_budget_bytes);

  let bftree = BfTreeHarness::new_with_budget(actual_budget_bytes)?;
  let rocksdb = RocksDbHarness::new_with_budget(cache_budget, memtable_budget)?;
  let fjall = FjallHarness::new_with_budget(cache_budget, memtable_budget)?;

  // 1. 顺序批量写入 (Bulk Sequential Write)
  println!(
    ">>> 正在执行基准测试 1/10: 顺序批量写入 (Bulk Sequential Write {} ops)...",
    format_ops(seq_count as f64)
  );
  results.push(ComparisonItem {
    wedb: wedb.bench_upsert_batch(NAME_SEQ_INSERT, &seq_pairs)?,
    bftree: bftree.bench_upsert_batch(NAME_SEQ_INSERT, &seq_pairs)?,
    rocksdb: rocksdb.bench_upsert_batch(NAME_SEQ_INSERT, &seq_pairs)?,
    fjall: fjall.bench_upsert_batch(NAME_SEQ_INSERT, &seq_pairs)?,
  });

  // 2. 随机批量写入 (Bulk Random Write)
  println!(
    ">>> 正在执行基准测试 2/10: 随机批量写入 (Bulk Random Write {} ops)...",
    format_ops(rand_count as f64)
  );
  results.push(ComparisonItem {
    wedb: wedb.bench_upsert_batch(NAME_RAND_INSERT, &rand_pairs)?,
    bftree: bftree.bench_upsert_batch(NAME_RAND_INSERT, &rand_pairs)?,
    rocksdb: rocksdb.bench_upsert_batch(NAME_RAND_INSERT, &rand_pairs)?,
    fjall: fjall.bench_upsert_batch(NAME_RAND_INSERT, &rand_pairs)?,
  });

  // 3. 顺序批量读取 (Bulk Sequential Read)
  println!(
    ">>> 正在执行基准测试 3/10: 顺序批量读取 (Bulk Sequential Read {} ops)...",
    format_ops(seq_count as f64)
  );
  results.push(ComparisonItem {
    wedb: wedb.bench_read_batch(NAME_SEQ_READ, &seq_keys)?,
    bftree: bftree.bench_read_batch(NAME_SEQ_READ, &seq_keys)?,
    rocksdb: rocksdb.bench_read_batch(NAME_SEQ_READ, &seq_keys)?,
    fjall: fjall.bench_read_batch(NAME_SEQ_READ, &seq_keys)?,
  });

  // 4. 纯均匀随机读取 (Uniform Random Read, 冷热穿透)
  println!(
    ">>> 正在执行基准测试 4/10: 纯均匀随机读取 (Uniform Random Read {} ops)...",
    format_ops(rand_count as f64)
  );
  let mut uniform_rand_keys = rand_keys.clone();
  rng.shuffle(&mut uniform_rand_keys);
  results.push(ComparisonItem {
    wedb: wedb.bench_read_batch(NAME_RAND_READ, &uniform_rand_keys)?,
    bftree: bftree.bench_read_batch(NAME_RAND_READ, &uniform_rand_keys)?,
    rocksdb: rocksdb.bench_read_batch(NAME_RAND_READ, &uniform_rand_keys)?,
    fjall: fjall.bench_read_batch(NAME_RAND_READ, &uniform_rand_keys)?,
  });

  // 5. 任务 A: 混合更新 (Task A: Update Heavy 50% Read / 50% Write, Zipfian 0.99)
  println!(
    ">>> 正在执行基准测试 5/10: 任务 A 混合更新 (Task A: 50%R/50%W, Zipf 0.99, {} ops)...",
    format_ops(read_count as f64)
  );
  let zipf = ZipfianGenerator::new(seq_keys.len(), 0.99);
  let task_a_ops = generate_zipf_ops(&zipf, &seq_keys, &data_pool, val_len, read_count, 0.50, 101);
  results.push(ComparisonItem {
    wedb: wedb.bench_mixed_ops(NAME_TASK_A, &task_a_ops)?,
    bftree: bftree.bench_mixed_ops(NAME_TASK_A, &task_a_ops)?,
    rocksdb: rocksdb.bench_mixed_ops(NAME_TASK_A, &task_a_ops)?,
    fjall: fjall.bench_mixed_ops(NAME_TASK_A, &task_a_ops)?,
  });

  // 6. 任务 B: 读多写少 (Task B: Read Mostly 95% Read / 5% Write, Zipfian 0.99)
  println!(
    ">>> 正在执行基准测试 6/10: 任务 B 读多写少 (Task B: 95%R/5%W, Zipf 0.99, {} ops)...",
    format_ops(read_count as f64)
  );
  let task_b_ops = generate_zipf_ops(&zipf, &seq_keys, &data_pool, val_len, read_count, 0.05, 102);
  results.push(ComparisonItem {
    wedb: wedb.bench_mixed_ops(NAME_TASK_B, &task_b_ops)?,
    bftree: bftree.bench_mixed_ops(NAME_TASK_B, &task_b_ops)?,
    rocksdb: rocksdb.bench_mixed_ops(NAME_TASK_B, &task_b_ops)?,
    fjall: fjall.bench_mixed_ops(NAME_TASK_B, &task_b_ops)?,
  });

  // 7. 任务 C: 热点只读 (Task C: Read Only 100% Read, Zipfian 0.99)
  println!(
    ">>> 正在执行基准测试 7/10: 任务 C 热点只读 (Task C: 100% Read, Zipf 0.99, {} ops)...",
    format_ops(read_count as f64)
  );
  let task_c_ops = generate_zipf_ops(&zipf, &seq_keys, &data_pool, val_len, read_count, 0.0, 103);
  results.push(ComparisonItem {
    wedb: wedb.bench_mixed_ops(NAME_TASK_C, &task_c_ops)?,
    bftree: bftree.bench_mixed_ops(NAME_TASK_C, &task_c_ops)?,
    rocksdb: rocksdb.bench_mixed_ops(NAME_TASK_C, &task_c_ops)?,
    fjall: fjall.bench_mixed_ops(NAME_TASK_C, &task_c_ops)?,
  });

  // 8. 任务 D: 读取最新 (Task D: Read Latest 95% Read Latest / 5% Append)
  let append_keys: Vec<[u8; SEQ_KEY_LEN]> = (seq_count..(seq_count + append_keys_len(read_count)))
    .map(|i| num_key::<SEQ_KEY_LEN>(b"seq:", i))
    .collect();
  println!(
    ">>> 正在执行基准测试 8/10: 任务 D 读取最新 (Task D: 95% Read / 5% Append, {} ops)...",
    format_ops(read_count as f64)
  );
  let task_d_ops = generate_task_d_ops(
    &seq_keys,
    &append_keys,
    &data_pool,
    val_len,
    read_count,
    104,
  );
  // 追加键实际写入数（固定随机流下确定，精确计入净数据集）
  let task_d_writes = task_d_ops
    .iter()
    .filter(|op| matches!(op, MixedOp::Write(..)))
    .count();
  results.push(ComparisonItem {
    wedb: wedb.bench_mixed_ops(NAME_TASK_D, &task_d_ops)?,
    bftree: bftree.bench_mixed_ops(NAME_TASK_D, &task_d_ops)?,
    rocksdb: rocksdb.bench_mixed_ops(NAME_TASK_D, &task_d_ops)?,
    fjall: fjall.bench_mixed_ops(NAME_TASK_D, &task_d_ops)?,
  });

  // 9. 任务 E: 动态有序范围检索 (Dynamic Ordered Range Scan)
  //    四引擎统一纯 KV 有序扫描口径：WeDB 直接驱动底层 BfTree 原始 KV 扫描，
  //    不经 ZSet 元数据/TTL/分数键编码等上层路径，与其他三引擎完全同构
  let range_query_count = (seq_count / 15).clamp(2_000, 10_000);
  println!(
    ">>> 正在执行基准测试 9/10: 动态有序范围检索 (Task E: Range Scan {} ops)...",
    format_ops(range_query_count as f64)
  );

  let sorted_keys: Vec<[u8; ORDER_KEY_LEN]> = (0..RANGE_DATASET_SIZE)
    .map(|i| num_key::<ORDER_KEY_LEN>(b"order:", i))
    .collect();
  let kv_init_data: Vec<(&[u8], &[u8])> = sorted_keys
    .iter()
    .enumerate()
    .map(|(i, k)| (k.as_slice(), data_pool.get_slice(i * val_len, val_len)))
    .collect();

  wedb.kv_put_raw_batch(&kv_init_data)?;
  bftree.bench_upsert_batch("range_init", &kv_init_data)?;
  rocksdb.bench_upsert_batch("range_init", &kv_init_data)?;
  fjall.bench_upsert_batch("range_init", &kv_init_data)?;

  let mut kv_ranges = Vec::with_capacity(range_query_count);

  for _ in 0..range_query_count {
    let span = rng.usize(10..=150);
    let start_idx = rng.usize(0..=(RANGE_DATASET_SIZE - span));
    let stop_idx = start_idx + span - 1;
    kv_ranges.push((
      sorted_keys[start_idx].as_slice(),
      sorted_keys[stop_idx].as_slice(),
    ));
  }

  results.push(ComparisonItem {
    wedb: wedb.bench_range_queries(NAME_RANGE, &kv_ranges)?,
    bftree: bftree.bench_range_queries(NAME_RANGE, &kv_ranges)?,
    rocksdb: rocksdb.bench_range_queries(NAME_RANGE, &kv_ranges)?,
    fjall: fjall.bench_range_queries(NAME_RANGE, &kv_ranges)?,
  });

  // 10. 多线程高并发混合读写 (4 Threads, 80% Read / 20% Write)
  let thread_count = 4usize;
  let ops_per_thread = (seq_count / thread_count).clamp(1_000, 25_000);
  let total_conc_ops = thread_count * ops_per_thread;
  let total_conc_bytes = (total_conc_ops as u64) * ((CONC_KEY_LEN + val_len) as u64);
  let wl = ConcWorkload {
    total_ops: total_conc_ops,
    total_bytes: total_conc_bytes,
  };
  println!(
    ">>> 正在执行基准测试 10/10: 多线程高并发混合读写 (4 Threads 80%R/20%W, {} ops)...",
    format_ops(total_conc_ops as f64)
  );

  // WeDB 并发测试 (4 OS 线程，每线程独立 compio runtime 与 Session)
  let s_ref = Arc::clone(&wedb.store);
  let pool = Arc::clone(&data_pool);
  let (wedb_dur, wedb_lat) = run_threaded(
    thread_count,
    move |_t| {
      let rt = Runtime::new()?;
      let session = s_ref.new_session()?;
      Ok((rt, session))
    },
    move |t, (rt, session)| {
      let mut r = Rng::with_seed((t as u64) + 100);
      let mut lat = Vec::with_capacity(ops_per_thread / CONC_LAT_SAMPLE_EVERY + 1);
      rt.block_on(async {
        for _ in 0..ops_per_thread {
          let k_id = r.usize(0..CONC_KEYSPACE);
          let key = num_key::<CONC_KEY_LEN>(b"c:k:", k_id);
          // 概率抽样防与周期性刷盘相位混叠（四引擎共享同一随机流保持键分布一致）
          let t0 = (r.usize(0..CONC_LAT_SAMPLE_EVERY) == 0).then(Instant::now);
          if r.usize(0..100) < 20 {
            let val = pool.get_slice((t * CONC_KEYSPACE + k_id) * val_len, val_len);
            session.upsert(&key, val).await?;
          } else {
            let _ = session.read(&key).await?;
          }
          if let Some(t0) = t0 {
            lat.push(t0.elapsed().as_nanos() as u64);
          }
        }
        Ok::<(), Error>(())
      })?;
      Ok(lat)
    },
  )?;
  let wedb_conc_stat = wl.stats(
    WEDB,
    wedb_dur,
    wedb_lat,
    wedb.disk_usage(),
    wedb.memory_usage(),
  );

  // BfTree 并发测试
  let bftree_ref = Arc::clone(&bftree.service);
  let pool = Arc::clone(&data_pool);
  let (bftree_dur, bftree_lat) = run_threaded(
    thread_count,
    |_t| Ok(()),
    move |t, ()| {
      let mut r = Rng::with_seed((t as u64) + 100);
      let mut buf = [0u8; READ_BUF_SIZE];
      let mut lat = Vec::with_capacity(ops_per_thread / CONC_LAT_SAMPLE_EVERY + 1);
      for _ in 0..ops_per_thread {
        let k_id = r.usize(0..CONC_KEYSPACE);
        let key = num_key::<CONC_KEY_LEN>(b"c:k:", k_id);
        // 概率抽样防与周期性刷盘相位混叠（四引擎共享同一随机流保持键分布一致）
        let t0 = (r.usize(0..CONC_LAT_SAMPLE_EVERY) == 0).then(Instant::now);
        if r.usize(0..100) < 20 {
          let val = pool.get_slice((t * CONC_KEYSPACE + k_id) * val_len, val_len);
          let _ = bftree_ref.insert(&key, val);
        } else {
          let (res, len) = bftree_ref.read_into(&key, &mut buf);
          black_box((res, len));
        }
        if let Some(t0) = t0 {
          lat.push(t0.elapsed().as_nanos() as u64);
        }
      }
      Ok(lat)
    },
  )?;
  let bftree_conc_stat = wl.stats(
    BFTREE,
    bftree_dur,
    bftree_lat,
    bftree.disk_usage(),
    bftree.memory_usage(),
  );

  // RocksDB 并发测试
  let rocks_db_ref = Arc::clone(&rocksdb.db);
  let pool = Arc::clone(&data_pool);
  let (rocks_dur, rocks_lat) = run_threaded(
    thread_count,
    |_t| Ok(()),
    move |t, ()| {
      let mut r = Rng::with_seed((t as u64) + 100);
      let mut lat = Vec::with_capacity(ops_per_thread / CONC_LAT_SAMPLE_EVERY + 1);
      for _ in 0..ops_per_thread {
        let k_id = r.usize(0..CONC_KEYSPACE);
        let key = num_key::<CONC_KEY_LEN>(b"c:k:", k_id);
        // 概率抽样防与周期性刷盘相位混叠（四引擎共享同一随机流保持键分布一致）
        let t0 = (r.usize(0..CONC_LAT_SAMPLE_EVERY) == 0).then(Instant::now);
        if r.usize(0..100) < 20 {
          let val = pool.get_slice((t * CONC_KEYSPACE + k_id) * val_len, val_len);
          rocks_db_ref.put(key, val)?;
        } else {
          let v = rocks_db_ref.get_pinned(key)?;
          black_box(v);
        }
        if let Some(t0) = t0 {
          lat.push(t0.elapsed().as_nanos() as u64);
        }
      }
      Ok(lat)
    },
  )?;
  let rocks_conc_stat = wl.stats(
    ROCKSDB,
    rocks_dur,
    rocks_lat,
    rocksdb.disk_usage(),
    rocksdb.memory_usage(),
  );

  // Fjall 并发测试
  let fjall_ks = fjall.keyspace.clone();
  let pool = Arc::clone(&data_pool);
  let (fjall_dur, fjall_lat) = run_threaded(
    thread_count,
    |_t| Ok(()),
    move |t, ()| {
      let ks = fjall_ks.clone();
      let mut r = Rng::with_seed((t as u64) + 100);
      let mut lat = Vec::with_capacity(ops_per_thread / CONC_LAT_SAMPLE_EVERY + 1);
      for _ in 0..ops_per_thread {
        let k_id = r.usize(0..CONC_KEYSPACE);
        let key = num_key::<CONC_KEY_LEN>(b"c:k:", k_id);
        // 概率抽样防与周期性刷盘相位混叠（四引擎共享同一随机流保持键分布一致）
        let t0 = (r.usize(0..CONC_LAT_SAMPLE_EVERY) == 0).then(Instant::now);
        if r.usize(0..100) < 20 {
          let val = pool.get_slice((t * CONC_KEYSPACE + k_id) * val_len, val_len);
          ks.insert(key, val)?;
        } else {
          let _ = ks.get(key)?;
        }
        if let Some(t0) = t0 {
          lat.push(t0.elapsed().as_nanos() as u64);
        }
      }
      Ok(lat)
    },
  )?;
  let fjall_conc_stat = wl.stats(
    FJALL,
    fjall_dur,
    fjall_lat,
    fjall.disk_usage(),
    fjall.memory_usage(),
  );

  results.push(ComparisonItem {
    wedb: wedb_conc_stat,
    bftree: bftree_conc_stat,
    rocksdb: rocks_conc_stat,
    fjall: fjall_conc_stat,
  });

  // 采样各存储引擎实际工作集常驻内存（在落盘 flush 之前采集，避免将已被清空的写缓冲与未预热缓存误判为零）
  let wedb_mem_bytes = wedb.memory_usage();
  let bftree_mem_bytes = bftree.memory_usage();
  let rocksdb_mem_bytes = rocksdb.memory_usage();
  let fjall_mem_bytes = fjall.memory_usage();
  let process_rss_bytes = get_process_rss_bytes();

  // 刷盘同步底层存储设备（落盘后精准统计实际磁盘物理占用）
  wedb.flush_all()?;
  bftree.flush_all()?;
  rocksdb.flush_all()?;
  fjall.flush_all()?;

  // 净数据集口径：实际唯一记录数（任务 D 仅计实际发生的追加写，并发段覆盖独立 1 万键空间）
  let total_unique_records =
    (seq_count + rand_count + RANGE_DATASET_SIZE + CONC_KEYSPACE) as u64 + task_d_writes as u64;
  let total_dataset_bytes = (seq_count * (SEQ_KEY_LEN + val_len)
    + rand_count * (RAND_KEY_LEN + val_len)
    + task_d_writes * (SEQ_KEY_LEN + val_len)
    + RANGE_DATASET_SIZE * (ORDER_KEY_LEN + val_len)
    + CONC_KEYSPACE * (CONC_KEY_LEN + val_len)) as u64;

  Ok(BenchmarkReport {
    options: options.clone(),
    items: results,
    total_dataset_bytes,
    total_unique_records,
    wedb_disk_bytes: wedb.disk_usage(),
    bftree_disk_bytes: bftree.disk_usage(),
    rocksdb_disk_bytes: rocksdb.disk_usage(),
    fjall_disk_bytes: fjall.disk_usage(),
    wedb_mem_bytes,
    bftree_mem_bytes,
    rocksdb_mem_bytes,
    fjall_mem_bytes,
    process_rss_bytes,
  })
}

/// 包含详细磁盘统计、内存占用与空间放大的综合评测报告
#[derive(Debug, Clone)]
pub struct BenchmarkReport {
  pub options: BenchOpt,
  pub items: Vec<ComparisonItem>,
  pub total_dataset_bytes: u64,
  /// 实际写入的唯一记录数（seq + rand + 任务 D 实际追加 + 范围数据集 + 并发键空间）
  pub total_unique_records: u64,
  pub wedb_disk_bytes: u64,
  pub bftree_disk_bytes: u64,
  pub rocksdb_disk_bytes: u64,
  pub fjall_disk_bytes: u64,
  pub wedb_mem_bytes: u64,
  pub bftree_mem_bytes: u64,
  pub rocksdb_mem_bytes: u64,
  pub fjall_mem_bytes: u64,
  pub process_rss_bytes: u64,
}

impl BenchmarkReport {
  /// 十项测试的加权几何平均权重（读/扫描与并发场景权重更高）
  pub const WEIGHTS: [f64; 10] = [1.0, 1.0, 1.5, 2.0, 1.5, 1.0, 1.0, 1.5, 1.0, 1.0];

  #[inline]
  fn compute_weighted_geomean<F>(&self, selector: F) -> (f64, f64, f64, f64)
  where
    F: Fn(&BenchStats) -> f64,
  {
    let mut wedb_log = 0.0f64;
    let mut bftree_log = 0.0f64;
    let mut rocks_log = 0.0f64;
    let mut fjall_log = 0.0f64;
    let mut total_weight = 0.0f64;

    for (item, w) in self.items.iter().zip(Self::WEIGHTS) {
      let w_val = selector(&item.wedb);
      let b_val = selector(&item.bftree);
      let r_val = selector(&item.rocksdb);
      let f_val = selector(&item.fjall);
      if w_val > 0.0 && b_val > 0.0 && r_val > 0.0 && f_val > 0.0 {
        wedb_log += w * w_val.ln();
        bftree_log += w * b_val.ln();
        rocks_log += w * r_val.ln();
        fjall_log += w * f_val.ln();
        total_weight += w;
      }
    }

    if total_weight == 0.0 {
      return (0.0, 0.0, 0.0, 0.0);
    }

    (
      (wedb_log / total_weight).exp(),
      (bftree_log / total_weight).exp(),
      (rocks_log / total_weight).exp(),
      (fjall_log / total_weight).exp(),
    )
  }

  /// 计算加权几何平均吞吐量 (QPS)
  #[inline]
  pub fn weighted_geomean(&self) -> (f64, f64, f64, f64) {
    self.compute_weighted_geomean(|s| s.ops_per_sec)
  }

  /// 计算加权几何平均吞吐量 (GB/s)
  #[inline]
  pub fn weighted_geomean_gb(&self) -> (f64, f64, f64, f64) {
    self.compute_weighted_geomean(|s| s.gb_per_sec)
  }
}
