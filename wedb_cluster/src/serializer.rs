use std::{
  ffi::OsString,
  fs::{self, File},
  io::Write,
  path::{Path, PathBuf},
  str,
};

use crate::{
  config::{ClusterConfig, LOCAL_WORKER_ID},
  error::{Error, Result},
  node::{LinkState, NodeRole},
  slot::{HashSlot, SlotState, TOTAL_HASH_SLOTS},
  worker::Worker,
};

/// 集群配置序列化与物理持久化管理
pub struct ClusterConfigSerializer;

/// 临时文件路径：在原文件名后追加 ".tmp"
/// 避免替换扩展名导致文本与二进制两种格式的临时文件互相覆盖
fn tmp_path_of(path: &Path) -> PathBuf {
  let mut file_name = path
    .file_name()
    .map_or_else(OsString::new, ToOwned::to_owned);
  file_name.push(".tmp");
  path.with_file_name(file_name)
}

use serde::{Deserialize, Serialize};

/// 单个集群节点的 NestedText 序列化格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterNodeConfig {
  pub id: String,
  pub addr: String,
  #[serde(default = "default_primary_role")]
  pub role: String,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub master_id: Option<String>,
  #[serde(default)]
  pub epoch: i64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub hostname: Option<String>,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub slots: Option<String>,
}

fn default_primary_role() -> String {
  "master".to_string()
}

/// 集群整体配置的 NestedText 序列化格式
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClusterFileConfig {
  pub current_epoch: i64,
  #[serde(default, skip_serializing_if = "Option::is_none")]
  pub myself: Option<String>,
  pub nodes: Vec<ClusterNodeConfig>,
}

impl ClusterConfigSerializer {
  /// 导出为 NestedText 文本格式
  pub fn to_nested_text(config: &ClusterConfig) -> Result<String> {
    let myself_id = config.local_node_id().map(|s| s.to_string());
    let current_epoch = config.local_node_config_epoch();

    let mut nodes = Vec::with_capacity(config.workers.len().saturating_sub(1));
    for (i, w) in config.workers.iter().enumerate().skip(1) {
      let id = w.node_id.clone().unwrap_or_default();
      let addr = format!("{}:{}", w.address, w.port);
      let role = w.role.as_str().to_string();
      let master_id = w.replica_of_node_id.clone();
      let epoch = w.config_epoch;
      let hostname = w.hostname.clone();

      let mut slot_str = String::new();
      config.append_slot_ranges(&mut slot_str, i as u16);
      if i == LOCAL_WORKER_ID {
        config.append_special_states(&mut slot_str);
      }
      let trimmed = slot_str.trim();
      let slots = if trimmed.is_empty() {
        None
      } else {
        Some(trimmed.to_string())
      };

      nodes.push(ClusterNodeConfig {
        id,
        addr,
        role,
        master_id,
        epoch,
        hostname,
        slots,
      });
    }

    let file_config = ClusterFileConfig {
      current_epoch,
      myself: myself_id,
      nodes,
    };

    nested_text::to_string(&file_config).map_err(|e| Error::ConfigSerialization(e.to_string()))
  }

