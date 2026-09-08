use std::{
  collections::VecDeque,
  sync::Arc,
  time::{Duration, Instant},
};

use bytes::Bytes;
use compio::time::timeout;
use parking_lot::Mutex;
use wedb_resp::RespCommand;
use whasher::{GxPapayaMap, new_papaya_map};

use crate::{
  error::{Error, Result},
  observer::{CollectionItemObserver, ObserverRx},
  provider::{CollectionProvider, Direction, MemoryCollectionStore},
  result::CollectionItemResult,
};

/// keysToObservers 周期性惰性清理的最小间隔（对齐 Garnet `MIN_SECS_BETWEEN_KEYS_TO_OBSERVERS_CLEANS`）
const CLEAN_INTERVAL: Duration = Duration::from_secs(5 * 60);

/// 超时上限：超过后视为永久等待
///
/// 兜底防止底层定时器 `Instant::now() + duration` 溢出 panic
const MAX_TIMEOUT: Duration = Duration::from_secs(365 * 24 * 60 * 60);

/// 单 key 的等待者队列：FIFO 等待者 + 信号记账
///
/// 不变式：所有字段的访问与变更均须持有 broker 的 `assign_lock`，
/// 此处 Mutex 仅为经 `Arc` 共享后的可变性要求。
type ObserverQueue = Mutex<QueueState>;

/// 等待者队列内部状态
#[derive(Default)]
struct QueueState {
  /// FIFO 等待者队列（对齐 Garnet `ConcurrentQueue<CollectionItemObserver>`）
  waiters: VecDeque<Arc<CollectionItemObserver>>,
  /// 信号驱动模式下未被任何等待者见证的元素到达数：
  /// notify_waiters 时若队列中无等待者，将剩余到达数记账于此，
  /// 后续注册的观察者据此立即获得信号提示，杜绝丢失唤醒
  pending_signals: usize,
}

/// 单 key 探测命中结果
enum ProbeHit {
  /// 已终结（结果已设置在观察者上）
  Done,
  /// 已终结且向目标 key 完成了一次转移，需继续唤醒目标 key 的等待者
  Moved(Bytes),
  /// 信号提示终结（客户端将从真实存储复检）
  Notified,
}

/// 阻塞集合命令核心调度中继器
///
/// 1:1 对齐 Microsoft Garnet `CollectionItemBroker`，
/// 负责管理 BLPOP, BRPOP, BLMOVE, BLMPOP, BRPOPLPUSH, BZPOPMIN, BZPOPMAX, BZMPOP 等阻塞命令调度。
///
/// 与 C# 的刻意差异：Garnet 由独立主循环线程串行消费事件队列；本实现面向 compio
/// 线程每核（thread-per-core）模型，注册与唤醒改为调用方直连 + 异步通道通知，
/// 阻塞等待只挂起当前任务、绝不占用 reactor 线程。主循环的串行化语义由
/// `assign_lock` 等价承担（仅存在等待者时才会发生竞争，无等待者路径零锁开销）。
pub struct CollectionItemBroker {
  /// Key 到观察者 FIFO 队列的映射（对齐 Garnet `keysToObservers`）
  keys_to_observers: GxPapayaMap<Bytes, Arc<ObserverQueue>>,

  /// 会话 ID 到当前等待观察者的映射（对齐 Garnet `sessionIdToObserver`）
  session_to_observer: GxPapayaMap<u64, Arc<CollectionItemObserver>>,

  /// 底层集合存储提供者
  provider: Arc<dyn CollectionProvider>,

  /// 内存集合存储引用（若使用内置内存提供者）
  mem_store: Option<Arc<MemoryCollectionStore>>,

  /// 分配临界区锁：串行化 [扫描队列 + 弹出元素 + 设置结果 + 入队/出队]，
  /// 等价于 C# 单线程主循环 + keysToObserversLock 的组合语义
  assign_lock: Mutex<()>,

  /// 上次 keysToObservers 清理时间
  last_clean: Mutex<Instant>,

  /// 信号提示开关：内置内存存储仅作影子（真实数据在外部存储）时必须开启。
  /// 开启后，注册时若消费到补发信号或 key 条目为新建，将立即返回
  /// [CollectionItemResult::key_update] 提示客户端从真实存储复检弹出
  signal_hint: bool,
}

