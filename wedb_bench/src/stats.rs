//! 基准统计指标、延迟分位数与跨平台资源采样

use std::{
  fmt::{self, Display},
  fs,
  path::Path,
  time::{Duration, Instant},
};

/// GB 换算字节数
pub(crate) const BYTES_IN_GB: f64 = (1024 * 1024 * 1024) as f64;
/// MB 换算字节数
pub(crate) const BYTES_IN_MB: f64 = (1024 * 1024) as f64;
/// 纳秒/微秒换算系数
pub(crate) const NANOS_PER_MICRO: f64 = 1_000.0;

/// 零堆分配 GB/s 格式化显示包装类型
#[derive(Debug, Clone, Copy)]
pub(crate) struct GbVal(pub f64);

impl Display for GbVal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let val = self.0;
    if !val.is_finite() || val <= 0.0 {
      f.write_str("0.0000 GB/s")
    } else if val >= 1.0 {
      write!(f, "{val:.2} GB/s")
    } else if val >= 0.01 {
      write!(f, "{val:.3} GB/s")
    } else {
      write!(f, "{val:.4} GB/s")
    }
  }
}

/// 零堆分配 MB 格式化显示包装类型
#[derive(Debug, Clone, Copy)]
pub(crate) struct MbVal(pub f64);

impl Display for MbVal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let val = self.0;
    if !val.is_finite() || val <= 0.0 {
      f.write_str("0.00 MB")
    } else if val >= 100.0 {
      write!(f, "{val:.1} MB")
    } else {
      write!(f, "{val:.2} MB")
    }
  }
}

/// 零堆分配空间放大倍率格式化显示包装类型
#[derive(Debug, Clone, Copy)]
pub(crate) struct SpaceAmpVal(pub f64);

impl Display for SpaceAmpVal {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    let val = self.0;
    if !val.is_finite() || val <= 0.0 {
      f.write_str("0.00x")
    } else {
      write!(f, "{val:.2}x")
    }
  }
}

/// 基准评测结果统计指标
#[derive(Debug, Clone)]
pub struct BenchStats {
  /// 测试名称
  pub name: Box<str>,
  /// 存储引擎名称
  pub engine: Box<str>,
  /// 总操作次数
  pub total_ops: usize,
  /// 总数据载荷量（字节）
  pub total_bytes: u64,
  /// 总耗时
  pub duration: Duration,
  /// 吞吐量（ops/sec）
  pub ops_per_sec: f64,
  /// 吞吐量（GB/s）
  pub gb_per_sec: f64,
  /// P50 延迟（微秒）
  pub p50_us: f64,
  /// P95 延迟（微秒）
  pub p95_us: f64,
  /// P99 延迟（微秒）
  pub p99_us: f64,
  /// 平均延迟（微秒）
  pub avg_us: f64,
  /// 磁盘占用字节数
  pub disk_bytes: u64,
  /// 内存占用字节数
  pub mem_bytes: u64,
}

impl BenchStats {
  /// 计算一组操作耗时的统计指标
  pub fn compute(
    name: impl Into<Box<str>>,
    engine: impl Into<Box<str>>,
    mut latencies_ns: Vec<u64>,
    duration: Duration,
    total_bytes: u64,
    disk_bytes: u64,
    mem_bytes: u64,
  ) -> Self {
    let total_ops = latencies_ns.len();
    let secs = duration.as_secs_f64();
    if total_ops == 0 || secs <= 0.0 {
      return Self {
        name: name.into(),
        engine: engine.into(),
        total_ops: 0,
        total_bytes: 0,
        duration,
        ops_per_sec: 0.0,
        gb_per_sec: 0.0,
        p50_us: 0.0,
        p95_us: 0.0,
        p99_us: 0.0,
        avg_us: 0.0,
        disk_bytes,
        mem_bytes,
      };
    }

    latencies_ns.sort_unstable();

    let total_ns: u128 = latencies_ns.iter().copied().map(u128::from).sum();
    let avg_us = (total_ns as f64) / (total_ops as f64) / NANOS_PER_MICRO;
    let ops_per_sec = (total_ops as f64) / secs;
    let gb_per_sec = (total_bytes as f64) / secs / BYTES_IN_GB;

    let p50_idx = (total_ops / 2).min(total_ops - 1);
    let p95_idx = (((total_ops as u64).saturating_mul(95) / 100) as usize).min(total_ops - 1);
    let p99_idx = (((total_ops as u64).saturating_mul(99) / 100) as usize).min(total_ops - 1);

    // 安全：索引均受 min(total_ops - 1) 严格约束且 total_ops > 0
    let p50_us = (unsafe { *latencies_ns.get_unchecked(p50_idx) } as f64) / NANOS_PER_MICRO;
    let p95_us = (unsafe { *latencies_ns.get_unchecked(p95_idx) } as f64) / NANOS_PER_MICRO;
    let p99_us = (unsafe { *latencies_ns.get_unchecked(p99_idx) } as f64) / NANOS_PER_MICRO;

    Self {
      name: name.into(),
      engine: engine.into(),
      total_ops,
      total_bytes,
      duration,
      ops_per_sec,
      gb_per_sec,
      p50_us,
      p95_us,
      p99_us,
      avg_us,
      disk_bytes,
      mem_bytes,
    }
  }
}

