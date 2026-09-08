use core::iter::repeat_n;
use std::{fmt::Arguments, io::Write, mem, sync::Arc};

use compio::buf::{IntoInner, IoBuf, Slice};
use itoa::Buffer as ItoaBuffer;
use wedb_resp::{CRLF, OK, PONG, QUEUED, RESP2_NULL_ARRAY, RESP2_NULL_BULK};
use zmij::Buffer as ZmijBuffer;

use crate::{
  error::{Error, Result},
  pool::LimitedFixedBufferPool,
};

/// 2 的幂次定额池化直接映射网络接收缓冲区（严格对标 Garnet `transportReceiveBuffer` 与 `LimitedFixedBufferPool`）
///
/// 核心优势：
/// 1. 消除二次内存拷贝：操作系统/Compio 直接向池化切片追加写入，协议解析器就地零拷贝解析；
/// 2. 避免单连接无限制堆分配：内存块全部从 `LimitedFixedBufferPool` 租借并自动归还；
/// 3. 支持就地滑动对齐（Slide）与按需 2 的幂次翻倍扩容与缩容（Double/ShrinkNetworkReceiveBuffer）。
#[derive(Debug)]
pub struct PooledReceiveBuffer {
  pool: Arc<LimitedFixedBufferPool>,
  buf: Option<Vec<u8>>,
  head: usize,
  initial_size: usize,
  max_size: usize,
}

impl PooledReceiveBuffer {
  /// 创建基于定额缓冲池的池化接收缓冲区
  pub fn new(pool: Arc<LimitedFixedBufferPool>, initial_size: usize, max_size: usize) -> Self {
    let init_cap = initial_size.max(pool.min_allocation_size());
    let buf = pool.get(init_cap);
    Self {
      pool,
      buf: Some(buf),
      head: 0,
      initial_size: init_cap,
      max_size,
    }
  }

  /// 获取当前待解析的未消费数据切片
  #[inline(always)]
  pub fn unparsed_slice(&self) -> &[u8] {
    let Some(buf) = self.buf.as_ref() else {
      return &[];
    };
    let len = buf.len();
    if self.head >= len {
      &[]
    } else {
      // SAFETY: self.head < len 分支保证切片 100% 处于合法区间，消除编译器二次边界检查
      unsafe { buf.get_unchecked(self.head..len) }
    }
  }

  /// 当前待解析字节数
  #[inline(always)]
  pub fn unparsed_len(&self) -> usize {
    self
      .buf
      .as_ref()
      .map_or(0, |buf| buf.len().saturating_sub(self.head))
  }

  /// 标记已成功消费解析字节数
  #[inline(always)]
  pub fn advance(&mut self, bytes: usize) {
    self.head += bytes;
    debug_assert!(
      self.buf.as_ref().is_none_or(|b| self.head <= b.len()),
      "PooledReceiveBuffer::advance 越界: head={} > len={}",
      self.head,
      self.buf.as_ref().map_or(0, |b| b.len())
    );
  }

  /// 当前待解析切片是否为空
  #[inline(always)]
  pub fn is_empty(&self) -> bool {
    self.unparsed_len() == 0
  }

  /// 获取当前内部缓冲区的物理容量
  #[inline(always)]
  pub fn capacity(&self) -> usize {
    self.buf.as_ref().map_or(0, |b| b.capacity())
  }

  /// 将未消费数据就地滑动对齐到缓冲区头部并重置偏移指针
  #[inline]
  pub fn slide(&mut self) {
    if let Some(buf) = self.buf.as_mut() {
      if self.head == buf.len() {
        buf.clear();
        self.head = 0;
      } else if self.head > 0 {
        let len = buf.len();
        let unparsed = len - self.head;
        buf.copy_within(self.head..len, 0);
        buf.truncate(unparsed);
        self.head = 0;
      }
    }
  }

