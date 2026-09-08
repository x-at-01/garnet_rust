use std::{
  fmt,
  fs::{File, OpenOptions, remove_file, rename},
  io::{Read, Write},
  path::{Path, PathBuf},
  str::from_utf8_unchecked,
};

use crate::error::{Error, Result};

/// 复制编号字符长度（固定 40 字节十六进制）
pub const REPL_ID_LEN: usize = 40;

/// 复制历史磁盘持久化格式版本号
pub const REPLICATION_HISTORY_VERSION: u8 = 1;

/// 复制历史序列化二进制最小长度 (1 + 40 + 40 + 8 + 8 = 97)
pub const REPLICATION_HISTORY_BYTES_LEN: usize = 1 + REPL_ID_LEN * 2 + 16;

const OFFSET_PRIMARY_REPLID: usize = 1;
const OFFSET_PRIMARY_REPLID2: usize = OFFSET_PRIMARY_REPLID + REPL_ID_LEN;
const OFFSET_REPL_OFFSET: usize = OFFSET_PRIMARY_REPLID2 + REPL_ID_LEN;
const OFFSET_REPL_OFFSET2: usize = OFFSET_REPL_OFFSET + 8;

/// 十六进制字符查找表
const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

/// 40 字节十六进制复制编号（全局唯一标识一次复制拓扑纪元）
#[derive(Clone, Copy, PartialEq, Eq, Hash, bitcode::Encode, bitcode::Decode)]
pub struct ReplId(pub [u8; REPL_ID_LEN]);

impl ReplId {
  /// 生成随机 40 字符十六进制复制编号（使用 160 位熵源与位运算单次查表转换）
  pub fn generate() -> Self {
    let mut bytes = [0u8; REPL_ID_LEN];
    let mut rand_bytes = [0u8; 20];
    fastrand::fill(&mut rand_bytes);
    for (i, &b) in rand_bytes.iter().enumerate() {
      let idx = i * 2;
      unsafe {
        *bytes.get_unchecked_mut(idx) = *HEX_CHARS.get_unchecked((b >> 4) as usize);
        *bytes.get_unchecked_mut(idx + 1) = *HEX_CHARS.get_unchecked((b & 0x0f) as usize);
      }
    }
    Self(bytes)
  }

  /// 创建全 0 的空复制编号
  #[inline]
  pub const fn empty() -> Self {
    Self([b'0'; REPL_ID_LEN])
  }

  /// 创建全 '?' 的问号复制编号（用于初次协商）
  #[inline]
  pub const fn question_mark() -> Self {
    Self([b'?'; REPL_ID_LEN])
  }

  /// 检查是否为全 0 空编号
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.0 == [b'0'; REPL_ID_LEN]
  }

  /// 检查是否为 '?' 问号编号
  #[inline]
  pub const fn is_question_mark(&self) -> bool {
    self.0[0] == b'?'
  }

  /// 获取字符串切片引用（零拷贝零分支）
  #[inline]
  pub fn as_str(&self) -> &str {
    // 内部字节仅包含 HEX_CHARS 或 b'?'，100% 保证为合法 ASCII 字符串
    unsafe { from_utf8_unchecked(&self.0) }
  }

  /// 获取底层字节数组引用
  #[inline]
  pub const fn as_bytes(&self) -> &[u8; REPL_ID_LEN] {
    &self.0
  }

  /// 获取底层字节切片引用
  #[inline]
  pub const fn as_slice(&self) -> &[u8] {
    &self.0
  }

  /// 从字节切片中解析复制编号（支持标准 40 字节与 '?' 简写）
  pub fn from_bytes(slice: &[u8]) -> Result<Self> {
    if slice == b"?" || (slice.len() == REPL_ID_LEN && slice.iter().all(|&b| b == b'?')) {
      return Ok(Self::question_mark());
    }
    if slice.len() != REPL_ID_LEN {
      return Err(Error::InvalidReplId(format!(
        "长度必须为 40 字节，实际为 {}",
        slice.len()
      )));
    }
    for &b in slice {
      if !b.is_ascii_hexdigit() {
        return Err(Error::InvalidReplId(format!(
          "字符 '{}' 非合法十六进制字符",
          b as char
        )));
      }
    }
    let bytes: [u8; REPL_ID_LEN] = unsafe { slice.try_into().unwrap_unchecked() };
    Ok(Self(bytes))
  }

  /// 从字符串解析复制编号
  #[inline]
  pub fn from_str_val(s: &str) -> Result<Self> {
    Self::from_bytes(s.as_bytes())
  }
}

