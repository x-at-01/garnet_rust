//! RESP 命令硬件 CRC32 哈希快速查找器
//!
//! 1:1 对标 Microsoft Garnet `RespCommandHashLookup.cs` 与 `RespCommandHashLookupData.cs`:
//! - 32 字节条目（恰好半个缓存行，`#[repr(C, align(8))]`），开放寻址与线性探测（linear probing）
//! - 表结构以 64 字节缓存行严格对齐（`#[repr(C, align(64))]`），每对条目 (2k, 2k+1) 位于同一缓存行，线性探测具备极致空间局部性
//! - 单指令硬件 CRC32 加速：
//!   - aarch64：单周期 `crc32cx`（以 wzr 零寄存器为初始累加器，零冗余指令）+ `crc32cw`，双指令完成哈希
//!   - x86_64：`_mm_crc32_u64` + `_mm_crc32_u32` SSE4.2 单周期指令
//!   - 软件回退：Fibonacci multiply-shift (0x9E3779B97F4A7C15)
//! - 主表 512 槽（16KB），完全驻留在 CPU L1 数据缓存中
//! - 13 个二级子命令表全部采用紧凑定长对齐数组存储，零堆内存分配（无 Vec/Box）
//! - 零分支名称匹配：循环外预加载 64 位字并根据长度特化分流，循环内仅需 1~3 次寄存器整数比较，无多余内存读取
//! - 静态初始化全量双向自校验（正向列表全量查回 + 反向各槽位名称重建查回，确保 100% 正确性）

use core::{
  mem::{align_of, size_of},
  str::from_utf8,
};
use std::sync::LazyLock;

use crate::cmd::RespCommand;

/// 表项结构体：大小严格 32 字节（半个 Cache Line），2 个表项恰好对齐 1 个 64 字节 Cache Line
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(C, align(8))]
pub struct CommandEntry {
  /// 命令枚举（2 字节）
  pub command: RespCommand,
  /// 命令名称长度（1 字节）
  pub name_length: u8,
  /// 标志位（1 字节）：FLAG_HAS_SUBCOMMANDS 等
  pub flags: u8,
  /// 保留对齐填充（4 字节）
  pub _pad: u32,
  /// 命令名大写前 8 字节（小端存储，不足 8 字节零填充）
  pub name_word0: u64,
  /// 命令名大写中间/末尾 8 字节（用于 9~16 字节或 17~24 字节名称）
  pub name_word1: u64,
  /// 命令名大写末尾 8 字节（用于 17~24 字节的长名称）
  pub name_word2: u64,
}

// 静态编译期严格断言：表项大小严格 32 字节，对齐严格 8 字节
const _: () = assert!(size_of::<CommandEntry>() == 32);
const _: () = assert!(align_of::<CommandEntry>() == 8);

/// 64 字节缓存行对齐的表包装结构
///
/// 确保表头严格位于 CPU 缓存行边界，使每一对相邻表项 (2k, 2k+1) 完美共存于同一个 64 字节缓存行内，
/// 彻底规避跨缓存行分裂访问（split-line access）与多余缓存未命中。
#[derive(Clone, Copy, Debug)]
#[repr(C, align(64))]
pub struct AlignedTable<const N: usize>(pub [CommandEntry; N]);

/// 标志位：该命令包含子命令，需要执行二级子命令查找
pub const FLAG_HAS_SUBCOMMANDS: u8 = 1;

/// 主命令表槽位数（512 项 = 16KB，完全驻留在 CPU L1 缓存）
pub const PRIMARY_TABLE_BITS: usize = 9;
pub const PRIMARY_TABLE_SIZE: usize = 1 << PRIMARY_TABLE_BITS; // 512
pub const PRIMARY_TABLE_MASK: usize = PRIMARY_TABLE_SIZE - 1; // 511

/// 线性探测最大次数（对标 Garnet MaxProbes = 16）
pub const MAX_PROBES: usize = 16;

// 静态断言：主表 512 项严格对齐 64 字节，总大小精确为 16,384 字节 (16KB)
const _: () = assert!(size_of::<AlignedTable<PRIMARY_TABLE_SIZE>>() == 16384);
const _: () = assert!(align_of::<AlignedTable<PRIMARY_TABLE_SIZE>>() == 64);

/// 从指针安全读取至多 8 字节并小端零扩展为 64 位无符号整数
///
/// # Safety
/// 调用方必须确保 `p` 指向的有效内存区域至少包含 `len` 个可读字节。
#[inline(always)]
unsafe fn read_partial_word(p: *const u8, len: usize) -> u64 {
  // SAFETY: 由调用方保证 p..p+len 在有效内存边界内，此处所有读取均为未对齐读取（read_unaligned），
  // 在 x86_64 与 aarch64 下均为硬件原生支持的快速未对齐加载，并按小端序正确零扩展高位。
  match len {
    1 => unsafe { *p as u64 },
    2 => unsafe { (p as *const u16).read_unaligned() as u64 },
    3 => unsafe { ((p as *const u16).read_unaligned() as u64) | ((*p.add(2) as u64) << 16) },
    4 => unsafe { (p as *const u32).read_unaligned() as u64 },
    5 => unsafe { ((p as *const u32).read_unaligned() as u64) | ((*p.add(4) as u64) << 32) },
    6 => unsafe {
      ((p as *const u32).read_unaligned() as u64)
        | (((p.add(4) as *const u16).read_unaligned() as u64) << 32)
    },
    7 => unsafe {
      ((p as *const u32).read_unaligned() as u64)
        | (((p.add(4) as *const u16).read_unaligned() as u64) << 32)
        | ((*p.add(6) as u64) << 48)
    },
    _ => 0,
  }
}

/// 读取命令前 8 字节字（当长度不足 8 时读取部分并零填充）
///
/// # Safety
/// 调用方保证 `len > 0` 且 `p` 指向至少 `len` 字节的有效内存。
#[inline(always)]
unsafe fn read_word0(p: *const u8, len: usize) -> u64 {
  if len >= 8 {
    // SAFETY: len >= 8 且 p 至少有 len 字节可读，安全执行 8 字节未对齐读取
    unsafe { (p as *const u64).read_unaligned() }
  } else {
    // SAFETY: len < 8，由 read_partial_word 精确安全读取 len 字节
    unsafe { read_partial_word(p, len) }
  }
}

