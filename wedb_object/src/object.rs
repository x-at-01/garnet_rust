use std::{fmt, str::FromStr};

#[cfg(feature = "wedb_hash")]
use wedb_hash::HashObject;
#[cfg(feature = "wedb_list")]
use wedb_list::ListObject;
#[cfg(feature = "wedb_set")]
use wedb_set::SetObject;
#[cfg(feature = "wedb_zset")]
use wedb_zset::SortedSetObject;

use crate::error::{Error, Result};

/// Garnet 对象类型枚举（对标 Garnet `GarnetObjectType`）
///
/// 序列化持久化格式为 `[type_byte][payload]`，第一个字节持久化为该对象的类型标识。
/// 0xFC..=0xFF 为未来格式版本保留段，永不作为类型字节写入。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(u8)]
pub enum GarnetObjectType {
  /// 空对象 (0)
  Null = 0,
  /// 有序集合 (1)
  SortedSet = 1,
  /// 列表 (2)
  List = 2,
  /// 哈希 (3)
  Hash = 3,
  /// 集合 (4)
  Set = 4,
}

impl GarnetObjectType {
  /// 最后一个内置对象类型编号（对标 C# `LastObjectType = GarnetObjectType.Set`）
  pub const LAST_OBJECT_TYPE: Self = Self::Set;
  /// 保留内置对象类型编号上限（对标 C# `LastReservedBuiltinType = 0x3F`）
  pub const LAST_RESERVED_BUILTIN_TYPE: u8 = 0x3F;
  /// 自定义对象命令标识（对标 C# `All = 0xFB`）
  pub const ALL: u8 = 0xFB;
  /// 保留对象格式版本字节起始值（对标 C# `ReservedObjectFormatByteStart = 0xFC`）
  /// 0xFC..=0xFF 为未来格式版本保留段，遇到即快速失败拒绝
  pub const RESERVED_FORMAT_BYTE_START: u8 = 0xFC;

  /// 判断指定字节是否位于保留格式版本段 (>= 0xFC)
  #[inline(always)]
  pub const fn is_reserved(val: u8) -> bool {
    val >= Self::RESERVED_FORMAT_BYTE_START
  }

  /// 判断是否属于有效集合对象类型（非空对象）
  #[inline(always)]
  pub const fn is_collection(self) -> bool {
    !matches!(self, Self::Null)
  }

  /// 判断原始字节是否属于四大内置集合对象类型编码 (1..=4)
  #[inline(always)]
  pub const fn is_builtin_collection_byte(val: u8) -> bool {
    matches!(val, 1..=4)
  }

  /// 从单字节解析对象类型（类型字节校验的唯一入口）
  ///
  /// 0xFC..=0xFF 为未来对象格式版本保留段（对标 C# `ReservedObjectFormatByteStart`），
  /// 遇到即说明数据由更高版本写入，快速失败而非静默丢弃。
  #[inline]
  pub const fn from_u8(val: u8) -> Result<Self> {
    match val {
      0 => Ok(Self::Null),
      1 => Ok(Self::SortedSet),
      2 => Ok(Self::List),
      3 => Ok(Self::Hash),
      4 => Ok(Self::Set),
      Self::RESERVED_FORMAT_BYTE_START..=0xFF => Err(Error::UnsupportedFormatMarker(val)),
      _ => Err(Error::UnknownObjectType(val)),
    }
  }

  /// 返回 Redis 规范类型名称（"zset", "list", "hash", "set", "none"）
  #[inline]
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::Null => "none",
      Self::SortedSet => "zset",
      Self::List => "list",
      Self::Hash => "hash",
      Self::Set => "set",
    }
  }

  /// 返回 Redis 规范类型名称字节切片（b"zset", b"list", b"hash", b"set", b"none"）
  #[inline]
  pub const fn as_bytes(self) -> &'static [u8] {
    self.as_str().as_bytes()
  }
}

impl TryFrom<u8> for GarnetObjectType {
  type Error = Error;