impl Default for ReplId {
  fn default() -> Self {
    Self::empty()
  }
}

impl fmt::Debug for ReplId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "ReplId({})", self.as_str())
  }
}

impl fmt::Display for ReplId {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

/// 复制历史记录（管理双复制编号架构与复制位点）
#[derive(Debug, Clone, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub struct ReplicationHistory {
  /// 当前主节点复制编号
  pub primary_replid: ReplId,
  /// 上一代主节点复制编号（故障转移时继承，使滞后节点可继续增量追赶）
  pub primary_replid2: ReplId,
  /// 当前主节点最新的复制写入位点
  pub replication_offset: u64,
  /// 上一代主节点复制编号有效的最大截断边界偏移量
  pub replication_offset2: u64,
}

impl ReplicationHistory {
  /// 创建新的复制历史实例
  pub fn new(initial_offset: u64) -> Self {
    Self {
      primary_replid: ReplId::generate(),
      primary_replid2: ReplId::empty(),
      replication_offset: initial_offset,
      replication_offset2: u64::MAX,
    }
  }

  /// 发生故障转移升主时更新拓扑（对应微软 Garnet ReplicationHistory.FailoverUpdate）：
  /// - 将当前主复制编号降级为上一代复制编号
  /// - 将上一代截断位点记录为当前尾部位点
  /// - 生成全新主复制编号
  ///
  /// 与 C# 的差异：C# FailoverUpdate 不改 replicationOffset（主节点位点直接读 AOF 尾地址）；
  /// 本实现无 per-sublog AofAddress，history.replication_offset 即全局位点，
  /// 升主后必须原子对齐到新主自身的日志尾部 current_tail，否则 PSYNC 判定会拿到旧主位点
  pub fn failover_update(&self, tail_offset: u64) -> Self {
    Self {
      primary_replid2: self.primary_replid,
      primary_replid: ReplId::generate(),
      replication_offset: tail_offset,
      replication_offset2: tail_offset,
    }
  }

  /// 手动更新当前主节点复制编号（对应微软 Garnet UpdateReplicationId）
  pub fn update_primary_replid(&mut self, new_id: ReplId) {
    self.primary_replid = new_id;
  }

  /// 更新最新复制偏移量
  pub fn set_offset(&mut self, offset: u64) {
    self.replication_offset = offset;
  }

  /// bitcode 极速二进制序列化
  #[inline]
  pub fn to_bitcode(&self) -> Vec<u8> {
    bitcode::encode(self)
  }

  /// 从 bitcode 反序列化复制历史
  #[inline]
  pub fn from_bitcode(bytes: &[u8]) -> Result<Self> {
    bitcode::decode(bytes).map_err(|e| Error::Protocol(format!("bitcode 反序列化失败: {e}")))
  }

  /// 二进制定长数组序列化（栈分配，零拷贝无堆开销，对应微软 Garnet 字节数组序列化）
  #[inline]
  pub fn to_byte_array(&self) -> [u8; REPLICATION_HISTORY_BYTES_LEN] {
    let mut buf = [0u8; REPLICATION_HISTORY_BYTES_LEN];
    buf[0] = REPLICATION_HISTORY_VERSION;
    buf[OFFSET_PRIMARY_REPLID..OFFSET_PRIMARY_REPLID2].copy_from_slice(&self.primary_replid.0);
    buf[OFFSET_PRIMARY_REPLID2..OFFSET_REPL_OFFSET].copy_from_slice(&self.primary_replid2.0);
    buf[OFFSET_REPL_OFFSET..OFFSET_REPL_OFFSET2]
      .copy_from_slice(&self.replication_offset.to_le_bytes());
    buf[OFFSET_REPL_OFFSET2..REPLICATION_HISTORY_BYTES_LEN]
      .copy_from_slice(&self.replication_offset2.to_le_bytes());
    buf
  }