impl CollectionItemBroker {
  /// 按部件组装 Broker 实例
  fn with_parts(
    provider: Arc<dyn CollectionProvider>,
    mem_store: Option<Arc<MemoryCollectionStore>>,
    signal_hint: bool,
  ) -> Self {
    Self {
      keys_to_observers: new_papaya_map(),
      session_to_observer: new_papaya_map(),
      provider,
      mem_store,
      assign_lock: Mutex::new(()),
      last_clean: Mutex::new(Instant::now()),
      signal_hint,
    }
  }

  /// 创建使用默认内存存储实现的 Broker 实例（信号提示开启）
  pub fn new() -> Self {
    let mem = Arc::new(MemoryCollectionStore::new());
    // wedb_server 以 new() 构造且真实数据在外部存储：
    // 必须开启核验提示，否则「预检 → 注册」间隙内的推送会丢失唤醒
    Self::with_parts(mem.clone(), Some(mem), true)
  }

  /// 使用指定的集合提供者创建 Broker 实例（数据精确模式，无信号提示）
  ///
  /// 此模式下提供者即唯一真实数据源，弹出结果精确，无需客户端复检。
  pub fn with_provider(provider: Arc<dyn CollectionProvider>) -> Self {
    Self::with_parts(provider, None, false)
  }

  /// 流式设置信号提示开关
  #[inline]
  pub fn with_signal_hint(mut self, enabled: bool) -> Self {
    self.signal_hint = enabled;
    self
  }

  /// 向列表推入元素并尝试唤醒等待的观察者 (LPUSH / RPUSH + 唤醒)
  fn push_and_notify(&self, key: &[u8], value: impl Into<Bytes>, front: bool) -> Result<usize> {
    let len = match (self.mem_store.as_ref(), front) {
      (Some(mem), true) => mem.push_list_left(key, value)?,
      (Some(mem), false) => mem.push_list_right(key, value)?,
      // 未挂载内存存储时应由外部写入真实存储后调用 notify_waiters
      (None, _) => return Err(Error::InvalidArgument("broker 未挂载内存存储，禁止推送")),
    };
    self.handle_collection_update(key);
    Ok(len)
  }

  /// 向列表左侧推入元素并尝试唤醒等待的观察者
  pub fn push_list_left_and_notify(&self, key: &[u8], value: impl Into<Bytes>) -> Result<usize> {
    self.push_and_notify(key, value, true)
  }

  /// 向列表右侧推入元素并尝试唤醒等待的观察者
  pub fn push_list_right_and_notify(&self, key: &[u8], value: impl Into<Bytes>) -> Result<usize> {
    self.push_and_notify(key, value, false)
  }

  /// 获取底层集合提供者引用
  #[inline]
  pub fn provider(&self) -> &Arc<dyn CollectionProvider> {
    &self.provider
  }

  /// 尝试获取指定会话 ID 的观察者（对齐 Garnet `TryGetObserver`）
  #[inline]
  pub fn try_get_observer(&self, session_id: u64) -> Option<Arc<CollectionItemObserver>> {
    self.session_to_observer.pin().get(&session_id).cloned()
  }

  /// 支持 CLIENT UNBLOCK 强制解除阻塞（对齐 Garnet `TryForceUnblock`）
  ///
  /// throw_error: 若为 true，对应 CLIENT UNBLOCK id ERROR，返回 -UNBLOCKED 错误标记；
  /// 若为 false，对应 CLIENT UNBLOCK id TIMEOUT，返回 nil。
  pub fn try_unblock(&self, session_id: u64, throw_error: bool) -> bool {
    let pin = self.session_to_observer.pin();
    if let Some(observer) = pin.remove(&session_id) {
      observer.try_force_unblock(throw_error)
    } else {
      false
    }
  }

  /// 处理客户端会话销毁释放（对齐 Garnet `HandleSessionDisposed`）
  pub fn handle_session_disposed(&self, session_id: u64) {
    let pin = self.session_to_observer.pin();
    if let Some(observer) = pin.remove(&session_id) {
      observer.handle_session_disposed();
    }
  }

