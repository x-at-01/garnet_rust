//! RangeIndex 检查点快照与流式复制同步模块 (1:1 对标 Garnet RangeIndexFileDataSink, RangeIndexFileDataSource 与 RangeIndexManager.Replication)

use std::{
  fs::{self, File},
  io::{self, Read, Write},
  iter::repeat_n,
  path::PathBuf,
  str::{from_utf8, from_utf8_unchecked},
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicU8, Ordering},
  },
};

use arrayvec::ArrayVec;
use itoa::Buffer;
use log::{info, warn};
use parking_lot::{Mutex, RwLock};
use wdev::Device;
use wedb_cluster::{ClusterConfig, hash_slot};
use whasher::{HashMap, new_hash_map};
use wkv::{RangeIndexChunkedDeserializer, RangeIndexError, RangeIndexManager, StoreSession};

use crate::error::{Error, Result};

const ERR_RECEIVE_STATE_DISPOSED: &str = "接收状态机已释放";
const ERR_SLOT_NOT_IMPORTING: &str = "槽位非导入状态";
const ERR_DISPOSED_BEFORE_PUBLISH: &str = "发布前状态机已释放";

/// 键哈希十六进制字符串定长字节数 (32 字节 ASCII)
pub const KEY_HASH_LEN: usize = 32;
/// 逻辑地址定长字节数 (8 字节小端无符号/有符号整数)
pub const ADDRESS_LEN: usize = 8;
/// 刷盘元数据定长字节数 (40 字节 = 32 字节 key_hash + 8 字节 address)
pub const FLUSH_METADATA_LEN: usize = KEY_HASH_LEN + ADDRESS_LEN;
/// 默认流复制块大小 (64KB, 1:1 对标 Garnet DefaultMigrationChunkSize / ReplicationChunkSize)
pub const DEFAULT_CHUNK_SIZE: usize = 64 * 1024;

/// 检查点与流复制文件类型标识 (1:1 对标 Garnet CheckpointFileType)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, bitcode::Encode, bitcode::Decode)]
#[repr(u8)]
pub enum CheckpointFileType {
  /// 无
  None = 0,
  /// 主存储 HybridLog 日志
  StoreHlog = 1,
  /// 对象存储 HybridLog 日志
  StoreHlogObj = 2,
  /// 主存储哈希索引
  StoreIndex = 4,
  /// 主存储快照
  StoreSnapshot = 5,
  /// 对象存储快照
  StoreSnapshotObj = 6,
  /// RangeIndex 逐次刷盘快照文件 (*.flush.bftree)
  StoreRangeIndexFlush = 7,
  /// RangeIndex 检查点快照文件 (*.bftree)
  StoreRangeIndexSnapshot = 8,
}

impl CheckpointFileType {
  /// 从原生字节反序列化为枚举
  pub const fn from_u8(val: u8) -> Option<Self> {
    match val {
      0 => Some(Self::None),
      1 => Some(Self::StoreHlog),
      2 => Some(Self::StoreHlogObj),
      4 => Some(Self::StoreIndex),
      5 => Some(Self::StoreSnapshot),
      6 => Some(Self::StoreSnapshotObj),
      7 => Some(Self::StoreRangeIndexFlush),
      8 => Some(Self::StoreRangeIndexSnapshot),
      _ => None,
    }
  }

  /// 转换为对应原生字节
  #[inline]
  pub const fn to_u8(self) -> u8 {
    self as u8
  }
}

/// 从节点 RangeIndex 文件分块接收写入器 (1:1 对标 Garnet RangeIndexFileDataSink)
///
/// 负责接收主节点在全量复制时下发的文件切片，写入本地 `.tmp` 临时文件，
/// 经 64KB 缓冲聚合避免频繁系统调用与单块小刷盘，并在校验完成后原子重命名至最终工作数据文件。
pub struct RangeIndexFileDataSink {
  /// 文件类型 (Flush 或 Snapshot)
  pub file_type: CheckpointFileType,
  /// 检查点快照 Token
  pub token: u128,
  /// 目标文件最终磁盘路径
  pub file_path: PathBuf,
  /// 写入期间的临时文件路径 (以 .tmp 结尾)
  pub tmp_path: PathBuf,
  /// 聚合写入缓冲器 (64KB 容量对标 Garnet 复制切片大小，避免频繁系统调用与小单块刷盘)
  writer: Option<io::BufWriter<File>>,
  /// 当前预期写入的流偏移位置 (用于严格按序校验)
  pub current_position: u64,
  /// 是否已成功落盘并完成重命名
  pub completed: bool,
}

