/// 哈希槽总数量 (0..=16383，共 16384 个)
pub const TOTAL_HASH_SLOTS: usize = 16384;
/// 最小哈希槽编号
pub const MIN_HASH_SLOT_VALUE: usize = 0;
/// 最大哈希槽编号界限
pub const MAX_HASH_SLOT_VALUE: usize = 16384;
/// 槽位编号掩码 (CRC16 取模 16384 的位运算等价形式)
const SLOT_MASK: u16 = (TOTAL_HASH_SLOTS - 1) as u16;
/// 位图机器字数量 (256 个 u64 恰好覆盖 16384 个槽位)
const SLOT_WORDS: usize = TOTAL_HASH_SLOTS / 64;
/// 位图序列化字节数 (16384 位 = 2048 字节，小端序对齐 Redis / Garnet 协议)
const SLOT_BYTES: usize = TOTAL_HASH_SLOTS / 8;

/// 哈希槽状态枚举
/// 与 Garnet 的 SlotState 保持 1:1 二进制兼容与序号一致
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, bitcode::Encode, bitcode::Decode)]
#[repr(u8)]
pub enum SlotState {
  /// 离线 / 未分配
  #[default]
  Offline = 0x0,
  /// 稳定运行状态
  Stable = 0x1,
  /// 迁移中状态 (迁出到目标节点)
  Migrating = 0x2,
  /// 导入中状态 (从源节点迁入)
  Importing = 0x3,
  /// 故障状态
  Fail = 0x4,
  /// 仅用于 SETSLOT 命令状态传递
  Node = 0x5,
  /// 无效状态
  Invalid = 0x6,
}

impl SlotState {
  /// 从原生字节值转换
  #[inline]
  pub const fn from_u8(val: u8) -> Self {
    match val {
      0x0 => Self::Offline,
      0x1 => Self::Stable,
      0x2 => Self::Migrating,
      0x3 => Self::Importing,
      0x4 => Self::Fail,
      0x5 => Self::Node,
      _ => Self::Invalid,
    }
  }

  /// 转换为对应字节值
  #[inline]
  pub const fn to_u8(self) -> u8 {
    self as u8
  }
}

/// 单个哈希槽的归属与状态信息
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, bitcode::Encode, bitcode::Decode)]
pub struct HashSlot {
  /// 槽位所有者的工作节点编号 (0 表示未分配)
  pub worker_id: u16,
  /// 槽位当前状态
  pub state: SlotState,
}

impl HashSlot {
  /// 创建新的哈希槽实例
  #[inline]
  pub const fn new(worker_id: u16, state: SlotState) -> Self {
    Self { worker_id, state }
  }

  /// 获取对外的有效工作节点编号
  /// 当处于 Migrating 状态时，底层仍指向本地所有者 (worker_id 1)，直至迁移最终完成
  #[inline]
  pub fn effective_worker_id(&self) -> u16 {
    if self.state == SlotState::Migrating {
      1
    } else {
      self.worker_id
    }
  }
}

/// 检查哈希槽是否超出合法取值范围
#[inline]
pub const fn out_of_range(slot: usize) -> bool {
  slot >= MAX_HASH_SLOT_VALUE
}