  /// 从 NestedText 文本反序列化还原 ClusterConfig
  pub fn from_nested_text(text: &str, myself_id: Option<&str>) -> Result<ClusterConfig> {
    let file_config: ClusterFileConfig =
      nested_text::from_str(text).map_err(|e| Error::ConfigSerialization(e.to_string()))?;

    if file_config.nodes.is_empty() {
      return Err(Error::ConfigSerialization("集群配置中无有效节点".into()));
    }

    let target_myself = myself_id.or(file_config.myself.as_deref());

    let mut parsed_workers: Vec<(Worker, bool, Vec<String>)> =
      Vec::with_capacity(file_config.nodes.len());

    for node in file_config.nodes {
      let (addr, port, hostname) = Self::parse_address_part(&node.addr)?;
      let hostname = node.hostname.or(hostname);
      // 角色非法即报错：损坏配置必须走 .tmp 崩溃自愈通道，严禁静默回退默认值
      let role = NodeRole::from_role_str(&node.role)
        .map_err(|_| Error::ConfigSerialization(format!("非法节点角色 '{}'", node.role)))?;
      let is_myself = target_myself.is_some_and(|my| my == node.id)
        || (target_myself.is_none() && parsed_workers.is_empty());

      let worker = Worker {
        node_id: if node.id.is_empty() {
          None
        } else {
          Some(node.id)
        },
        address: addr,
        port,
        config_epoch: node.epoch,
        role,
        replica_of_node_id: node.master_id,
        replication_offset: 0,
        hostname,
        link_state: LinkState::Connected,
        is_pfail: false,
        is_fail: false,
        handshake: false,
      };

      let slot_tokens: Vec<String> = node
        .slots
        .map(|s| s.split_whitespace().map(|x| x.to_string()).collect())
        .unwrap_or_default();

      parsed_workers.push((worker, is_myself, slot_tokens));
    }

    // 确定本地 myself 节点索引
    let myself_pos = parsed_workers
      .iter()
      .position(|(_, is_my, _)| *is_my)
      .unwrap_or(0);

    let (mut local_worker, _, local_slots) = parsed_workers.remove(myself_pos);
    if file_config.current_epoch > 0 && local_worker.config_epoch == 0 {
      local_worker.config_epoch = file_config.current_epoch;
    }

    let mut workers = Vec::with_capacity(parsed_workers.len() + 2);
    workers.push(Worker::unassigned());
    workers.push(local_worker);

    let mut all_slots = vec![(LOCAL_WORKER_ID as u16, local_slots)];

    for (w, _, slots) in parsed_workers {
      let wid = workers.len() as u16;
      workers.push(w);
      all_slots.push((wid, slots));
    }

    let mut slot_map = Box::new([HashSlot::default(); TOTAL_HASH_SLOTS]);

    // 第一遍：填充各节点稳定持有的槽位区间
    for (wid, tokens) in &all_slots {
      for token in tokens {
        if token.starts_with('[') {
          continue;
        }
        if let Some((start_s, end_s)) = token.split_once('-') {
          if let (Ok(start), Ok(end)) = (start_s.parse::<usize>(), end_s.parse::<usize>()) {
            for slot in start..=end {
              if slot < TOTAL_HASH_SLOTS {
                slot_map[slot] = HashSlot::new(*wid, SlotState::Stable);
              }
            }
          }
        } else if let Ok(slot) = token.parse::<usize>()
          && slot < TOTAL_HASH_SLOTS
        {
          slot_map[slot] = HashSlot::new(*wid, SlotState::Stable);
        }
      }
    }

    // 第二遍：叠加迁移/导入标记
    for (_, tokens) in &all_slots {
      for token in tokens {
        let Some(inner) = token.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
          continue;
        };
        if let Some((slot_str, target_id)) = inner.split_once("->-")
          && let Ok(slot) = slot_str.parse::<usize>()
          && slot < TOTAL_HASH_SLOTS
        {
          let target_wid = workers
            .iter()
            .position(|w| w.node_id.as_deref() == Some(target_id))
            .map(|idx| idx as u16)
            .unwrap_or(0);
          slot_map[slot] = HashSlot::new(target_wid, SlotState::Migrating);
        } else if let Some((slot_str, src_id)) = inner.split_once("-<-")
          && let Ok(slot) = slot_str.parse::<usize>()
          && slot < TOTAL_HASH_SLOTS
        {
          let src_wid = workers
            .iter()
            .position(|w| w.node_id.as_deref() == Some(src_id))
            .map(|idx| idx as u16)
            .unwrap_or(0);
          slot_map[slot] = HashSlot::new(src_wid, SlotState::Importing);
        }
      }
    }