impl RangeIndexFileDataSink {
  /// 根据元数据协议构造写入接收器 (1:1 对标 Garnet RangeIndexFileDataSink.FromMetadata)
  ///
  /// - `STORE_RANGEINDEX_FLUSH`: `keyHash` (32B ASCII) + `address` (8B LE) = 40 字节
  /// - `STORE_RANGEINDEX_SNAPSHOT`: `keyHash` (32B ASCII) = 32 字节
  pub fn from_metadata(
    file_type: CheckpointFileType,
    token: u128,
    metadata: &[u8],
    ri_manager: &RangeIndexManager,
  ) -> Result<Self> {
    if metadata.len() < KEY_HASH_LEN {
      let len = metadata.len();
      return Err(Error::Protocol(format!(
        "RangeIndex 元数据长度过短 ({len} 字节)，最少需要 {KEY_HASH_LEN} 字节"
      )));
    }

    let key_hash_str = from_utf8(&metadata[..KEY_HASH_LEN])
      .map_err(|e| Error::Protocol(format!("无效的 keyHash 编码: {e}")))?;

    let file_path = match file_type {
      CheckpointFileType::StoreRangeIndexFlush => {
        if metadata.len() < FLUSH_METADATA_LEN {
          let len = metadata.len();
          return Err(Error::Protocol(format!(
            "RangeIndex flush 元数据长度过短 ({len} 字节)，需要 {FLUSH_METADATA_LEN} 字节"
          )));
        }
        let addr_bytes: [u8; 8] = unsafe {
          metadata[KEY_HASH_LEN..FLUSH_METADATA_LEN]
            .try_into()
            .unwrap_unchecked()
        };
        let address = i64::from_le_bytes(addr_bytes);
        ri_manager.log_flush_path(key_hash_str, address)
      }
      CheckpointFileType::StoreRangeIndexSnapshot => {
        let mut b = Buffer::new();
        let token_str = b.format(token);
        ri_manager.checkpoint_snapshot_path(token_str, key_hash_str)
      }
      _ => {
        return Err(Error::Protocol(format!(
          "不支持的 RangeIndex 文件类型: {file_type:?}"
        )));
      }
    };

    Self::from_path(file_type, token, file_path)
  }

  /// 直接从指定目标路径创建文件接收器（先写入 .tmp 临时文件，complete 时原子重命名）
  pub fn from_path(file_type: CheckpointFileType, token: u128, file_path: PathBuf) -> Result<Self> {
    let mut tmp_name = file_path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    if let Some(parent) = tmp_path.parent() {
      let _ = fs::create_dir_all(parent);
    }

    // 若残留有先前的未完成 .tmp 临时文件，先予以清理以保证干净写入
    let _ = fs::remove_file(&tmp_path);

    let file = File::create(&tmp_path)?;
    let writer = io::BufWriter::with_capacity(DEFAULT_CHUNK_SIZE, file);

    Ok(Self {
      file_type,
      token,
      file_path,
      tmp_path,
      writer: Some(writer),
      current_position: 0,
      completed: false,
    })
  }

  /// 写入一个数据切片并校验流偏移 (1:1 对标 Garnet WriteChunk)
  pub fn write_chunk(&mut self, start_address: u64, data: &[u8]) -> Result<()> {
    if self.completed {
      return Err(Error::Protocol("文件写入器已完成".into()));
    }

    let writer = self
      .writer
      .as_mut()
      .ok_or_else(|| Error::Protocol("文件写入器未就绪或已关闭".into()))?;

    if self.current_position != start_address {
      let curr = self.current_position;
      return Err(Error::Protocol(format!(
        "RangeIndexFileDataSink 偏移不一致: 预期 {curr}，实际分块起始位置 {start_address}"
      )));
    }

    writer.write_all(data)?;
    self.current_position += data.len() as u64;
    Ok(())
  }

  /// 完成写入，执行刷盘落盘并原子重命名为正式文件 (1:1 对标 Garnet Complete)
  ///
  /// 先保证落盘无误后再执行原子替换；若落盘或重命名失败，保留未完成状态以便 Drop 自动清理 .tmp
  pub fn complete(&mut self) -> Result<()> {
    if self.completed {
      return Ok(());
    }

    if let Some(mut writer) = self.writer.take() {
      writer.flush()?;
      let file = writer.into_inner().map_err(|e| Error::Io(e.into_error()))?;
      file.sync_all()?;
      drop(file);

      fs::rename(&self.tmp_path, &self.file_path)?;
      if let Some(parent) = self.file_path.parent()
        && let Ok(dir) = File::open(parent)
      {
        let _ = dir.sync_all();
      }
    }

    self.completed = true;
    info!(
      "RangeIndexFileDataSink: 成功完成写入 {:?} 至 {:?}",
      self.file_type, self.file_path
    );
    Ok(())
  }
}