/// Fibonacci 散列 64 位黄金分割乘法常数
const FIB_HASH_MUL: u64 = 0x9E37_79B9_7F4A_7C15;
/// Fibonacci 散列 32 位黄金分割乘法常数
const FIB_HASH_MUL_32: u32 = 2654435761;

/// 硬件 CRC32 哈希；当前平台无可用硬件路径时返回 None
///
/// 1:1 对标 Garnet ComputeHash:
/// - aarch64 平台：单条 `crc32cx` 指令（以 wzr 零寄存器为初始累加器，零多余指令）+ 单条 `crc32cw` 指令，双指令完成哈希
/// - x86_64 平台：`_mm_crc32_u64` + `_mm_crc32_u32` SSE4.2 单周期指令
#[inline(always)]
fn hw_crc32(word0: u64, len: u32) -> Option<u32> {
  #[cfg(target_arch = "aarch64")]
  {
    #[cfg(target_feature = "crc")]
    {
      let crc: u32;
      // SAFETY: 编译期开启 aarch64 CRC 扩展（如 Apple Silicon 默认支持）。
      // 采用硬件单周期指令 crc32cx 与 crc32cw 计算 CRC32C，wzr 为零寄存器。
      unsafe {
        core::arch::asm!(
          "crc32cx {crc:w}, wzr, {word:x}",
          "crc32cw {crc:w}, {crc:w}, {len:w}",
          crc = out(reg) crc,
          word = in(reg) word0,
          len = in(reg) len,
          options(pure, nomem, nostack, preserves_flags)
        );
      }
      Some(crc)
    }
    #[cfg(not(target_feature = "crc"))]
    {
      if std::arch::is_aarch64_feature_detected!("crc") {
        let crc: u32;
        // SAFETY: 运行时 CPU 探测确认支持 ARMv8 CRC 扩展后调用硬件指令。
        unsafe {
          core::arch::asm!(
            ".arch_extension crc",
            "crc32cx {crc:w}, wzr, {word:x}",
            "crc32cw {crc:w}, {crc:w}, {len:w}",
            crc = out(reg) crc,
            word = in(reg) word0,
            len = in(reg) len,
            options(pure, nomem, nostack, preserves_flags)
          );
        }
        Some(crc)
      } else {
        None
      }
    }
  }

  #[cfg(target_arch = "x86_64")]
  {
    #[cfg(target_feature = "sse4.2")]
    {
      // SAFETY: 编译期开启 SSE4.2 扩展，直接调用硬件指令内建函数。
      Some(unsafe {
        let crc = core::arch::x86_64::_mm_crc32_u64(0, word0);
        core::arch::x86_64::_mm_crc32_u32(crc as u32, len)
      })
    }
    #[cfg(not(target_feature = "sse4.2"))]
    {
      if is_x86_feature_detected!("sse4.2") {
        // SAFETY: 运行时探测支持 SSE4.2，调用硬件指令。
        Some(unsafe {
          let crc = core::arch::x86_64::_mm_crc32_u64(0, word0);
          core::arch::x86_64::_mm_crc32_u32(crc as u32, len)
        })
      } else {
        None
      }
    }
  }

  #[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
  {
    None
  }
}

/// 计算命令哈希（基于预提取的 word0 与 length）
///
/// 硬件 CRC32 不可用时回退 Fibonacci multiply-shift 软件哈希（高散列分布，单周期整数乘法与异或）
#[inline(always)]
fn compute_hash_word(word0: u64, len: u32) -> u32 {
  hw_crc32(word0, len).unwrap_or_else(|| {
    ((word0.wrapping_mul(FIB_HASH_MUL) >> 32) as u32) ^ len.wrapping_mul(FIB_HASH_MUL_32)
  })
}

/// 计算字节切片的命令哈希值（公开辅助函数）
#[inline(always)]
pub fn compute_cmd_hash(name: &[u8]) -> u32 {
  let len = name.len();
  if len == 0 {
    return 0;
  }
  // SAFETY: len > 0 且切片保证 name.as_ptr() 指向至少 len 字节的有效内存
  let word0 = unsafe { read_word0(name.as_ptr(), len) };
  compute_hash_word(word0, len as u32)
}

/// 核心通用哈希表探测查找函数
///
/// 极致优化点：
/// 1. 编译期长度特化分流（<=8 字节、<=16 字节、<=24 字节）：
///    - <=8 字节（绝大多数 Redis 命令）：单次 word0 预读取，探测循环内部仅 1 次 u64 比较，彻底规避多次解引用；
///    - <=16 字节：2 次 u64 比较（位与无分支）；
///    - <=24 字节：3 次 u64 比较（位与无分支）。
/// 2. 探测循环内部消除切片越界检查：通过 `get_unchecked` 消除边界检查，掩码严格约束 `idx < table.len()`。
/// 3. 空槽提前退出：若探测到 `entry.name_length == 0`，证明该桶后无冲突聚集，立即返回未找到。
#[inline(always)]
fn lookup_core(table: &[CommandEntry], mask: usize, name: &[u8]) -> (RespCommand, bool) {
  let len = name.len();
  if len == 0 || len > 24 {
    return (RespCommand::None, false);
  }
  let p = name.as_ptr();
  let target_len = len as u8;

  if len <= 8 {
    // SAFETY: len 在 1..=8 之间，p 指向切片起始地址，至少有 len 字节可读
    let w0 = unsafe { read_word0(p, len) };
    let hash = compute_hash_word(w0, len as u32);
    let mut idx = (hash as usize) & mask;

    for _ in 0..MAX_PROBES {
      // SAFETY: idx < table.len() 由 mask = table.len() - 1 位与保证（表大小恒为 2 的幂）
      let entry = unsafe { table.get_unchecked(idx) };
      if entry.name_length == 0 {
        return (RespCommand::None, false);
      }
      if entry.name_length == target_len && entry.name_word0 == w0 {
        let has_sub = (entry.flags & FLAG_HAS_SUBCOMMANDS) != 0;
        return (entry.command, has_sub);
      }
      idx = (idx + 1) & mask;
    }
  } else if len <= 16 {
    // SAFETY: len 在 9..=16 之间，读取前 8 字节与末尾 8 字节（两段区间覆盖全部 9..16 字节）
    let (w0, w1) = unsafe {
      (
        (p as *const u64).read_unaligned(),
        (p.add(len - 8) as *const u64).read_unaligned(),
      )
    };
    let hash = compute_hash_word(w0, len as u32);
    let mut idx = (hash as usize) & mask;

    for _ in 0..MAX_PROBES {
      // SAFETY: idx < table.len() 由 mask 严格保证
      let entry = unsafe { table.get_unchecked(idx) };
      if entry.name_length == 0 {
        return (RespCommand::None, false);
      }
      if entry.name_length == target_len && (entry.name_word0 == w0) & (entry.name_word1 == w1) {
        let has_sub = (entry.flags & FLAG_HAS_SUBCOMMANDS) != 0;
        return (entry.command, has_sub);
      }
      idx = (idx + 1) & mask;
    }
  } else {
    // SAFETY: len 在 17..=24 之间，读取第 0..8, 8..16, len-8..len 字节（覆盖全部 17..24 字节）
    let (w0, w1, w2) = unsafe {
      (
        (p as *const u64).read_unaligned(),
        (p.add(8) as *const u64).read_unaligned(),
        (p.add(len - 8) as *const u64).read_unaligned(),
      )
    };
    let hash = compute_hash_word(w0, len as u32);
    let mut idx = (hash as usize) & mask;

    for _ in 0..MAX_PROBES {
      // SAFETY: idx < table.len() 由 mask 严格保证
      let entry = unsafe { table.get_unchecked(idx) };
      if entry.name_length == 0 {
        return (RespCommand::None, false);
      }
      if entry.name_length == target_len
        && (entry.name_word0 == w0) & (entry.name_word1 == w1) & (entry.name_word2 == w2)
      {
        let has_sub = (entry.flags & FLAG_HAS_SUBCOMMANDS) != 0;
        return (entry.command, has_sub);
      }
      idx = (idx + 1) & mask;
    }
  }

  (RespCommand::None, false)
}