  /// 获取指定 key 当前等待中的观察者数量
  pub fn waiting_count(&self, key: &[u8]) -> usize {
    let _g = self.assign_lock.lock();
    let pin = self.keys_to_observers.pin();
    match pin.get(key) {
      Some(q) => q.lock().waiters.iter().filter(|o| o.is_receptive()).count(),
      None => 0,
    }
  }

  /// 高层封装：发起阻塞获取单键或多键元素（对齐 Garnet `GetCollectionItemAsync`）
  pub async fn get_collection_item(
    &self,
    session_id: u64,
    command: RespCommand,
    keys: &[Bytes],
    timeout_in_seconds: f64,
    args: Vec<Bytes>,
  ) -> CollectionItemResult {
    let (observer, rx) = CollectionItemObserver::new(session_id, command, args);
    self
      .get_collection_item_async(observer, keys, timeout_in_seconds, rx)
      .await
  }

  /// 高层封装：发起阻塞转移元素（对齐 Garnet `MoveCollectionItemAsync`）
  pub async fn move_collection_item(
    &self,
    session_id: u64,
    command: RespCommand,
    src_key: &[u8],
    dst_key: &[u8],
    directions: (Direction, Direction),
    timeout_in_seconds: f64,
  ) -> CollectionItemResult {
    let (src_dir, dst_dir) = directions;
    let dir_bytes = |d: Direction| -> &'static [u8] {
      if d == Direction::Left {
        b"LEFT"
      } else {
        b"RIGHT"
      }
    };
    let args = vec![
      Bytes::copy_from_slice(dst_key),
      Bytes::from_static(dir_bytes(src_dir)),
      Bytes::from_static(dir_bytes(dst_dir)),
    ];
    let (observer, rx) = CollectionItemObserver::new(session_id, command, args);
    self
      .move_collection_item_async(observer, src_key, timeout_in_seconds, rx)
      .await
  }

  /// 异步等待集合元素（对齐 Garnet `GetCollectionItemAsync`）
  pub async fn get_collection_item_async(
    &self,
    observer: Arc<CollectionItemObserver>,
    keys: &[Bytes],
    timeout_in_seconds: f64,
    rx: ObserverRx,
  ) -> CollectionItemResult {
    // 周期性惰性清理失效观察者（对齐 Garnet 主循环 5 分钟清理）
    self.maybe_clean();

    // 1. 记录 Session 到 Observer 的映射
    self
      .session_to_observer
      .pin()
      .insert(observer.session_id, observer.clone());

    // 2. 注册观察者：若可立即满足（数据 / WRONGTYPE / 信号提示）则直接返回
    if self.initialize_observer(&observer, keys) {
      self.session_to_observer.pin().remove(&observer.session_id);
      return observer.get_result();
    }

    // 3. 异步等待通知或超时
    // 超时 <= 0 / NaN / 溢出一律视为永久等待（对齐 Garnet TimeSpan.FromMilliseconds(-1)）
    let result = if timeout_in_seconds > 0.0 {
      // 上限封顶，防止底层定时器 Instant 溢出 panic
      let duration = Duration::try_from_secs_f64(timeout_in_seconds)
        .unwrap_or(MAX_TIMEOUT)
        .min(MAX_TIMEOUT);
      match timeout(duration, rx.recv()).await {
        Ok(Ok(res)) => res,
        _ => {
          // 超时或接收端异常：若仍未被满足则以空结果终结
          // （handle_set_result 内部持有状态锁复检，与分配路径天然串行）
          observer.handle_set_result(CollectionItemResult::empty());
          // 主动摘除各 key 等待队列中的残留，抑制超时空转积压
          // （5 分钟周期清理仍是最终兜底）
          self.deregister_observer(&observer, keys);
          observer.get_result()
        }
      }
    } else {
      match rx.recv().await {
        Ok(res) => res,
        Err(_) => observer.get_result(),
      }
    };

    // 4. 清理 session 映射
    self.session_to_observer.pin().remove(&observer.session_id);

    result
  }

  /// 异步等待集合转移（对齐 Garnet `MoveCollectionItemAsync`）
  pub async fn move_collection_item_async(
    &self,
    observer: Arc<CollectionItemObserver>,
    src_key: &[u8],
    timeout_in_seconds: f64,
    rx: ObserverRx,
  ) -> CollectionItemResult {
    self
      .get_collection_item_async(
        observer,
        &[Bytes::copy_from_slice(src_key)],
        timeout_in_seconds,
        rx,
      )
      .await
  }

  /// 从各 key 的等待队列中主动摘除指定观察者（超时终结后的回收）
  ///
  /// 若无主动摘除，每次超时的观察者会以死条目形式滞留队列，
  /// 高频短超时轮询可在清理周期内造成无上界积压；
  /// 回收条目前仍受 [gc_able_st] 约束：补发信号记账会阻止条目销毁，
  /// 因此不会破坏「防丢失唤醒」记账语义。
  fn deregister_observer(&self, observer: &Arc<CollectionItemObserver>, keys: &[Bytes]) {
    let _g = self.assign_lock.lock();
    let pin = self.keys_to_observers.pin();
    for key in keys {
      let Some(q) = pin.get(key).cloned() else {
        continue;
      };
      let mut st = q.lock();
      st.waiters.retain(|o| !Arc::ptr_eq(o, observer));
      if gc_able_st(&st) {
        pin.remove(key);
      }
    }
  }

  /// 注册观察者（内部持有 assign_lock）
  ///
  /// 返回 true 表示观察者已被终结（取得元素 / WRONGTYPE / 信号提示）。
  ///
  /// 两阶段设计保证与并发更新互不丢唤醒：
  /// 1) 按 key 优先级探测立即满足或消费信号提示（条目首次新建时提示客户端复检真实存储）；
  /// 2) 全部落空时，才将观察者正式挂载至各 key 的 FIFO 队尾并进入睡眠。
  fn initialize_observer(&self, observer: &Arc<CollectionItemObserver>, keys: &[Bytes]) -> bool {
    let _g = self.assign_lock.lock();

    // 第一阶段：按 key 优先级探测（对齐 C# InitializeObserver 的 failOnSrcTypeMismatch: true）
    for key in keys {
      if let Some(hit) = self.probe_key(observer, key) {
        self.forward_move(hit);
        return true;
      }
    }

    // 第二阶段：全部落空，正式挂载至各 key 的 FIFO 队尾并进入睡眠
    // （已被并发超时 / 解除阻塞终结的观察者不再挂载，留待惰性清理）
    if observer.is_receptive() {
      let pin = self.keys_to_observers.pin();
      for key in keys {
        let q = pin
          .get_or_insert_with(key.clone(), || Arc::new(ObserverQueue::default()))
          .clone();
        q.lock().waiters.push_back(observer.clone());
      }
    }

    false
  }

  /// 快路径探测 + 持锁分配：仅存在等待者时才进入分配临界区
  fn assign_if_waiters(&self, key: &[u8]) -> bool {
    // 快路径：无任何等待者时直接返回，避免锁竞争
    if self.keys_to_observers.pin().is_empty() {
      return false;
    }
    let _g = self.assign_lock.lock();
    self.try_assign_locked(key)
  }

  /// 底层集合写入或更新触发通知（对齐 Garnet `HandleCollectionUpdate`）
  ///
  /// 弹出元素并唤醒队列首个有效等待观察者，返回 true 表示成功唤醒了一名观察者。
  pub fn handle_collection_update(&self, key: &[u8]) -> bool {
    self.assign_if_waiters(key)
  }

  /// 批量更新触发通知：循环唤醒直至集合无元素或无等待者
  pub fn handle_collection_update_all(&self, key: &[u8]) -> usize {
    if self.keys_to_observers.pin().is_empty() {
      return 0;
    }
    let _g = self.assign_lock.lock();
    let mut count = 0;
    while self.try_assign_locked(key) {
      count += 1;
    }
    count
  }

  /// 信号驱动模式唤醒：向等待指定 key 的观察者发送唤醒信号（至多唤醒 count 个）
  ///
  /// 观察者被唤醒后直接从真实存储引擎（如 wedb_store）中弹出数据，
  /// 彻底杜绝影子内存副本脱节与重复消费问题。
  ///
  /// 若队列中无等待者，剩余到达数将记为补发信号（pending_signals），
  /// 保证后续注册的观察者不会丢失本次唤醒。
  pub fn notify_waiters(&self, key: &[u8], count: usize) -> usize {
    if count == 0 || self.keys_to_observers.pin().is_empty() {
      return 0;
    }
    let _g = self.assign_lock.lock();
    let pin = self.keys_to_observers.pin();
    // 条目缺失说明该 key 从无注册历史：不存在可能错过的等待者，
    // 后续首次注册会通过信号提示模式建立条目并提示复检真实存储
    let Some(q) = pin.get(key).cloned() else {
      return 0;
    };

    let key_bytes = Bytes::copy_from_slice(key);
    let mut woken = 0;
    {
      let mut st = q.lock();
      while woken < count
        && let Some(observer) = st.waiters.pop_front()
      {
        // 状态锁内原子认领投递权（对齐 C# 在 ObserverStatusLock 写锁内
        // 校验 + HandleSetResult 的原子语义）：与超时 / CLIENT UNBLOCK /
        // 会话销毁路径竞争时，认领失败（已终结或接收端断开）则不投递、
        // 不计入唤醒数，该次到达落入下方 pending_signals 记账，
        // 杜绝「到达被终结路径抢占后凭空蒸发」的丢失唤醒窗口
        let claimed =
          observer.assign_atomic(|| Ok(Some(CollectionItemResult::key_update(key_bytes.clone()))));
        if let Some(Ok(Some(_))) = claimed {
          self.session_to_observer.pin().remove(&observer.session_id);
          woken += 1;
        }
      }
      if woken < count {
        // 剩余未见证到达记账，供后续注册的观察者立即消费。
        // 记账仅服务于信号提示模式（唯一消费方）；数据精确模式下
        // 账目永无出头之日，只会永久阻止空条目回收
        if self.signal_hint {
          st.pending_signals = st.pending_signals.saturating_add(count - woken);
        }
      }
    }

    if gc_able(&q) {
      pin.remove(key);
    }
    woken
  }

  /// 尝试将集合中可用的元素分配给队列中下一个等待的观察者
  ///
  /// 对齐 Garnet `TryAssignItemFromKey`
  pub fn try_assign_item_from_key(&self, key: &[u8]) -> bool {
    self.assign_if_waiters(key)
  }

  /// 尝试将集合中可用的元素分配给队列中下一个等待的观察者（须持有 assign_lock）
  fn try_assign_locked(&self, key: &[u8]) -> bool {
    let pin = self.keys_to_observers.pin();
    let Some(q) = pin.get(key).cloned() else {
      return false;
    };

    let mut assigned = false;
    let mut forward = None;
    {
      let mut st = q.lock();
      while let Some(observer) = st.waiters.pop_front() {
        // 惰性跳过已终结或接收端断开的观察者
        if !observer.is_receptive() {
          continue;
        }
        // 状态锁内原子完成「校验 → 弹出 → 投递」，
        // 杜绝元素弹出后被并发超时 / 解除阻塞 / 会话销毁路径抢占而凭空丢失。
        // fail_on_mismatch = false：类型不符视为暂不可消费（对齐 C#
        // TryAssignItemFromKey 的 failOnSrcTypeMismatch: false）
        match observer
          .assign_atomic(|| pop_for_observer(self.provider.as_ref(), key, &observer, false))
        {
          Some(Ok(Some(res))) => {
            self.finish_observer(&observer);
            assigned = true;
            // BLMOVE / BRPOPLPUSH 成功转移后，目标 key 的等待者需继续分配
            // （补齐 C# 缺口：Garnet 转移落位后不会主动唤醒 dst 等待者）；
            // found 守卫：以 WRONGTYPE 终结时未向 dst 落位任何元素，不得惊动 dst 等待者
            if res.found()
              && matches!(
                observer.command,
                RespCommand::Blmove | RespCommand::Brpoplpush
              )
              && let Some(dst) = observer.command_args.first()
            {
              forward = Some(dst.clone());
            }
            break;
          }
          // 集合已无元素或暂时性错误：放回队首，保持 FIFO 优先级
          Some(Ok(None) | Err(_)) => {
            st.waiters.push_front(observer);
            break;
          }
          // 观察者已被并发终结：按死条目跳过（惰性清理）
          None => continue,
        }
      }
    }

    if let Some(dst) = forward {
      let _ = self.try_assign_locked(&dst);
    }

    if gc_able(&q) {
      pin.remove(key);
    }
    assigned
  }

  /// 尝试从指定 key 立即满足观察者（须持有 assign_lock）
  ///
  /// 返回 Some 表示观察者结果已设置（数据 / WRONGTYPE / 信号提示）；
  /// None 表示该 key 暂无法满足，应继续探测后续 key。
  fn probe_key(&self, observer: &Arc<CollectionItemObserver>, key: &[u8]) -> Option<ProbeHit> {
    let pin = self.keys_to_observers.pin();

    if let Some(q) = pin.get(key).cloned() {
      let mut st = q.lock();
      // 顺带压缩：惰性剔除已终结或接收端断开的观察者
      st.waiters.retain(|o| o.is_receptive());
      // FIFO：存在更早的等待者时不可抢先获取
      if has_earlier_waiter(&st) {
        return None;
      }

      // 1) 直接从提供者弹出 / 转移（数据精确模式）——状态锁内原子完成，
      //    fail_on_mismatch = true：类型不符立即返回 WRONGTYPE（对齐 C#
      //    InitializeObserver 的 failOnSrcTypeMismatch: true）
      if let Some(Ok(Some(res))) =
        observer.assign_atomic(|| pop_for_observer(self.provider.as_ref(), key, observer, true))
      {
        drop(st);
        self.finish_observer(observer);
        if gc_able(&q) {
          pin.remove(key);
        }
        return Some(move_hit(observer, &res));
      }

      // 2) 信号提示模式：消费补发信号，提示客户端从真实存储复检弹出。
      //    经 assign_atomic 认领后才消费记账，与终结路径竞态时不丢信号
      if self.signal_hint && observer.is_waiting() && st.pending_signals > 0 {
        let notified = observer.assign_atomic(|| {
          st.pending_signals = st.pending_signals.saturating_sub(1);
          Ok(Some(CollectionItemResult::key_update(
            Bytes::copy_from_slice(key),
          )))
        });
        if let Some(Ok(Some(_))) = notified {
          drop(st);
          self.finish_observer(observer);
          return Some(ProbeHit::Notified);
        }
      }

      return None;
    }

    // 无条目（首次注册）：
    // 1) 仍尝试直接从提供者取数
    if let Some(Ok(Some(res))) =
      observer.assign_atomic(|| pop_for_observer(self.provider.as_ref(), key, observer, true))
    {
      self.finish_observer(observer);
      return Some(move_hit(observer, &res));
    }

    // 2) 信号驱动模式且无条目：写者在此前推送的通知可能未被见证。
    //    建立空条目并立即提示客户端复检真实存储，闭合预检与注册间的竞争窗口
    if self.signal_hint && observer.is_waiting() {
      let _q = pin.get_or_insert_with(Bytes::copy_from_slice(key), || {
        Arc::new(ObserverQueue::default())
      });
      let notified = observer.assign_atomic(|| {
        Ok(Some(CollectionItemResult::key_update(
          Bytes::copy_from_slice(key),
        )))
      });
      if let Some(Ok(Some(_))) = notified {
        self.finish_observer(observer);
        return Some(ProbeHit::Notified);
      }
    }

    None
  }

  /// 终结观察者：移除会话映射（结果已由 [CollectionItemObserver::assign_atomic] 同步投递）
  fn finish_observer(&self, observer: &Arc<CollectionItemObserver>) {
    self.session_to_observer.pin().remove(&observer.session_id);
  }

  /// 转移命中后继续唤醒目标 key 的等待者
  fn forward_move(&self, hit: ProbeHit) {
    if let ProbeHit::Moved(dst) = hit {
      let _ = self.try_assign_locked(&dst);
    }
  }

  /// 惰性清理所有队列中已失效的观察者并回收空 key（对齐 Garnet `CleanKeysToObservers`）
  pub fn clean_keys_to_observers(&self) {
    let _g = self.assign_lock.lock();
    // 回收会话映射中接收端已断开的残留：
    // 等待任务被中止（连接异常关闭且未走释放钩子）时无人执行步骤 4 的移除
    self
      .session_to_observer
      .pin()
      .retain(|_, o| o.is_receptive());
    let pin = self.keys_to_observers.pin();
    let mut removable = Vec::new();
    for (key, q) in pin.iter() {
      let is_gc = {
        let mut st = q.lock();
        st.waiters.retain(|o| o.is_receptive());
        gc_able_st(&st)
      };
      if is_gc {
        removable.push(key.clone());
      }
    }
    for key in removable {
      pin.remove(&key);
    }
  }

  /// 周期性惰性清理（间隔 [CLEAN_INTERVAL]）
  fn maybe_clean(&self) {
    if self.keys_to_observers.pin().is_empty() && self.session_to_observer.pin().is_empty() {
      return;
    }
    let mut last = self.last_clean.lock();
    if last.elapsed() < CLEAN_INTERVAL {
      return;
    }
    *last = Instant::now();
    drop(last);
    self.clean_keys_to_observers();
  }
}