impl Drop for RangeIndexFileDataSink {
  fn drop(&mut self) {
    if !self.completed {
      drop(self.writer.take());
      let _ = fs::remove_file(&self.tmp_path);
    }
  }
}

/// 主节点 RangeIndex 文件分块读取发送源 (1:1 对标 Garnet RangeIndexFileDataSource)
///
/// 负责读取本地快照或刷盘文件并切分为指定块大小发往复制从节点。
pub struct RangeIndexFileDataSource {
  /// 文件类型
  pub file_type: CheckpointFileType,
  /// 32 字节键哈希
  pub key_hash: String,
  /// 逻辑地址 (快照文件为 0)
  pub address: i64,
  /// 物理文件路径
  pub file_path: PathBuf,
  /// 读取的文件句柄
  file: File,
  /// 文件总字节数
  pub file_len: u64,
  /// 当前已读取的偏移
  pub current_offset: u64,
}

impl RangeIndexFileDataSource {
  /// 创建新的文件读取数据源
  pub fn new(
    file_type: CheckpointFileType,
    key_hash: impl Into<String>,
    address: i64,
    file_path: PathBuf,
  ) -> Result<Self> {
    let file = File::open(&file_path)?;
    let file_len = file.metadata()?.len();

    Ok(Self {
      file_type,
      key_hash: key_hash.into(),
      address,
      file_path,
      file,
      file_len,
      current_offset: 0,
    })
  }

  /// 获取用于握手与流分发的元数据协议头 (1:1 对标 Garnet GetMetadata)
  ///
  /// 栈上定容缓冲，零堆分配：32 字节 key_hash，flush 类型追加 8 字节逻辑地址
  pub fn get_metadata(&self) -> ArrayVec<u8, FLUSH_METADATA_LEN> {
    let mut meta = ArrayVec::new();
    let hash_bytes = self.key_hash.as_bytes();
    let len = hash_bytes.len().min(KEY_HASH_LEN);
    let _ = meta.try_extend_from_slice(&hash_bytes[..len]);
    if len < KEY_HASH_LEN {
      meta.extend(repeat_n(0, KEY_HASH_LEN - len));
    }

    if self.file_type == CheckpointFileType::StoreRangeIndexFlush {
      let _ = meta.try_extend_from_slice(&self.address.to_le_bytes());
    }
    meta
  }

  /// 读取下一个数据块并向前推进流偏移 (直接填充至调用方缓冲区，实现单次 I/O 与零冗余内存拷贝)
  ///
  /// 文件在传输中途被截断（读不满预期字节数）必须严格报错（1:1 对标 C# RangeIndexFileDataSource
  /// unexpected EOF 异常），绝不允许把残缺文件当作完整数据块发布到从节点造成主从数据不一致
  pub fn read_next_chunk(&mut self, buf: &mut [u8]) -> io::Result<usize> {
    let remaining = self.file_len.saturating_sub(self.current_offset);
    if remaining == 0 {
      return Ok(0);
    }
    let to_read = (remaining as usize).min(buf.len());
    let mut read_total = 0;
    while read_total < to_read {
      let n = self.file.read(&mut buf[read_total..to_read])?;
      if n == 0 {
        return Err(io::Error::new(
          io::ErrorKind::UnexpectedEof,
          format!(
            "RangeIndexFileDataSource 意外 EOF: 偏移 {} 处期望再读 {} 字节，实际已读 {} 字节",
            self.current_offset, to_read, read_total
          ),
        ));
      }
      read_total += n;
    }
    self.current_offset += read_total as u64;
    Ok(read_total)
  }

  /// 数据流是否已完全读取完毕
  #[inline]
  pub fn is_complete(&self) -> bool {
    self.current_offset >= self.file_len
  }
}

/// RangeIndex 检查点快照与刷盘数据读取迭代器 (1:1 对标 Garnet RangeIndexSnapshotReader)
///
/// 枚举指定检查点与 HybridLog 逻辑地址区间内的所有待传输文件 (*.flush.bftree 与 *.bftree)，
/// 构造对应的数据源列表，并复用 64KB 共享读取缓冲区进行分块读取与零拷贝迭代。
pub struct RangeIndexSnapshotReader {
  /// 待传输的数据源列表
  data_sources: Vec<RangeIndexFileDataSource>,
  /// 共享读取缓冲区 (避免每个文件单独分配，容量为 DEFAULT_CHUNK_SIZE 64KB)
  shared_buffer: Vec<u8>,
  /// 当前迭代的数据源索引
  current_source_idx: usize,
}

