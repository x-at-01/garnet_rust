//! wedb_redis: Redis 语义命令实现层 (对标 Garnet libs/server Storage/Session 各命令族)
#![allow(async_fn_in_trait)]
// thread-per-core 模型: 命令 future 无需跨线程 Send,显式 async fn 保持签名简洁
//!
//! 经扩展 trait 挂载于 [`wkv::StoreSession`]：调用方 `use wedb_redis::prelude::*;`
//! 后即可在会话上直接调用 HSET/ZADD/LPUSH 等 Redis 命令方法。
//!
//! # 模块组成
//! - object: 对象值装载/落存、集合元数据、RENAME、通用集合操作
//! - string: 字符串键操作 (INCR/MGET/MSET/KEYS/APPEND/SETRANGE 等)
//! - bitmap: 位图操作 (SETBIT/BITCOUNT/BITPOS/BITOP，SIMD 向量加速)
//! - hll: HyperLogLog 基数估算 (PFADD/PFCOUNT/PFMERGE)
//! - hash: 散列哈希表 API
//! - list: 双向列表 API
//! - set: 无序集合 API
//! - zset: 有序集合 API (打平路由/聚合/范围检索)
//! - geo: 地理位置 API (底层复用 ZSet)
//! - bitmap_simd: 基于 fearless_simd 的硬件加速位图原语

mod bitmap;
mod geo;
mod hash;
mod hll;
mod list;
mod object;
mod set;
mod string;
mod zset;

use wval::{KeyTag, META_VALUE_SIZE, MetaValue, NamespaceDbCodec};

pub mod bitmap_simd;
pub mod error;

pub use bitmap_simd::{
  BitPosOffsetType, BitmapError, BitmapOp, OFFSET_TYPE_BIT, OFFSET_TYPE_BYTE, bitpos_bit_search,
  bitpos_byte_search, bitpos_driver, process_negative_offset, simd_bit_count, simd_bit_count_range,
  simd_bitop, simd_bitop_alloc, simd_bitop_binary, simd_bitop_not,
};

pub mod prelude {
  pub use super::{
    bitmap::BitmapCommands, geo::GeoCommands, hash::HashCommands, hll::HllCommands,
    list::ListCommands, object::ObjectCommands, set::SetCommands, string::StringCommands,
    zset::ZSetCommands,
  };
}

pub use bitmap::BitmapCommands;
pub use error::{Error, Result};
pub use geo::{
  GeoCommands, GeoSearchCenter, GeoSearchOpt, GeoSearchResult, GeoSearchShape, GeoSortOrder,
};
pub use hash::HashCommands;
pub use hll::HllCommands;
pub use list::ListCommands;
pub use object::ObjectCommands;
pub use set::SetCommands;
pub use string::StringCommands;
pub use wval::glob_match;
pub use zset::{AggregateType, LexBound, ZSetCommands};

/// 打平集合元素降级收缩为紧凑内联存储的阈值 (16)
pub const AUTO_DEMOTE_MAX_SIZE: u64 = 16;

/// 富对象未知集合类型哨兵标记（用于错误映射）
pub const TYPE_OBJECT_MARKER: u8 = 0xFF;

/// 键重命名结果状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenameResult {
  /// 源键不存在
  NoSuchKey,
  /// 目标键已存在（NX 模式触发拦截）
  AlreadyExists,
  /// 源键与目标键相同
  SameKey,
  /// 重命名执行成功
  Success,
}

/// 用户键扫描探针结果
#[derive(Debug)]
enum KeyScanProbe {
  /// 本条目不可见（墓碑 / 内部子键 / 幽灵元记录），继续遍历
  Skip,
  /// 回调已执行且要求继续遍历
  Continue,
  /// 回调已执行且要求提前终止遍历
  Stop,
}

#[inline(always)]
pub(crate) fn is_live_meta(val: &[u8]) -> bool {
  val.len() >= META_VALUE_SIZE && matches!(MetaValue::read_size(val), Ok(size) if size > 0)
}

/// 从底层记录键与值中解析当前会话存活用户逻辑键（过滤掉内部集合打平子键与分块键）
///
/// 方案 A 规范：单次 SIMD 比对当前会话前缀，若为 String 或存活 Meta 则返回用户逻辑键，
/// 其余子键或非当前会话数据一律过滤返回 None。彻底删除裸键回退，实现全库物理键统一规范。
#[inline]
pub(crate) fn live_user_key<'a>(
  key: &'a [u8],
  val: &[u8],
  session_prefix: &[u8],
) -> Option<&'a [u8]> {
  if let Some((tag, user_key)) = NamespaceDbCodec::extract_live_user_key(key, session_prefix) {
    match tag {
      KeyTag::String => Some(user_key),
      KeyTag::Meta => is_live_meta(val).then_some(user_key),
      _ => None,
    }
  } else {
    None
  }
}

/// 标准化 Redis 字符串/字节切片区间范围（处理负向索引与越界裁剪）
#[inline]
pub(crate) fn normalize_range(len: usize, start: isize, end: isize) -> Option<(usize, usize)> {
  if len == 0 || len > isize::MAX as usize {
    return None;
  }
  let len_isize = len as isize;
  let mut s = if start < 0 {
    start.saturating_add(len_isize)
  } else {
    start
  };
  let mut e = if end < 0 {
    end.saturating_add(len_isize)
  } else {
    end
  };
  if s < 0 {
    s = 0;
  }
  if e >= len_isize {
    e = len_isize - 1;
  }
  if s > e {
    return None;
  }
  Some((s as usize, e as usize))
}

/// 分值归一：-0.0 折叠为 +0.0（与 wedb_zset 对象层 encode_sortable_f64 的保序编码口径一致，
/// 消除 record 层（Compact 载荷/BfTree 子键）与对象层保序编码不一致）
#[inline]
pub(crate) fn normalize_zero(score: f64) -> f64 {
  if score == 0.0 { 0.0 } else { score }
}
