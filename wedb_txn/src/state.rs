/// 事务状态机枚举（对标 Garnet `TxnState.cs`：None / Started / Running / Aborted）
///
/// 状态迁移全部收敛于 [`crate::manager::TransactionManager`] 内部：
/// - `None → Started`：[`crate::manager::TransactionManager::multi`]；
/// - `Started → Running`：EXEC 准备校验通过（[`crate::manager::TransactionManager::begin_run`]）；
/// - `Started / Running → Aborted`：入队阶段发生错误（[`crate::manager::TransactionManager::abort`]）；
/// - `任意 → None`：提交 / 冲突 / 放弃后的重置（[`crate::manager::TransactionManager::reset`]）。
///
/// 唯一的例外是 [`crate::manager::TransactionManager::promote_to_transaction`]，
/// 对标 Garnet `PromoteToTransaction`：即便处于 Aborted 也强制置为 Running（内部事务）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default, bitcode::Encode, bitcode::Decode)]
#[repr(u8)]
pub enum TxnState {
  /// 初始状态：未开启事务
  #[default]
  None = 0,
  /// 已开启状态：事务已开启，后续命令进入队列缓冲（对标 Garnet Started / Redis MULTI）
  Started = 1,
  /// 运行中状态：事务执行中，正在逐条执行已缓存的命令（对标 Garnet Running / Redis EXECUTING）
  Running = 2,
  /// 中止状态：缓冲阶段发生错误，执行时将直接中止丢弃（对标 Garnet Aborted）
  Aborted = 3,
}

impl TxnState {
  /// 是否处于未开启状态
  #[inline]
  pub const fn is_none(self) -> bool {
    matches!(self, Self::None)
  }

  /// 是否处于已开启状态
  #[inline]
  pub const fn is_started(self) -> bool {
    matches!(self, Self::Started)
  }

  /// 是否处于运行中状态
  #[inline]
  pub const fn is_running(self) -> bool {
    matches!(self, Self::Running)
  }

  /// 是否处于中止状态
  #[inline]
  pub const fn is_aborted(self) -> bool {
    matches!(self, Self::Aborted)
  }

  /// 是否处于事务流程中（非空闲状态）
  #[inline]
  pub const fn is_in_txn(self) -> bool {
    !self.is_none()
  }

  /// 是否跳过命令的即时执行（已开启或已中止状态下命令仅缓冲排队，对标 Garnet `IsSkippingOperations`）
  #[inline]
  pub const fn is_skipping_operations(self) -> bool {
    matches!(self, Self::Started | Self::Aborted)
  }
}

/// 状态谓词单元测试（迁移逻辑由管理器集成测试覆盖）
#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn state_predicates() {
    assert!(TxnState::None.is_none());
    assert!(!TxnState::None.is_in_txn());
    assert!(!TxnState::None.is_skipping_operations());

    assert!(TxnState::Started.is_started());
    assert!(TxnState::Started.is_in_txn());
    assert!(TxnState::Started.is_skipping_operations());

    assert!(TxnState::Running.is_running());
    assert!(TxnState::Running.is_in_txn());
    assert!(!TxnState::Running.is_skipping_operations());

    assert!(TxnState::Aborted.is_aborted());
    assert!(TxnState::Aborted.is_in_txn());
    assert!(TxnState::Aborted.is_skipping_operations());

    // 默认态必须为 None，bitcode 往返保持判别值
    assert_eq!(TxnState::default(), TxnState::None);
    let decoded: TxnState = bitcode::decode(&bitcode::encode(&TxnState::Aborted)).unwrap();
    assert_eq!(decoded, TxnState::Aborted);
  }
}
