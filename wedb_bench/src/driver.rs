//! 四引擎评测公共骨架：操作循环按各引擎 API 定制，计时窗口、字节统计与指标收口统一驱动

use std::time::{Duration, Instant};

use crate::{
  error::Result,
  stats::{BenchStats, MixedOp, OpTimer},
};

/// 单批评测的原始运行数据（逐操作延迟样本、传输字节与总耗时）
pub(crate) struct BatchRun {
  /// 逐操作纳秒延迟样本
  pub lat: Vec<u64>,
  /// 累计传输字节（读按 key+val 计，写按 key+val 计）
  pub bytes: u64,
  /// 整批评测总耗时
  pub duration: Duration,
}

/// 存储引擎评测驱动统一抽象
///
/// 逐操作方法由各引擎给出最小实现，批量计时骨架提供默认实现；
/// 引擎若有更优的原生批量路径（如 WeDB 流水线读、异步单 Runtime 批处理），
/// 直接覆写对应 `*_batch` 钩子即可，计时口径保持一致。
pub(crate) trait BenchEngine {
  /// 引擎显示名
  fn name(&self) -> &'static str;

  /// 磁盘占用采样（字节）
  fn disk(&self) -> u64;

  /// 内存占用采样（字节）
  fn mem(&self) -> u64;

  /// 单条点写
  fn put(&self, k: &[u8], v: &[u8]) -> Result<()>;

  /// 单条点读，返回值长（不存在为 0）
  fn get_len(&self, k: &[u8]) -> Result<usize>;

  /// 单条删除
  fn del(&self, k: &[u8]) -> Result<()>;

  /// 批量点写骨架：先计字节再计时，严格排除统计开销
  fn put_batch<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, pairs: &[(K, V)]) -> Result<BatchRun> {
    let mut run = BatchRun {
      lat: Vec::with_capacity(pairs.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start = Instant::now();
    for (k, v) in pairs {
      let (k, v) = (k.as_ref(), v.as_ref());
      run.bytes += (k.len() + v.len()) as u64;
      let timer = OpTimer::start();
      self.put(k, v)?;
      run.lat.push(timer.elapsed_ns());
    }
    run.duration = start.elapsed();
    Ok(run)
  }

  /// 批量点读骨架：逐条独立计时
  fn get_batch<K: AsRef<[u8]>>(&self, keys: &[K]) -> Result<BatchRun> {
    let mut run = BatchRun {
      lat: Vec::with_capacity(keys.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start = Instant::now();
    for k in keys {
      let k = k.as_ref();
      let timer = OpTimer::start();
      let len = self.get_len(k)?;
      run.lat.push(timer.elapsed_ns());
      run.bytes += (k.len() + len) as u64;
    }
    run.duration = start.elapsed();
    Ok(run)
  }

  /// 批量删除骨架
  fn del_batch<K: AsRef<[u8]>>(&self, keys: &[K]) -> Result<BatchRun> {
    let mut run = BatchRun {
      lat: Vec::with_capacity(keys.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start = Instant::now();
    for k in keys {
      let k = k.as_ref();
      run.bytes += k.len() as u64;
      let timer = OpTimer::start();
      self.del(k)?;
      run.lat.push(timer.elapsed_ns());
    }
    run.duration = start.elapsed();
    Ok(run)
  }

  /// 混合读写骨架（对齐 YCSB Task A / B / D 逐操作独立计时口径）
  fn mixed_batch<K: AsRef<[u8]>, V: AsRef<[u8]>>(&self, ops: &[MixedOp<K, V>]) -> Result<BatchRun> {
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
          let len = self.get_len(k)?;
          run.lat.push(timer.elapsed_ns());
          run.bytes += (k.len() + len) as u64;
        }
        MixedOp::Write(k, v) => {
          let (k, v) = (k.as_ref(), v.as_ref());
          run.bytes += (k.len() + v.len()) as u64;
          let timer = OpTimer::start();
          self.put(k, v)?;
          run.lat.push(timer.elapsed_ns());
        }
      }
    }
    run.duration = start.elapsed();
    Ok(run)
  }

  /// 统一指标收口：延迟样本 + 总时长 + 磁盘/内存采样 → BenchStats
  fn stats(&self, name: &str, run: BatchRun) -> BenchStats {
    BenchStats::compute(
      name,
      self.name(),
      run.lat,
      run.duration,
      run.bytes,
      self.disk(),
      self.mem(),
    )
  }
}

/// 有序范围扫描评测抽象（WeDB 走 ZSet 跳表切片专用路径，不实现此 trait）
pub(crate) trait RangeEngine: BenchEngine {
  /// 有序范围扫描 [start..=end]，返回扫描字节量
  fn scan(&self, start: &[u8], end: &[u8]) -> Result<u64>;

  /// 范围扫描骨架：按查询粒度计时
  fn scan_batch<K: AsRef<[u8]>>(&self, ranges: &[(K, K)]) -> Result<BatchRun> {
    let mut run = BatchRun {
      lat: Vec::with_capacity(ranges.len()),
      bytes: 0,
      duration: Duration::ZERO,
    };
    let start = Instant::now();
    for (start_k, end_k) in ranges {
      let timer = OpTimer::start();
      let scanned = self.scan(start_k.as_ref(), end_k.as_ref())?;
      run.lat.push(timer.elapsed_ns());
      run.bytes += scanned;
    }
    run.duration = start.elapsed();
    Ok(run)
  }
}

/// 批量写入评测驱动
pub(crate) fn bench_upsert_batch<E: BenchEngine + ?Sized, K: AsRef<[u8]>, V: AsRef<[u8]>>(
  engine: &E,
  name: &str,
  pairs: &[(K, V)],
) -> Result<BenchStats> {
  Ok(engine.stats(name, engine.put_batch(pairs)?))
}

/// 批量点读评测驱动
pub(crate) fn bench_read_batch<E: BenchEngine + ?Sized, K: AsRef<[u8]>>(
  engine: &E,
  name: &str,
  keys: &[K],
) -> Result<BenchStats> {
  Ok(engine.stats(name, engine.get_batch(keys)?))
}

/// 批量删除评测驱动
pub(crate) fn bench_delete_batch<E: BenchEngine + ?Sized, K: AsRef<[u8]>>(
  engine: &E,
  name: &str,
  keys: &[K],
) -> Result<BenchStats> {
  Ok(engine.stats(name, engine.del_batch(keys)?))
}

/// 混合读写评测驱动
pub(crate) fn bench_mixed_ops<E: BenchEngine + ?Sized, K: AsRef<[u8]>, V: AsRef<[u8]>>(
  engine: &E,
  name: &str,
  ops: &[MixedOp<K, V>],
) -> Result<BenchStats> {
  Ok(engine.stats(name, engine.mixed_batch(ops)?))
}

/// 有序范围扫描评测驱动
pub(crate) fn bench_range_queries<E: RangeEngine + ?Sized, K: AsRef<[u8]>>(
  engine: &E,
  name: &str,
  ranges: &[(K, K)],
) -> Result<BenchStats> {
  Ok(engine.stats(name, engine.scan_batch(ranges)?))
}