  #[inline]
  fn try_from(val: u8) -> Result<Self> {
    Self::from_u8(val)
  }
}

impl From<GarnetObjectType> for u8 {
  #[inline]
  fn from(ty: GarnetObjectType) -> Self {
    ty as Self
  }
}

impl fmt::Display for GarnetObjectType {
  #[inline]
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl AsRef<str> for GarnetObjectType {
  #[inline]
  fn as_ref(&self) -> &str {
    self.as_str()
  }
}

impl AsRef<[u8]> for GarnetObjectType {
  #[inline]
  fn as_ref(&self) -> &[u8] {
    self.as_bytes()
  }
}

impl FromStr for GarnetObjectType {
  type Err = Error;

  #[inline]
  fn from_str(s: &str) -> Result<Self> {
    match s {
      "none" => Ok(Self::Null),
      "zset" => Ok(Self::SortedSet),
      "list" => Ok(Self::List),
      "hash" => Ok(Self::Hash),
      "set" => Ok(Self::Set),
      _ => Err(Error::WrongType),
    }
  }
}

/// Garnet 统一堆对象枚举（封装 Hash, SortedSet, List, Set）
///
/// 各变体按 feature 裁剪；四类全关时退化为空枚举，任何构造路径均返回 Err，
/// 方法内 `match *self {}` 对空枚举天然穷尽，无需兜底分支。
#[derive(Debug, Clone)]
pub enum GarnetObject {
  #[cfg(feature = "wedb_hash")]
  Hash(HashObject),
  #[cfg(feature = "wedb_zset")]
  SortedSet(SortedSetObject),
  #[cfg(feature = "wedb_list")]
  List(ListObject),
  #[cfg(feature = "wedb_set")]
  Set(SetObject),
}

// 序列化容量预估常量（启发式，宁小勿滥，仅作 reserve 参考不入盘；按所属对象 feature 裁剪）
/// 哈希头部定长：版本 1B + 计数 4B
#[cfg(feature = "wedb_hash")]
const HASH_HEADER: usize = 5;
/// 集合/有序集合头部定长：版本 1B + 计数 4B
#[cfg(any(feature = "wedb_zset", feature = "wedb_set"))]
const COUNTED_HEADER: usize = 5;
/// 列表头部定长：仅计数 4B（无版本字节）
#[cfg(feature = "wedb_list")]
const LIST_HEADER: usize = 4;
/// 哈希单字段预估：双长度前缀 8B + 平均负载
#[cfg(feature = "wedb_hash")]
const HASH_FIELD_EST: usize = 16;
/// 有序集合单成员预估：分数 8B + 长度前缀 4B + 平均负载
#[cfg(feature = "wedb_zset")]
const ZSET_MEMBER_EST: usize = 20;
/// 列表单元素预估：长度前缀 4B + 平均负载
#[cfg(feature = "wedb_list")]
const LIST_ITEM_EST: usize = 8;
/// 集合单成员预估：长度前缀 4B + 平均负载
#[cfg(feature = "wedb_set")]
const SET_MEMBER_EST: usize = 12;

impl GarnetObject {
  /// 根据对象类型创建初始空对象（对标 C# `GarnetObject.Create`）
  pub fn new(obj_type: GarnetObjectType) -> Result<Self> {
    match obj_type {
      GarnetObjectType::Null => Err(Error::NullObject),
      #[cfg(feature = "wedb_hash")]
      GarnetObjectType::Hash => Ok(Self::Hash(HashObject::default())),
      #[cfg(feature = "wedb_zset")]
      GarnetObjectType::SortedSet => Ok(Self::SortedSet(SortedSetObject::default())),
      #[cfg(feature = "wedb_list")]
      GarnetObjectType::List => Ok(Self::List(ListObject::default())),
      #[cfg(feature = "wedb_set")]
      GarnetObjectType::Set => Ok(Self::Set(SetObject::default())),
      // 类型已穷尽匹配，仅当有对象类型 feature 被关闭时才需要兜底
      #[cfg(not(all(
        feature = "wedb_hash",
        feature = "wedb_zset",
        feature = "wedb_list",
        feature = "wedb_set"
      )))]
      _ => Err(Error::UnknownObjectType(obj_type as u8)),
    }
  }

  /// 获取对象类型枚举
  #[inline]
  pub fn object_type(&self) -> GarnetObjectType {
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(_) => GarnetObjectType::Hash,
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(_) => GarnetObjectType::SortedSet,
      #[cfg(feature = "wedb_list")]
      Self::List(_) => GarnetObjectType::List,
      #[cfg(feature = "wedb_set")]
      Self::Set(_) => GarnetObjectType::Set,
    }
  }

  /// 获取对象类型字符串（如 "hash", "zset", "list", "set"）
  #[inline]
  pub fn type_name(&self) -> &'static str {
    self.object_type().as_str()
  }

  /// 获取对象当前有效元素数量（触发底层过期回收）
  pub fn len(&mut self) -> usize {
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(ref mut h) => h.len(),
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(ref mut z) => z.len(),
      #[cfg(feature = "wedb_list")]
      Self::List(ref mut l) => l.len(),
      #[cfg(feature = "wedb_set")]
      Self::Set(ref mut s) => s.len(),
    }
  }

  /// 获取对象当前逻辑有效元素数量（只读引用，不触发可变过期剔除）
  pub fn len_ref(&self) -> usize {
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(ref h) => h.len_ref(),
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(ref z) => z.len_ref(),
      #[cfg(feature = "wedb_list")]
      Self::List(ref l) => l.len(),
      #[cfg(feature = "wedb_set")]
      Self::Set(ref s) => s.len(),
    }
  }

  /// 判断对象是否为空集合
  ///
  /// 需要可变借用：先触发底层对象回收过期条目，空判断才可靠
  /// （下游 save_object 据此决定删除空对象，对标 C# 过期回收语义）。
  pub fn is_empty(&mut self) -> bool {
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(ref mut h) => h.is_empty(),
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(ref mut z) => z.is_empty(),
      #[cfg(feature = "wedb_list")]
      Self::List(ref mut l) => l.is_empty(),
      #[cfg(feature = "wedb_set")]
      Self::Set(ref mut s) => s.is_empty(),
    }
  }

  /// 判断对象是否为空（只读引用版本，不产生写副作用）
  #[inline]
  pub fn is_empty_ref(&self) -> bool {
    self.len_ref() == 0
  }

  /// 获取底层集合容器当前已分配容量 (capacity)
  pub fn capacity(&self) -> usize {
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(ref h) => h.capacity(),
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(ref z) => z.capacity(),
      #[cfg(feature = "wedb_list")]
      Self::List(ref l) => l.capacity(),
      #[cfg(feature = "wedb_set")]
      Self::Set(ref s) => s.capacity(),
    }
  }

  /// 收缩底层集合内存，释放多余空闲容量 (shrink_to_fit)
  pub fn shrink_to_fit(&mut self) {
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(ref mut h) => h.shrink_to_fit(),
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(ref mut z) => z.shrink_to_fit(),
      #[cfg(feature = "wedb_list")]
      Self::List(ref mut l) => l.shrink_to_fit(),
      #[cfg(feature = "wedb_set")]
      Self::Set(ref mut s) => s.shrink_to_fit(),
    }
  }

  /// 清理底层集合中的过期条目（对标 C# HEXPIRE/ZEXPIRE 淘汰与清理语义），返回物理回收的过期项总数
  pub fn purge_expired(&mut self) -> usize {
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(ref mut h) => h.purge_expired(),
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(ref mut z) => z.purge_expired(),
      // List/Set 无逐元素 TTL
      #[cfg(feature = "wedb_list")]
      Self::List(_) => 0,
      #[cfg(feature = "wedb_set")]
      Self::Set(_) => 0,
    }
  }

  /// 预估序列化所需字节容量（启发式，避免缓冲区多次小额重分配）
  pub fn serialized_len_hint(&self) -> usize {
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(ref h) => 1 + HASH_HEADER + h.len_ref() * HASH_FIELD_EST,
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(ref z) => 1 + COUNTED_HEADER + z.len_ref() * ZSET_MEMBER_EST,
      #[cfg(feature = "wedb_list")]
      Self::List(ref l) => 1 + LIST_HEADER + l.len() * LIST_ITEM_EST,
      #[cfg(feature = "wedb_set")]
      Self::Set(ref s) => 1 + COUNTED_HEADER + s.len() * SET_MEMBER_EST,
    }
  }

  /// 序列化为 Garnet 兼容二进制（首字节为类型标识，后接对象具体序列化数据）
  ///
  /// 对标 C# `GarnetObjectSerializer.SerializeInternal`：C# 由 `WriteType` 先写类型字节，
  /// 此处将类型字节写入与对象分发合并为单次 match；底层各对象自带精确容量预留，
  /// 外层 `reserve` 仅作启发式预分配（`Vec::reserve` 自带容量不足判断，无需手工比较）。
  pub fn serialize(&mut self, buf: &mut Vec<u8>) {
    buf.reserve(self.serialized_len_hint());
    match *self {
      #[cfg(feature = "wedb_hash")]
      Self::Hash(ref mut h) => {
        buf.push(GarnetObjectType::Hash as u8);
        h.serialize(buf);
      }
      #[cfg(feature = "wedb_zset")]
      Self::SortedSet(ref mut z) => {
        buf.push(GarnetObjectType::SortedSet as u8);
        z.serialize(buf);
      }
      #[cfg(feature = "wedb_list")]
      Self::List(ref mut l) => {
        buf.push(GarnetObjectType::List as u8);
        l.serialize(buf);
      }
      #[cfg(feature = "wedb_set")]
      Self::Set(ref mut s) => {
        buf.push(GarnetObjectType::Set as u8);
        s.serialize(buf);
      }
    }
  }

  /// 转换为紧凑字节向量
  pub fn to_vec(&mut self) -> Vec<u8> {
    let mut buf = Vec::with_capacity(self.serialized_len_hint());
    self.serialize(&mut buf);
    buf
  }

  /// 反序列化二进制字节切片为 GarnetObject
  ///
  /// 格式 `[type_byte][payload]`，与 C# `GarnetObjectSerializer` 持久化布局一致；
  /// payload 尾部多余字节按底层各类型自身契约处理。类型字节校验（含保留段拒绝）
  /// 统一收敛在 [`GarnetObjectType::from_u8`]。
  pub fn deserialize(buf: &[u8]) -> Result<Self> {
    let Some(&type_byte) = buf.first() else {
      return Err(Error::UnexpectedEof);
    };
    let obj_type = GarnetObjectType::from_u8(type_byte)?;
    match obj_type {
      // C# 反序列化 Null 得到 null 引用；Rust 枚举无 Null 变体，以 Err(NullObject) 表达
      GarnetObjectType::Null => Err(Error::NullObject),
      #[cfg(feature = "wedb_zset")]
      GarnetObjectType::SortedSet => Ok(Self::SortedSet(SortedSetObject::deserialize(&buf[1..])?)),
      #[cfg(feature = "wedb_list")]
      GarnetObjectType::List => Ok(Self::List(ListObject::deserialize(&buf[1..])?)),
      #[cfg(feature = "wedb_hash")]
      GarnetObjectType::Hash => Ok(Self::Hash(HashObject::deserialize(&buf[1..])?)),
      #[cfg(feature = "wedb_set")]
      GarnetObjectType::Set => Ok(Self::Set(SetObject::deserialize(&buf[1..])?)),
      // 类型已穷尽匹配，仅当有对象类型 feature 被关闭时才需要兜底
      #[cfg(not(all(
        feature = "wedb_hash",
        feature = "wedb_zset",
        feature = "wedb_list",
        feature = "wedb_set"
      )))]
      _ => Err(Error::UnknownObjectType(type_byte)),
    }
  }
}