/// CRC-16-CCITT 多项式 (0x1021) 查表法映射表
const CRC16_TABLE: [u16; 256] = [
  0x0000, 0x1021, 0x2042, 0x3063, 0x4084, 0x50A5, 0x60C6, 0x70E7, 0x8108, 0x9129, 0xA14A, 0xB16B,
  0xC18C, 0xD1AD, 0xE1CE, 0xF1EF, 0x1231, 0x0210, 0x3273, 0x2252, 0x52B5, 0x4294, 0x72F7, 0x62D6,
  0x9339, 0x8318, 0xB37B, 0xA35A, 0xD3BD, 0xC39C, 0xF3FF, 0xE3DE, 0x2462, 0x3443, 0x0420, 0x1401,
  0x64E6, 0x74C7, 0x44A4, 0x5485, 0xA56A, 0xB54B, 0x8528, 0x9509, 0xE5EE, 0xF5CF, 0xC5AC, 0xD58D,
  0x3653, 0x2672, 0x1611, 0x0630, 0x76D7, 0x66F6, 0x5695, 0x46B4, 0xB75B, 0xA77A, 0x9719, 0x8738,
  0xF7DF, 0xE7FE, 0xD79D, 0xC7BC, 0x48C4, 0x58E5, 0x6886, 0x78A7, 0x0840, 0x1861, 0x2802, 0x3823,
  0xC9CC, 0xD9ED, 0xE98E, 0xF9AF, 0x8948, 0x9969, 0xA90A, 0xB92B, 0x5AF5, 0x4AD4, 0x7AB7, 0x6A96,
  0x1A71, 0x0A50, 0x3A33, 0x2A12, 0xDBFD, 0xCBDC, 0xFBBF, 0xEB9E, 0x9B79, 0x8B58, 0xBB3B, 0xAB1A,
  0x6CA6, 0x7C87, 0x4CE4, 0x5CC5, 0x2C22, 0x3C03, 0x0C60, 0x1C41, 0xEDAE, 0xFD8F, 0xCDEC, 0xDDCD,
  0xAD2A, 0xBD0B, 0x8D68, 0x9D49, 0x7E97, 0x6EB6, 0x5ED5, 0x4EF4, 0x3E13, 0x2E32, 0x1E51, 0x0E70,
  0xFF9F, 0xEFBE, 0xDFDD, 0xCFFC, 0xBF1B, 0xAF3A, 0x9F59, 0x8F78, 0x9188, 0x81A9, 0xB1CA, 0xA1EB,
  0xD10C, 0xC12D, 0xF14E, 0xE16F, 0x1080, 0x00A1, 0x30C2, 0x20E3, 0x5004, 0x4025, 0x7046, 0x6067,
  0x83B9, 0x9398, 0xA3FB, 0xB3DA, 0xC33D, 0xD31C, 0xE37F, 0xF35E, 0x02B1, 0x1290, 0x22F3, 0x32D2,
  0x4235, 0x5214, 0x6277, 0x7256, 0xB5EA, 0xA5CB, 0x95A8, 0x8589, 0xF56E, 0xE54F, 0xD52C, 0xC50D,
  0x34E2, 0x24C3, 0x14A0, 0x0481, 0x7466, 0x6447, 0x5424, 0x4405, 0xA7DB, 0xB7FA, 0x8799, 0x97B8,
  0xE75F, 0xF77E, 0xC71D, 0xD73C, 0x26D3, 0x36F2, 0x0691, 0x16B0, 0x6657, 0x7676, 0x4615, 0x5634,
  0xD94C, 0xC96D, 0xF90E, 0xE92F, 0x99C8, 0x89E9, 0xB98A, 0xA9AB, 0x5844, 0x4865, 0x7806, 0x6827,
  0x18C0, 0x08E1, 0x3882, 0x28A3, 0xCB7D, 0xDB5C, 0xEB3F, 0xFB1E, 0x8BF9, 0x9BD8, 0xABBB, 0xBB9A,
  0x4A75, 0x5A54, 0x6A37, 0x7A16, 0x0AF1, 0x1AD0, 0x2AB3, 0x3A92, 0xFD2E, 0xED0F, 0xDD6C, 0xCD4D,
  0xBDAA, 0xAD8B, 0x9DE8, 0x8DC9, 0x7C26, 0x6C07, 0x5C64, 0x4C45, 0x3CA2, 0x2C83, 0x1CE0, 0x0CC1,
  0xEF1F, 0xFF3E, 0xCF5D, 0xDF7C, 0xAF9B, 0xBFBA, 0x8FD9, 0x9FF8, 0x6E17, 0x7E36, 0x4E55, 0x5E74,
  0x2E93, 0x3EB2, 0x0ED1, 0x1EF0,
];

/// 使用 CRC-16-CCITT 算法计算字节切片的校验值
#[inline]
pub fn crc16(data: &[u8]) -> u16 {
  let mut result: u16 = 0;
  for &b in data {
    let index = (((result >> 8) as u8) ^ b) as usize;
    result = CRC16_TABLE[index] ^ (result << 8);
  }
  result
}

use std::fmt;

use memchr::memchr;

use crate::error::{Error, Result};

/// 哈希槽位图（16384 位 = 2048 字节），用于高效跟踪节点负责的槽位集合
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct SlotBitmap {
  /// 按机器字存储的槽位占用位
  bits: [u64; SLOT_WORDS],
}

impl Default for SlotBitmap {
  #[inline]
  fn default() -> Self {
    Self::new()
  }
}

impl fmt::Debug for SlotBitmap {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "SlotBitmap(count={})", self.count())
  }
}

impl SlotBitmap {
  /// 创建空的槽位位图
  #[inline]
  pub const fn new() -> Self {
    Self {
      bits: [0u64; SLOT_WORDS],
    }
  }

  /// 创建全满槽位位图 (全设为 1)
  #[inline]
  pub const fn full() -> Self {
    Self {
      bits: [u64::MAX; SLOT_WORDS],
    }
  }

  /// 检查指定槽位是否已设置
  #[inline]
  pub fn is_set(&self, slot: u16) -> bool {
    let s = slot as usize;
    if s >= TOTAL_HASH_SLOTS {
      return false;
    }
    let word = s / 64;
    let bit = s % 64;
    (self.bits[word] & (1u64 << bit)) != 0
  }

  /// 设置指定槽位 (设为 1)
  #[inline]
  pub fn set(&mut self, slot: u16) {
    let s = slot as usize;
    if s < TOTAL_HASH_SLOTS {
      let word = s / 64;
      let bit = s % 64;
      self.bits[word] |= 1u64 << bit;
    }
  }

