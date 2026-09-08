use std::mem;

use bytes::Bytes;
use wedb_resp::RespCommand;

use super::{
  error::{Error, Result},
  key_entry::{LockType, TxnKeyEntries},
  queued_cmd::QueuedCommand,
  state::TxnState,
  version_map::WatchVersionMap,
  watched_keys::WatchedKeysContainer,
};

/// 事务准备阶段结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecPreparation {
  /// 准备成功：校验通过，返回待执行命令队列及死锁防御锁集合
  Success {
    /// 暂存的命令队列
    commands: Vec<QueuedCommand>,
    /// 经过死锁防御排序与合并后的锁条目集合
    key_entries: TxnKeyEntries,
  },
  /// 乐观锁冲突：监视的键被并发修改，放弃执行
  Conflict,
  /// 事务中止：入队期间存在错误，放弃执行
  Aborted,
}

/// 事务完整执行结果
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecResult<T> {
  /// 事务全部命令成功原子执行完毕
  Success(T),
  /// 乐观锁冲突，事务未执行
  Conflict,
  /// 事务入队错误已中止，事务未执行
  Aborted,
}

/// 内部准备阶段结论
#[derive(Debug)]
enum Prepared {
  /// 事务先前已中止，重置后直接丢弃
  Aborted,
  /// 受监视键版本冲突，重置后放弃执行
  Conflict,
  /// 校验通过，已进入运行态并移交命令队列
  Running(Vec<QueuedCommand>),
}

/// 会话级事务管理器
///
/// 实现 Redis 语义的 WATCH / MULTI / EXEC / DISCARD / UNWATCH 乐观锁事务：
/// - 排队阶段命令仅缓冲入队，键登记至死锁防御锁集合；
/// - EXEC 时统一校验受监视键版本，任一键被并发修改即整体放弃（返回 nil）；
/// - 提交时对全部排他锁键自增全局版本号，向其他会话广播失效通知。
#[derive(Debug, Default)]
pub struct TransactionManager {
  /// 当前事务状态机
  state: TxnState,
  /// 暂存的排队命令列表
  queue: Vec<QueuedCommand>,
  /// 当前会话受监视键容器
  watched_keys: WatchedKeysContainer,
  /// 本事务涉及的键锁条目集
  key_entries: TxnKeyEntries,
  /// 是否包含写入修改操作
  perform_writes: bool,
  /// 是否包含 FLUSHDB / FLUSHALL 等全局变异（提交时全表广播失效）
  invalidate_all: bool,
  /// 是否处于日志重放模式
  is_replaying: bool,
}

impl TransactionManager {
  /// 创建新的会话事务管理器
  #[inline]
  pub const fn new() -> Self {
    Self {
      state: TxnState::None,
      queue: Vec::new(),
      watched_keys: WatchedKeysContainer::new(),
      key_entries: TxnKeyEntries::new(),
      perform_writes: false,
      invalidate_all: false,
      is_replaying: false,
    }
  }

  /// 获取当前事务状态
  #[inline]
  pub const fn state(&self) -> TxnState {
    self.state
  }

  /// 是否正处于事务流程中（非空闲状态）
  #[inline]
  pub const fn is_in_txn(&self) -> bool {
    self.state.is_in_txn()
  }

  /// 是否处于跳过即时执行阶段（已开启或已中止）
  #[inline]
  pub const fn is_skipping_operations(&self) -> bool {
    self.state.is_skipping_operations()
  }

  /// 获取暂存的命令队列只读引用
  #[inline]
  pub fn queue(&self) -> &[QueuedCommand] {
    &self.queue
  }

  /// 获取当前已排队的命令数量
  #[inline]
  pub fn queued_count(&self) -> usize {
    self.queue.len()
  }

  /// 获取受监视键容器只读引用
  #[inline]
  pub const fn watched_keys(&self) -> &WatchedKeysContainer {
    &self.watched_keys
  }

  /// 获取受监视键容器可变引用
  #[inline]
  pub fn watched_keys_mut(&mut self) -> &mut WatchedKeysContainer {
    &mut self.watched_keys
  }

