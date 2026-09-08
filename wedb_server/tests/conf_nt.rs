use std::path::PathBuf;

use aok::{OK, Void};
use compio::runtime::Runtime;
use wedb_server::{ServerArgs, WedbServer};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_load_demo_conf_nt() -> Void {
  let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
  let conf_path = manifest_dir.join("tests").join("conf.nt");
  assert!(conf_path.exists(), "演示配置文件 tests/conf.nt 必须存在");

  let conf_str = conf_path.to_str().unwrap();
  let args = ServerArgs::from_argv(&["-c", conf_str])?;

  // 校验基础配置
  assert_eq!(args.port, 6379);
  assert_eq!(args.bind, "127.0.0.1");
  assert_eq!(args.dir, "./data");
  assert!(args.cluster_enabled);
  assert_eq!(args.requirepass.as_deref(), Some("secret123"));
  assert!(args.gc_enabled);
  assert_eq!(args.expired_scan_interval_ms, 1000);
  assert_eq!(args.compaction_interval_ms, 60000);
  assert_eq!(args.compaction_max_segments, 8);
  assert_eq!(args.compaction_num_segments, 1);
  assert_eq!(args.gc_max_batch_deletes, 256);
  assert_eq!(args.cluster_seeds, vec!["127.0.0.1:6379", "127.0.0.1:6380"]);

  // 验证纯动态组网：启动 Server 实例（绑定 127.0.0.1:6379）
  let rt = Runtime::new()?;
  rt.block_on(async move {
    let mut server_args = args;
    let temp_dir = tempfile::tempdir()?;
    server_args.dir = temp_dir.path().to_str().unwrap().to_string();
    server_args.port = 6379; // 与种子配置一致，测试自身地址过滤

    let server = WedbServer::new(server_args).await?;
    let cluster_opt = server.context.cluster.as_ref();
    assert!(cluster_opt.is_some(), "集群管理器必须已就绪");
    let cm = cluster_opt.unwrap();
    let current = cm.current_config();
    let local_id = current
      .local_node_id()
      .expect("本地节点必须具有唯一 Node ID");
    assert_eq!(local_id.len(), 40, "Node ID 必须为 40 字符十六进制");

    // 方式 2：开机自动纳管种子节点（自身 6379 自动跳过，对端 6380 自动入网）
    assert_eq!(
      cm.current_config().num_workers(),
      2,
      "方式2: 自动向种子节点握手后已知节点数应为 2"
    );

    // 方式 1：运行时继续通过 try_meet 动态追加节点 (命令驱动)
    cm.try_meet("127.0.0.1", 6381, None)?;
    assert_eq!(
      cm.current_config().num_workers(),
      3,
      "方式1: 运行时动态 MEET 后已知节点数应为 3"
    );

    server.close();
    OK
  })?;

  OK
}

#[test]
fn test_is_myself_addr_boundary() {
  use wedb_server::context::is_myself_addr;

  // 1. 端口不同绝非自身
  assert!(!is_myself_addr("127.0.0.1", 6379, "127.0.0.1", 6380));
  assert!(!is_myself_addr("0.0.0.0", 6379, "0.0.0.0", 6380));

  // 2. 端口为 0 属于非法端口，绝非自身
  assert!(!is_myself_addr("127.0.0.1", 0, "127.0.0.1", 0));
  assert!(!is_myself_addr("127.0.0.1", 6379, "127.0.0.1", 0));

  // 3. 完全相同 IP 与端口（忽略大小写）
  assert!(is_myself_addr("127.0.0.1", 6379, "127.0.0.1", 6379));
  assert!(is_myself_addr("192.168.1.100", 7000, "192.168.1.100", 7000));
  assert!(is_myself_addr("node1.cluster", 7000, "NODE1.CLUSTER", 7000));

  // 4. 回环与通配同义识别 (0.0.0.0, 127.0.0.1, localhost, ::1)
  assert!(is_myself_addr("0.0.0.0", 6379, "127.0.0.1", 6379));
  assert!(is_myself_addr("127.0.0.1", 6379, "0.0.0.0", 6379));
  assert!(is_myself_addr("127.0.0.1", 6379, "localhost", 6379));
  assert!(is_myself_addr("localhost", 6379, "127.0.0.1", 6379));
  assert!(is_myself_addr("0.0.0.0", 6379, "localhost", 6379));
  assert!(is_myself_addr("::", 6379, "::1", 6379));

  // 5. 跨物理节点区分
  assert!(!is_myself_addr("192.168.1.10", 6379, "192.168.1.11", 6379));
  assert!(!is_myself_addr("127.0.0.1", 6379, "192.168.1.11", 6379));
}
