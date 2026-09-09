//! AOF 门面（对标 C# `GarnetLog` 门面 + `StoreWrapper.AppendOnlyFile`）
//!
//! 持有可选 waof 日志与提交策略：引擎写监听（hlog 效果 + BfTree 效果）同栈
//! 追加帧，恢复重放经 [`AofReplayer`] 应用，checkpoint 后截断已覆盖前缀。

use std::{
  path::Path,
  sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
  },
};

use log::{error, info, warn};
use waof::{AofScanIterator, WalConfig, WalLog};
use wbftree::BfTreeListenerFn;
use wdev::SegmentedDevice;
use wkv::{RangeIndexListenerFn, WedbStore, WriteListenerFn};

use crate::{
  Result,
  frame::{self, AofOp},
  replayer::AofReplayer,
};

/// AOF 环形内存缓冲容量（64 MiB）
pub const DEFAULT_AOF_BUFFER_SIZE: usize = 64 * 1024 * 1024;

/// AOF 分段存储文件单段大小（64 MB；与环形缓冲容量数值巧合、语义无关——
/// 前者是磁盘段文件大小，后者是提交前内存驻留窗，且显式覆盖 waof 默认 16MB）
const AOF_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;

/// AOF 数据文件名
pub const AOF_FILE_NAME: &str = "append.aof";

/// 复制流推流回调端口（对标 C# `AofSyncTask` 的流消费端）
///
/// 每帧成功入队后同栈触发（参数为帧字节）。主侧把帧喂进复制积压缓冲，
/// 推送任务自积压缓冲推送网络；回调位于写入热路径，必须无阻塞。
pub type ReplicationSinkFn = Arc<dyn Fn(&[u8]) + Send + Sync>;

/// AOF 门面：持有可选 waof 日志与提交策略，提供写监听适配、提交、恢复重放与 checkpoint 截断
pub struct AofLog {
  /// 底层 waof 日志（未启用 AOF 时为 None）
  wal: Option<Arc<WalLog<SegmentedDevice>>>,
  /// 提交策略毫秒数（对标 Garnet CommitFrequencyMs 三档：0=每批应答前提交；
  /// >0=周期毫秒提交；-1=仅 SAVE/停机时提交）
  commit_ms: i64,
  /// enqueue 失败计数（环形满且未及时 commit 释放时的可观测降级）
  dropped_writes: Arc<AtomicU64>,
  /// 提交失败计数（设备持续故障时量化静默丢失窗口）
  commit_failures: Arc<AtomicU64>,
  /// 复制流推流端口（宿主经 [`Self::set_replication_sink`] 注入；
  /// Arc 包装使监听闭包可在注入后注册仍能观察到端口）
  replication_sink: Arc<std::sync::OnceLock<ReplicationSinkFn>>,
}

impl AofLog {
  /// 打开 AOF 日志（含崩溃恢复位点扫描，对标 Garnet RecoverAOFAsync 的元数据恢复）
  pub async fn open(dir: &Path, commit_ms: i64) -> Result<Self> {
    let device = Arc::new(SegmentedDevice::new(
      dir.join(AOF_FILE_NAME),
      Some(AOF_SEGMENT_SIZE),
      4096,
    )?);
    let wal = WalLog::open(device, WalConfig::new(DEFAULT_AOF_BUFFER_SIZE)).await?;
    Ok(Self {
      wal: Some(Arc::new(wal)),
      commit_ms,
      dropped_writes: Arc::new(AtomicU64::new(0)),
      commit_failures: Arc::new(AtomicU64::new(0)),
      replication_sink: Arc::new(std::sync::OnceLock::new()),
    })
  }

  /// 未启用 AOF 的空门面
  pub fn disabled() -> Self {
    Self {
      wal: None,
      commit_ms: -1,
      dropped_writes: Arc::new(AtomicU64::new(0)),
      commit_failures: Arc::new(AtomicU64::new(0)),
      replication_sink: Arc::new(std::sync::OnceLock::new()),
    }
  }

  /// 是否启用
  #[inline]
  pub fn is_enabled(&self) -> bool {
    self.wal.is_some()
  }