    Ok(ClusterConfig { slot_map, workers })
  }

  /// 解析地址段: ip:port@cport[,hostname]
  /// 端口非法立即报错：静默回退默认端口会导致损坏配置被误加载
  fn parse_address_part(addr_str: &str) -> Result<(String, u16, Option<String>)> {
    let (endpoint_part, hostname) = match addr_str.split_once(',') {
      Some((ep, host)) => (ep, Some(host.to_string())),
      None => (addr_str, None),
    };

    let ip_port = match endpoint_part.split_once('@') {
      Some((ip_p, _)) => ip_p,
      None => endpoint_part,
    };

    let Some((ip, port_str)) = ip_port.rsplit_once(':') else {
      return Err(Error::ConfigSerialization(format!(
        "地址缺少端口: '{addr_str}'"
      )));
    };
    let port: u16 = port_str.parse().map_err(|_| {
      Error::ConfigSerialization(format!("地址端口非法: '{port_str}' in '{addr_str}'"))
    })?;

    Ok((ip.to_string(), port, hostname))
  }

  /// 原子落盘保存配置文件（断电与崩溃一致性保护，含 .tmp 临时文件与父目录 fsync）
  #[inline]
  pub fn save_to_file(config: &ClusterConfig, path: &Path) -> Result<()> {
    let content = Self::to_nested_text(config)?;
    atomic_save_file(path, content.as_bytes())
  }

  /// 从指定路径安全加载配置，支持崩溃自愈
  pub fn load_from_file(path: &Path, myself_id: Option<&str>) -> Result<ClusterConfig> {
    load_with_recovery(
      path,
      "配置文件不存在",
      "配置文件损坏且无有效恢复副本",
      |bytes| {
        let text = str::from_utf8(bytes).map_err(|e| Error::ConfigSerialization(e.to_string()))?;
        if text.trim().is_empty() {
          return Err(Error::ConfigSerialization("配置文本为空".into()));
        }
        Self::from_nested_text(text, myself_id)
      },
    )
  }

  /// 序列化为 Bitcode 格式字节数组
  #[inline]
  pub fn to_bitcode(config: &ClusterConfig) -> Vec<u8> {
    config.to_bitcode()
  }

  /// 从 Bitcode 格式字节数组还原配置
  #[inline]
  pub fn from_bitcode(bytes: &[u8]) -> Result<ClusterConfig> {
    ClusterConfig::from_bitcode(bytes)
  }

  /// 原子落盘保存 Bitcode 格式配置文件（断电一致性保护，含 .tmp 与父目录 fsync）
  #[inline]
  pub fn save_bitcode_to_file(config: &ClusterConfig, path: &Path) -> Result<()> {
    atomic_save_file(path, &Self::to_bitcode(config))
  }

  /// 从指定路径安全加载 Bitcode 配置，支持崩溃自愈
  pub fn load_bitcode_from_file(path: &Path) -> Result<ClusterConfig> {
    load_with_recovery(
      path,
      "Bitcode 配置文件不存在",
      "Bitcode 配置文件损坏且无有效恢复副本",
      Self::from_bitcode,
    )
  }
}

/// 物理原子落盘辅助：先写入 .tmp 文件并 fsync，再重命名覆盖目标文件，最后对父目录执行 fsync
fn atomic_save_file(path: &Path, content: &[u8]) -> Result<()> {
  if let Some(parent) = path.parent() {
    fs::create_dir_all(parent)?;
  }

  let tmp_path = tmp_path_of(path);
  {
    let mut file = File::create(&tmp_path)?;
    file.write_all(content)?;
    file.flush()?;
    file.sync_all()?;
  }

  fs::rename(&tmp_path, path)?;

  // 父目录 fsync 确保重命名的目录项物理持久化
  if let Some(parent) = path.parent()
    && let Ok(dir) = File::open(parent)
  {
    let _ = dir.sync_all();
  }

  Ok(())
}

/// 支持崩溃自愈的通用配置文件加载器
fn load_with_recovery<T>(
  path: &Path,
  missing_msg: &'static str,
  corrupted_msg: &'static str,
  parse: impl Fn(&[u8]) -> Result<T>,
) -> Result<T> {
  let tmp_path = tmp_path_of(path);

  if !path.exists() {
    if tmp_path.exists() {
      fs::rename(&tmp_path, path)?;
    } else {
      return Err(Error::ConfigSerialization(missing_msg.into()));
    }
  }

  match fs::read(path) {
    Ok(bytes) if !bytes.is_empty() => match parse(&bytes) {
      Ok(conf) => Ok(conf),
      Err(e) => {
        if tmp_path.exists()
          && let Ok(tmp_bytes) = fs::read(&tmp_path)
          && !tmp_bytes.is_empty()
          && let Ok(conf) = parse(&tmp_bytes)
        {
          let _ = fs::rename(&tmp_path, path);
          return Ok(conf);
        }
        Err(e)
      }
    },
    _ => {
      if tmp_path.exists()
        && let Ok(tmp_bytes) = fs::read(&tmp_path)
        && !tmp_bytes.is_empty()
        && let Ok(conf) = parse(&tmp_bytes)
      {
        let _ = fs::rename(&tmp_path, path);
        return Ok(conf);
      }
      Err(Error::CorruptedConfig(corrupted_msg.into()))
    }
  }
}