impl RangeIndexSnapshotReader {
  /// 创建新的 RangeIndex 快照读取器 (1:1 对标 Garnet RangeIndexSnapshotReader 构造函数)
  pub fn new(
    ri_manager: &RangeIndexManager,
    checkpoint_token: u128,
    hlog_start_address: i64,
    hlog_end_address: i64,
  ) -> Result<Self> {
    let mut b = Buffer::new();
    let token_str = b.format(checkpoint_token);
    let entries = ri_manager
      .enumerate_files_for_replication(token_str, hlog_start_address, hlog_end_address)
      .map_err(|e| Error::Protocol(format!("枚举待复制 RangeIndex 文件失败: {e}")))?;

    let mut data_sources = Vec::with_capacity(entries.len());
    for entry in entries {
      let file_type = if entry.is_flush_file {
        CheckpointFileType::StoreRangeIndexFlush
      } else {
        CheckpointFileType::StoreRangeIndexSnapshot
      };

      let source =
        RangeIndexFileDataSource::new(file_type, entry.key_hash, entry.address, entry.path)?;
      data_sources.push(source);
    }

    Ok(Self {
      data_sources,
      shared_buffer: vec![0u8; DEFAULT_CHUNK_SIZE],
      current_source_idx: 0,
    })
  }

  /// 获取待传输的数据源列表切片
  #[inline]
  pub fn data_sources(&self) -> &[RangeIndexFileDataSource] {
    &self.data_sources
  }

  /// 获取共享读取缓冲区切片
  #[inline]
  pub fn shared_buffer(&self) -> &[u8] {
    &self.shared_buffer
  }

  /// 待传输文件总数
  #[inline]
  pub fn len(&self) -> usize {
    self.data_sources.len()
  }

  /// 是否没有待传输文件
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.data_sources.is_empty()
  }

  /// 当前正在读取的数据源索引
  #[inline]
  pub fn current_source_index(&self) -> usize {
    self.current_source_idx
  }

  /// 重置迭代器位点至首个数据源
  #[inline]
  pub fn reset(&mut self) {
    self.current_source_idx = 0;
  }

  /// 顺序拉取下一个文件分块 (零拷贝借用共享缓冲区切片，返回对应数据源引用与分块切片)
  pub fn next_chunk(&mut self) -> io::Result<Option<(&RangeIndexFileDataSource, &[u8])>> {
    while self.current_source_idx < self.data_sources.len() {
      let source = unsafe { self.data_sources.get_unchecked_mut(self.current_source_idx) };
      if source.is_complete() {
        self.current_source_idx += 1;
        continue;
      }
      let n = source.read_next_chunk(&mut self.shared_buffer)?;
      if n == 0 {
        self.current_source_idx += 1;
        continue;
      }
      let source_ref = unsafe { self.data_sources.get_unchecked(self.current_source_idx) };
      let chunk = &self.shared_buffer[..n];
      return Ok(Some((source_ref, chunk)));
    }
    Ok(None)
  }

  /// 遍历所有文件并逐块回调处理 (零拷贝借用共享缓冲区切片，单次连续迭代)
  pub fn for_each_chunk<F>(&mut self, mut f: F) -> io::Result<()>
  where
    F: FnMut(&RangeIndexFileDataSource, &[u8]) -> io::Result<()>,
  {
    while let Some((source, chunk)) = self.next_chunk()? {
      f(source, chunk)?;
    }
    Ok(())
  }

  /// 清空并释放所有数据源
  pub fn clear(&mut self) {
    self.data_sources.clear();
    self.current_source_idx = 0;
  }
}

/// AOF 流式增量复制分块重组器 (1:1 对标 Garnet RangeIndexManager.rangeIndexAofStreamReassembly 与 ProcessStreamChunk)
///
/// 当迁移或复杂命令通过 RangeIndexStream 流式传输时，从节点可能会交织接收到多个不同 key 的数据块。
/// 该重组器按键隔离反序列化状态，并在单个流完整接收后自动发布还原并注册至本地存储引擎。
pub struct RangeIndexStreamReassembler {
  /// 临时目录路径
  temp_dir: PathBuf,
  /// 正在重组中的按键反序列化器字典
  assemblers: HashMap<Vec<u8>, RangeIndexChunkedDeserializer>,
}

