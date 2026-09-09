use std::{fs, iter::once, path::Path};

use clap::{self, Arg, ArgAction, ArgMatches, Command, parser::ValueSource};
use serde::{Deserialize, Serialize};

use crate::error::{Error, Result};

/// 默认监听端口
pub const DEFAULT_PORT: u16 = 6379;
/// 默认监听地址
pub const DEFAULT_BIND: &str = "127.0.0.1";
/// 默认数据目录
pub const DEFAULT_DIR: &str = "./data";

/// 默认内置 GC 主动过期扫描间隔毫秒数（0 表示禁用定时扫描，对标 Garnet ExpiredKeyDeletionScanFrequencySecs = -1）
pub const DEFAULT_EXPIRED_SCAN_INTERVAL_MS: u64 = 0;

/// 默认内置 GC 日志紧缩调度间隔毫秒数（0 表示禁用定时调度，对标 Garnet CompactionType = None）
pub const DEFAULT_COMPACTION_INTERVAL_MS: u64 = 0;

/// 默认触发日志紧缩的只读区段数水位（8，对齐 wkv::GcConfig::default）
pub const DEFAULT_COMPACTION_MAX_SEGMENTS: usize = 8;

/// 默认单次紧缩参与的日志回退段数（1，对齐 wkv::GcConfig::default）
pub const DEFAULT_COMPACTION_NUM_SEGMENTS: usize = 1;

/// 默认单轮过期扫描最大物理删除键数（256，对齐 wkv::GcConfig::default）
pub const DEFAULT_GC_MAX_BATCH_DELETES: usize = 256;

/// 默认 AOF 自动提交策略（0 = 每批应答前提交落盘，对标 Garnet AofAutoCommit）
pub const DEFAULT_AOF_COMMIT_MS: i64 = 0;

// store 级周期紧缩（对标 Garnet CompactionFrequencySecs / CompactionMaxSeekForward）：
// 由 `StoreConfig::compaction_freq_secs` 与 `StoreConfig::compaction_max_seek_bytes`
// 驱动 `run_compaction_task`（按字节推进，auto() 默认 60 秒开启）；本模块的 gc_* 参数
// 面向内置 `GcManager`（TTL 主动过期扫描 + 紧缩调度），两套调度并存需注意合并治理。