impl Default for CollectionItemBroker {
  fn default() -> Self {
    Self::new()
  }
}

/// 状态是否可回收：无等待者且无补发信号
#[inline]
fn gc_able_st(st: &QueueState) -> bool {
  st.waiters.is_empty() && st.pending_signals == 0
}

/// 队列是否可回收：无等待者且无补发信号（须持有 assign_lock）
#[inline]
fn gc_able(q: &ObserverQueue) -> bool {
  gc_able_st(&q.lock())
}

/// 若观察者命令为 BLMOVE / BRPOPLPUSH 且元素已实际落位目标 key（found），
/// 返回 [ProbeHit::Moved]；WRONGTYPE 终结不得级联唤醒目标 key 等待者
fn move_hit(observer: &CollectionItemObserver, res: &CollectionItemResult) -> ProbeHit {
  if res.found()
    && matches!(
      observer.command,
      RespCommand::Blmove | RespCommand::Brpoplpush
    )
    && let Some(dst) = observer.command_args.first()
  {
    return ProbeHit::Moved(dst.clone());
  }
  ProbeHit::Done
}

/// 队列中是否存在更早的有效等待者（FIFO 公平性：先到先得）
///
/// 调用前已执行 `retain(|o| o.is_receptive())`，队列中现存元素均保证有效；
/// 注册探测阶段观察者尚未挂载，队列非空即意味着存在更早等待者。
#[inline]
fn has_earlier_waiter(st: &QueueState) -> bool {
  !st.waiters.is_empty()
}