  /// 确保有足够用于下一次网络直读的剩余空闲容量
  pub fn ensure_read_capacity(&mut self, min_read_space: usize) -> Result<()> {
    let Some(buf) = self.buf.as_mut() else {
      return Ok(());
    };

    // 1. 全部消费完毕：清空并在容量足够时零开销重置
    if self.head == buf.len() {
      buf.clear();
      self.head = 0;
      if buf.capacity() >= min_read_space {
        return Ok(());
      }
    }

    // 2. 边界检查：若当前未读数据加上所需读取空间超出上限，立即报错阻断
    let (buf_len, buf_cap) = (buf.len(), buf.capacity());
    let needed = buf_len
      .saturating_sub(self.head)
      .saturating_add(min_read_space);
    if needed > self.max_size {
      return Err(Error::BufferOverflow(needed, self.max_size));
    }

    // 3. 若尾部可用空间不足且存在前导已读空间，执行就地滑动对齐
    if buf_cap - buf_len < min_read_space && self.head > 0 {
      let unparsed = buf_len - self.head;
      buf.copy_within(self.head..buf_len, 0);
      buf.truncate(unparsed);
      self.head = 0;
    }

    // 4. 滑动后若依然不足，按 2 的幂次翻倍扩容（整块换入换出池化容器）
    if buf.capacity() - buf.len() < min_read_space {
      let target_cap = (buf.capacity() * 2)
        .max(buf.len() + min_read_space)
        .min(self.max_size);

      let mut new_buf = self.pool.get(target_cap);
      new_buf.extend_from_slice(&buf[self.head..buf.len()]);

      let old_buf = mem::replace(buf, new_buf);
      self.pool.return_buffer(old_buf);
      self.head = 0;
    }

    Ok(())
  }

  /// 取出底层池化容器空闲尾部的切片视图，准备传给异步网络直读（compio `read`）
  ///
  /// 利用 `Slice<Vec<u8>>` 直指现有有效数据末尾，compio 直接由操作系统填入后续空闲空间，
  /// 并在读取完成后自动将底层 `Vec<u8>` 的长度原子拓展，实现跨数据帧接收的真正全链路零二次拷贝！
  #[inline(always)]
  pub fn take_for_read(&mut self) -> Slice<Vec<u8>> {
    let buf = self.buf.take().unwrap_or_default();
    let len = buf.len();
    buf.slice(len..)
  }

  /// 归还完成异步读取后的切片视图，恢复底层容器并更新状态
  #[inline(always)]
  pub fn put_after_read(&mut self, returned: Slice<Vec<u8>>) {
    self.buf = Some(returned.into_inner());
  }

  /// 在一批命令消费完毕后执行收尾压缩：
  /// - 若已全部消费完毕，重置 len=0, head=0；
  /// - 若容量膨胀且已清空，缩容归还大块回池中；
  /// - 若消费进度已超过一半，及时滑动对齐。
  pub fn compact(&mut self) {
    if let Some(buf) = self.buf.as_mut() {
      if self.head == buf.len() {
        if buf.capacity() > self.initial_size * 2 {
          let old_buf = mem::replace(buf, self.pool.get(self.initial_size));
          self.pool.return_buffer(old_buf);
        } else {
          buf.clear();
        }
        self.head = 0;
      } else if self.head > 0 && self.head >= buf.capacity() / 2 {
        self.slide();
      }
    }
  }
}

impl Drop for PooledReceiveBuffer {
  fn drop(&mut self) {
    if let Some(buf) = self.buf.take() {
      self.pool.return_buffer(buf);
    }
  }
}

/// 可变长度数组响应头预留占位字节数（'*' + 11 位十进制数字 + CRLF，覆盖至 99999999999 个元素）
const ARRAY_HEADER_RESERVED: usize = 14;
/// 数组头部预留占位符（'*' + ARRAY_HEADER_RESERVED-1 个空格，编译期绑定占位长度）
const ARRAY_HEADER_PLACEHOLDER: [u8; ARRAY_HEADER_RESERVED] = *b"*             ";