/// 跨平台获取当前进程的物理常驻内存大小（Resident Set Size, RSS，单位：字节）
#[cfg(target_os = "macos")]
pub fn get_process_rss_bytes() -> u64 {
  use std::mem::{MaybeUninit, size_of};

  unsafe extern "C" {
    fn mach_task_self() -> libc::mach_port_t;
  }

  unsafe {
    let mut info = MaybeUninit::<libc::mach_task_basic_info>::uninit();
    let mut count = (size_of::<libc::mach_task_basic_info>() / size_of::<libc::natural_t>())
      as libc::mach_msg_type_number_t;
    let kerr = libc::task_info(
      mach_task_self(),
      libc::MACH_TASK_BASIC_INFO,
      info.as_mut_ptr().cast(),
      &mut count,
    );
    if kerr == libc::KERN_SUCCESS {
      let info = info.assume_init();
      info.resident_size
    } else {
      0
    }
  }
}

/// 跨平台获取当前进程的物理常驻内存大小（Resident Set Size, RSS，单位：字节）
#[cfg(target_os = "linux")]
pub fn get_process_rss_bytes() -> u64 {
  use std::{fs::File, io::Read, sync::OnceLock};

  static PAGE_SIZE: OnceLock<u64> = OnceLock::new();
  let page_size = *PAGE_SIZE.get_or_init(|| {
    let sz = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    if sz > 0 { sz as u64 } else { 4096 }
  });

  // 零堆分配：使用 64 字节栈缓冲区读取 /proc/self/statm
  let mut buf = [0u8; 64];
  let Ok(mut f) = File::open("/proc/self/statm") else {
    return 0;
  };
  let Ok(n) = f.read(&mut buf) else {
    return 0;
  };
  if n == 0 {
    return 0;
  }

  // 单趟解析第 2 个字段 (resident pages)，杜绝 String 堆分配与 split_ascii_whitespace 迭代器开销
  let s = &buf[..n];
  let mut idx = 0;
  while idx < n && s[idx] != b' ' && s[idx] != b'\t' {
    idx += 1;
  }
  while idx < n && (s[idx] == b' ' || s[idx] == b'\t') {
    idx += 1;
  }
  let mut resident_pages = 0u64;
  let mut has_digit = false;
  while idx < n && s[idx].is_ascii_digit() {
    resident_pages = resident_pages
      .saturating_mul(10)
      .saturating_add((s[idx] - b'0') as u64);
    has_digit = true;
    idx += 1;
  }

  if has_digit {
    resident_pages * page_size
  } else {
    0
  }
}

/// 跨平台获取当前进程的物理常驻内存大小（Resident Set Size, RSS，单位：字节）
#[cfg(target_os = "windows")]
pub fn get_process_rss_bytes() -> u64 {
  #[repr(C)]
  struct ProcessMemoryCounters {
    cb: u32,
    page_fault_count: u32,
    peak_working_set_size: usize,
    working_set_size: usize,
    quota_peak_paged_pool_usage: usize,
    quota_paged_pool_usage: usize,
    quota_peak_non_paged_pool_usage: usize,
    quota_non_paged_pool_usage: usize,
    pagefile_usage: usize,
    peak_pagefile_usage: usize,
  }
  unsafe extern "system" {
    fn GetCurrentProcess() -> *mut std::ffi::c_void;
    fn K32GetProcessMemoryInfo(
      process: *mut std::ffi::c_void,
      counters: *mut ProcessMemoryCounters,
      cb: u32,
    ) -> i32;
  }
  unsafe {
    let mut pmc = std::mem::MaybeUninit::<ProcessMemoryCounters>::zeroed();
    let cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    (*pmc.as_mut_ptr()).cb = cb;
    if K32GetProcessMemoryInfo(GetCurrentProcess(), pmc.as_mut_ptr(), cb) != 0 {
      pmc.assume_init().working_set_size as u64
    } else {
      0
    }
  }
}