/// 配置并返回命令行解析器
pub fn cmd(cmd: Command) -> Command {
  cmd
    .name("wedb-server")
    .about("WeDB 高性能 Redis 兼容服务端守护进程")
    .arg(
      Arg::new("config")
        .short('c')
        .long("config")
        .value_name("CONFIG")
        .help("服务端配置文件路径 (NestedText 格式，例如 wedb.nt)"),
    )
    .arg(
      Arg::new("port")
        .short('p')
        .long("port")
        .value_name("PORT")
        .help("监听业务端口 (默认 6379)")
        .value_parser(clap::value_parser!(u16))
        .default_value("6379"),
    )
    .arg(
      Arg::new("bind")
        .short('b')
        .long("bind")
        .value_name("BIND")
        .help("绑定监听 IP 地址 (默认 127.0.0.1)")
        .default_value(DEFAULT_BIND),
    )
    .arg(
      Arg::new("dir")
        .short('d')
        .long("dir")
        .value_name("DIR")
        .help("数据与持久化存储工作目录 (默认 ./data)")
        .default_value(DEFAULT_DIR),
    )
    .arg(
      Arg::new("cluster_enabled")
        .long("cluster-enabled")
        .help("是否启用分布式集群模式 (默认 false)")
        .action(ArgAction::SetTrue),
    )
    .arg(
      Arg::new("cluster_seeds")
        .long("cluster-seeds")
        .value_name("ADDRS")
        .help("集群种子节点列表，逗号分隔 (如 127.0.0.1:7000,127.0.0.1:7001)")
        .action(ArgAction::Set),
    )
    .arg(
      Arg::new("replicaof")
        .long("replicaof")
        .value_name("REPLICAOF")
        .help("主从复制目标主节点地址 (\"ip:port\")"),
    )
    .arg(
      Arg::new("requirepass")
        .long("requirepass")
        .value_name("REQUIREPASS")
        .help("访问认证密码"),
    )
    .arg(
      Arg::new("unixsocket")
        .long("unixsocket")
        .value_name("UNIXSOCKET")
        .help("Unix 域套接字监听路径 (可选)"),
    )
    .arg(
      Arg::new("quiet")
        .long("quiet")
        .help("静默模式 (不打印 ASCII banner)")
        .action(ArgAction::SetTrue),
    )
    .arg(
      Arg::new("gc_enabled")
        .long("gc-enabled")
        .value_name("BOOL")
        .help("是否启用内置 GC（主动过期扫描 + 日志紧缩调度，默认 false，对标 Garnet）")
        .value_parser(clap::value_parser!(bool))
        .default_value("false"),
    )
    .arg(
      Arg::new("expired_scan_interval_ms")
        .long("expired-scan-interval-ms")
        .value_name("MS")
        .help("内置 GC 主动过期扫描间隔毫秒数 (默认 0 表示禁用)")
        .value_parser(clap::value_parser!(u64))
        .default_value("0"),
    )
    .arg(
      Arg::new("compaction_interval_ms")
        .long("compaction-interval-ms")
        .value_name("MS")
        .help("内置 GC 日志紧缩调度间隔毫秒数 (默认 0 表示禁用)")
        .value_parser(clap::value_parser!(u64))
        .default_value("0"),
    )
    .arg(
      Arg::new("compaction_max_segments")
        .long("compaction-max-segments")
        .value_name("N")
        .help("触发日志紧缩的只读区段数水位 (默认 8)")
        .value_parser(clap::value_parser!(usize))
        .default_value("8"),
    )
    .arg(
      Arg::new("compaction_num_segments")
        .long("compaction-num-segments")
        .value_name("N")
        .help("单次紧缩参与的日志回退段数 (默认 1)")
        .value_parser(clap::value_parser!(usize))
        .default_value("1"),
    )
    .arg(
      Arg::new("gc_max_batch_deletes")
        .long("gc-max-batch-deletes")
        .value_name("N")
        .help("单轮过期扫描最大物理删除键数 (默认 256)")
        .value_parser(clap::value_parser!(usize))
        .default_value("256"),
    )
    .arg(
      Arg::new("aof_enabled")
        .long("aof-enabled")
        .value_name("BOOL")
        .help("是否启用 AOF 追加日志（增量持久化与复制流，默认 false，对标 Garnet EnableAOF）")
        .value_parser(clap::value_parser!(bool))
        .default_value("false"),
    )
    .arg(
      Arg::new("aof_commit_ms")
        .long("aof-commit-ms")
        .value_name("MS")
        .help("AOF 提交策略：0=每批应答前提交落盘；>0=周期毫秒提交；-1=仅 SAVE/停机时提交且不保证持久（默认 0，对标 Garnet CommitFrequencyMs）")
        .value_parser(clap::value_parser!(i64).range(-1..))
        .default_value("0"),
    )
}

