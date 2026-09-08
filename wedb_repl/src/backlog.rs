/// 默认复制积压缓冲区容量 (1MB)
pub const DEFAULT_BACKLOG_SIZE: usize = 1024 * 1024;
/// 复制积压缓冲区环形队列 (Replication Backlog)
///
/// 严格对标 Redis / Garnet Replication Backlog 规范：
/// - 内存定长环形队列，支持高频追加写入与 O(1) 循环覆盖
/// - 记录主节点全局复制偏移量与积压有效历史区间
/// - 为从节点短时间断线重连提供超高速增量追赶，避免物理快照全量传输
#[derive(Debug)]
pub struct ReplicationBacklog {
  /// 环形定长存储缓冲区
  buf: Box<[u8]>,
  /// 环形缓冲区容量大小
  capacity: usize,
  /// 当前下一个写入位置的环形索引 (0..capacity)
  write_idx: usize,
  /// 环形缓冲区当前包含的数据有效总长度 (0..capacity)
  hist_len: usize,
  /// 主节点当前累计已产生日志的全局复制绝对位点 (master_repl_offset)
  master_repl_offset: u64,
}

impl Default for ReplicationBacklog {
  fn default() -> Self {
    Self::new(DEFAULT_BACKLOG_SIZE)
  }
}

impl ReplicationBacklog {
  /// 创建指定容量的复制积压缓冲区
  pub fn new(capacity: usize) -> Self {
    let cap = capacity.max(1024);
    Self {
      buf: vec![0u8; cap].into_boxed_slice(),
      capacity: cap,
      write_idx: 0,
      hist_len: 0,
      master_repl_offset: 0,
    }
  }

  /// 获取缓冲区当前容量
  #[inline]
  pub const fn capacity(&self) -> usize {
    self.capacity
  }

  /// 获取当前有效积压历史长度
  #[inline]
  pub const fn hist_len(&self) -> usize {
    self.hist_len
  }

  /// 获取主节点最新全局复制偏移量
  #[inline]
  pub const fn master_offset(&self) -> u64 {
    self.master_repl_offset
  }

  /// 设置当前绝对复制偏移量
  #[inline]
  pub fn set_master_offset(&mut self, offset: u64) {
    self.master_repl_offset = offset;
  }

  /// 获取当前积压缓冲区中包含的最早有效字节位点
  #[inline]
  pub const fn first_byte_offset(&self) -> u64 {
    self.master_repl_offset.saturating_sub(self.hist_len as u64)
  }

  /// 判断指定绝对位点是否依然在积压缓冲区的有效范围内
  #[inline]
  pub const fn is_offset_in_range(&self, req_offset: u64) -> bool {
    if self.hist_len == 0 {
      return false;
    }
    let first = self.first_byte_offset();
    req_offset >= first && req_offset <= self.master_repl_offset
  }

  /// 向积压缓冲区追加写入数据流（利用环形双段切片拷贝，零额外内存分配）
  pub fn feed(&mut self, data: &[u8]) {
    let len = data.len();
    if len == 0 {
      return;
    }

    if len >= self.capacity {
      // 写入量超出整个缓冲区容量，仅保留末尾 capacity 字节
      let start = len - self.capacity;
      self.buf.copy_from_slice(&data[start..]);
      self.write_idx = 0;
      self.hist_len = self.capacity;
      self.master_repl_offset += len as u64;
      return;
    }

    let space_to_end = self.capacity - self.write_idx;
    if len <= space_to_end {
      self.buf[self.write_idx..self.write_idx + len].copy_from_slice(data);
      self.write_idx += len;
      if self.write_idx == self.capacity {
        self.write_idx = 0;
      }
    } else {
      self.buf[self.write_idx..].copy_from_slice(&data[..space_to_end]);
      let remain = len - space_to_end;
      self.buf[..remain].copy_from_slice(&data[space_to_end..]);
      self.write_idx = remain;
    }

    self.hist_len = (self.hist_len + len).min(self.capacity);
    self.master_repl_offset += len as u64;
  }

  /// 从指定起始绝对位点以零拷贝借用环形缓冲区中的连续切片（最多包含一段或两段切片）
  #[inline]
  pub fn slices(&self, start_offset: u64, max_bytes: usize) -> (&[u8], &[u8]) {
    if max_bytes == 0 || !self.is_offset_in_range(start_offset) {
      return (&[], &[]);
    }

    // start_offset 距环形写入头的回退距离；is_offset_in_range 已保证 back_dist <= hist_len <= capacity，
    // 因此无需取模回绕，且可读长度上限即为该回退距离
    let back_dist = (self.master_repl_offset - start_offset) as usize;
    let to_read = max_bytes.min(back_dist);
    if to_read == 0 {
      return (&[], &[]);
    }

    let ring_start = (self.write_idx + self.capacity - back_dist) % self.capacity;

    let space_to_end = self.capacity - ring_start;
    if to_read <= space_to_end {
      (&self.buf[ring_start..ring_start + to_read], &[])
    } else {
      let remain = to_read - space_to_end;
      (&self.buf[ring_start..], &self.buf[..remain])
    }
  }

  /// 从指定位点读取一段数据为字节向量（基于切片免零初始化精准预分配）
  pub fn read_bytes(&self, start_offset: u64, max_bytes: usize) -> Vec<u8> {
    let (s1, s2) = self.slices(start_offset, max_bytes);
    let total = s1.len() + s2.len();
    if total == 0 {
      return Vec::new();
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(s1);
    out.extend_from_slice(s2);
    out
  }

  /// 清空积压缓冲区
  pub fn clear(&mut self) {
    self.write_idx = 0;
    self.hist_len = 0;
  }
}