/// 为单个对象变体批量生成类型收窄访问器与双向转换 impl
///
/// 生成 `as_*` / `as_*_mut` / `into_*` 三个访问器与 `From<$ty>` / `TryFrom<_>` 四组转换，
/// 对应 C# 侧对象类型强转失败即 WRONGTYPE 的语义。`;$($other_feature => $other_variant),*`
/// 显式枚举其余变体分支（各自按所属 feature 裁剪），与手写展开完全等价，
/// 且在任意 feature 组合下（含单 feature、无 feature）均无非穷尽/不可达告警。
macro_rules! object_conv {
  (
    $feature:literal, $variant:ident, $ty:ty, $as:ident, $as_mut:ident, $into:ident;
    $($other_feature:literal => $other_variant:ident),* $(,)?
  ) => {
    #[cfg(feature = $feature)]
    impl GarnetObject {
      /// 只读借用底层对象，类型不匹配返回 WRONGTYPE
      #[inline]
      pub fn $as(&self) -> Result<&$ty> {
        match self {
          Self::$variant(v) => Ok(v),
          $(#[cfg(feature = $other_feature)] Self::$other_variant(_) => Err(Error::WrongType),)*
        }
      }

      /// 可变借用底层对象，类型不匹配返回 WRONGTYPE
      #[inline]
      pub fn $as_mut(&mut self) -> Result<&mut $ty> {
        match self {
          Self::$variant(v) => Ok(v),
          $(#[cfg(feature = $other_feature)] Self::$other_variant(_) => Err(Error::WrongType),)*
        }
      }

      /// 消耗所有权取出底层对象，类型不匹配返回 WRONGTYPE
      #[inline]
      pub fn $into(self) -> Result<$ty> {
        match self {
          Self::$variant(v) => Ok(v),
          $(#[cfg(feature = $other_feature)] Self::$other_variant(_) => Err(Error::WrongType),)*
        }
      }
    }

    #[cfg(feature = $feature)]
    impl From<$ty> for GarnetObject {
      #[inline]
      fn from(v: $ty) -> Self {
        Self::$variant(v)
      }
    }

    #[cfg(feature = $feature)]
    impl TryFrom<GarnetObject> for $ty {
      type Error = Error;

      #[inline]
      fn try_from(obj: GarnetObject) -> Result<Self> {
        obj.$into()
      }
    }

    #[cfg(feature = $feature)]
    impl<'a> TryFrom<&'a GarnetObject> for &'a $ty {
      type Error = Error;

      #[inline]
      fn try_from(obj: &'a GarnetObject) -> Result<Self> {
        obj.$as()
      }
    }

    #[cfg(feature = $feature)]
    impl<'a> TryFrom<&'a mut GarnetObject> for &'a mut $ty {
      type Error = Error;

      #[inline]
      fn try_from(obj: &'a mut GarnetObject) -> Result<Self> {
        obj.$as_mut()
      }
    }
  };
}

// 各调用点穷举除自身外的全部对象变体，保证访问器匹配在任意 feature 下穷尽
object_conv!(
  "wedb_hash", Hash, HashObject, as_hash, as_hash_mut, into_hash;
  "wedb_zset" => SortedSet, "wedb_list" => List, "wedb_set" => Set
);
object_conv!(
  "wedb_zset", SortedSet, SortedSetObject, as_sorted_set, as_sorted_set_mut, into_sorted_set;
  "wedb_hash" => Hash, "wedb_list" => List, "wedb_set" => Set
);
object_conv!(
  "wedb_list", List, ListObject, as_list, as_list_mut, into_list;
  "wedb_hash" => Hash, "wedb_zset" => SortedSet, "wedb_set" => Set
);
object_conv!(
  "wedb_set", Set, SetObject, as_set, as_set_mut, into_set;
  "wedb_hash" => Hash, "wedb_zset" => SortedSet, "wedb_list" => List
);