impl RangeIndexStreamReassembler {
  /// 创建新的流重组器
  pub fn new(temp_dir: impl Into<PathBuf>) -> Self {
    let temp_dir = temp_dir.into();
    let _ = fs::create_dir_all(&temp_dir);
    Self {
      temp_dir,
      assemblers: new_hash_map(),
    }
  }

  /// 获取当前正在进行重组的索引键数量
  #[inline]
  pub fn pending_count(&self) -> usize {
    self.assemblers.len()
  }

  /// 处理接收到的单个 RangeIndexStreamChunk (1:1 对标 Garnet ProcessStreamChunk)
  ///
  /// - `is_first`: 该块是否为该流的第一个数据块。若为 true 则丢弃该键的历史残余状态。
  /// - `is_last`: 该块是否标记为最后一个数据块。
  ///
  /// 若流重组完成并成功发布入库，返回 `Ok(true)`；若仍在接收后续分块，返回 `Ok(false)`。
  pub async fn process_chunk<D: Device>(
    &mut self,
    session: &StoreSession<D>,
    key: &[u8],
    chunk: &[u8],
    is_first: bool,
    is_last: bool,
  ) -> Result<bool> {
    if is_first {
      self.assemblers.remove(key);
    }

    let deserializer = match self.assemblers.get_mut(key) {
      Some(d) => d,
      None => {
        let rand_id = fastrand::u128(..);
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut hex_bytes = [0u8; 32];
        for (i, byte) in rand_id.to_le_bytes().iter().enumerate() {
          hex_bytes[i * 2] = HEX[(byte >> 4) as usize];
          hex_bytes[i * 2 + 1] = HEX[(byte & 0x0f) as usize];
        }
        let hex_str = unsafe { from_utf8_unchecked(&hex_bytes) };
        let temp_path = self.temp_dir.join(format!("reassemble_{hex_str}.bftree"));
        let deserializer = RangeIndexChunkedDeserializer::new(temp_path)
          .map_err(|e| Error::Protocol(format!("创建分块反序列化器失败: {e}")))?;
        self.assemblers.entry(key.to_vec()).or_insert(deserializer)
      }
    };

    let feed_ok = match deserializer.process_chunk(chunk) {
      Ok(ok) => ok,
      Err(e) => {
        self.assemblers.remove(key);
        let mut msg = String::from("处理分块数据失败: ");
        msg.push_str(&e.to_string());
        return Err(Error::Protocol(msg));
      }
    };

    if !feed_ok || deserializer.has_error() {
      self.assemblers.remove(key);
      let key_str = String::from_utf8_lossy(key);
      let mut msg = String::from("分块流校验失败或数据已损坏: key=");
      msg.push_str(&key_str);
      return Err(Error::Protocol(msg));
    }

    if deserializer.is_complete() {
      // 流已完全接收，立即从重组字典中移除该键的状态机，避免后续发布失败时发生状态泄漏
      if let Some(mut deserializer) = self.assemblers.remove(key) {
        // 发布已重组完毕的 RangeIndex 到本地存储引擎 (1:1 对标 Garnet PublishMigratedIndex)
        if let Err(e) = session
          .publish_migrated_range_index(key, deserializer.stub(), deserializer.temp_path(), false)
          .await
        {
          // 显式清理临时文件并避免状态泄漏 (1:1 对标 Garnet RemoveAndDisposeStreamReassembly(..., "PublishFailed"))
          deserializer.dispose();
          let mut msg = String::from("发布重组后的 RangeIndex 失败: ");
          msg.push_str(&e.to_string());
          return Err(Error::Protocol(msg));
        }
      }

      return Ok(true);
    }

    if is_last {
      self.assemblers.remove(key);
      let key_str = String::from_utf8_lossy(key);
      let mut msg = String::from("接收到流结束标志但反序列化器未达到完成状态: key=");
      msg.push_str(&key_str);
      return Err(Error::Protocol(msg));
    }

    Ok(false)
  }

  /// 释放并清理所有未完成的重组临时文件与状态
  pub fn clear(&mut self) {
    self.assemblers.clear();
  }
}

/// 协作式安全释放状态返回值 (1:1 对标 Garnet CooperativeDisposeGuard.DisposeResult)
#[derive(Debug, Clone, Copy, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub enum DisposeResult {
  /// 之前已经被其他调用释放过
  AlreadyDisposed,
  /// 标记已释放，但仍有工作线程在临界区内；清理工作推迟至最后一个工作线程退出时执行
  DeferredToWorker,
  /// 标记已释放且当前无活跃工作线程；调用方应立即执行清理
  CleanupNow,
}

