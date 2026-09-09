use std::{
  fs::create_dir_all,
  net::IpAddr,
  path::{Path, PathBuf},
  sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
  },
};

use coarsetime::{Clock, Instant};
use log::{info, warn};
use wdev::SegmentedDevice;
use wedb_acl::AccessControlList;
use wedb_aof::AofLog;
use wedb_blocking::CollectionItemBroker;
use wedb_cluster::{ClusterManager, Worker};
use wedb_net::LimitedFixedBufferPool;
use wedb_pubsub::SubscribeBroker;
use wedb_repl::{NodeRole as ReplNodeRole, ReplicationManager};
use wkv::{CheckpointManager, GcConfig, StoreConfig, WedbStore};

use crate::{
  config::ServerArgs,
  error::{Error, Result},
  ns_alloc::NamespaceAllocator,
};

/// 单分段存储文件大小：64 MB
const DEFAULT_SEGMENT_SIZE: u64 = 64 * 1024 * 1024;
/// 扇区对齐大小：4096 字节
const DEFAULT_SECTOR_SIZE: usize = 4096;
/// 数据库文件名称
const STORE_DB_FILE: &str = "store.db";
/// 检查点快照目录名
const CHECKPOINT_DIR: &str = "checkpoints";

/// 全局共享服务上下文
pub struct ServerContext {
  /// 启动配置参数
  pub args: ServerArgs,
  /// 混合日志底层存储引擎
  pub store: Arc<WedbStore<SegmentedDevice>>,
  /// AOF 追加日志门面（增量持久化与复制流，未启用时为 disabled 门面）
  pub aof: Arc<AofLog>,
  /// 检查点快照目录 (SAVE/BGSAVE 落盘目标)
  pub checkpoint_dir: PathBuf,
  /// 最近一次成功 SAVE 的 UNIX 毫秒时间戳 (LASTSAVE 依据，初始为服务启动时刻)
  pub last_save_ms: Arc<AtomicU64>,
  /// 用户访问权限控制管理器
  pub acl: Arc<AccessControlList>,
  /// 高并发发布订阅中继器
  pub pubsub: Arc<SubscribeBroker>,
  /// 阻塞集合命令核心调度中继器
  pub blocking: Arc<CollectionItemBroker>,
  /// 分布式集群管理器（若未启用则为 None）
  pub cluster: Option<Arc<ClusterManager>>,
  /// 主从复制与拓扑管理器
  pub repl: Arc<ReplicationManager>,
  /// 2 的幂次分级定额全局网络缓冲池（对标 Garnet LimitedFixedBufferPool）
  pub network_pool: Arc<LimitedFixedBufferPool>,
  /// 向量集键空间管理器 (V* 命令)
  pub vector: Arc<wedb_vector::VectorManager>,
  /// Lua 脚本引擎（互斥串行运行，对标 Garnet SessionScriptCache）
  pub scripts: parking_lot::Mutex<wedb_lua::ScriptEngine>,
  /// 模块注册表 (模块扩展 API)
  pub modules: Arc<wedb_module::ModuleRegistry>,
  /// 名字空间控制面分配器 (`NS 0` 自动分配 / `NS N` 水位推进，落盘 `[0x20]`)
  pub ns_alloc: NamespaceAllocator,
  /// 服务启动时间点
  pub start_time: Instant,
  /// 运行状态标志
  pub is_running: Arc<AtomicBool>,
  /// 当前活跃客户端连接数
  pub active_connections: Arc<AtomicUsize>,
  /// 累计接收客户端连接数
  pub total_received: Arc<AtomicUsize>,
  /// 累计断开销毁客户端连接数
  pub total_disposed: Arc<AtomicUsize>,
}