  /// 获取键锁集合只读引用
  #[inline]
  pub const fn key_entries(&self) -> &TxnKeyEntries {
    &self.key_entries
  }

  /// 获取键锁集合可变引用（供调用方在释放物理锁后调用 [`TxnKeyEntries::unlock_all_keys`] 清理）
  #[inline]
  pub fn key_entries_mut(&mut self) -> &mut TxnKeyEntries {
    &mut self.key_entries
  }

  /// 本事务是否包含写操作
  #[inline]
  pub const fn perform_writes(&self) -> bool {
    self.perform_writes
  }

  /// 获取是否处于日志重放中
  #[inline]
  pub const fn is_replaying(&self) -> bool {
    self.is_replaying
  }

  /// 设置是否处于日志重放中
  #[inline]
  pub fn set_is_replaying(&mut self, is_replaying: bool) {
    self.is_replaying = is_replaying;
  }

  /// 开启事务（嵌套开启报错并置为中止，对标 Garnet NetworkMULTI）
  pub fn multi(&mut self) -> Result<()> {
    if self.state != TxnState::None {
      self.state = TxnState::Aborted;
      return Err(Error::NestedMulti);
    }
    self.state = TxnState::Started;
    self.queue.clear();
    self.key_entries.clear();
    self.perform_writes = false;
    self.invalidate_all = false;
    Ok(())
  }

  /// 监视指定键（记录当前全局版本基线）
  ///
  /// 事务已开启或已中止（Started / Running / Aborted）期间一律拦截，
  /// 对标 Garnet `NetworkSKIP` 的 isWatch 分支：仅报错，不改变事务状态。
  pub fn watch(&mut self, key: Bytes, version_map: &WatchVersionMap) -> Result<()> {
    if self.state.is_in_txn() {
      return Err(Error::WatchInsideMulti);
    }
    self.watched_keys.watch(key, version_map);
    Ok(())
  }

  /// 取消所有键的监视（仅空闲状态生效，事务期间为空操作，对标 Garnet NetworkUNWATCH）
  pub fn unwatch(&mut self) {
    if self.state.is_none() {
      self.watched_keys.reset();
    }
  }

  /// 放弃事务并重置会话状态（含清空监视键，对标 Garnet NetworkDISCARD）
  pub fn discard(&mut self) -> Result<()> {
    if self.state.is_none() {
      return Err(Error::DiscardWithoutMulti);
    }
    self.reset();
    Ok(())
  }

  /// 将命令加入事务执行队列（零额外堆分配提取键）
  ///
  /// 已开启（Started）与已中止（Aborted）状态下均接受入队（对标 Garnet
  /// `NetworkSKIP` 与 Redis：中止后命令继续返回 +QUEUED，由 EXEC 统一报
  /// EXECABORT）；未开启或执行中入队报 [`Error::QueueWithoutMulti`]。
  pub fn queue_command(&mut self, cmd: QueuedCommand) -> Result<()> {
    self.queue_command_with_key_prefix(cmd, &[])
  }