  /// 周期提交间隔毫秒（仅 `>0` 档返回 Some，供后台任务注册）
  #[inline]
  pub fn periodic_commit_ms(&self) -> Option<u64> {
    self
      .wal
      .is_some()
      .then_some(self.commit_ms)
      .filter(|ms| *ms > 0)
      .map(|ms| ms as u64)
  }

  /// 混合日志写监听适配器（注入 [`WedbStore::set_write_listener`] 端口）
  ///
  /// 在写入热路径同步执行：编帧 + 无锁预占入队。环形容量有限且回调无法
  /// 阻塞等待刷盘：环形满时该帧丢弃并计数告警。`commit_ms > 0` 档在写入
  /// 速率持续超过窗口时、`-1` 档在累计未提交超过窗口时必然触达——两档均
  /// 不保证完整持久性，要求持久请用 `0` 档
  pub fn write_listener(&self) -> Option<WriteListenerFn> {
    let enqueue = self.frame_enqueuer();
    Some(Arc::new(move |key: &[u8], val: &[u8], tombstone: bool| {
      let op = if tombstone {
        AofOp::Tombstone
      } else {
        AofOp::Upsert
      };
      enqueue(op, key, val);
    }))
  }

  /// 共享 BfTree 写监听适配器（注入 [`BfTreeService::set_write_listener`] 端口）
  ///
  /// 覆盖 Flattened ZSET 的 score 键值等 BfTree 唯一副本写效果
  /// （对标 C# AofEntryType.RangeIndexStreamChunk 的专用帧思路）
  pub fn bftree_listener(&self) -> Option<BfTreeListenerFn> {
    let enqueue = self.frame_enqueuer();
    Some(Arc::new(move |key: &[u8], val: &[u8], delete: bool| {
      let op = if delete {
        AofOp::BfTreeDelete
      } else {
        AofOp::BfTreePut
      };
      enqueue(op, key, val);
    }))
  }

  /// 帧入队闭包工厂：编帧 + 无锁入队，失败计数与频控告警（两类 listener 共用）
  #[inline]
  fn frame_enqueuer(&self) -> impl Fn(AofOp, &[u8], &[u8]) + Send + Sync + use<> {
    let wal = self.wal.as_ref().map(Arc::clone);
    let dropped = Arc::clone(&self.dropped_writes);
    let sink = Arc::clone(&self.replication_sink);
    move |op, key, val| {
      let Some(wal) = &wal else { return };
      let frame = frame::encode_frame(op, key, val);
      match wal.enqueue(&frame) {
        Ok(_) => {
          if let Some(sink) = sink.get() {
            sink(&frame);
          }
        }
        Err(e) => {
          let n = dropped.fetch_add(1, Ordering::Relaxed) + 1;
          // 首次与 2 的幂次告警，防日志风暴
          if n == 1 || n.is_power_of_two() {
            warn!("AOF 入队失败（{e}），累计={n}");
          }
        }
      }
    }
  }

  /// 注入复制流推流端口（帧入队成功后同栈回调；重复注入返回 false）
  pub fn set_replication_sink(&self, sink: ReplicationSinkFn) -> bool {
    self.replication_sink.set(sink).is_ok()
  }

  /// RangeIndex 写监听适配器（注入 [`WedbStore::set_range_listener`] 端口）
  ///
  /// RangeIndex 树文件中的字段是唯一副本（独立于共享 BfTree 与 hlog），
  /// 帧携带索引名 + field/value 编码，对标 C# RangeIndexStreamChunk 语义
  pub fn range_listener(&self) -> Option<RangeIndexListenerFn> {
    let enqueue = self.frame_enqueuer();
    Some(Arc::new(
      move |key: &[u8], field: &[u8], value: &[u8], delete: bool| {
        let op = if delete {
          AofOp::RangeIndexDelete
        } else {
          AofOp::RangeIndexSet
        };
        let val = frame::encode_range_val(field, value);
        enqueue(op, key, &val);
      },
    ))
  }

  /// 每批应答前提交落盘（仅 `0` 档生效，对标 Garnet AofAutoCommit：
  /// 一次批刷盘确认整条 pipeline 的写效果，确保应答即持久）
  ///
  /// 提交失败仅告警不阻断应答（数据仍在缓冲，下一批提交重试）
  pub async fn commit_before_response(&self) {
    if self.commit_ms == 0
      && let Some(wal) = &self.wal
      && let Err(e) = wal.commit().await
    {
      warn!("AOF 应答前提交失败（数据仍在缓冲，下批重试）: {e}");
    }
  }