/// 协作式无锁非阻塞安全释放守卫 (1:1 对标 Garnet CooperativeDisposeGuard)
///
/// 采用原子位掩码控制（高 1 位标记 DISPOSED，低 7 位记录活跃工作线程计数）：
/// 1. 彻底杜绝已释放状态下 `try_enter` 污染活跃位的问题；
/// 2. 原生支持多工作线程并发重入/并发执行，确保延迟清理在最后一个工作线程退出时触发；
/// 3. 全程基于无锁 CAS 循环，不阻塞任何网络或 I/O 线程。
#[derive(Debug, Default)]
pub struct CooperativeDisposeGuard {
  state: AtomicU8,
}

impl CooperativeDisposeGuard {
  const FLAG_DISPOSED: u8 = 1 << 7;
  const ACTIVE_COUNT_MASK: u8 = 0x7F;

  /// 创建初始守卫
  #[inline]
  pub const fn new() -> Self {
    Self {
      state: AtomicU8::new(0),
    }
  }

  /// 检查是否已被标记为释放
  #[inline]
  pub fn is_disposed(&self) -> bool {
    (self.state.load(Ordering::SeqCst) & Self::FLAG_DISPOSED) != 0
  }

  /// 尝试进入临界区。若已被释放则立即返回 false（不污染活跃计数）
  #[inline]
  pub fn try_enter(&self) -> bool {
    let mut current = self.state.load(Ordering::Acquire);
    loop {
      if (current & Self::FLAG_DISPOSED) != 0 {
        return false;
      }
      if (current & Self::ACTIVE_COUNT_MASK) == Self::ACTIVE_COUNT_MASK {
        return false;
      }
      match self.state.compare_exchange_weak(
        current,
        current + 1,
        Ordering::SeqCst,
        Ordering::Acquire,
      ) {
        Ok(_) => return true,
        Err(actual) => current = actual,
      }
    }
  }

  /// 退出临界区，并在释放态下由最后一个退出的活跃工作线程返回 true 执行延迟清理
  #[inline]
  pub fn exit_and_check_should_cleanup(&self) -> bool {
    let mut current = self.state.load(Ordering::Acquire);
    loop {
      let active = current & Self::ACTIVE_COUNT_MASK;
      debug_assert!(active > 0, "退出临界区时活跃计数必须大于 0");
      let next = (current & Self::FLAG_DISPOSED) | active.saturating_sub(1);
      match self
        .state
        .compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::Acquire)
      {
        Ok(_) => {
          return (current & Self::FLAG_DISPOSED) != 0 && active == 1;
        }
        Err(actual) => current = actual,
      }
    }
  }

  /// 尝试执行释放，返回决策动作
  #[inline]
  pub fn try_dispose(&self) -> DisposeResult {
    let mut current = self.state.load(Ordering::Acquire);
    loop {
      if (current & Self::FLAG_DISPOSED) != 0 {
        return DisposeResult::AlreadyDisposed;
      }
      let next = current | Self::FLAG_DISPOSED;
      match self
        .state
        .compare_exchange_weak(current, next, Ordering::SeqCst, Ordering::Acquire)
      {
        Ok(_) => {
          if (current & Self::ACTIVE_COUNT_MASK) > 0 {
            return DisposeResult::DeferredToWorker;
          }
          return DisposeResult::CleanupNow;
        }
        Err(actual) => current = actual,
      }
    }
  }
}

/// 槽位热迁移 RangeIndex 接收状态枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, bitcode::Encode, bitcode::Decode)]
pub enum ReceiveStatus {
  /// 空闲待命状态
  #[default]
  Idle,
  /// 正在接收并反序列化数据块
  Receiving,
}

/// 迁移分块解析产出状态
pub enum MigrationChunkStatus {
  /// 流接收中，尚有后续分块需等待
  InProgress,
  /// 流已全部接收完成并校验成功，产出发布元组
  Complete {
    key: Vec<u8>,
    stub: Vec<u8>,
    temp_path: PathBuf,
  },
}

/// 内部可变接收状态
struct ReceiveStateInner {
  current_deserializer: Option<RangeIndexChunkedDeserializer>,
  chunk_count: usize,
}

/// 守卫临界区退出的 ScopeGuard，确保延迟清理在 worker 退出时 100% 触发
struct DisposeScopeGuard<'a>(&'a RangeIndexMigrationReceiveState);

impl Drop for DisposeScopeGuard<'_> {
  fn drop(&mut self) {
    if self.0.dispose_guard.exit_and_check_should_cleanup() {
      self.0.dispose_internal();
    }
  }
}