/// WeDB 高性能 Redis 兼容服务端守护进程
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerArgs {
  /// 监听业务端口 (默认 6379)
  pub port: u16,

  /// 绑定监听 IP 地址 (默认 127.0.0.1)
  pub bind: String,

  /// 数据与持久化存储工作目录 (默认 ./data)
  pub dir: String,

  /// 是否启用分布式集群模式 (默认 false)
  pub cluster_enabled: bool,

  /// 主从复制目标主节点地址 ("ip:port")
  pub replicaof: Option<String>,

  /// 访问认证密码
  pub requirepass: Option<String>,

  /// Unix 域套接字监听路径 (可选)
  pub unixsocket: Option<String>,

  /// 静默模式 (不打印 ASCII banner)
  pub quiet: bool,

  /// 是否启用内置 GC（主动过期扫描 + 日志紧缩调度，对标 Garnet ExpiredKeyDeletionTask /
  /// CompactionTask；默认 false，对标 Garnet 默认禁用后台主动定时任务）
  pub gc_enabled: bool,

  /// 内置 GC 主动过期扫描间隔毫秒数（默认 0，0 表示禁用定时扫描，对标 Garnet）
  pub expired_scan_interval_ms: u64,

  /// 内置 GC 日志紧缩调度间隔毫秒数（默认 0，0 表示禁用定时调度，对标 Garnet）
  pub compaction_interval_ms: u64,

  /// 触发日志紧缩的只读区段数水位（read_only - begin 超过该段数即调度紧缩，默认 8）
  pub compaction_max_segments: usize,

  /// 单次紧缩参与的日志回退段数（默认 1）
  pub compaction_num_segments: usize,

  /// 单轮过期扫描最大物理删除键数（默认 256）
  pub gc_max_batch_deletes: usize,

  /// 是否启用 AOF 追加日志（默认 false，对标 Garnet EnableAOF）
  pub aof_enabled: bool,

  /// AOF 提交策略毫秒数（默认 0）：0=每批应答前提交落盘；>0=周期毫秒提交；
  /// -1=仅 SAVE/停机时提交（对标 Garnet CommitFrequencyMs 三档语义）
  pub aof_commit_ms: i64,

  /// 存储引擎内存预算覆盖（字节）。None = 按物理内存自适应；测试/嵌入式场景
  /// 显式给小值，避免巨型自适应配置（16GB 级页面+索引分配）在并发下抖动
  pub store_memory_budget: Option<u64>,

  /// 集群种子节点列表 ("ip:port")，配置后开机自动发起握手与拓扑发现
  pub cluster_seeds: Vec<String>,
}

impl Default for ServerArgs {
  fn default() -> Self {
    Self {
      port: DEFAULT_PORT,
      bind: DEFAULT_BIND.to_string(),
      dir: DEFAULT_DIR.to_string(),
      cluster_enabled: false,
      replicaof: None,
      requirepass: None,
      unixsocket: None,
      quiet: false,
      gc_enabled: false,
      expired_scan_interval_ms: DEFAULT_EXPIRED_SCAN_INTERVAL_MS,
      compaction_interval_ms: DEFAULT_COMPACTION_INTERVAL_MS,
      compaction_max_segments: DEFAULT_COMPACTION_MAX_SEGMENTS,
      compaction_num_segments: DEFAULT_COMPACTION_NUM_SEGMENTS,
      gc_max_batch_deletes: DEFAULT_GC_MAX_BATCH_DELETES,
      aof_enabled: false,
      aof_commit_ms: DEFAULT_AOF_COMMIT_MS,
      store_memory_budget: None,
      cluster_seeds: Vec::new(),
    }
  }
}

/// NestedText 格式的服务端配置文件模型
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ServerConfigFile {
  pub port: Option<u16>,
  pub bind: Option<String>,
  pub dir: Option<String>,
  pub cluster_enabled: Option<bool>,
  pub replicaof: Option<String>,
  pub requirepass: Option<String>,
  pub unixsocket: Option<String>,
  pub quiet: Option<bool>,
  pub gc_enabled: Option<bool>,
  pub expired_scan_interval_ms: Option<u64>,
  pub compaction_interval_ms: Option<u64>,
  pub compaction_max_segments: Option<usize>,
  pub compaction_num_segments: Option<usize>,
  pub gc_max_batch_deletes: Option<usize>,
  pub aof_enabled: Option<bool>,
  pub aof_commit_ms: Option<i64>,
  pub store_memory_budget: Option<u64>,

  /// 集群自动发现种子节点列表 (支持 NestedText 列表，每项为 "ip:port")
  #[serde(default, skip_serializing_if = "Vec::is_empty")]
  pub seeds: Vec<String>,
}