  /// 将命令加入事务执行队列，键锁集合按会话复合键哈希登记
  ///
  /// `key_prefix` 为会话键命名空间前缀（Namespace × DB 变长编码，由调用方从
  /// 存储会话派生）。登记死锁防御锁集合时以 `key_prefix ++ 用户键` 的复合键
  /// 哈希寻址，使提交期版本广播精确到 `(ns, db, key)`：与 WATCH 注册侧、
  /// 非事务单命令写侧的复合版本键三方一致，杜绝跨库同名键漏失效/误失效。
  ///
  /// 本方法对键字节保持透明：不解析、不改写排队命令参数，前缀仅参与哈希登记；
  /// `key_prefix` 为空时与 [`Self::queue_command`] 完全等价。
  pub fn queue_command_with_key_prefix(
    &mut self,
    cmd: QueuedCommand,
    key_prefix: &[u8],
  ) -> Result<()> {
    match self.state {
      TxnState::Started | TxnState::Aborted => {}
      TxnState::None | TxnState::Running => return Err(Error::QueueWithoutMulti),
    }

    if !QueuedCommand::is_allowed_in_txn(cmd.cmd) {
      self.abort();
      return Err(Error::CommandNotAllowedInTxn(cmd.cmd.as_str()));
    }

    // 全局变异命令：提交时须对全部监视键广播失效（对标 Redis 触发全库 WATCH 通知）
    if matches!(cmd.cmd, RespCommand::FLUSHALL | RespCommand::FLUSHDB) {
      self.invalidate_all = true;
    }

    if key_prefix.is_empty() {
      // 零分配直接将键哈希与锁类型登记至锁集合
      cmd.for_each_key(|k, lock_type| {
        self
          .key_entries
          .add_key_hash(whasher::fast_hash(k.as_ref()), lock_type);
      });
    } else {
      // 复合键登记：前缀仅拷贝一次，循环内追加键后截断复用缓冲
      let mut scoped = key_prefix.to_vec();
      let prefix_len = scoped.len();
      cmd.for_each_key(|k, lock_type| {
        scoped.truncate(prefix_len);
        scoped.extend_from_slice(k.as_ref());
        self
          .key_entries
          .add_key_hash(whasher::fast_hash(&scoped), lock_type);
      });
    }

    if cmd.is_write {
      self.perform_writes = true;
    }

    self.queue.push(cmd);
    Ok(())
  }

  /// 将事务置为中止状态（缓冲阶段发生错误，后续 EXEC 直接丢弃）
  #[inline]
  pub fn abort(&mut self) {
    self.state = TxnState::Aborted;
  }

  /// 重置会话事务状态与暂存数据，可选是否保留监视键（对标 Garnet Reset）。
  ///
  /// 注意：本方法不清空 [`Self::key_entries`] —— 拆分式流程冲突后调用方
  /// 仍需按原锁集合释放物理锁；确认释放完毕后应另行调用
  /// [`TxnKeyEntries::unlock_all_keys`] 清理。
  fn reset_session(&mut self, continue_watch: bool) {
    self.state = TxnState::None;
    self.queue.clear();
    if !continue_watch {
      self.watched_keys.reset();
    }
    self.perform_writes = false;
    self.invalidate_all = false;
  }

  /// 重置事务管理器状态，可选是否保留监视键（对标 Garnet Reset(bool continueWatch)）
  pub fn reset_all(&mut self, continue_watch: bool) {
    self.reset_session(continue_watch);
    self.key_entries.clear();
  }

  /// 重置事务管理器全部状态至空闲（清空监视键）
  #[inline]
  pub fn reset(&mut self) {
    self.reset_all(false);
  }

  /// 登记键与锁类型至待锁定列表
  pub fn save_key_entry_to_lock(&mut self, key: impl AsRef<[u8]>, lock_type: LockType) {
    if lock_type.is_exclusive() {
      self.perform_writes = true;
    }
    self.key_entries.add_key_slice(key.as_ref(), lock_type);
  }

  /// 获取当前锁集合调试文本
  pub fn get_lockset(&self) -> String {
    let phase = u8::from(self.state == TxnState::Running);
    self.key_entries.get_lockset(phase)
  }

  /// 事务预处理准备阶段：
  /// 1. 检查事务状态（若为中止则重置并返回中止标记）
  /// 2. 检查是否开启了事务
  /// 3. 将受监视键以共享读锁注册入锁集合，并执行死锁防御排序与锁合并
  /// 4. 校验受监视键的全局版本一致性（冲突则重置并返回冲突标记）
  /// 5. 校验通过则转为运行中状态并移交命令队列与锁条目集合
  ///
  /// 注意：本方法在返回前完成版本校验。若调用方需要在执行前获取物理键锁
  /// （如全局哈希锁），应改用 [`Self::prepare_lockset`] + [`Self::validate_watches`]
  /// 拆分流程，将校验置于加锁之后以闭合竞态窗口。
  pub fn exec_prepare(&mut self, version_map: &WatchVersionMap) -> Result<ExecPreparation> {
    Ok(match self.prepare_inner(version_map)? {
      Prepared::Aborted => ExecPreparation::Aborted,
      Prepared::Conflict => ExecPreparation::Conflict,
      Prepared::Running(commands) => ExecPreparation::Success {
        commands,
        key_entries: self.key_entries.clone(),
      },
    })
  }