/// 在主命令表中查找命令，并返回匹配的命令及是否包含子命令
#[inline(always)]
pub fn lookup_primary(name: &[u8]) -> (RespCommand, bool) {
  lookup_core(&PRIMARY_TABLE.0, PRIMARY_TABLE_MASK, name)
}

/// 查找指定父命令的子命令
#[inline(always)]
pub fn lookup_subcommand(parent: RespCommand, sub_name: &[u8]) -> RespCommand {
  let tables = &*SUB_TABLES;
  let (table, mask): (&[CommandEntry], usize) = match parent {
    RespCommand::Cluster => (&tables.cluster.0, 127),
    RespCommand::Client => (&tables.client.0, 15),
    RespCommand::Acl => (&tables.acl.0, 15),
    RespCommand::Command => (&tables.command.0, 15),
    RespCommand::Config => (&tables.config.0, 15),
    RespCommand::Script => (&tables.script.0, 15),
    RespCommand::Latency => (&tables.latency.0, 15),
    RespCommand::Slowlog => (&tables.slowlog.0, 15),
    RespCommand::Module => (&tables.module.0, 15),
    RespCommand::Pubsub => (&tables.pubsub.0, 15),
    RespCommand::Memory => (&tables.memory.0, 15),
    RespCommand::Object => (&tables.object.0, 15),
    RespCommand::Bitop => (&tables.bitop.0, 15),
    _ => return RespCommand::None,
  };

  lookup_core(table, mask, sub_name).0
}

// ====== 表构建与自校验内部实现 ======

fn get_word_from_slice(slice: &[u8], offset: usize) -> u64 {
  if offset >= slice.len() {
    return 0;
  }
  let remaining = slice.len() - offset;
  // SAFETY: offset < slice.len()，p 指向切片内部有效地址
  let p = unsafe { slice.as_ptr().add(offset) };
  if remaining >= 8 {
    // SAFETY: remaining >= 8，安全执行 8 字节未对齐读取
    unsafe { (p as *const u64).read_unaligned() }
  } else {
    // SAFETY: 0 < remaining < 8，安全读取剩余字节
    unsafe { read_partial_word(p, remaining) }
  }
}

fn insert_into_table(
  table: &mut [CommandEntry],
  mask: usize,
  name: &[u8],
  command: RespCommand,
  flags: u8,
) {
  assert!(
    !name.is_empty() && name.len() <= 24,
    "命令名长度必须在 1~24 字节之间"
  );
  let hash = compute_cmd_hash(name);
  let mut idx = (hash as usize) & mask;

  for _ in 0..MAX_PROBES {
    let entry = &mut table[idx];
    if entry.name_length == 0 {
      entry.command = command;
      entry.name_length = name.len() as u8;
      entry.flags = flags;
      entry.name_word0 = get_word_from_slice(name, 0);
      entry.name_word1 = 0;
      entry.name_word2 = 0;

      if name.len() > 16 {
        entry.name_word1 = get_word_from_slice(name, 8);
        entry.name_word2 = get_word_from_slice(name, name.len() - 8);
      } else if name.len() > 8 {
        entry.name_word1 = get_word_from_slice(name, name.len() - 8);
      }
      return;
    }
    idx = (idx + 1) & mask;
  }

  panic!(
    "命令哈希表溢出：未能将 {:?} 插入表中",
    from_utf8(name).unwrap_or("")
  );
}

fn build_sub_table<const N: usize>(subcommands: &[(&[u8], RespCommand)]) -> AlignedTable<N> {
  let mask = N - 1;
  let mut table = [CommandEntry::default(); N];
  for &(name, cmd) in subcommands {
    insert_into_table(&mut table, mask, name, cmd, 0);
  }
  AlignedTable(table)
}

fn validate_sub_table(
  parent: RespCommand,
  subcommands: &[(&[u8], RespCommand)],
  table: &[CommandEntry],
  mask: usize,
) {
  for &(name, expected_cmd) in subcommands {
    let (found, _) = lookup_core(table, mask, name);
    assert_eq!(
      found,
      expected_cmd,
      "子命令表自校验失败：父命令 {:?} 的子命令 {:?} 期望 {:?}，但查得 {:?}",
      parent,
      from_utf8(name).unwrap_or(""),
      expected_cmd,
      found
    );
  }
}

