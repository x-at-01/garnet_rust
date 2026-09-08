use std::sync::Arc;

use bytes::Bytes;
use wdev::SegmentedDevice;
use wedb_pubsub::SessionHandle;
use wedb_resp::RespCommand;
use wkv::StoreSession;

/// 事务排队待执行指令结构
#[derive(Debug, Clone)]
pub struct QueuedCommand {
  /// RESP 命令操作码
  pub cmd: RespCommand,
  /// 预保存的命令参数列表
  pub args: Vec<Vec<u8>>,
}

/// 客户端连接会话状态机
pub struct ServerSession {
  /// 会话自增唯一标识
  pub id: u64,
  /// 连接是否已完成密码身份认证
  pub authenticated: bool,
  /// 当前认证的用户名
  pub user: Option<String>,
  /// 认证用户绑定的名字空间（None = 超管全局视界，Some(n) = 租户沙箱，对标 doc/zh/ns.md 十）
  pub user_ns: Option<u64>,
  /// 底层存储引擎会话操作句柄
  pub store_session: Arc<StoreSession<SegmentedDevice>>,
  /// 专用发布订阅句柄
  pub pubsub_session: SessionHandle,
  /// 是否处于 MULTI 事务排队中
  pub in_txn: bool,
  /// 事务排队期间是否已发生语法或严重错误（触发 EXECABORT）
  pub txn_aborted: bool,
  /// 事务已排队的指令列表
  pub txn_queue: Vec<QueuedCommand>,
  /// 是否携带 ASKING 重定向标记
  pub asking: bool,
  /// 是否启用 READONLY 只读模式
  pub is_readonly: bool,
  /// 是否处于发布订阅会话模式（严格对标 C# Garnet isSubscriptionSession）
  pub is_subscription_session: bool,
  /// 当前会话选择的活跃数据库编号 (SELECT <db>)
  pub active_db: u64,
  /// 连接是否已请求关闭
  pub is_closed: bool,
}

impl ServerSession {
  /// 创建新的服务端会话实例
  pub fn new(
    id: u64,
    has_password: bool,
    store_session: Arc<StoreSession<SegmentedDevice>>,
    pubsub_session: SessionHandle,
  ) -> Self {
    Self {
      id,
      authenticated: !has_password,
      user: None,
      user_ns: None,
      store_session,
      pubsub_session,
      in_txn: false,
      txn_aborted: false,
      txn_queue: Vec::new(),
      asking: false,
      is_readonly: false,
      is_subscription_session: false,
      active_db: 0,
      is_closed: false,
    }
  }

  /// 名字空间限定全名键：会话前缀 (Ns × Db) + 用户键
  ///
  /// 供阻塞命令 broker、发布订阅 broker 等跨会话匹配面使用，
  /// 保证不同名字空间/数据库的同名键互不串扰（wedb_txn 键契约同源）
  #[inline]
  pub fn full_key(&self, key: &[u8]) -> Bytes {
    let prefix = self.store_session.session_prefix();
    let mut full = bytes::BytesMut::with_capacity(prefix.len() + key.len());
    full.extend_from_slice(prefix.as_slice());
    full.extend_from_slice(key);
    full.freeze()
  }

  /// 认证成功后注入用户身份：记录用户名与绑定名字空间，并同步切换存储会话数据面名字空间
  #[inline]
  pub fn set_authenticated(&mut self, user: &str, ns: Option<u64>) {
    self.authenticated = true;
    self.user = Some(user.to_string());
    self.user_ns = ns;
    self.store_session.set_namespace(ns.unwrap_or(0));
  }

  /// 清除 ASKING 临时重定向标记
  #[inline]
  pub fn clear_asking(&mut self) {
    self.asking = false;
  }

  /// 标记关闭当前连接
  #[inline]
  pub fn close(&mut self) {
    self.is_closed = true;
  }

  /// 重置事务状态并排空事务队列
  #[inline]
  pub fn reset_txn(&mut self) {
    self.in_txn = false;
    self.txn_aborted = false;
    self.txn_queue.clear();
  }
}