impl ServerConfigFile {
  /// 从 NestedText 字符串反序列化
  pub fn from_nested_text(text: &str) -> Result<Self> {
    nested_text::from_str(text).map_err(|e| Error::Custom(format!("解析 NestedText 配置失败: {e}")))
  }

  /// 序列化为 NestedText 格式字符串
  pub fn to_nested_text(&self) -> Result<String> {
    nested_text::to_string(self)
      .map_err(|e| Error::Custom(format!("生成 NestedText 配置失败: {e}")))
  }

  /// 从文件读取并反序列化 NestedText 配置
  pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
    let text = fs::read_to_string(path)?;
    Self::from_nested_text(&text)
  }
}

impl ServerArgs {
  /// 校验并从 ArgMatches 解析配置项（支持合并 NestedText 配置文件，命令行参数优先）
  pub fn try_from_matches(m: &ArgMatches) -> Result<Self> {
    let mut file_cfg = if let Some(path) = m.get_one::<String>("config") {
      Some(ServerConfigFile::load_from_file(path)?)
    } else {
      None
    };

    let port = if m.value_source("port") == Some(ValueSource::CommandLine) {
      *m.get_one::<u16>("port").unwrap_or(&DEFAULT_PORT)
    } else {
      file_cfg
        .as_ref()
        .and_then(|c| c.port)
        .unwrap_or(DEFAULT_PORT)
    };

    let bind = if m.value_source("bind") == Some(ValueSource::CommandLine) {
      m.get_one::<String>("bind")
        .cloned()
        .unwrap_or_else(|| DEFAULT_BIND.to_string())
    } else {
      file_cfg
        .as_mut()
        .and_then(|c| c.bind.take())
        .unwrap_or_else(|| DEFAULT_BIND.to_string())
    };

    let dir = if m.value_source("dir") == Some(ValueSource::CommandLine) {
      m.get_one::<String>("dir")
        .cloned()
        .unwrap_or_else(|| DEFAULT_DIR.to_string())
    } else {
      file_cfg
        .as_mut()
        .and_then(|c| c.dir.take())
        .unwrap_or_else(|| DEFAULT_DIR.to_string())
    };

    let has_cmd_seeds = m.value_source("cluster_seeds") == Some(ValueSource::CommandLine);
    let seeds_not_empty = if has_cmd_seeds {
      m.get_one::<String>("cluster_seeds")
        .map(|s| s.split(',').any(|x| !x.trim().is_empty()))
        .unwrap_or(false)
    } else {
      file_cfg
        .as_ref()
        .map(|c| !c.seeds.is_empty())
        .unwrap_or(false)
    };

    let cluster_enabled = if m.get_flag("cluster_enabled") {
      true
    } else {
      file_cfg
        .as_ref()
        .and_then(|c| c.cluster_enabled)
        .unwrap_or(seeds_not_empty)
    };

    let replicaof = if m.value_source("replicaof") == Some(ValueSource::CommandLine) {
      m.get_one::<String>("replicaof").cloned()
    } else {
      m.get_one::<String>("replicaof")
        .cloned()
        .or_else(|| file_cfg.as_mut().and_then(|c| c.replicaof.take()))
    };

    let requirepass = if m.value_source("requirepass") == Some(ValueSource::CommandLine) {
      m.get_one::<String>("requirepass").cloned()
    } else {
      m.get_one::<String>("requirepass")
        .cloned()
        .or_else(|| file_cfg.as_mut().and_then(|c| c.requirepass.take()))
    };

    let unixsocket = if m.value_source("unixsocket") == Some(ValueSource::CommandLine) {
      m.get_one::<String>("unixsocket").cloned()
    } else {
      m.get_one::<String>("unixsocket")
        .cloned()
        .or_else(|| file_cfg.as_mut().and_then(|c| c.unixsocket.take()))
    };

    let quiet = if m.get_flag("quiet") {
      true
    } else {
      file_cfg.as_ref().and_then(|c| c.quiet).unwrap_or(false)
    };

    let gc_enabled = if m.value_source("gc_enabled") == Some(ValueSource::CommandLine) {
      *m.get_one::<bool>("gc_enabled").unwrap_or(&false)
    } else {
      file_cfg
        .as_ref()
        .and_then(|c| c.gc_enabled)
        .unwrap_or(false)
    };

    let expired_scan_interval_ms =
      if m.value_source("expired_scan_interval_ms") == Some(ValueSource::CommandLine) {
        *m.get_one::<u64>("expired_scan_interval_ms")
          .unwrap_or(&DEFAULT_EXPIRED_SCAN_INTERVAL_MS)
      } else {
        file_cfg
          .as_ref()
          .and_then(|c| c.expired_scan_interval_ms)
          .unwrap_or(DEFAULT_EXPIRED_SCAN_INTERVAL_MS)
      };

    let compaction_interval_ms =
      if m.value_source("compaction_interval_ms") == Some(ValueSource::CommandLine) {
        *m.get_one::<u64>("compaction_interval_ms")
          .unwrap_or(&DEFAULT_COMPACTION_INTERVAL_MS)
      } else {
        file_cfg
          .as_ref()
          .and_then(|c| c.compaction_interval_ms)
          .unwrap_or(DEFAULT_COMPACTION_INTERVAL_MS)
      };

    let compaction_max_segments =
      if m.value_source("compaction_max_segments") == Some(ValueSource::CommandLine) {
        *m.get_one::<usize>("compaction_max_segments")
          .unwrap_or(&DEFAULT_COMPACTION_MAX_SEGMENTS)
      } else {
        file_cfg
          .as_ref()
          .and_then(|c| c.compaction_max_segments)
          .unwrap_or(DEFAULT_COMPACTION_MAX_SEGMENTS)
      };

    let compaction_num_segments =
      if m.value_source("compaction_num_segments") == Some(ValueSource::CommandLine) {
        *m.get_one::<usize>("compaction_num_segments")
          .unwrap_or(&DEFAULT_COMPACTION_NUM_SEGMENTS)
      } else {
        file_cfg
          .as_ref()
          .and_then(|c| c.compaction_num_segments)
          .unwrap_or(DEFAULT_COMPACTION_NUM_SEGMENTS)
      };

    let gc_max_batch_deletes =
      if m.value_source("gc_max_batch_deletes") == Some(ValueSource::CommandLine) {
        *m.get_one::<usize>("gc_max_batch_deletes")
          .unwrap_or(&DEFAULT_GC_MAX_BATCH_DELETES)
      } else {
        file_cfg
          .as_ref()
          .and_then(|c| c.gc_max_batch_deletes)
          .unwrap_or(DEFAULT_GC_MAX_BATCH_DELETES)
      };

    let aof_enabled = if m.value_source("aof_enabled") == Some(ValueSource::CommandLine) {
      *m.get_one::<bool>("aof_enabled").unwrap_or(&false)
    } else {
      file_cfg
        .as_ref()
        .and_then(|c| c.aof_enabled)
        .unwrap_or(false)
    };

    let aof_commit_ms = if m.value_source("aof_commit_ms") == Some(ValueSource::CommandLine) {
      *m.get_one::<i64>("aof_commit_ms")
        .unwrap_or(&DEFAULT_AOF_COMMIT_MS)
    } else {
      file_cfg
        .as_ref()
        .and_then(|c| c.aof_commit_ms)
        .unwrap_or(DEFAULT_AOF_COMMIT_MS)
    };

    let store_memory_budget = file_cfg.as_ref().and_then(|c| c.store_memory_budget);

    let cluster_seeds = if has_cmd_seeds {
      m.get_one::<String>("cluster_seeds")
        .map(|s| {
          s.split(',')
            .map(str::trim)
            .filter(|x| !x.is_empty())
            .map(ToString::to_string)
            .collect()
        })
        .unwrap_or_default()
    } else {
      file_cfg.map(|c| c.seeds).unwrap_or_default()
    };

    Ok(Self {
      port,
      bind,
      dir,
      cluster_enabled,
      replicaof,
      requirepass,
      unixsocket,
      quiet,
      gc_enabled,
      expired_scan_interval_ms,
      compaction_interval_ms,
      compaction_max_segments,
      compaction_num_segments,
      gc_max_batch_deletes,
      aof_enabled,
      aof_commit_ms,
      store_memory_budget,
      cluster_seeds,
    })
  }