  /// 无条件提交全部已入队写效果至持久化（返回已提交位点）
  ///
  /// 周期任务与停机路径使用
  pub async fn commit(&self) -> Result<u64> {
    match &self.wal {
      Some(wal) => {
        let res = wal.commit().await;
        if res.is_err() {
          let n = self.commit_failures.fetch_add(1, Ordering::Relaxed) + 1;
          if n == 1 || n.is_power_of_two() {
            error!("AOF 提交连续失败（应答照常，持久窗口扩大）: 第 {n} 次");
          }
        }
        Ok(res?)
      }
      None => Ok(0),
    }
  }

  /// 当前累计提交失败次数
  pub fn commit_failures(&self) -> u64 {
    self.commit_failures.load(Ordering::Relaxed)
  }

  /// checkpoint 覆盖位点（创建检查点前采样；None = 未启用）
  ///
  /// 采样时刻所有已入存储面的写效果，其 AOF 帧均不晚于本位点
  /// （listener 与引擎写同栈顺序），checkpoint ⊆ [begin, 本位点) 的重放区间
  pub fn covered_address(&self) -> Option<u64> {
    self.wal.as_ref().map(|wal| wal.tail_address())
  }

  /// checkpoint 完成后物理截断已覆盖前缀（对标 Garnet checkpoint 后 TruncateUntil）
  ///
  /// `truncate` 内部钳制到已提交位点，未提交帧不会被误删
  pub async fn truncate_covered(&self, covered: u64) -> Result<()> {
    if let Some(wal) = &self.wal {
      wal.truncate(covered).await?;
    }
    Ok(())
  }

  /// 崩溃恢复重放：扫描已提交 AOF 帧，经 [`AofReplayer`] 按帧类型应用回存储引擎
  ///
  /// 必须在注入写监听之前调用（重放写不二次进入 AOF），对标 Garnet ReplayAOF。
  /// 返回重放帧数
  pub async fn recover_and_replay(&self, store: &Arc<WedbStore<SegmentedDevice>>) -> Result<u64> {
    let Some(wal) = &self.wal else {
      return Ok(0);
    };
    let committed = wal.committed_until_address();
    if committed == wal.begin_address() {
      return Ok(0);
    }

    let replayer = AofReplayer::new(store)?;
    let mut iter = wal.scan_committed();
    let mut count = 0u64;
    while let Some(rec) = iter.next().await? {
      replayer.apply(&rec.payload).await?;
      count += 1;
    }
    info!("AOF 恢复重放完成: 重放 {count} 帧, 提交位点={committed:#x}");
    Ok(count)
  }

  /// 已提交位点（None = 未启用）；复制全量同步以 [begin, committed) 为权威区间
  pub fn committed_address(&self) -> Option<u64> {
    self.wal.as_ref().map(|wal| wal.committed_until_address())
  }

  /// 已提交帧顺序扫描迭代器（复制全量同步直推用；None = 未启用）
  pub fn scan_committed_frames(&self) -> Option<AofScanIterator<SegmentedDevice>> {
    let wal = self.wal.as_ref()?;
    Some(wal.scan_committed())
  }

  /// 从节点摄取：主节点推来的效果帧写入本地日志（帧头由本地确定性重算，
  /// 与主侧逐字节一致；对标 C# 从侧 UnsafeEnqueueRaw 的保真落盘语义）
  ///
  /// 返回本地起始逻辑地址，调用方须校验位点连续性
  pub async fn replica_ingest(&self, frame: &[u8]) -> Result<u64> {
    match &self.wal {
      Some(wal) => Ok(wal.enqueue(frame)?),
      None => Ok(0),
    }
  }

  /// 从节点全量重同步前的本地日志重置（位点回退至起始；物理段文件保留，
  /// 全量帧随后覆写前缀——从侧场景下 reset 的崩溃复活窗口可接受）
  pub async fn reset(&self) -> Result<()> {
    match &self.wal {
      Some(wal) => Ok(wal.reset().await?),
      None => Ok(()),
    }
  }

  /// 当前累计入队失败次数
  pub fn dropped_writes(&self) -> u64 {
    self.dropped_writes.load(Ordering::Relaxed)
  }
}