/// 跨平台获取当前进程的物理常驻内存大小（Resident Set Size, RSS，单位：字节）
#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub fn get_process_rss_bytes() -> u64 {
  0
}

/// 迭代获取指定路径或文件夹的实际磁盘占用大小（字节），避免递归栈溢出并消除符号链接循环引用
pub(crate) fn get_path_size(path: &Path) -> u64 {
  let Ok(meta) = fs::symlink_metadata(path) else {
    return 0;
  };
  if meta.is_file() {
    return meta.len();
  }
  if !meta.is_dir() {
    return 0;
  }

  let mut total = 0u64;
  let mut stack = Vec::with_capacity(16);
  stack.push(path.to_path_buf());

  while let Some(dir) = stack.pop() {
    let Ok(entries) = fs::read_dir(&dir) else {
      continue;
    };
    for entry in entries.flatten() {
      // 零额外 stat 开销：直接从目录项 dirent 提取文件类型
      let Ok(ft) = entry.file_type() else {
        continue;
      };
      if ft.is_dir() && !ft.is_symlink() {
        stack.push(entry.path());
      } else if ft.is_file()
        && let Ok(m) = entry.metadata()
      {
        total += m.len();
      }
    }
  }

  total
}

/// 纳秒高精度计时器辅助结构
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpTimer {
  start: Instant,
}

impl Default for OpTimer {
  #[inline(always)]
  fn default() -> Self {
    Self::start()
  }
}

impl OpTimer {
  #[inline(always)]
  pub fn start() -> Self {
    Self {
      start: Instant::now(),
    }
  }

  #[inline(always)]
  pub fn elapsed_ns(&self) -> u64 {
    self.start.elapsed().as_nanos() as u64
  }
}

/// 混合读写操作原子单元（对齐 rust-storage-bench / YCSB 标准操作模型）
#[derive(Debug, Clone, Copy)]
pub enum MixedOp<K, V> {
  Read(K),
  Write(K, V),
}

/// 计算空间放大系数 (Space Amplification: 磁盘物理占用字节数 / 有效数据集净字节数)
#[inline]
pub(crate) fn calc_space_amp(disk_bytes: u64, dataset_bytes: u64) -> f64 {
  if dataset_bytes == 0 {
    0.0
  } else {
    (disk_bytes as f64) / (dataset_bytes as f64)
  }
}

/// 将 f64 按指定小数精度格式化后追加到字符串
///
/// 使用 zmij 处理 NaN/inf 等特殊值，普通有限值采用缩放取整 + itoa 手动格式化，
/// 避免标准库 format! 的开销，同时保证精度可控。
#[inline]
pub(crate) fn push_f64_precise(s: &mut String, val: f64, prec: usize) {
  // 特殊值交给 zmij 处理
  if !val.is_finite() {
    let mut buf = zmij::Buffer::new();
    s.push_str(buf.format(val));
    return;
  }

  let negative = val < 0.0;
  let abs_val = if negative { -val } else { val };

  // 缩放后四舍五入到指定精度
  let multiplier = 10f64.powi(prec as i32);
  let scaled = (abs_val * multiplier).round();
  let total = scaled as u64;
  let int_part = total / multiplier as u64;
  let frac_part = total % multiplier as u64;

  let mut itoa_buf = itoa::Buffer::new();
  if negative {
    s.push('-');
  }
  s.push_str(itoa_buf.format(int_part));
  if prec > 0 {
    s.push('.');
    let frac_s = itoa_buf.format(frac_part);
    // 小数部分补前导零，确保固定位数
    for _ in 0..(prec - frac_s.len()) {
      s.push('0');
    }
    s.push_str(frac_s);
  }
}