fn validate_primary_table(definitions: &[(&[u8], RespCommand, bool)], table: &[CommandEntry]) {
  // 1. 正向校验：所有注册的命令定义必须能 100% 查回且标志位匹配
  for &(name, expected_cmd, expected_has_sub) in definitions {
    let (found, has_sub) = lookup_core(table, PRIMARY_TABLE_MASK, name);
    assert_eq!(
      found,
      expected_cmd,
      "主表正向自校验失败：命令 {:?} 期望 {:?}，但查得 {:?}",
      from_utf8(name).unwrap_or(""),
      expected_cmd,
      found
    );
    assert_eq!(
      has_sub,
      expected_has_sub,
      "主表 has_sub 标志自校验失败：命令 {:?} 期望 has_sub={}",
      from_utf8(name).unwrap_or(""),
      expected_has_sub
    );
  }

  // 2. 反向校验：对标 Garnet ValidatePrimaryTable，扫描表中所有占用的槽位，还原命令名并反查
  for entry in table {
    if entry.name_length == 0 {
      continue;
    }
    let len = entry.name_length as usize;
    let mut name_buf = [0u8; 24];
    let w0 = entry.name_word0.to_le_bytes();
    let w1 = entry.name_word1.to_le_bytes();
    let w2 = entry.name_word2.to_le_bytes();

    if len <= 8 {
      name_buf[..len].copy_from_slice(&w0[..len]);
    } else if len <= 16 {
      name_buf[..8].copy_from_slice(&w0);
      name_buf[len - 8..len].copy_from_slice(&w1[..8]);
    } else {
      name_buf[..8].copy_from_slice(&w0);
      name_buf[8..16].copy_from_slice(&w1);
      name_buf[len - 8..len].copy_from_slice(&w2[..8]);
    }

    let reconstructed = &name_buf[..len];
    let (found, _) = lookup_core(table, PRIMARY_TABLE_MASK, reconstructed);
    assert_eq!(
      found,
      entry.command,
      "主表槽位反向自校验失败：槽位命令 {:?} 还原名 {:?} 查得 {:?}",
      entry.command,
      from_utf8(reconstructed).unwrap_or(""),
      found
    );
  }
}

// ====== 命令全量静态定义表 ======

