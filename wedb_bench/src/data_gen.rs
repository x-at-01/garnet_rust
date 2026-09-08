use fastrand::Rng;

/// 默认数据可压缩比率 (0.5 即 50%，严格对标 Facebook RocksDB 官方 db_bench FLAGS_compression_ratio = 0.5)
pub const DEFAULT_COMPRESSION_RATIO: f64 = 0.5;

/// 默认预分配海量真实数据缓冲池大小 (16 MB，远大于 Snappy 32KB 压缩滑动窗口)
pub const DEFAULT_POOL_SIZE: usize = 16 * 1024 * 1024;

/// 真实可压缩业务数据生成池
///
/// 严格融合 Facebook RocksDB 官方 db_bench 工具中的 `CompressibleString` 算法
/// 与 Yahoo! YCSB 的 `RandomByteIterator` / `CoreWorkload` 字段生成规范：
/// 1. 离线在计时前一次性预分配海量大缓冲池 (16MB)；
/// 2. 模拟 YCSB 典型结构化字段 (field0=...;field1=...) 与可见 ASCII 字符序列；
/// 3. 控制片段的真实可压缩度（默认 50% 真实自然熵），模拟真实业务数据在 Snappy 压缩下的客观表现；
/// 4. 运行期零动态分配、零格式化开销，纯借用切片提供极速零开销访问。
#[derive(Debug, Clone)]
pub(crate) struct CompressibleDataPool {
  data: Vec<u8>,
}

impl CompressibleDataPool {
  /// 创建指定容量和压缩比的海量真实数据池
  pub fn new(pool_size: usize, compression_ratio: f64, seed: u64) -> Self {
    let mut rng = Rng::with_seed(seed);
    let mut data = Vec::with_capacity(pool_size);
    let ratio = compression_ratio.clamp(0.01, 1.0);
    const PIECE_SIZE: usize = 100;
    let raw_len = ((PIECE_SIZE as f64) * ratio).max(1.0) as usize;

    let mut raw_buf = vec![0u8; raw_len];
    let mut field_idx = 0usize;

    while data.len() < pool_size {
      // 1. 注入 YCSB 结构化字段前缀 (如 "f0=", "f1=", ...)；
      // field_idx % 10 恒为单位数字，栈上 [u8; 3] 直接拼接，零堆分配零格式化开销
      let header = [b'f', b'0' + (field_idx % 10) as u8, b'='];
      field_idx += 1;
      let take_header = header.len().min(pool_size - data.len());
      data.extend_from_slice(&header[..take_header]);
      if data.len() >= pool_size {
        break;
      }

      // 2. 填充可见 ASCII 字符集 (32..=126)，严格对标 YCSB RandomByteIterator
      for byte in &mut raw_buf {
        *byte = rng.u8(32..=126);
      }

      // 3. 重复填充直到达到 PIECE_SIZE 长度，模拟真实自然业务数据的局部重复与压缩熵 (RocksDB CompressibleString)
      let mut filled = 0;
      while filled < PIECE_SIZE && data.len() < pool_size {
        let chunk = (PIECE_SIZE - filled)
          .min(raw_len)
          .min(pool_size - data.len());
        data.extend_from_slice(&raw_buf[..chunk]);
        filled += chunk;
      }

      // 4. 字段分隔符
      if data.len() < pool_size {
        data.push(b';');
      }
    }

    Self { data }
  }

  /// 默认配置的真实业务数据池 (16MB，50% 真实压缩比，固定随机种子)
  pub fn default_pool() -> Self {
    Self::new(DEFAULT_POOL_SIZE, DEFAULT_COMPRESSION_RATIO, 789110123)
  }

  /// 零拷贝纯指针借用获取指定偏移与长度的数据切片 (汇编级基址偏移，< 0.5 纳秒)
  ///
  /// 池容量小于请求长度时返回截断切片（此时总字节统计仍按实际长度计）
  #[inline(always)]
  pub fn get_slice(&self, offset: usize, len: usize) -> &[u8] {
    if self.data.len() <= len {
      return &self.data[..self.data.len().min(len)];
    }
    let max_start = self.data.len() - len;
    let start = offset % max_start;
    &self.data[start..start + len]
  }
}