  /// 从 ArgMatches 构造解析配置项（若有文件读取失败则记录 warn 并回退到命令行/默认）
  pub fn from_matches(m: &ArgMatches) -> Self {
    match Self::try_from_matches(m) {
      Ok(args) => args,
      Err(e) => {
        log::warn!("解析配置文件失败，回退至命令行默认配置: {e}");
        Self::default()
      }
    }
  }

  /// 从命令行参数切片构造解析配置项
  pub fn from_argv(argv: &[&str]) -> Result<Self> {
    let command = cmd(Command::new("wedb-server"));
    let m = if argv.first().is_some_and(|&s| s == "wedb-server") {
      command.try_get_matches_from(argv)
    } else {
      command.try_get_matches_from(once("wedb-server").chain(argv.iter().copied()))
    }
    .map_err(|e| Error::Custom(e.to_string()))?;
    Self::try_from_matches(&m)
  }
}

#[cfg(test)]
mod tests {
  use std::io::Write;

  use tempfile::NamedTempFile;

  use super::*;

  #[test]
  fn test_nested_text_config_serde_roundtrip() -> Result<()> {
    let cfg = ServerConfigFile {
      port: Some(6380),
      bind: Some("0.0.0.0".to_string()),
      dir: Some("/var/lib/wedb".to_string()),
      cluster_enabled: Some(true),
      requirepass: Some("secret123".to_string()),
      seeds: vec!["127.0.0.1:7000".to_string(), "127.0.0.1:7001".to_string()],
      ..Default::default()
    };

    let text = cfg.to_nested_text()?;
    let restored = ServerConfigFile::from_nested_text(&text)?;
    assert_eq!(cfg, restored);
    Ok(())
  }