const PRIMARY_DEFINITIONS: &[(&[u8], RespCommand, bool)] = &[
  // 字符串命令
  (b"GET", RespCommand::Get, false),
  (b"SET", RespCommand::Set, false),
  (b"DEL", RespCommand::Del, false),
  (b"INCR", RespCommand::Incr, false),
  (b"DECR", RespCommand::Decr, false),
  (b"INCRBY", RespCommand::Incrby, false),
  (b"DECRBY", RespCommand::Decrby, false),
  (b"INCRBYFLOAT", RespCommand::Incrbyfloat, false),
  (b"APPEND", RespCommand::Append, false),
  (b"GETSET", RespCommand::Getset, false),
  (b"GETDEL", RespCommand::Getdel, false),
  (b"GETEX", RespCommand::Getex, false),
  (b"GETRANGE", RespCommand::Getrange, false),
  (b"SETRANGE", RespCommand::Setrange, false),
  (b"STRLEN", RespCommand::Strlen, false),
  (b"SUBSTR", RespCommand::Substr, false),
  (b"SETNX", RespCommand::Setnx, false),
  (b"SETEX", RespCommand::Setex, false),
  (b"PSETEX", RespCommand::Psetex, false),
  (b"MGET", RespCommand::Mget, false),
  (b"MSET", RespCommand::Mset, false),
  (b"MSETNX", RespCommand::Msetnx, false),
  (b"DUMP", RespCommand::Dump, false),
  (b"RESTORE", RespCommand::Restore, false),
  (b"GETBIT", RespCommand::Getbit, false),
  (b"SETBIT", RespCommand::Setbit, false),
  (b"GETWITHETAG", RespCommand::Getwithetag, false),
  (b"GETIFNOTMATCH", RespCommand::Getifnotmatch, false),
  (b"SETIFMATCH", RespCommand::Setifmatch, false),
  (b"SETIFGREATER", RespCommand::Setifgreater, false),
  (b"SETWITHETAG", RespCommand::Setwithetag, false),
  (b"DELIFGREATER", RespCommand::Delifgreater, false),
  (b"LCS", RespCommand::Lcs, false),
  // 键命令
  (b"EXISTS", RespCommand::Exists, false),
  (b"TTL", RespCommand::Ttl, false),
  (b"PTTL", RespCommand::Pttl, false),
  (b"EXPIRE", RespCommand::Expire, false),
  (b"PEXPIRE", RespCommand::Pexpire, false),
  (b"EXPIREAT", RespCommand::Expireat, false),
  (b"PEXPIREAT", RespCommand::Pexpireat, false),
  (b"EXPIRETIME", RespCommand::Expiretime, false),
  (b"PEXPIRETIME", RespCommand::Pexpiretime, false),
  (b"PERSIST", RespCommand::Persist, false),
  (b"TYPE", RespCommand::Type, false),
  (b"RENAME", RespCommand::Rename, false),
  (b"RENAMENX", RespCommand::Renamenx, false),
  (b"UNLINK", RespCommand::Unlink, false),
  (b"KEYS", RespCommand::Keys, false),
  (b"SCAN", RespCommand::Scan, false),
  (b"DBSIZE", RespCommand::Dbsize, false),
  (b"SELECT", RespCommand::Select, false),
  (b"SWAPDB", RespCommand::Swapdb, false),
  (b"MIGRATE", RespCommand::Migrate, false),
  // 位图与 HyperLogLog
  (b"BITCOUNT", RespCommand::Bitcount, false),
  (b"BITPOS", RespCommand::Bitpos, false),
  (b"BITFIELD", RespCommand::Bitfield, false),
  (b"BITFIELD_RO", RespCommand::BitfieldRo, false),
  (b"BITOP", RespCommand::Bitop, true),
  (b"PFADD", RespCommand::Pfadd, false),
  (b"PFCOUNT", RespCommand::Pfcount, false),
  (b"PFMERGE", RespCommand::Pfmerge, false),
  // 哈希命令
  (b"HSET", RespCommand::Hset, false),
  (b"HGET", RespCommand::Hget, false),
  (b"HDEL", RespCommand::Hdel, false),
  (b"HLEN", RespCommand::Hlen, false),
  (b"HEXISTS", RespCommand::Hexists, false),
  (b"HGETALL", RespCommand::Hgetall, false),
  (b"HKEYS", RespCommand::Hkeys, false),
  (b"HVALS", RespCommand::Hvals, false),
  (b"HMSET", RespCommand::Hmset, false),
  (b"HMGET", RespCommand::Hmget, false),
  (b"HSETNX", RespCommand::Hsetnx, false),
  (b"HINCRBY", RespCommand::Hincrby, false),
  (b"HINCRBYFLOAT", RespCommand::Hincrbyfloat, false),
  (b"HRANDFIELD", RespCommand::Hrandfield, false),
  (b"HSCAN", RespCommand::Hscan, false),
  (b"HSTRLEN", RespCommand::Hstrlen, false),
  (b"HTTL", RespCommand::Httl, false),
  (b"HPTTL", RespCommand::Hpttl, false),
  (b"HEXPIRE", RespCommand::Hexpire, false),
  (b"HPEXPIRE", RespCommand::Hpexpire, false),
  (b"HEXPIREAT", RespCommand::Hexpireat, false),
  (b"HPEXPIREAT", RespCommand::Hpexpireat, false),
  (b"HEXPIRETIME", RespCommand::Hexpiretime, false),
  (b"HPEXPIRETIME", RespCommand::Hpexpiretime, false),
  (b"HPERSIST", RespCommand::Hpersist, false),
  (b"HCOLLECT", RespCommand::Hcollect, false),
  // 列表命令
  (b"LPUSH", RespCommand::Lpush, false),
  (b"RPUSH", RespCommand::Rpush, false),
  (b"LPUSHX", RespCommand::Lpushx, false),
  (b"RPUSHX", RespCommand::Rpushx, false),
  (b"LPOP", RespCommand::Lpop, false),
  (b"RPOP", RespCommand::Rpop, false),
  (b"LLEN", RespCommand::Llen, false),
  (b"LINDEX", RespCommand::Lindex, false),
  (b"LINSERT", RespCommand::Linsert, false),
  (b"LRANGE", RespCommand::Lrange, false),
  (b"LREM", RespCommand::Lrem, false),
  (b"LSET", RespCommand::Lset, false),
  (b"LTRIM", RespCommand::Ltrim, false),
  (b"LPOS", RespCommand::Lpos, false),
  (b"LMOVE", RespCommand::Lmove, false),
  (b"LMPOP", RespCommand::Lmpop, false),
  (b"RPOPLPUSH", RespCommand::Rpoplpush, false),
  (b"BLPOP", RespCommand::Blpop, false),
  (b"BRPOP", RespCommand::Brpop, false),
  (b"BLMOVE", RespCommand::Blmove, false),
  (b"BRPOPLPUSH", RespCommand::Brpoplpush, false),
  (b"BLMPOP", RespCommand::Blmpop, false),
  // 集合命令
  (b"SADD", RespCommand::Sadd, false),
  (b"SREM", RespCommand::Srem, false),
  (b"SPOP", RespCommand::Spop, false),
  (b"SCARD", RespCommand::Scard, false),
  (b"SMEMBERS", RespCommand::Smembers, false),
  (b"SISMEMBER", RespCommand::Sismember, false),
  (b"SMISMEMBER", RespCommand::Smismember, false),
  (b"SRANDMEMBER", RespCommand::Srandmember, false),
  (b"SMOVE", RespCommand::Smove, false),
  (b"SSCAN", RespCommand::Sscan, false),
  (b"SDIFF", RespCommand::Sdiff, false),
  (b"SDIFFSTORE", RespCommand::Sdiffstore, false),
  (b"SINTER", RespCommand::Sinter, false),
  (b"SINTERCARD", RespCommand::Sintercard, false),
  (b"SINTERSTORE", RespCommand::Sinterstore, false),
  (b"SUNION", RespCommand::Sunion, false),
  (b"SUNIONSTORE", RespCommand::Sunionstore, false),
  // 有序集合命令
  (b"ZADD", RespCommand::Zadd, false),
  (b"ZREM", RespCommand::Zrem, false),
  (b"ZCARD", RespCommand::Zcard, false),
  (b"ZSCORE", RespCommand::Zscore, false),
  (b"ZMSCORE", RespCommand::Zmscore, false),
  (b"ZRANK", RespCommand::Zrank, false),
  (b"ZREVRANK", RespCommand::Zrevrank, false),
  (b"ZCOUNT", RespCommand::Zcount, false),
  (b"ZLEXCOUNT", RespCommand::Zlexcount, false),
  (b"ZRANGE", RespCommand::Zrange, false),
  (b"ZRANGEBYLEX", RespCommand::Zrangebylex, false),
  (b"ZRANGEBYSCORE", RespCommand::Zrangebyscore, false),
  (b"ZRANGESTORE", RespCommand::Zrangestore, false),
  (b"ZREVRANGE", RespCommand::Zrevrange, false),
  (b"ZREVRANGEBYLEX", RespCommand::Zrevrangebylex, false),
  (b"ZREVRANGEBYSCORE", RespCommand::Zrevrangebyscore, false),
  (b"ZPOPMIN", RespCommand::Zpopmin, false),
  (b"ZPOPMAX", RespCommand::Zpopmax, false),
  (b"ZRANDMEMBER", RespCommand::Zrandmember, false),
  (b"ZSCAN", RespCommand::Zscan, false),
  (b"ZINCRBY", RespCommand::Zincrby, false),
  (b"ZDIFF", RespCommand::Zdiff, false),
  (b"ZDIFFSTORE", RespCommand::Zdiffstore, false),
  (b"ZINTER", RespCommand::Zinter, false),
  (b"ZINTERCARD", RespCommand::Zintercard, false),
  (b"ZINTERSTORE", RespCommand::Zinterstore, false),
  (b"ZUNION", RespCommand::Zunion, false),
  (b"ZUNIONSTORE", RespCommand::Zunionstore, false),
  (b"ZMPOP", RespCommand::Zmpop, false),
  (b"BZMPOP", RespCommand::Bzmpop, false),
  (b"BZPOPMAX", RespCommand::Bzpopmax, false),
  (b"BZPOPMIN", RespCommand::Bzpopmin, false),
  (b"ZREMRANGEBYLEX", RespCommand::Zremrangebylex, false),
  (b"ZREMRANGEBYRANK", RespCommand::Zremrangebyrank, false),
  (b"ZREMRANGEBYSCORE", RespCommand::Zremrangebyscore, false),
  (b"ZTTL", RespCommand::Zttl, false),
  (b"ZPTTL", RespCommand::Zpttl, false),
  (b"ZEXPIRE", RespCommand::Zexpire, false),
  (b"ZPEXPIRE", RespCommand::Zpexpire, false),
  (b"ZEXPIREAT", RespCommand::Zexpireat, false),
  (b"ZPEXPIREAT", RespCommand::Zpexpireat, false),
  (b"ZEXPIRETIME", RespCommand::Zexpiretime, false),
  (b"ZPEXPIRETIME", RespCommand::Zpexpiretime, false),
  (b"ZPERSIST", RespCommand::Zpersist, false),
  (b"ZCOLLECT", RespCommand::Zcollect, false),
  // 向量集合与 Range Index
  (b"VADD", RespCommand::Vadd, false),
  (b"VCARD", RespCommand::Vcard, false),
  (b"VDIM", RespCommand::Vdim, false),
  (b"VEMB", RespCommand::Vemb, false),
  (b"VGETATTR", RespCommand::Vgetattr, false),
  (b"VINFO", RespCommand::Vinfo, false),
  (b"VISMEMBER", RespCommand::Vismember, false),
  (b"VLINKS", RespCommand::Vlinks, false),
  (b"VRANDMEMBER", RespCommand::Vrandmember, false),
  (b"VREM", RespCommand::Vrem, false),
  (b"VSETATTR", RespCommand::Vsetattr, false),
  (b"VSIM", RespCommand::Vsim, false),
  (b"RI.CREATE", RespCommand::Ricreate, false),
  (b"RI.SET", RespCommand::Riset, false),
  (b"RI.GET", RespCommand::Riget, false),
  (b"RI.DEL", RespCommand::Ridel, false),
  (b"RI.RANGE", RespCommand::Rirange, false),
  (b"RI.SCAN", RespCommand::Riscan, false),
  (b"RI.EXISTS", RespCommand::Riexists, false),
  (b"RI.CONFIG", RespCommand::Riconfig, false),
  (b"RI.METRICS", RespCommand::Rimetrics, false),
  // 地理空间
  (b"GEOADD", RespCommand::Geoadd, false),
  (b"GEOPOS", RespCommand::Geopos, false),
  (b"GEOHASH", RespCommand::Geohash, false),
  (b"GEODIST", RespCommand::Geodist, false),
  (b"GEOSEARCH", RespCommand::Geosearch, false),
  (b"GEOSEARCHSTORE", RespCommand::Geosearchstore, false),
  (b"GEORADIUS", RespCommand::Georadius, false),
  (b"GEORADIUS_RO", RespCommand::GeoradiusRo, false),
  (b"GEORADIUSBYMEMBER", RespCommand::Georadiusbymember, false),
  (
    b"GEORADIUSBYMEMBER_RO",
    RespCommand::GeoradiusbymemberRo,
    false,
  ),
  // 脚本与管理
  (b"EVAL", RespCommand::Eval, false),
  (b"EVALSHA", RespCommand::Evalsha, false),
  (b"PUBLISH", RespCommand::Publish, false),
  (b"SUBSCRIBE", RespCommand::Subscribe, false),
  (b"PSUBSCRIBE", RespCommand::Psubscribe, false),
  (b"UNSUBSCRIBE", RespCommand::Unsubscribe, false),
  (b"PUNSUBSCRIBE", RespCommand::Punsubscribe, false),
  (b"SPUBLISH", RespCommand::Spublish, false),
  (b"SSUBSCRIBE", RespCommand::Ssubscribe, false),
  (b"CUSTOMOBJECTSCAN", RespCommand::Coscan, false),
  (b"PING", RespCommand::Ping, false),
  (b"ECHO", RespCommand::Echo, false),
  (b"QUIT", RespCommand::Quit, false),
  (b"AUTH", RespCommand::Auth, false),
  (b"HELLO", RespCommand::Hello, false),
  (b"INFO", RespCommand::Info, false),
  (b"TIME", RespCommand::Time, false),
  (b"ROLE", RespCommand::Role, false),
  (b"SAVE", RespCommand::Save, false),
  (b"LASTSAVE", RespCommand::Lastsave, false),
  (b"BGSAVE", RespCommand::Bgsave, false),
  (b"COMMITAOF", RespCommand::Commitaof, false),
  (b"FLUSHALL", RespCommand::Flushall, false),
  (b"FLUSHDB", RespCommand::Flushdb, false),
  (b"FAILOVER", RespCommand::Failover, false),
  (b"MONITOR", RespCommand::Monitor, false),
  (b"REGISTERCS", RespCommand::Registercs, false),
  (b"ASYNC", RespCommand::Async, false),
  (b"DEBUG", RespCommand::Debug, false),
  (b"EXPDELSCAN", RespCommand::Expdelscan, false),
  (b"WATCH", RespCommand::Watch, false),
  (b"WATCHMS", RespCommand::Watchms, false),
  (b"WATCHOS", RespCommand::Watchos, false),
  (b"MULTI", RespCommand::Multi, false),
  (b"EXEC", RespCommand::Exec, false),
  (b"DISCARD", RespCommand::Discard, false),
  (b"UNWATCH", RespCommand::Unwatch, false),
  (b"RUNTXP", RespCommand::Runtxp, false),
  (b"ASKING", RespCommand::Asking, false),
  (b"READONLY", RespCommand::Readonly, false),
  (b"READWRITE", RespCommand::Readwrite, false),
  (b"REPLICAOF", RespCommand::Replicaof, false),
  (b"SECONDARYOF", RespCommand::Secondaryof, false),
  (b"SLAVEOF", RespCommand::Secondaryof, false),
  // 带子命令的父命令
  (b"SCRIPT", RespCommand::Script, true),
  (b"CONFIG", RespCommand::Config, true),
  (b"CLIENT", RespCommand::Client, true),
  (b"CLUSTER", RespCommand::Cluster, true),
  (b"ACL", RespCommand::Acl, true),
  (b"COMMAND", RespCommand::Command, true),
  (b"LATENCY", RespCommand::Latency, true),
  (b"SLOWLOG", RespCommand::Slowlog, true),
  (b"MODULE", RespCommand::Module, true),
  (b"PUBSUB", RespCommand::Pubsub, true),
  (b"MEMORY", RespCommand::Memory, true),
  (b"OBJECT", RespCommand::Object, true),
];