/// 批量写出发送缓冲区
///
/// 支持原生协议响应序列化编码，零拷贝整块输出并循环复用已分配内存块。
#[derive(Debug)]
pub struct SendBuffer {
  /// 输出字节流暂存区
  buf: Vec<u8>,
  /// 循环复用的空闲备用缓冲区
  spare: Option<Vec<u8>>,
  /// 基准分配容量
  capacity: usize,
}

impl SendBuffer {
  /// 创建新的发送缓冲区
  #[inline]
  pub fn new(capacity: usize) -> Self {
    Self {
      buf: Vec::with_capacity(capacity),
      spare: None,
      capacity,
    }
  }

  /// 缓冲区是否为空
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.buf.is_empty()
  }

  /// 获取当前已排队字节数
  #[inline]
  pub fn len(&self) -> usize {
    self.buf.len()
  }

  /// 取出当前填充好的缓冲区用于异步写出，优先复用已回收的备用容器以消除垃圾分配
  #[inline]
  pub fn take(&mut self) -> Vec<u8> {
    let next = self
      .spare
      .take()
      .unwrap_or_else(|| Vec::with_capacity(self.capacity));
    mem::replace(&mut self.buf, next)
  }

  /// 回收异步写出完毕的旧容器，以便重复利用堆内存，消除持续垃圾分配
  #[inline]
  pub fn recycle(&mut self, mut recycled: Vec<u8>) {
    recycled.clear();
    if recycled.capacity() > self.capacity * 4 {
      recycled.shrink_to(self.capacity);
    }
    self.spare = Some(recycled);
  }

  /// 清空当前发送缓冲区
  #[inline]
  pub fn clear(&mut self) {
    self.buf.clear();
  }

  /// 写入原始字节切片
  #[inline]
  pub fn write_raw(&mut self, bytes: &[u8]) {
    self.buf.extend_from_slice(bytes);
  }

  /// 写入状态成功响应
  #[inline]
  pub fn write_ok(&mut self) {
    self.buf.extend_from_slice(OK);
  }

  /// 写入心跳回复响应
  #[inline]
  pub fn write_pong(&mut self) {
    self.buf.extend_from_slice(PONG);
  }

  /// 写入事务排队成功响应
  #[inline]
  pub fn write_queued(&mut self) {
    self.buf.extend_from_slice(QUEUED);
  }

  /// 写入空字符串响应
  #[inline]
  pub fn write_null(&mut self) {
    self.buf.extend_from_slice(RESP2_NULL_BULK);
  }

  /// 写入空数组响应
  #[inline]
  pub fn write_null_array(&mut self) {
    self.buf.extend_from_slice(RESP2_NULL_ARRAY);
  }

  /// 写入简单字符串
  #[inline]
  pub fn write_simple_string(&mut self, s: &[u8]) {
    self.buf.reserve(1 + s.len() + 2);
    self.buf.push(b'+');
    self.buf.extend_from_slice(s);
    self.buf.extend_from_slice(CRLF);
  }

  /// 写入错误字符串
  #[inline]
  pub fn write_error(&mut self, err: &[u8]) {
    self.buf.reserve(1 + err.len() + 2);
    self.buf.push(b'-');
    self.buf.extend_from_slice(err);
    self.buf.extend_from_slice(CRLF);
  }

  /// 格式化写入错误字符串
  #[inline]
  pub fn write_error_fmt(&mut self, args: Arguments<'_>) {
    self.buf.reserve(64);
    self.buf.push(b'-');
    let _ = write!(&mut self.buf, "{args}");
    self.buf.extend_from_slice(CRLF);
  }

  /// 写入六十四位有符号整数
  #[inline]
  pub fn write_integer(&mut self, val: i64) {
    let mut num_buf = ItoaBuffer::new();
    let num_bytes = num_buf.format(val).as_bytes();
    self.buf.reserve(1 + num_bytes.len() + 2);
    self.buf.push(b':');
    self.buf.extend_from_slice(num_bytes);
    self.buf.extend_from_slice(CRLF);
  }

  /// 写入定长批量字符串
  #[inline]
  pub fn write_bulk_string(&mut self, payload: &[u8]) {
    let mut num_buf = ItoaBuffer::new();
    let num_bytes = num_buf.format(payload.len()).as_bytes();
    self
      .buf
      .reserve(1 + num_bytes.len() + 2 + payload.len() + 2);
    self.buf.push(b'$');
    self.buf.extend_from_slice(num_bytes);
    self.buf.extend_from_slice(CRLF);
    self.buf.extend_from_slice(payload);
    self.buf.extend_from_slice(CRLF);
  }

  /// 写入双精度浮点数定长批量字符串
  #[inline]
  pub fn write_double_bulk(&mut self, val: f64) {
    if val.is_nan() {
      self.write_bulk_string(b"nan");
      return;
    }
    if val.is_infinite() {
      if val.is_sign_positive() {
        self.write_bulk_string(b"inf");
      } else {
        self.write_bulk_string(b"-inf");
      }
      return;
    }
    let mut num_buf = ZmijBuffer::new();
    let s = num_buf.format_finite(val);
    self.write_bulk_string(s.as_bytes());
  }

  /// 写入数组头部
  #[inline]
  pub fn write_array_header(&mut self, len: usize) {
    let mut num_buf = ItoaBuffer::new();
    let num_bytes = num_buf.format(len).as_bytes();
    self.buf.reserve(1 + num_bytes.len() + 2);
    self.buf.push(b'*');
    self.buf.extend_from_slice(num_bytes);
    self.buf.extend_from_slice(CRLF);
  }

  /// 预留可变长度数组响应头占位符（用于流式输出未知总数的范围查询，消除中间 Vec 分配）
  /// 返回预留起始偏移量
  #[inline]
  pub fn start_array(&mut self) -> usize {
    let marker = self.buf.len();
    self.buf.extend_from_slice(&ARRAY_HEADER_PLACEHOLDER);
    marker
  }

  /// 完成可变长度数组响应，将实际元素计数回填入头部并将后续载荷紧凑前移对齐
  #[inline]
  pub fn finish_array(&mut self, marker: usize, count: usize) {
    let mut num_buf = ItoaBuffer::new();
    let num_bytes = num_buf.format(count).as_bytes();
    let header_len = 1 + num_bytes.len() + 2;
    let body_start = marker + ARRAY_HEADER_RESERVED;
    let body_end = self.buf.len();

    if header_len > ARRAY_HEADER_RESERVED {
      // 防御：元素数位数超出占位容量，在载荷前插入空隙扩宽头部（载荷随插入自然后移对齐）
      self.buf.splice(
        body_start..body_start,
        repeat_n(b' ', header_len - ARRAY_HEADER_RESERVED),
      );
    } else if header_len < ARRAY_HEADER_RESERVED {
      self
        .buf
        .copy_within(body_start..body_end, marker + header_len);
      self
        .buf
        .truncate(marker + header_len + (body_end - body_start));
    }
    self.buf[marker] = b'*';
    self.buf[marker + 1..marker + 1 + num_bytes.len()].copy_from_slice(num_bytes);
    self.buf[marker + 1 + num_bytes.len()] = b'\r';
    self.buf[marker + 2 + num_bytes.len()] = b'\n';
  }

  /// 写入发布订阅统一格式回复消息
  #[inline]
  pub fn write_sub_reply(&mut self, kind: &[u8], target: Option<&[u8]>, count: usize) {
    self.write_array_header(3);
    self.write_bulk_string(kind);
    match target {
      Some(t) => self.write_bulk_string(t),
      None => self.write_null(),
    }
    self.write_integer(count as i64);
  }
}