/// 集群会话 RangeIndex 迁移分块数据接收状态机 (1:1 对标 Garnet RangeIndexMigrationReceiveState)
///
/// 遵循 IDLE -> RECEIVING -> IDLE 状态转移，支持在会话断开或安全并发 Dispose 时
/// 协作式无锁清理未完成状态与临时文件。
pub struct RangeIndexMigrationReceiveState {
  ri_manager: Arc<RangeIndexManager>,
  inner: Mutex<ReceiveStateInner>,
  dispose_guard: CooperativeDisposeGuard,
  has_pause_hook: AtomicBool,
  test_pause_hook: RwLock<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl RangeIndexMigrationReceiveState {
  /// 创建新的 RangeIndex 迁移接收状态机
  pub fn new(ri_manager: impl Into<Arc<RangeIndexManager>>) -> Self {
    Self {
      ri_manager: ri_manager.into(),
      inner: Mutex::new(ReceiveStateInner {
        current_deserializer: None,
        chunk_count: 0,
      }),
      dispose_guard: CooperativeDisposeGuard::new(),
      has_pause_hook: AtomicBool::new(false),
      test_pause_hook: RwLock::new(None),
    }
  }

  /// 当前是否正在接收数据流 (1:1 对标 Garnet IsReceiving)
  #[inline]
  pub fn is_receiving(&self) -> bool {
    self.inner.lock().current_deserializer.is_some()
  }

  /// 当前流已接收的数据分块数量 (1:1 对标 Garnet CurrentChunkCount)
  #[inline]
  pub fn current_chunk_count(&self) -> usize {
    self.inner.lock().chunk_count
  }

  /// 获取当前状态机状态
  #[inline]
  pub fn status(&self) -> ReceiveStatus {
    if self.is_receiving() {
      ReceiveStatus::Receiving
    } else {
      ReceiveStatus::Idle
    }
  }

  /// 处理接收到的 RangeIndexMigration 分块记录 (1:1 对标 Garnet ProcessRecord)
  ///
  /// - 首分块自动初始化反序列化器并创建临时文件；
  /// - 后续分块持续写入；
  /// - 完成时校验槽位处于 Importing 态，并自动调用 `publish_migrated_range_index` 注册发布。
  pub async fn process_record<D: Device>(
    &self,
    record_payload: &[u8],
    current_config: Option<&ClusterConfig>,
    session: Option<&StoreSession<D>>,
    replace_option: bool,
  ) -> Result<bool> {
    if !self.dispose_guard.try_enter() {
      return Err(Error::ObjectDisposed(ERR_RECEIVE_STATE_DISPOSED.into()));
    }

    let _guard = DisposeScopeGuard(self);
    self
      .process_record_internal(record_payload, current_config, session, replace_option)
      .await
  }

  /// 同步处理接收到的分块记录（适用于无 StoreSession 的单元测试与纯分块流处理）
  pub fn process_record_sync(
    &self,
    record_payload: &[u8],
    current_config: Option<&ClusterConfig>,
    replace_option: bool,
  ) -> Result<bool> {
    if !self.dispose_guard.try_enter() {
      return Err(Error::ObjectDisposed(ERR_RECEIVE_STATE_DISPOSED.into()));
    }

    let _guard = DisposeScopeGuard(self);
    self.process_record_sync_internal(record_payload, current_config, replace_option)
  }

  async fn process_record_internal<D: Device>(
    &self,
    record_payload: &[u8],
    current_config: Option<&ClusterConfig>,
    session: Option<&StoreSession<D>>,
    replace_option: bool,
  ) -> Result<bool> {
    let Some(status) = self.feed_chunk_internal(record_payload)? else {
      return Ok(false);
    };

    let (key, stub, temp_path) = match status {
      MigrationChunkStatus::InProgress => return Ok(true),
      MigrationChunkStatus::Complete {
        key,
        stub,
        temp_path,
      } => (key, stub, temp_path),
    };

    if !self.validate_slot_and_liveness(&key, current_config)? {
      return Ok(false);
    }

    // 调用 publish_migrated_range_index 落地存储
    if let Some(s) = session {
      match s
        .publish_migrated_range_index(&key, &stub, &temp_path, replace_option)
        .await
      {
        Ok(()) => {}
        Err(RangeIndexError::AlreadyExists) if !replace_option => {
          // Garnet 1:1: SkippedAlreadyExists 属于非错误正常跳过
        }
        Err(e) => {
          let mut msg = String::from("发布迁移 RangeIndex 失败: ");
          msg.push_str(&e.to_string());
          return self.handle_error(&msg);
        }
      }
    }

    self.reset();
    Ok(true)
  }

  fn process_record_sync_internal(
    &self,
    record_payload: &[u8],
    current_config: Option<&ClusterConfig>,
    _replace_option: bool,
  ) -> Result<bool> {
    let Some(status) = self.feed_chunk_internal(record_payload)? else {
      return Ok(false);
    };

    let key = match status {
      MigrationChunkStatus::InProgress => return Ok(true),
      MigrationChunkStatus::Complete { key, .. } => key,
    };

    if !self.validate_slot_and_liveness(&key, current_config)? {
      return Ok(false);
    }

    self.reset();
    Ok(true)
  }

  /// 校验槽位状态与活跃态，保证逻辑单点收敛
  fn validate_slot_and_liveness(
    &self,
    key: &[u8],
    current_config: Option<&ClusterConfig>,
  ) -> Result<bool> {
    let slot = hash_slot(key);
    if let Some(config) = current_config
      && !config.is_importing_slot(slot)
    {
      return self.handle_error(ERR_SLOT_NOT_IMPORTING);
    }

    if self.dispose_guard.is_disposed() {
      return self.handle_error(ERR_DISPOSED_BEFORE_PUBLISH);
    }

    Ok(true)
  }

  fn feed_chunk_internal(&self, record_payload: &[u8]) -> Result<Option<MigrationChunkStatus>> {
    // 空载荷与 C# 一致走 HandleError 软重置路径（重置状态并返回 false），不向上抛错
    if record_payload.is_empty() {
      return self.handle_error("分块载荷为空").map(|_| None);
    }

    let status = {
      let mut inner = self.inner.lock();
      if inner.current_deserializer.is_none() {
        let temp_path = self.ri_manager.derive_temp_migration_path();
        let deserializer = RangeIndexChunkedDeserializer::new(temp_path).map_err(|e| {
          let mut msg = String::from("创建分块反序列化器失败: ");
          msg.push_str(&e.to_string());
          Error::Protocol(msg)
        })?;
        inner.current_deserializer = Some(deserializer);
      }

      inner.chunk_count += 1;
      let deserializer = inner
        .current_deserializer
        .as_mut()
        .expect("反序列化器必须已就绪");

      let feed_ok = deserializer.process_chunk(record_payload).map_err(|e| {
        let mut msg = String::from("处理分块数据失败: ");
        msg.push_str(&e.to_string());
        Error::Protocol(msg)
      })?;

      if !feed_ok || deserializer.has_error() {
        Self::reset_locked(&mut inner);
        warn!("RangeIndexMigrationReceiveState 错误: 分块数据损坏或处理失败");
        return Ok(None);
      }

      if deserializer.is_complete() {
        MigrationChunkStatus::Complete {
          key: deserializer.key().to_vec(),
          stub: deserializer.stub().to_vec(),
          temp_path: deserializer.temp_path().to_path_buf(),
        }
      } else {
        MigrationChunkStatus::InProgress
      }
    };

    // 快速无锁检查测试钩子：生产热路径仅 1 个 Relaxed 内存访问，旁路绕过锁开销
    if self.has_pause_hook.load(Ordering::Relaxed)
      && let Some(ref hook) = *self.test_pause_hook.read()
    {
      hook();
    }

    Ok(Some(status))
  }

  fn handle_error(&self, msg: &str) -> Result<bool> {
    warn!("RangeIndexMigrationReceiveState 错误: {msg}");
    self.reset();
    Ok(false)
  }

  #[inline]
  fn reset_locked(inner: &mut ReceiveStateInner) {
    if let Some(mut d) = inner.current_deserializer.take() {
      d.dispose();
    }
    inner.chunk_count = 0;
  }

  /// 重置当前接收状态，清理反序列化器与未完成的临时文件
  pub fn reset(&self) {
    Self::reset_locked(&mut self.inner.lock());
  }

  /// 释放资源 (1:1 对标 Garnet IDisposable.Dispose)
  pub fn dispose(&self) {
    if self.dispose_guard.try_dispose() == DisposeResult::CleanupNow {
      self.dispose_internal();
    }
  }

  fn dispose_internal(&self) {
    self.reset();
  }

  /// 设置测试暂停回调钩子 (用于模拟多线程竞争与延迟释放测试)
  #[doc(hidden)]
  pub fn set_test_pause_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
    self.has_pause_hook.store(hook.is_some(), Ordering::SeqCst);
    *self.test_pause_hook.write() = hook;
  }
}

impl Drop for RangeIndexMigrationReceiveState {
  fn drop(&mut self) {
    self.dispose();
  }
}
