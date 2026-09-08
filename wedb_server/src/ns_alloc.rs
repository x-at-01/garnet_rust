//! 名字空间分配器 (NamespaceAllocator，对标 doc/zh/ns.md 五)
//!
//! 控制面原子单调自增分配器：`NS 0` 自动分配与 `NS N` 手动指定统一经此处推进水位，
//! 持久化键为 BfTree 系统元数据 `BfTag::NextNamespace`（物理键 `[0x20]`，Val: 8B 大端 next_id）。
//!
//! 崩溃一致性：先落盘 `next_id = current + 1`，成功后才提交内存水位并授予 `current`，
//! 宕机重启加载到的高水位保证后续分配绝不回退碰撞。

use std::sync::Arc;

use log::{info, warn};
use parking_lot::Mutex;
use wbftree::{BfTreeInsertResult, BfTreeReadResult, BfTreeService};
use wrecord::BfTag;

use crate::error::{Error, Result};

/// 持久化水位初始值（预留 1 作为主业务库，首个自动分配值为 2）
const INITIAL_WATERMARK: u64 = 2;

/// 名字空间控制面分配器
pub struct NamespaceAllocator {
  /// 串行落盘临界区保护的水位（值语义 = 下一个可用分配值 next_id）
  disk_watermark: Mutex<u64>,
  /// BfTree 系统元数据引擎（水位落盘目标）
  bftree: Arc<BfTreeService>,
}

impl NamespaceAllocator {
  /// 从 BfTree 恢复水位；`[0x20]` 不存在时初始化写入 2（预留 1 作为主业务库）
  pub fn from_bftree(bftree: Arc<BfTreeService>) -> Result<Self> {
    let key = BfTag::NextNamespace.prefix();
    let mut buf = [0u8; 8];
    let (res, len) = bftree.read_into(&key, &mut buf);
    let watermark = if res == BfTreeReadResult::Found && len == 8 {
      u64::from_be_bytes(buf)
    } else {
      Self::persist(bftree.as_ref(), INITIAL_WATERMARK)?;
      info!("名字空间水位初始化为 {INITIAL_WATERMARK} (预留 1 作为主业务库)");
      INITIAL_WATERMARK
    };
    Ok(Self {
      disk_watermark: Mutex::new(watermark),
      bftree,
    })
  }

  /// 当前水位快照（下一个可用分配值）
  #[inline]
  pub fn watermark(&self) -> u64 {
    *self.disk_watermark.lock()
  }

  /// 自动分配模式：授予当前值并同步落盘推进水位（`ACL SETUSER ... NS 0` 专用）
  pub fn allocate(&self) -> Result<u64> {
    let mut wm = self.disk_watermark.lock();
    if *wm > wedb_acl::MAX_TENANT_NAMESPACE {
      return Err(Error::Custom("名字空间已耗尽，无法再分配新租户".into()));
    }
    let granted = *wm;
    *wm = granted + 1;
    if let Err(e) = Self::persist(self.bftree.as_ref(), *wm) {
      // 落盘失败回滚内存水位，杜绝内存态领先持久态造成重启后 ID 碰撞
      *wm = granted;
      return Err(e);
    }
    Ok(granted)
  }

  /// 手动指定模式：`NS N` 高于水位时推进水位至 `N + 1`，杜绝后续自增碰撞
  pub fn advance_to(&self, n: u64) -> Result<()> {
    let mut wm = self.disk_watermark.lock();
    if n >= *wm {
      let next = n + 1;
      Self::persist(self.bftree.as_ref(), next)?;
      *wm = next;
    }
    Ok(())
  }

  /// 水位同步落盘至 `[0x20]`（8 字节大端 next_id）
  fn persist(bftree: &BfTreeService, next_id: u64) -> Result<()> {
    let res = bftree.insert(&BfTag::NextNamespace.prefix(), &next_id.to_be_bytes());
    if res != BfTreeInsertResult::Success {
      warn!("持久化名字空间水位失败: res={res:?}, next_id={next_id}");
      return Err(Error::Custom(format!("持久化名字空间水位失败: {res:?}")));
    }
    Ok(())
  }
}