impl ServerContext {
  /// 根据命令行配置参数初始化创建服务端上下文 (含 Checkpoint 崩溃恢复)
  pub async fn new(args: ServerArgs) -> Result<Self> {
    create_dir_all(&args.dir)?;
    let checkpoint_dir = Path::new(&args.dir).join(CHECKPOINT_DIR);
    let db_path = Path::new(&args.dir).join(STORE_DB_FILE);
    let device = Arc::new(SegmentedDevice::new(
      db_path,
      Some(DEFAULT_SEGMENT_SIZE),
      DEFAULT_SECTOR_SIZE,
    )?);

    let gc_config = GcConfig {
      enabled: args.gc_enabled,
      scan_interval_ms: args.expired_scan_interval_ms,
      compaction_interval_ms: args.compaction_interval_ms,
      compaction_max_segments: args.compaction_max_segments,
      compaction_num_segments: args.compaction_num_segments,
      max_batch_deletes: args.gc_max_batch_deletes,
      ..GcConfig::default()
    };

    // 优先从最新有效 Checkpoint 崩溃恢复 (hlog + HashIndex + RangeIndex + 共享 BfTree)；
    // 目录中不存在任何检查点时全新开启。存在检查点但全部损坏属于数据事故，
    // 直接报错终止启动，绝不静默弃用既有数据重新开库。
    let store: Arc<WedbStore<SegmentedDevice>> = match CheckpointManager::recover_latest(
      &checkpoint_dir,
      Arc::clone(&device),
    )
    .await
    {
      Ok(mut store) => {
        info!(
          "从 Checkpoint 恢复存储引擎完成: checkpoint_dir={}",
          checkpoint_dir.display()
        );
        store.config.gc = gc_config;
        Arc::new(store)
      }
      Err(wcpr::Error::NoValidCheckpoint(_)) => {
        let store_config = match args.store_memory_budget {
          // 测试/嵌入式显式预算：绕过按物理内存的自适应巨型配置
          Some(budget) => StoreConfig::auto_with_budget(budget),
          None => StoreConfig::auto(),
        }
        .with_range_index_dir(&args.dir)
        .with_bftree_path(
          Path::new(&args.dir)
            .join("bftree")
            .join("shared.data.bftree"),
        )
        .with_gc(gc_config);
        info!(
          "WeDB 存储引擎自适应硬件配置就绪: 内存预算={:.2}MB, 页大小={}KB, 页面数={}, 索引桶数={}, 最大并发会话数={}, 内置GC={}",
          (store_config.page_size * store_config.num_pages) as f64 / (1024.0 * 1024.0),
          store_config.page_size / 1024,
          store_config.num_pages,
          store_config.index_size,
          store_config.max_sessions,
          store_config.gc.enabled
        );
        Arc::new(WedbStore::open(store_config, device)?)
      }
      Err(e) => {
        warn!("存在 Checkpoint 但恢复失败，拒绝启动以保护既有数据: {e}");
        return Err(e.into());
      }
    };

    let compactor = wcompact::LogCompactor::new(Arc::clone(&store));
    store.set_gc_compactor(Arc::new(move |_store, until| {
      let compactor = compactor.clone();
      Box::pin(async move {
        let stats = compactor
          .compact_with_filter(until, wcompact::CompactionType::Lookup, |_, _| false)
          .await
          .map_err(|e| wkv::Error::InvalidConfig(e.to_string()))?;
        Ok(wkv::GcCompactOutcome {
          dead_dropped: stats.dead_dropped as u64,
          bytes_freed: stats.bytes_freed,
          new_begin_address: stats.new_begin_address,
        })
      })
    }));

    // 对标 Garnet StartPrimaryTasks：仅主节点（非 replicaof）启动后台 GC 任务
    if args.replicaof.is_none() && store.config.gc.enabled {
      store.start_gc();
    }

    // AOF 门面：先打开日志并恢复重放（Checkpoint 快照之后的增量帧回放到引擎），
    // 再注入写监听端口——顺序保证重放写不会二次进入 AOF（对标 Garnet
    // RecoverCheckpointAsync → RecoverAOFAsync → ReplayAOF 的恢复次序）
    let aof = Arc::new(if args.aof_enabled {
      let aof = AofLog::open(Path::new(&args.dir), args.aof_commit_ms).await?;
      let replayed = aof.recover_and_replay(&store).await?;
      if replayed > 0 {
        info!("AOF 增量重放完成: {replayed} 帧已应用回存储引擎");
      }
      aof
    } else {
      AofLog::disabled()
    });
    // 两个引擎写端口分别注帧家族：hlog 效果（Upsert/Tombstone）与
    // 共享 BfTree 效果（BfTreePut/BfTreeDelete，覆盖 Flattened ZSET score
    // 唯一副本，对标 C# RangeIndexStreamChunk 专用帧）
    if let Some(listener) = aof.write_listener()
      && !store.set_write_listener(listener)
    {
      return Err(Error::Custom("AOF 写监听端口重复注入".into()));
    }
    if let Some(listener) = aof.bftree_listener()
      && !store.bftree.set_write_listener(listener)
    {
      return Err(Error::Custom("AOF BfTree 写监听端口重复注入".into()));
    }
    if let Some(listener) = aof.range_listener()
      && !store.set_range_listener(listener)
    {
      return Err(Error::Custom("AOF RangeIndex 写监听端口重复注入".into()));
    }

    let default_pwd = args.requirepass.as_deref().unwrap_or("");
    let acl = Arc::new(AccessControlList::new(default_pwd));
    acl.set_storage(Arc::new(crate::StoreAclStorage::new(store.bftree.clone())));

    let pubsub = Arc::new(SubscribeBroker::new());
    let blocking = Arc::new(CollectionItemBroker::new().with_signal_hint(true));
    let vector = Arc::new(wedb_vector::VectorManager::new());
    let scripts = parking_lot::Mutex::new(
      wedb_lua::ScriptEngine::new(wedb_lua::DEFAULT_TIMEOUT)
        .map_err(|e| Error::Custom(format!("Lua 脚本引擎初始化失败: {e}")))?,
    );
    let modules = Arc::new(wedb_module::ModuleRegistry::default());
    let ns_alloc = NamespaceAllocator::from_bftree(Arc::clone(&store.bftree))?;

    let cluster = if args.cluster_enabled {
      let cm = ClusterManager::new();
      let node_id = wedb_cluster::NodeId::generate().to_string();
      cm.init_local(Worker::primary(node_id, &args.bind, args.port, 1));
      let all_slots: Vec<u16> = (0..16384).collect();
      let _ = cm.try_add_slots(&all_slots);

      // 方式 2：自动向配置的种子节点列表发起握手组网 (参考 Redis clusterStartHandshake)
      for seed in &args.cluster_seeds {
        if let Some((ip, port_str)) = seed.trim().split_once(':') {
          let ip = ip.trim();
          if let Ok(port) = port_str.trim().parse::<u16>()
            && port > 0
            && !is_myself_addr(&args.bind, args.port, ip, port)
          {
            let _ = cm.try_meet(ip, port, None);
          }
        }
      }

      Some(Arc::new(cm))
    } else {
      None
    };

    let initial_role = if args.replicaof.is_some() {
      ReplNodeRole::Replica
    } else {
      ReplNodeRole::Primary
    };
    let repl = Arc::new(ReplicationManager::new(initial_role, 0));
    let network_pool = LimitedFixedBufferPool::default_pool();

    Ok(Self {
      args,
      store,
      aof,
      checkpoint_dir,
      last_save_ms: Arc::new(AtomicU64::new(Clock::now_since_epoch().as_millis())),
      acl,
      pubsub,
      blocking,
      cluster,
      repl,
      network_pool,
      vector,
      scripts,
      modules,
      ns_alloc,
      start_time: Instant::now(),
      is_running: Arc::new(AtomicBool::new(true)),
      active_connections: Arc::new(AtomicUsize::new(0)),
      total_received: Arc::new(AtomicUsize::new(0)),
      total_disposed: Arc::new(AtomicUsize::new(0)),
    })
  }