  #[test]
  fn test_config_file_and_cli_precedence() -> Result<()> {
    let mut file = NamedTempFile::new().unwrap();
    let file_content = r#"
port: 7001
bind: 10.0.0.1
cluster_enabled: true
requirepass: filepass
"#;
    file.write_all(file_content.trim().as_bytes()).unwrap();
    file.flush().unwrap();
    let file_path = file.path().to_str().unwrap();

    // 仅通过配置文件加载
    let args1 = ServerArgs::from_argv(&["-c", file_path])?;
    assert_eq!(args1.port, 7001);
    assert_eq!(args1.bind, "10.0.0.1");
    assert!(args1.cluster_enabled);
    assert_eq!(args1.requirepass.as_deref(), Some("filepass"));

    // 命令行显式参数覆盖配置文件
    let args2 = ServerArgs::from_argv(&[
      "-c",
      file_path,
      "--port",
      "7002",
      "--requirepass",
      "clipass",
      "--cluster-seeds",
      "10.0.0.2:7000,10.0.0.3:7000",
    ])?;
    assert_eq!(args2.port, 7002); // 命令行覆盖为 7002
    assert_eq!(args2.bind, "10.0.0.1"); // 保留配置文件
    assert_eq!(args2.requirepass.as_deref(), Some("clipass")); // 命令行覆盖
    assert_eq!(args2.cluster_seeds, vec!["10.0.0.2:7000", "10.0.0.3:7000"]);

    Ok(())
  }
}