// ====== 13 个子命令定义数组 ======

const CLUSTER_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"ADDSLOTS", RespCommand::ClusterAddslots),
  (b"ADDSLOTSRANGE", RespCommand::ClusterAddslotsrange),
  (b"ADVANCE_TIME", RespCommand::ClusterAdvanceTime),
  (b"APPENDLOG", RespCommand::ClusterAppendlog),
  (b"ATTACH_SYNC", RespCommand::ClusterAttachSync),
  (b"BANLIST", RespCommand::ClusterBanlist),
  (
    b"BEGIN_REPLICA_RECOVER",
    RespCommand::ClusterBeginReplicaRecover,
  ),
  (b"BUMPEPOCH", RespCommand::ClusterBumpepoch),
  (b"COUNTKEYSINSLOT", RespCommand::ClusterCountkeysinslot),
  (b"DELKEYSINSLOT", RespCommand::ClusterDelkeysinslot),
  (
    b"DELKEYSINSLOTRANGE",
    RespCommand::ClusterDelkeysinslotrange,
  ),
  (b"DELSLOTS", RespCommand::ClusterDelslots),
  (b"DELSLOTSRANGE", RespCommand::ClusterDelslotsrange),
  (b"ENDPOINT", RespCommand::ClusterEndpoint),
  (b"FAILOVER", RespCommand::ClusterFailover),
  (
    b"FAILREPLICATIONOFFSET",
    RespCommand::ClusterFailreplicationoffset,
  ),
  (b"FAILSTOPWRITES", RespCommand::ClusterFailstopwrites),
  (b"FLUSHALL", RespCommand::ClusterFlushall),
  (b"FORGET", RespCommand::ClusterForget),
  (b"GETKEYSINSLOT", RespCommand::ClusterGetkeysinslot),
  (b"GOSSIP", RespCommand::ClusterGossip),
  (b"HELP", RespCommand::ClusterHelp),
  (b"INFO", RespCommand::ClusterInfo),
  (
    b"INITIATE_REPLICA_SYNC",
    RespCommand::ClusterInitiateReplicaSync,
  ),
  (b"KEYSLOT", RespCommand::ClusterKeyslot),
  (b"MEET", RespCommand::ClusterMeet),
  (b"MIGRATE", RespCommand::ClusterMigrate),
  (b"MLOG_KEY_TIME", RespCommand::ClusterMlogKeyTime),
  (b"MTASKS", RespCommand::ClusterMtasks),
  (b"MYID", RespCommand::ClusterMyid),
  (b"MYPARENTID", RespCommand::ClusterMyparentid),
  (b"NODES", RespCommand::ClusterNodes),
  (b"PUBLISH", RespCommand::ClusterPublish),
  (b"SPUBLISH", RespCommand::ClusterSpublish),
  (b"REPLICAS", RespCommand::ClusterReplicas),
  (b"REPLICATE", RespCommand::ClusterReplicate),
  (b"RESERVE", RespCommand::ClusterReserve),
  (b"RESET", RespCommand::ClusterReset),
  (
    b"SEND_CKPT_FILE_SEGMENT",
    RespCommand::ClusterSendCkptFileSegment,
  ),
  (b"SEND_CKPT_METADATA", RespCommand::ClusterSendCkptMetadata),
  (b"SET-CONFIG-EPOCH", RespCommand::ClusterSetconfigepoch),
  (b"SETCONFIGEPOCH", RespCommand::ClusterSetconfigepoch),
  (b"SETSLOT", RespCommand::ClusterSetslot),
  (b"SETSLOTSRANGE", RespCommand::ClusterSetslotsrange),
  (b"SHARDS", RespCommand::ClusterShards),
  (b"SLOTS", RespCommand::ClusterSlots),
  (b"SLOTSTATE", RespCommand::ClusterSlotstate),
  (b"SNAPSHOT_DATA", RespCommand::ClusterSnapshotData),
  (b"SYNC", RespCommand::ClusterSync),
];