  /// 提交事务收尾，可选是否保留监视键（对标 Garnet Commit(bool continueWatch)）：
  /// - 若排队了 FLUSHDB / FLUSHALL 全局变异，则对版本表全表自增，广播失效所有监视键；
  /// - 否则若包含写操作，对所有排他锁定的键自增版本以触发并发失效；
  ///
  /// 随后按选项清空监视键并重置会话事务状态。
  pub fn commit_with_options(&mut self, version_map: &WatchVersionMap, continue_watch: bool) {
    if self.invalidate_all {
      version_map.bump_all();
    } else if self.perform_writes {
      self
        .key_entries
        .iter()
        .filter(|entry| entry.lock_type.is_exclusive())
        .for_each(|entry| {
          version_map.bump_version(entry.key_hash);
        });
    }
    self.reset_all(continue_watch);
  }

  /// 事务执行完毕后的提交收尾（清空监视键，对标 Garnet Commit()）
  #[inline]
  pub fn commit(&mut self, version_map: &WatchVersionMap) {
    self.commit_with_options(version_map, false);
  }

  /// 便捷一站式原子执行接口：
  /// 准备校验通过后按入队顺序逐条执行命令并提交，返回各命令执行结果。
  pub fn exec<F, R>(
    &mut self,
    version_map: &WatchVersionMap,
    mut executor: F,
  ) -> Result<ExecResult<Vec<R>>>
  where
    F: FnMut(&QueuedCommand) -> R,
  {
    match self.prepare_inner(version_map)? {
      Prepared::Aborted => Ok(ExecResult::Aborted),
      Prepared::Conflict => Ok(ExecResult::Conflict),
      Prepared::Running(commands) => {
        let results = commands.iter().map(&mut executor).collect();
        self.commit(version_map);
        Ok(ExecResult::Success(results))
      }
    }
  }

  /// 将当前单键操作升级为原子事务（对标 Garnet PromoteToTransaction）
  ///
  /// 若已处于运行态则返回空守卫（无任何副作用，对标 Garnet TransactionGuard.Null）；
  /// 否则登记键锁并进入运行态，守卫 Drop 时自动提交。
  ///
  /// # 契约
  /// 本方法假定空闲态下锁集合已清理。若先前的拆分式流程校验冲突，
  /// 调用方须先释放物理锁并 [`TxnKeyEntries::unlock_all_keys`]，否则遗留
  /// 条目将在守卫提交时被一并广播失效（保守但多余）。
  pub fn promote_to_transaction<'s, 'v>(
    &'s mut self,
    version_map: &'v WatchVersionMap,
    key: impl AsRef<[u8]>,
    lock_type: LockType,
  ) -> TransactionGuard<'s, 'v> {
    if self.state == TxnState::Running {
      return TransactionGuard::detached();
    }
    self.save_key_entry_to_lock(key, lock_type);
    self.state = TxnState::Running;
    TransactionGuard::new(self, version_map)
  }

  /// 拆分式准备阶段一：校验事务状态，将受监视键并入锁集合并完成死锁防御排序。
  ///
  /// 返回 `Ok(false)` 表示事务先前已中止（已重置，此时调用方尚未加锁，
  /// 直接放弃执行即可）；非已开启状态报 [`Error::ExecWithoutMulti`]。
  /// 与 [`Self::exec_prepare`] 不同，本方法不校验监视版本、不迁移状态：
  /// 调用方应依据 [`Self::key_entries`] 获取物理键锁，再加锁调用
  /// [`Self::validate_watches`]，使"校验-执行"在持锁状态下原子完成。
  pub fn prepare_lockset(&mut self) -> Result<bool> {
    if self.state == TxnState::Aborted {
      self.reset();
      return Ok(false);
    }
    if self.state != TxnState::Started {
      return Err(Error::ExecWithoutMulti);
    }
    self.watched_keys.save_keys_to_lock(&mut self.key_entries);
    self.key_entries.sort_by_key_hash();
    Ok(true)
  }