  /// 从紧凑二进制反序列化（单次边界检查，零越界开销）
  pub fn from_bytes(data: &[u8]) -> Result<Self> {
    if data.len() < REPLICATION_HISTORY_BYTES_LEN {
      return Err(Error::InvalidDataLength {
        expected: REPLICATION_HISTORY_BYTES_LEN,
        actual: data.len(),
      });
    }

    let version = data[0];
    if version != REPLICATION_HISTORY_VERSION {
      return Err(Error::InvalidVersion {
        expected: REPLICATION_HISTORY_VERSION,
        actual: version,
      });
    }

    // 已校验数据长度至少 97 字节，严格校验复制编号字符合法性以杜绝 UB
    let id1 = ReplId::from_bytes(&data[OFFSET_PRIMARY_REPLID..OFFSET_PRIMARY_REPLID2])?;
    let id2 = ReplId::from_bytes(&data[OFFSET_PRIMARY_REPLID2..OFFSET_REPL_OFFSET])?;
    let offset = u64::from_le_bytes(unsafe {
      data[OFFSET_REPL_OFFSET..OFFSET_REPL_OFFSET2]
        .try_into()
        .unwrap_unchecked()
    });
    let offset2 = u64::from_le_bytes(unsafe {
      data[OFFSET_REPL_OFFSET2..REPLICATION_HISTORY_BYTES_LEN]
        .try_into()
        .unwrap_unchecked()
    });

    Ok(Self {
      primary_replid: id1,
      primary_replid2: id2,
      replication_offset: offset,
      replication_offset2: offset2,
    })
  }

  /// 原子保存至指定配置文件（先写临时文件后原子重命名并同步父目录，栈数组无堆分配）
  pub fn save_to_file(&self, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let mut tmp_name = path.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);
    let bytes = self.to_byte_array();

    {
      let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&tmp_path)?;
      file.write_all(&bytes)?;
      file.sync_all()?;
    }

    rename(&tmp_path, path)?;
    if let Some(parent) = path.parent()
      && let Ok(dir) = File::open(parent)
    {
      let _ = dir.sync_all();
    }
    Ok(())
  }

  /// 从指定配置文件加载复制历史（栈分配读取，零堆分配，支持崩溃自愈）
  pub fn load_from_file(path: impl AsRef<Path>) -> Result<Self> {
    let p = path.as_ref();
    let mut tmp_name = p.as_os_str().to_os_string();
    tmp_name.push(".tmp");
    let tmp_path = PathBuf::from(tmp_name);

    // 首先尝试读取主文件
    let main_res = File::open(p)
      .and_then(|mut f| {
        let mut buffer = [0u8; REPLICATION_HISTORY_BYTES_LEN];
        f.read_exact(&mut buffer)?;
        Ok(buffer)
      })
      .map_err(Error::Io)
      .and_then(|buffer| Self::from_bytes(&buffer));

    match main_res {
      Ok(hist) => Ok(hist),
      // 主文件缺失或损坏（写坏/断电残缺），尝试从 .tmp 自愈后原子归位
      Err(_) if tmp_path.exists() => Self::load_from_valid_file(&tmp_path).inspect(|_| {
        let _ = remove_file(p);
        let _ = rename(&tmp_path, p);
      }),
      err @ Err(_) => err,
    }
  }

  fn load_from_valid_file(path: &Path) -> Result<Self> {
    let mut file = File::open(path)?;
    let mut buffer = [0u8; REPLICATION_HISTORY_BYTES_LEN];
    file.read_exact(&mut buffer)?;
    Self::from_bytes(&buffer)
  }
}