const CLIENT_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"ID", RespCommand::ClientId),
  (b"INFO", RespCommand::ClientInfo),
  (b"LIST", RespCommand::ClientList),
  (b"KILL", RespCommand::ClientKill),
  (b"GETNAME", RespCommand::ClientGetname),
  (b"SETNAME", RespCommand::ClientSetname),
  (b"SETINFO", RespCommand::ClientSetinfo),
  (b"UNBLOCK", RespCommand::ClientUnblock),
];

const ACL_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"CAT", RespCommand::AclCat),
  (b"DELUSER", RespCommand::AclDeluser),
  (b"GENPASS", RespCommand::AclGenpass),
  (b"GETUSER", RespCommand::AclGetuser),
  (b"LIST", RespCommand::AclList),
  (b"LOAD", RespCommand::AclLoad),
  (b"SAVE", RespCommand::AclSave),
  (b"SETUSER", RespCommand::AclSetuser),
  (b"USERS", RespCommand::AclUsers),
  (b"WHOAMI", RespCommand::AclWhoami),
];

const COMMAND_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"COUNT", RespCommand::CommandCount),
  (b"DOCS", RespCommand::CommandDocs),
  (b"INFO", RespCommand::CommandInfo),
  (b"GETKEYS", RespCommand::CommandGetkeys),
  (b"GETKEYSANDFLAGS", RespCommand::CommandGetkeysandflags),
];

const CONFIG_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"GET", RespCommand::ConfigGet),
  (b"REWRITE", RespCommand::ConfigRewrite),
  (b"SET", RespCommand::ConfigSet),
];

const SCRIPT_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"LOAD", RespCommand::ScriptLoad),
  (b"FLUSH", RespCommand::ScriptFlush),
  (b"EXISTS", RespCommand::ScriptExists),
];