  /// 清除指定槽位 (设为 0)
  #[inline]
  pub fn clear(&mut self, slot: u16) {
    let s = slot as usize;
    if s < TOTAL_HASH_SLOTS {
      let word = s / 64;
      let bit = s % 64;
      self.bits[word] &= !(1u64 << bit);
    }
  }

  /// 翻转指定槽位状态
  #[inline]
  pub fn toggle(&mut self, slot: u16) {
    let s = slot as usize;
    if s < TOTAL_HASH_SLOTS {
      let word = s / 64;
      let bit = s % 64;
      self.bits[word] ^= 1u64 << bit;
    }
  }

  /// 清空所有槽位
  #[inline]
  pub fn clear_all(&mut self) {
    self.bits = [0u64; SLOT_WORDS];
  }

  /// 统计已设置的槽位总数 (利用 CPU 硬件 popcnt 指令)
  #[inline]
  pub fn count(&self) -> usize {
    self.bits.iter().map(|w| w.count_ones() as usize).sum()
  }

  /// 检查位图是否为空 (无任何槽位设置)
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.bits.iter().all(|&w| w == 0)
  }

  /// 检查是否所有 16384 个槽位均已设置
  #[inline]
  pub fn is_full(&self) -> bool {
    self.bits.iter().all(|&w| w == u64::MAX)
  }

  /// 提取连续槽位区间列表，例如 [(0, 100), (500, 600)]
  /// 利用 trailing_zeros 按连续 1 段跳跃提取，避免逐位扫描
  pub fn ranges(&self) -> Vec<(u16, u16)> {
    let mut ranges = Vec::new();
    let mut start: Option<u16> = None;

    for (word_idx, &word) in self.bits.iter().enumerate() {
      if word == 0 {
        if let Some(s) = start.take() {
          ranges.push((s, (word_idx * 64 - 1) as u16));
        }
        continue;
      }

      let base = word_idx * 64;
      if word == u64::MAX {
        if start.is_none() {
          start = Some(base as u16);
        }
        continue;
      }

      // 若字首为 0 且上一字有未闭合区间，则在上一字末尾闭合
      if (word & 1) == 0
        && let Some(s) = start.take()
      {
        ranges.push((s, (base - 1) as u16));
      }

      // 部分置位字：逐段提取连续 1 区间
      let mut rest = word;
      let mut ofs = 0; // 已消费的低位数量
      while rest != 0 {
        let first = rest.trailing_zeros() as usize;
        let len = (!(rest >> first)).trailing_zeros() as usize;
        let begin = base + ofs + first;
        let end = begin + len - 1;
        // 字首延续上一字的开启区间；否则开启新区间
        if start.is_none() {
          start = Some(begin as u16);
        }
        let consumed = ofs + first + len;
        if consumed < 64 {
          // 段在字内闭合；触及字尾的段留待后续字延续合并
          ranges.push((start.take().unwrap(), end as u16));
        }
        ofs = consumed;
        rest = if consumed == 64 {
          0
        } else {
          rest >> (first + len)
        };
      }
    }

    if let Some(s) = start {
      ranges.push((s, (TOTAL_HASH_SLOTS - 1) as u16));
    }

    ranges
  }

  /// 导出为 2048 字节数组（小端序对齐 Redis / Garnet Cluster 协议）
  pub fn to_bytes(&self) -> [u8; SLOT_BYTES] {
    let mut bytes = [0u8; SLOT_BYTES];
    for (chunk, &word) in bytes.as_chunks_mut::<8>().0.iter_mut().zip(&self.bits) {
      *chunk = word.to_le_bytes();
    }
    bytes
  }

  /// 从 2048 字节切片还原位图
  pub fn from_bytes(slice: &[u8]) -> Result<Self> {
    if slice.len() != SLOT_BYTES {
      return Err(Error::SlotBitmapError(format!(
        "位图字节切片长度必须为 {SLOT_BYTES}，实际为 {}",
        slice.len()
      )));
    }
    let mut bits = [0u64; SLOT_WORDS];
    for (word, chunk) in bits.iter_mut().zip(slice.as_chunks::<8>().0) {
      *word = u64::from_le_bytes(*chunk);
    }
    Ok(Self { bits })
  }
}

/// 计算键所属的哈希槽位 (0..=16383)
/// 遵循 Redis / Garnet 标准 HashTag 解析规则：
/// 1. 查找第一个 '{'
/// 2. 查找其后第一个 '}'
/// 3. 若大括号之间内容非空，则仅对该子切片计算 CRC16
/// 4. 否则对全键计算 CRC16
/// 5. 结果对 16384 取模 (& 16383)
#[inline]
pub fn hash_slot(key: &[u8]) -> u16 {
  if let Some(start) = memchr(b'{', key)
    && let Some(offset) = memchr(b'}', &key[start + 1..])
  {
    let end = start + 1 + offset;
    if end > start + 1 {
      return crc16(&key[start + 1..end]) & SLOT_MASK;
    }
  }
  crc16(key) & SLOT_MASK
}