  /// 拆分式准备阶段二：在持有物理键锁的前提下校验受监视键版本一致性。
  ///
  /// 冲突时返回 `false` 并重置会话事务状态（状态机归零、清空队列与监视键，
  /// 对标 Garnet 冲突路径），但**保留 [`Self::key_entries`]**：调用方仍持有
  /// 按该集合获取的物理键锁，须依据原集合完成解锁，再调用
  /// [`Self::key_entries_mut`] 的 [`TxnKeyEntries::unlock_all_keys`] 清理
  /// （对标 Garnet `UnlockAllKeys` 先于条目清空的释放顺序）。
  #[inline]
  pub fn validate_watches(&mut self, version_map: &WatchVersionMap) -> bool {
    if self.watched_keys.validate_versions(version_map) {
      return true;
    }
    self.reset_session(false);
    false
  }

  /// 拆分式准备阶段三：由已开启状态进入运行态
  #[inline]
  pub fn begin_run(&mut self) {
    if self.state == TxnState::Started {
      self.state = TxnState::Running;
    }
  }

  /// 取出暂存命令队列（队列归零），供运行态下手动逐条执行
  #[inline]
  pub fn take_queue(&mut self) -> Vec<QueuedCommand> {
    mem::take(&mut self.queue)
  }

  /// 内部统一准备流程：锁集合构建 → 版本校验 → 进入运行态并移交队列
  fn prepare_inner(&mut self, version_map: &WatchVersionMap) -> Result<Prepared> {
    if !self.prepare_lockset()? {
      return Ok(Prepared::Aborted);
    }
    if !self.validate_watches(version_map) {
      // 一站式路径：物理锁由本流程内部管理，调用方未持锁，直接清空锁集合
      self.key_entries.unlock_all_keys();
      return Ok(Prepared::Conflict);
    }
    self.begin_run();
    Ok(Prepared::Running(self.take_queue()))
  }
}

/// 事务自动提交守卫（对标 Garnet TransactionGuard）
///
/// Drop 时自动提交：对排他锁键自增全局版本号（向其他会话广播监视失效通知），
/// 并保留监视键继续有效（对标 Garnet `Commit(internal_txn: true)`）。
///
/// 守卫以编译期借用（`'a` 管理器 + `'v` 版本表）托管资源，不存在悬垂引用；
/// 显式 [`Self::commit`] 会摘除托管上下文，随后的 Drop 退化为空操作，杜绝双提交。
pub struct TransactionGuard<'a, 'v> {
  /// 托管的事务管理器与全局版本表（None 表示空守卫，Drop 无副作用）
  ctx: Option<(&'a mut TransactionManager, &'v WatchVersionMap)>,
}

impl<'a, 'v> TransactionGuard<'a, 'v> {
  /// 构造新的事务守卫
  #[inline]
  pub fn new(txn_manager: &'a mut TransactionManager, version_map: &'v WatchVersionMap) -> Self {
    Self {
      ctx: Some((txn_manager, version_map)),
    }
  }

  /// 空守卫（对标 Garnet TransactionGuard.Null）
  #[inline]
  pub const fn detached() -> Self {
    Self { ctx: None }
  }

  /// 获取受托管事务管理器状态（空守卫返回 None）
  #[inline]
  pub fn state(&self) -> Option<TxnState> {
    self.ctx.as_ref().map(|(m, _)| m.state())
  }

  /// 提前提交事务并解除守卫托管（保留监视键，触发失效通知）
  pub fn commit(&mut self) {
    if let Some((m, vm)) = self.ctx.take() {
      m.commit_with_options(vm, true);
    }
  }
}

impl Drop for TransactionGuard<'_, '_> {
  fn drop(&mut self) {
    self.commit();
  }
}