const LATENCY_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"HELP", RespCommand::LatencyHelp),
  (b"HISTOGRAM", RespCommand::LatencyHistogram),
  (b"RESET", RespCommand::LatencyReset),
];

const SLOWLOG_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"HELP", RespCommand::SlowlogHelp),
  (b"GET", RespCommand::SlowlogGet),
  (b"LEN", RespCommand::SlowlogLen),
  (b"RESET", RespCommand::SlowlogReset),
];

const MODULE_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[(b"LOADCS", RespCommand::ModuleLoadcs)];

const PUBSUB_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"CHANNELS", RespCommand::PubsubChannels),
  (b"NUMSUB", RespCommand::PubsubNumsub),
  (b"NUMPAT", RespCommand::PubsubNumpat),
];

const MEMORY_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[(b"USAGE", RespCommand::MemoryUsage)];

const OBJECT_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"ENCODING", RespCommand::ObjectEncoding),
  (b"FREQ", RespCommand::ObjectFreq),
  (b"HELP", RespCommand::ObjectHelp),
  (b"IDLETIME", RespCommand::ObjectIdletime),
  (b"REFCOUNT", RespCommand::ObjectRefcount),
];

const BITOP_SUBCOMMANDS: &[(&[u8], RespCommand)] = &[
  (b"AND", RespCommand::BitopAnd),
  (b"OR", RespCommand::BitopOr),
  (b"XOR", RespCommand::BitopXor),
  (b"NOT", RespCommand::BitopNot),
  (b"DIFF", RespCommand::BitopDiff),
];

// ====== 统一紧凑子命令表容器（零堆分配） ======

struct SubTables {
  cluster: AlignedTable<128>,
  client: AlignedTable<16>,
  acl: AlignedTable<16>,
  command: AlignedTable<16>,
  config: AlignedTable<16>,
  script: AlignedTable<16>,
  latency: AlignedTable<16>,
  slowlog: AlignedTable<16>,
  module: AlignedTable<16>,
  pubsub: AlignedTable<16>,
  memory: AlignedTable<16>,
  object: AlignedTable<16>,
  bitop: AlignedTable<16>,
}

// ====== 静态初始化（单次运行 + 自校验） ======

static PRIMARY_TABLE: LazyLock<AlignedTable<PRIMARY_TABLE_SIZE>> = LazyLock::new(|| {
  let mut table = [CommandEntry::default(); PRIMARY_TABLE_SIZE];
  for &(name, cmd, has_sub) in PRIMARY_DEFINITIONS {
    let flags = if has_sub { FLAG_HAS_SUBCOMMANDS } else { 0 };
    insert_into_table(&mut table, PRIMARY_TABLE_MASK, name, cmd, flags);
  }

  // 1:1 对标 Garnet ValidatePrimaryTable，初始化时严格自校验全部主命令
  validate_primary_table(PRIMARY_DEFINITIONS, &table);

  AlignedTable(table)
});

static SUB_TABLES: LazyLock<SubTables> = LazyLock::new(|| {
  let cluster: AlignedTable<128> = build_sub_table(CLUSTER_SUBCOMMANDS);
  let client: AlignedTable<16> = build_sub_table(CLIENT_SUBCOMMANDS);
  let acl: AlignedTable<16> = build_sub_table(ACL_SUBCOMMANDS);
  let command: AlignedTable<16> = build_sub_table(COMMAND_SUBCOMMANDS);
  let config: AlignedTable<16> = build_sub_table(CONFIG_SUBCOMMANDS);
  let script: AlignedTable<16> = build_sub_table(SCRIPT_SUBCOMMANDS);
  let latency: AlignedTable<16> = build_sub_table(LATENCY_SUBCOMMANDS);
  let slowlog: AlignedTable<16> = build_sub_table(SLOWLOG_SUBCOMMANDS);
  let module: AlignedTable<16> = build_sub_table(MODULE_SUBCOMMANDS);
  let pubsub: AlignedTable<16> = build_sub_table(PUBSUB_SUBCOMMANDS);
  let memory: AlignedTable<16> = build_sub_table(MEMORY_SUBCOMMANDS);
  let object: AlignedTable<16> = build_sub_table(OBJECT_SUBCOMMANDS);
  let bitop: AlignedTable<16> = build_sub_table(BITOP_SUBCOMMANDS);

  // 1:1 对标 Garnet ValidateSubTable，严格自校验全部 13 个二级子命令表
  validate_sub_table(
    RespCommand::Cluster,
    CLUSTER_SUBCOMMANDS,
    &cluster.0,
    128 - 1,
  );
  validate_sub_table(RespCommand::Client, CLIENT_SUBCOMMANDS, &client.0, 16 - 1);
  validate_sub_table(RespCommand::Acl, ACL_SUBCOMMANDS, &acl.0, 16 - 1);
  validate_sub_table(
    RespCommand::Command,
    COMMAND_SUBCOMMANDS,
    &command.0,
    16 - 1,
  );
  validate_sub_table(RespCommand::Config, CONFIG_SUBCOMMANDS, &config.0, 16 - 1);
  validate_sub_table(RespCommand::Script, SCRIPT_SUBCOMMANDS, &script.0, 16 - 1);
  validate_sub_table(
    RespCommand::Latency,
    LATENCY_SUBCOMMANDS,
    &latency.0,
    16 - 1,
  );
  validate_sub_table(
    RespCommand::Slowlog,
    SLOWLOG_SUBCOMMANDS,
    &slowlog.0,
    16 - 1,
  );
  validate_sub_table(RespCommand::Module, MODULE_SUBCOMMANDS, &module.0, 16 - 1);
  validate_sub_table(RespCommand::Pubsub, PUBSUB_SUBCOMMANDS, &pubsub.0, 16 - 1);
  validate_sub_table(RespCommand::Memory, MEMORY_SUBCOMMANDS, &memory.0, 16 - 1);
  validate_sub_table(RespCommand::Object, OBJECT_SUBCOMMANDS, &object.0, 16 - 1);
  validate_sub_table(RespCommand::Bitop, BITOP_SUBCOMMANDS, &bitop.0, 16 - 1);

  SubTables {
    cluster,
    client,
    acl,
    command,
    config,
    script,
    latency,
    slowlog,
    module,
    pubsub,
    memory,
    object,
    bitop,
  }
});