  /// 获取当前活跃客户端连接数
  #[inline]
  pub fn active_connections(&self) -> usize {
    self.active_connections.load(Ordering::Relaxed)
  }

  /// 获取累计接收连接数
  #[inline]
  pub fn total_connections_received(&self) -> usize {
    self.total_received.load(Ordering::Relaxed)
  }

  /// 获取累计断开销毁连接数
  #[inline]
  pub fn total_connections_disposed(&self) -> usize {
    self.total_disposed.load(Ordering::Relaxed)
  }

  /// 重置累计接收连接数
  #[inline]
  pub fn reset_connections_received(&self) {
    self.total_received.store(0, Ordering::Relaxed);
  }

  /// 重置累计断开销毁连接数
  #[inline]
  pub fn reset_connections_disposed(&self) {
    self.total_disposed.store(0, Ordering::Relaxed);
  }

  /// 检查服务是否处于运行中
  #[inline]
  pub fn is_running(&self) -> bool {
    self.is_running.load(Ordering::Relaxed)
  }

  /// 停止运行状态标志
  #[inline]
  pub fn stop(&self) {
    self.is_running.store(false, Ordering::SeqCst);
  }
}

/// 判断目标地址与端口是否指向当前节点自身 (对标 Redis clusterNodeIsMyself)
#[inline]
pub fn is_myself_addr(
  local_bind: &str,
  local_port: u16,
  target_ip: &str,
  target_port: u16,
) -> bool {
  if local_port != target_port || target_port == 0 {
    return false;
  }
  if local_bind.eq_ignore_ascii_case(target_ip) {
    return true;
  }

  #[inline]
  fn is_local_or_loopback(host: &str) -> bool {
    if host.eq_ignore_ascii_case("localhost") {
      return true;
    }
    if let Ok(ip) = host.parse::<IpAddr>() {
      ip.is_loopback() || ip.is_unspecified()
    } else {
      false
    }
  }

  is_local_or_loopback(local_bind) && is_local_or_loopback(target_ip)
}
