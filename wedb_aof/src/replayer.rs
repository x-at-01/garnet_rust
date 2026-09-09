//! AOF 重放器（对标 C# `AofProcessor.ReplayOp`：按帧类型分发应用到存储面）

use std::sync::Arc;

use wdev::Device;
use wkv::{StoreSession, WedbStore};

use crate::{Error, Result, frame};

/// 把 AOF 帧逐条应用回存储引擎的重放器
///
/// 独立于恢复入口存在：standalone 启动恢复与复制从侧重放共用同一条
/// 「帧 → 存储面」路径，保证重放语义单一真源
pub struct AofReplayer<D: Device> {
  /// 共享 BfTree 主实例（BfTree 帧的应用目标）
  store: Arc<WedbStore<D>>,
  /// 混合日志重放会话（Upsert/Tombstone 帧的应用目标）
  session: StoreSession<D>,
}

impl<D: Device> AofReplayer<D> {
  /// 创建重放器（须在注入写监听之前使用，重放写不二次进入 AOF）
  pub fn new(store: &Arc<WedbStore<D>>) -> Result<Self> {
    Ok(Self {
      store: Arc::clone(store),
      session: store.new_session()?,
    })
  }

  /// 解码并应用单帧
  pub async fn apply(&self, frame_payload: &[u8]) -> Result<()> {
    let (op, key, val) = frame::decode_frame(frame_payload)?;
    match op {
      frame::AofOp::Upsert => {
        self.session.upsert_raw(key, val).await?;
      }
      frame::AofOp::Tombstone => {
        self.session.delete_raw(key).await?;
      }
      frame::AofOp::BfTreePut => {
        // 非 Success（空 val 等）说明帧语义损坏，fail-fast 拒绝启动而非静默丢 score
        if self.store.bftree.insert(key, val) != wbftree::BfTreeInsertResult::Success {
          return Err(Error::Frame(format!(
            "BfTreePut 重放失败: key={key:?} len={}",
            val.len()
          )));
        }
      }
      frame::AofOp::BfTreeDelete => {
        if self.store.bftree.delete(key) != wbftree::BfTreeDeleteResult::Success {
          return Err(Error::Frame(format!("BfTreeDelete 重放失败: key={key:?}")));
        }
      }
    }
    Ok(())
  }
}