/// 按观察者命令语义尝试从提供者弹出 / 转移元素
///
/// `fail_on_mismatch` 透传给提供者（对齐 C# `TryGetResult` 的 `failOnSrcTypeMismatch`）：
/// 注册探测传 true，等待分配路径传 false
fn pop_for_observer(
  provider: &dyn CollectionProvider,
  key: &[u8],
  observer: &CollectionItemObserver,
  fail_on_mismatch: bool,
) -> Result<Option<CollectionItemResult>> {
  if observer.command == RespCommand::Blmove && observer.command_args.len() >= 3 {
    let dst_key = &observer.command_args[0];
    let src_dir = Direction::from_bytes(&observer.command_args[1]).unwrap_or(Direction::Left);
    let dst_dir = Direction::from_bytes(&observer.command_args[2]).unwrap_or(Direction::Right);
    return provider.try_move_item(key, dst_key, src_dir, dst_dir, fail_on_mismatch);
  }
  if observer.command == RespCommand::Brpoplpush && !observer.command_args.is_empty() {
    let dst_key = &observer.command_args[0];
    let (src_dir, dst_dir) = if observer.command_args.len() >= 3 {
      let s = Direction::from_bytes(&observer.command_args[1]).unwrap_or(Direction::Right);
      let d = Direction::from_bytes(&observer.command_args[2]).unwrap_or(Direction::Left);
      (s, d)
    } else {
      // BRPOPLPUSH 固定右出左入
      (Direction::Right, Direction::Left)
    };
    return provider.try_move_item(key, dst_key, src_dir, dst_dir, fail_on_mismatch);
  }
  provider.try_pop_item(
    key,
    observer.command,
    &observer.command_args,
    fail_on_mismatch,
  )
}
