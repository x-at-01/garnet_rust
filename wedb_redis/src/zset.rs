use std::{collections::VecDeque, sync::atomic::Ordering};

use wdev::Device;
use wedb_zset::{ScoreRange, ZAddOpt};
use whasher::{HashMap, HashSet, new_hash_map};
use wkv::{
  MAX_COMPACT_TOTAL_BYTES, RawCollectionRead, StoreSession, ZSET_MAX_COMPACT_ENTRIES,
  ZSET_MAX_COMPACT_MEMBER,
};
use wval::{
  CollectionType, CompactZSetCodec, MetaValue, SCORE_KEY_HEADER_SIZE, StorageEncoding,
  ZSetEntryRef, ZSetSubKeyBuf, ZSetSubKeyCodec, encode_order_preserving_f64,
  sample_distinct_indices,
};

use super::{set::lock_keys_sorted, *};
use crate::error::Result;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AggregateType {
  #[default]
  Sum,
  Min,
  Max,
}

/// 有序集合字典序区间边界
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LexBound<'a> {
  UnboundedMin,
  UnboundedMax,
  Inclusive(&'a [u8]),
  Exclusive(&'a [u8]),
}

impl<'a> LexBound<'a> {
  pub fn parse(s: &'a [u8]) -> Option<Self> {
    if s.is_empty() {
      return None;
    }
    match s[0] {
      b'-' if s.len() == 1 => Some(Self::UnboundedMin),
      b'+' if s.len() == 1 => Some(Self::UnboundedMax),
      b'[' => Some(Self::Inclusive(&s[1..])),
      b'(' => Some(Self::Exclusive(&s[1..])),
      _ => None,
    }
  }

  #[inline]
  pub fn matches_min(&self, val: &[u8]) -> bool {
    match self {
      Self::UnboundedMin => true,
      Self::UnboundedMax => false,
      Self::Inclusive(b) => val >= *b,
      Self::Exclusive(b) => val > *b,
    }
  }

  #[inline]
  pub fn matches_max(&self, val: &[u8]) -> bool {
    match self {
      Self::UnboundedMax => true,
      Self::UnboundedMin => false,
      Self::Inclusive(b) => val <= *b,
      Self::Exclusive(b) => val < *b,
    }
  }
}

// ================= 有序集合 ZSet API (基于 MetaValue + ZSetSubKeyCodec + BTree 双向打平路由) =================

/// 递增定长字节数组以生成开区间右边界（用于前缀范围检索，零堆分配）
#[inline]
pub fn prefix_next_array<const N: usize>(mut arr: [u8; N]) -> Option<[u8; N]> {
  for byte in arr.iter_mut().rev() {
    if *byte < 0xff {
      *byte += 1;
      return Some(arr);
    }
    *byte = 0;
  }
  None
}

/// 归一化 ZRANGE 排名区间参数（Redis 语义：负索引从尾部计数，越界裁剪）
///
/// 返回 `None` 表示空区间（集合为空 / start 越界 / start > stop，含 stop 越过 -len
/// ——对齐 dev 侧 adaptive zset 边界语义：负向越界不回卷至首元素）；
/// 返回 `(actual_start, count)`，count 恒 ≥ 1
#[inline]
fn normalize_zrange_range(len: isize, start: isize, stop: isize) -> Option<(usize, usize)> {
  if len == 0 {
    return None;
  }
  let actual_start = if start < 0 {
    len.saturating_add(start).max(0)
  } else {
    start
  };
  let actual_stop = if stop < 0 {
    len.saturating_add(stop)
  } else {
    stop
  };
  if actual_start >= len || actual_start > actual_stop {
    return None;
  }
  let actual_stop = actual_stop.min(len - 1);
  Some((
    actual_start as usize,
    (actual_stop - actual_start + 1) as usize,
  ))
}

/// 编码有序集合分值子键公共 17 字节前缀 [0x05 | key_id: 8B | version: 8B] (const fn，零堆分配)
#[inline(always)]
pub const fn zset_score_prefix(key_id: u64, version: u64) -> [u8; 17] {
  ZSetSubKeyCodec::encode_score_prefix(key_id, version)
}

/// 构造针对指定 ZSet 与 ScoreRange 的 BfTree 查询字节边界（零堆分配，栈内存优化）
#[inline]
pub fn zset_score_range_keys(
  key_id: u64,
  version: u64,
  range: ScoreRange,
) -> (ZSetSubKeyBuf, ZSetSubKeyBuf) {
  let prefix = zset_score_prefix(key_id, version);
  let prefix_end = match prefix_next_array(prefix) {
    Some(end) => ZSetSubKeyBuf::from(&end[..]),
    None => ZSetSubKeyBuf::from(&[0xff; 18][..]),
  };

  let start_key = if range.min == f64::NEG_INFINITY || range.min.is_nan() {
    ZSetSubKeyBuf::from(&prefix[..])
  } else {
    let h = ZSetSubKeyCodec::encode_score_header(key_id, version, range.min);
    if range.min_inclusive {
      ZSetSubKeyBuf::from(&h[..])
    } else {
      match prefix_next_array(h) {
        Some(next_h) if &next_h[..] < prefix_end.as_slice() => ZSetSubKeyBuf::from(&next_h[..]),
        _ => prefix_end.clone(),
      }
    }
  };

  let end_key = if range.max == f64::INFINITY || range.max.is_nan() {
    prefix_end
  } else {
    let h = ZSetSubKeyCodec::encode_score_header(key_id, version, range.max);
    if range.max_inclusive {
      match prefix_next_array(h) {
        Some(next_h) if &next_h[..] < prefix_end.as_slice() => ZSetSubKeyBuf::from(&next_h[..]),
        _ => prefix_end,
      }
    } else {
      ZSetSubKeyBuf::from(&h[..])
    }
  };

  (start_key, end_key)
}

/// 有序集合分值索引条目的最小非空占位字节（bf-tree 要求 value 非空）
pub(crate) const ZSET_SCORE_ENTRY_PLACEHOLDER: &[u8] = &[0];

/// SRANDMEMBER 负数采样的防御性上限（与 `wedb_set` SetObject::MAX_RAND_SAMPLE_LIMIT 口径一致）
pub(crate) const SRANDMEMBER_MAX_SAMPLE: usize = 1_000_000;

/// 随机采样结果的初始容量预留上限（仅约束预分配，防御极端 count 值触发容量溢出中断）
pub(crate) const RAND_SAMPLE_RESERVE_CAP: usize = 8192;

/// SCAN 族 COUNT 提示的初始容量预留上限（仅约束预分配，实际结果集仍可按需增长，
/// 防御极端 COUNT 值触发容量溢出中断）
pub(crate) const SCAN_RESERVE_CAP: usize = 1024;

/// 有序集合成员归属探针（ZINTERCARD 预加载：循环前一次性装载，循环内零元数据重读）
enum ZSetProbe<'a> {
  /// 紧凑载荷一次性物化为成员哈希集（零拷贝借用成员切片）
  Compact(HashSet<&'a [u8]>),
  /// 打平编码：复用 key_id/version 仅构造子键查分
  Flattened { key_id: u64, version: u64 },
}

impl ZSetProbe<'_> {
  /// 成员存在性判定：Compact 纯内存哈希集 O(1)，Flattened 单子键 BfTree 点查
  fn contains<D: Device>(&self, session: &StoreSession<D>, member: &[u8]) -> Result<bool> {
    match self {
      Self::Compact(set) => Ok(set.contains(member)),
      Self::Flattened { key_id, version } => {
        let mkey = ZSetSubKeyBuf::from_member(*key_id, *version, member)?;
        Ok(bftree_get_score(session, &mkey).is_some())
      }
    }
  }
}

/// INCR 先求结果并拒绝 NaN，再执行 NX/GT/LT 条件判断。
pub(crate) fn zadd_score(old_score: f64, score: f64, options: ZAddOpt) -> Result<f64> {
  let score = if options.incr {
    old_score + score
  } else {
    score
  };
  if score.is_nan() {
    return Err(wedb_zset::Error::InvalidScore.into());
  }
  Ok(normalize_zero(score))
}

/// 读取有序集合成员当前分值（零堆分配，快速点查）
pub(crate) fn bftree_get_score<D: Device>(session: &StoreSession<D>, mkey: &[u8]) -> Option<f64> {
  let mut buf = [0u8; 8];
  let (res, len) = session.store.bftree.read_into(mkey, &mut buf);
  if res == wbftree::BfTreeReadResult::Found && len == 8 {
    Some(f64::from_be_bytes(buf))
  } else {
    None
  }
}

/// ZADD 无锁内核（调用方必须已持有 `key` 的独占桶锁）
pub(crate) async fn zadd_unlocked<D: Device>(
  session: &StoreSession<D>,
  key: &[u8],
  score: f64,
  member: impl AsRef<[u8]>,
  options: ZAddOpt,
) -> Result<(usize, f64)> {
  // -0.0 归一为 +0.0（normalize_zero 统一口径）
  let score = normalize_zero(score);
  let member = member.as_ref();
  let (mut meta, raw_opt) = match session
    .load_collection_raw_write(key, CollectionType::ZSet)
    .await?
  {
    Some(res) => res,
    None => {
      if options.xx {
        return Ok((0, score));
      }
      let key_id = session.store.next_key_id.fetch_add(1, Ordering::Relaxed);
      if member.len() <= ZSET_MAX_COMPACT_MEMBER && member.len() + 16 <= MAX_COMPACT_TOTAL_BYTES {
        let mut meta = MetaValue::new(key_id, CollectionType::ZSet, 1, 1);
        meta.set_encoding(StorageEncoding::Compact);
        // 预估容量：成员长 + 分值/长度前缀，避免插入时反复扩容
        let mut raw = Vec::with_capacity(member.len() + 16);
        CompactZSetCodec::insert(&mut raw, score, member)?;
        session.save_compact_meta(key, &meta, &raw).await?;
        return Ok((1, score));
      } else {
        let mut meta = MetaValue::new(key_id, CollectionType::ZSet, 1, 1);
        meta.set_encoding(StorageEncoding::Flattened);
        let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
        let skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, score, member)?;
        session
          .store
          .bftree
          .insert(&skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
        session.store.bftree.insert(&mkey, &score.to_be_bytes());
        session.save_meta(key, &meta).await?;
        return Ok((1, score));
      }
    }
  };

  if meta.encoding() == StorageEncoding::Compact {
    let mut raw = raw_opt.unwrap_or_default();
    let old_score_opt = CompactZSetCodec::score_of(&raw, member);

    match old_score_opt {
      Some(old_score) => {
        let score = zadd_score(old_score, score, options)?;
        if options.nx {
          return Ok((0, old_score));
        }
        if options.gt && score <= old_score {
          return Ok((0, old_score));
        }
        if options.lt && score >= old_score {
          return Ok((0, old_score));
        }

        // 精确相等比较 (对标 C# SortedSetObjectImpl `score == scoreStored`，epsilon 比较会静默丢弃亚 ε 分值更新)
        let changed = score != old_score;
        if changed {
          CompactZSetCodec::insert(&mut raw, score, member)?;
          session.save_compact_meta(key, &meta, &raw).await?;
        }

        let ret_count = if (options.ch || options.incr) && changed {
          1
        } else {
          0
        };
        return Ok((ret_count, score));
      }
      None => {
        if options.xx {
          return Ok((0, score));
        }

        if meta.size as usize >= ZSET_MAX_COMPACT_ENTRIES
          || member.len() > ZSET_MAX_COMPACT_MEMBER
          || (raw.len() + member.len() + 16 > MAX_COMPACT_TOTAL_BYTES)
        {
          promote_zset_to_flattened(session, &mut meta, &raw).await?;
          let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
          let skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, score, member)?;
          session
            .store
            .bftree
            .insert(&skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
          session.store.bftree.insert(&mkey, &score.to_be_bytes());
          meta.inc_size(1);
          session.save_meta(key, &meta).await?;
          return Ok((1, score));
        }

        CompactZSetCodec::insert(&mut raw, score, member)?;
        meta.inc_size(1);
        session.save_compact_meta(key, &meta, &raw).await?;
        return Ok((1, score));
      }
    }
  }

  // Flattened 模式
  let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
  let old_score_opt = bftree_get_score(session, &mkey);

  match old_score_opt {
    Some(old_score) => {
      let score = zadd_score(old_score, score, options)?;
      if options.nx {
        return Ok((0, old_score));
      }
      if options.gt && score <= old_score {
        return Ok((0, old_score));
      }
      if options.lt && score >= old_score {
        return Ok((0, old_score));
      }

      // 精确相等比较 (对标 C# SortedSetObjectImpl `score == scoreStored`)
      let changed = score != old_score;
      if changed {
        let old_skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, old_score, member)?;
        session.store.bftree.delete(&old_skey);

        let new_skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, score, member)?;
        session
          .store
          .bftree
          .insert(&new_skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
        session.store.bftree.insert(&mkey, &score.to_be_bytes());
      }

      let ret_count = if (options.ch || options.incr) && changed {
        1
      } else {
        0
      };
      Ok((ret_count, score))
    }
    None => {
      if options.xx {
        return Ok((0, score));
      }

      let new_skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, score, member)?;
      session
        .store
        .bftree
        .insert(&new_skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
      session.store.bftree.insert(&mkey, &score.to_be_bytes());

      meta.inc_size(1);
      session.save_meta(key, &meta).await?;
      Ok((1, score))
    }
  }
}

/// ZMADD 无锁内核（调用方必须已持有 `key` 的独占桶锁）
pub(crate) async fn zmadd_unlocked<M: AsRef<[u8]>, D: Device>(
  session: &StoreSession<D>,
  key: &[u8],
  mut item_vec: Vec<(f64, M)>,
  options: ZAddOpt,
) -> Result<usize> {
  // -0.0 归一为 +0.0（normalize_zero 统一口径）
  for (score, _) in item_vec.iter_mut() {
    *score = normalize_zero(*score);
  }

  let (mut meta, raw_opt) = match session
    .load_collection_raw_write(key, CollectionType::ZSet)
    .await?
  {
    Some(res) => res,
    None => {
      if options.xx {
        return Ok(0);
      }
      let key_id = session.store.next_key_id.fetch_add(1, Ordering::Relaxed);
      let has_large = item_vec
        .iter()
        .any(|(_, m)| m.as_ref().len() > ZSET_MAX_COMPACT_MEMBER);
      let total_bytes: usize = item_vec.iter().map(|(_, m)| m.as_ref().len() + 16).sum();
      if !has_large
        && item_vec.len() <= ZSET_MAX_COMPACT_ENTRIES
        && total_bytes <= MAX_COMPACT_TOTAL_BYTES
      {
        let mut meta = MetaValue::new(key_id, CollectionType::ZSet, 1, 0);
        meta.set_encoding(StorageEncoding::Compact);
        // 预估总字节数已知（成员长 + 16 字节前缀），一次性分配消除批量插入反复扩容
        (meta, Some(Vec::with_capacity(total_bytes)))
      } else {
        let mut meta = MetaValue::new(key_id, CollectionType::ZSet, 1, 0);
        meta.set_encoding(StorageEncoding::Flattened);
        (meta, None)
      }
    }
  };

  let mut promoted = false;
  if meta.encoding() == StorageEncoding::Compact {
    let mut raw = raw_opt.unwrap_or_default();
    let has_large = item_vec
      .iter()
      .any(|(_, m)| m.as_ref().len() > ZSET_MAX_COMPACT_MEMBER);
    let added_bytes: usize = item_vec.iter().map(|(_, m)| m.as_ref().len() + 16).sum();
    if has_large
      || (meta.size as usize + item_vec.len() > ZSET_MAX_COMPACT_ENTRIES)
      || (raw.len() + added_bytes > MAX_COMPACT_TOTAL_BYTES)
    {
      promote_zset_to_flattened(session, &mut meta, &raw).await?;
      promoted = true;
    } else {
      let mut total_added_or_changed = 0usize;
      let mut size_delta = 0u64;
      // 脏标记：任何新增或分数更新都必须持久化（无 CH 的分数更新返回 0 但绝不能丢失变更）
      let mut dirty = false;

      for (score, member) in &item_vec {
        let m = member.as_ref();
        let old_score_opt = CompactZSetCodec::score_of(&raw, m);
        match old_score_opt {
          Some(old_score) => {
            if options.nx {
              continue;
            }
            if options.gt && *score <= old_score {
              continue;
            }
            if options.lt && *score >= old_score {
              continue;
            }
            // 精确相等比较 (对标 C# SortedSetObjectImpl `score == scoreStored`)
            let changed = *score != old_score;
            if changed {
              CompactZSetCodec::insert(&mut raw, *score, m)?;
              dirty = true;
              if options.ch {
                total_added_or_changed += 1;
              }
            }
          }
          None => {
            if options.xx {
              continue;
            }
            CompactZSetCodec::insert(&mut raw, *score, m)?;
            dirty = true;
            size_delta += 1;
            total_added_or_changed += 1;
          }
        }
      }

      if dirty || meta.size == 0 {
        meta.inc_size(size_delta);
        session.save_compact_meta(key, &meta, &raw).await?;
      }
      return Ok(total_added_or_changed);
    }
  }

  // Flattened 分支
  let mut total_added_or_changed = 0usize;
  let mut size_delta = 0u64;

  for (score, member) in item_vec {
    let member = member.as_ref();
    let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
    let old_score_opt = bftree_get_score(session, &mkey);

    match old_score_opt {
      Some(old_score) => {
        if options.nx {
          continue;
        }
        if options.gt && score <= old_score {
          continue;
        }
        if options.lt && score >= old_score {
          continue;
        }

        // 精确相等比较 (对标 C# SortedSetObjectImpl `score == scoreStored`，epsilon 比较会静默丢弃亚 ε 分值更新)
        let changed = score != old_score;
        if changed {
          let old_skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, old_score, member)?;
          session.store.bftree.delete(&old_skey);

          let new_skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, score, member)?;
          session
            .store
            .bftree
            .insert(&new_skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
          session.store.bftree.insert(&mkey, &score.to_be_bytes());

          if options.ch {
            total_added_or_changed += 1;
          }
        }
      }
      None => {
        if options.xx {
          continue;
        }

        let new_skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, score, member)?;
        session
          .store
          .bftree
          .insert(&new_skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
        session.store.bftree.insert(&mkey, &score.to_be_bytes());

        size_delta += 1;
        total_added_or_changed += 1;
      }
    }
  }

  if promoted || size_delta > 0 || meta.size == 0 {
    meta.inc_size(size_delta);
    session.save_meta(key, &meta).await?;
  }

  Ok(total_added_or_changed)
}

/// 尝试将打平 ZSet 降级收缩为 Compact 编码（元素数 <= 16 时）
pub(crate) async fn try_demote_zset<D: Device>(
  session: &StoreSession<D>,
  user_key: &[u8],
  meta: &mut MetaValue,
) -> Result<bool> {
  if meta.encoding() != StorageEncoding::Flattened
    || meta.size == 0
    || meta.size > AUTO_DEMOTE_MAX_SIZE
  {
    return Ok(false);
  }
  let items = session.zrange_with_meta(meta, 0, -1, false)?;
  if items.is_empty() {
    // 崩溃一致性（先 meta 后数据）：先递增版本写 meta 判死（size==0 删元记录），
    // 后按旧版本清空 BfTree 双键；中途崩溃仅遗留读不可见的 BfTree 孤儿（磁盘泄漏可接受）
    let old_version = meta.version;
    meta.bump_version();
    meta.size = 0;
    session.save_meta(user_key, meta).await?;
    session.clear_bftree_zset(meta.key_id, old_version)?;
    return Ok(true);
  }
  for (m, _) in &items {
    if m.len() > ZSET_MAX_COMPACT_MEMBER {
      return Ok(false);
    }
  }
  let Ok(payload) = CompactZSetCodec::encode(items.iter().map(|(m, s)| (*s, m.as_slice()))) else {
    return Ok(false);
  };
  if payload.len() > MAX_COMPACT_TOTAL_BYTES {
    return Ok(false);
  }

  // 崩溃一致性（先 meta 后数据）：先翻转编码写 compact meta（版本递增隔离新载荷），
  // 后按旧版本清空 BfTree 双键。中途崩溃时读路径立即可从内联载荷自洽读取，遗留的
  // 旧版本 BfTree 双键读不可见（磁盘泄漏可接受），且不再被任何读路径命中
  let old_version = meta.version;
  meta.bump_version();
  meta.set_encoding(StorageEncoding::Compact);
  meta.size = items.len() as u64;
  // 清零打平分块信息，避免残留 chunk 位串扰 Compact 载荷的 has_expire 粘性标志位
  StoreSession::<D>::set_meta_chunk_info(&mut meta.reserved, 0, 0);
  session.save_compact_meta(user_key, meta, &payload).await?;
  session.clear_bftree_zset(meta.key_id, old_version)?;
  Ok(true)
}

/// ZREM 无锁内核（调用方必须已持有 `key` 的独占桶锁）
pub(crate) async fn zrem_unlocked<D: Device>(
  session: &StoreSession<D>,
  key: &[u8],
  members: &[&[u8]],
) -> Result<usize> {
  let (mut meta, raw_opt) = match session
    .load_collection_raw_write(key, CollectionType::ZSet)
    .await?
  {
    Some(res) => res,
    None => return Ok(0),
  };
  if meta.size == 0 {
    return Ok(0);
  }

  if meta.encoding() == StorageEncoding::Compact {
    let mut raw = raw_opt.unwrap_or_default();
    let mut removed = 0usize;
    for &member in members {
      if CompactZSetCodec::remove(&mut raw, member)? {
        removed += 1;
      }
    }
    if removed > 0 {
      meta.dec_size(removed as u64);
      session.save_compact_meta(key, &meta, &raw).await?;
    }
    return Ok(removed);
  }

  let mut removed = 0usize;
  for &member in members {
    let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
    if let Some(old_score) = bftree_get_score(session, &mkey) {
      session.store.bftree.delete(&mkey);
      if let Ok(skey) = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, old_score, member) {
        session.store.bftree.delete(&skey);
      }
      removed += 1;
    }
  }

  if removed > 0 {
    meta.dec_size(removed as u64);
    if meta.size == 0 {
      // 崩溃一致性（先 meta 后数据）：先递增版本写 meta 判死（size==0 删元记录），
      // 后按旧版本清空 BfTree 双键；中途崩溃仅遗留读不可见的 BfTree 孤儿（磁盘泄漏可接受）
      let old_version = meta.version;
      meta.bump_version();
      session.save_meta(key, &meta).await?;
      session.clear_bftree_zset(meta.key_id, old_version)?;
    } else if meta.size <= AUTO_DEMOTE_MAX_SIZE {
      if !try_demote_zset(session, key, &mut meta).await? {
        session.save_meta(key, &meta).await?;
      }
    } else {
      session.save_meta(key, &meta).await?;
    }
  }

  Ok(removed)
}

/// 从全量集合中辅助随机选取 ZSet 成员（接管入参所有权，全集命中零拷贝返回）
pub(crate) fn pick_random_zset_from_slice(
  all: Vec<(Vec<u8>, f64)>,
  count: isize,
) -> Result<Vec<(Vec<u8>, f64)>> {
  if all.is_empty() || count == 0 {
    return Ok(Vec::new());
  }
  if count > 0 {
    let k = (count as usize).min(all.len());
    if k == all.len() {
      return Ok(all);
    }
    // 无重复升序抽样：O(k) 位掩码/栈数组，替代全量 indices + shuffle 的 O(N) 分配
    let mut result = Vec::with_capacity(k);
    for idx in sample_distinct_indices(all.len(), k) {
      result.push(all[idx].clone());
    }
    Ok(result)
  } else {
    let pick_count = count.unsigned_abs().min(SRANDMEMBER_MAX_SAMPLE);
    let mut result = Vec::with_capacity(pick_count.min(RAND_SAMPLE_RESERVE_CAP));
    for _ in 0..pick_count {
      let idx = fastrand::usize(0..all.len());
      result.push(all[idx].clone());
    }
    Ok(result)
  }
}

/// ZUNIONSTORE/ZINTERSTORE/ZDIFFSTORE 共用落盘样板（调用方必须已持 dest 及全部源键整体独占桶锁）：
/// 清空 dest 后将聚合结果经 zmadd 无锁内核整批写入，已持锁环境下绝不可走会重新加锁的 zmadd 入口
pub(crate) async fn zstore_apply_unlocked<D: Device>(
  session: &StoreSession<D>,
  dest: &[u8],
  result: Vec<(Vec<u8>, f64)>,
) -> Result<usize> {
  // 聚合可能产生 NaN（如 Sum 下 (+inf)+(-inf)），此处是绕过 zmadd 入口校验的
  // 唯一防线：必须在清空 dest 之前拦截，错误口径与 zmadd 入口保持一致
  if result.iter().any(|(.., s)| s.is_nan()) {
    return Err(wedb_zset::Error::InvalidScore.into());
  }
  session.delete(dest).await?;
  if result.is_empty() {
    return Ok(0);
  }
  // 已持 dest 独占锁，直接复用无锁内核避免桶锁重入自锁
  zmadd_unlocked(
    session,
    dest,
    result.into_iter().map(|(m, s)| (s, m)).collect(),
    ZAddOpt::default(),
  )
  .await
}

pub trait ZSetCommands<D: Device> {
  /// 添加或更新有序集合成员 (ZADD)
  ///
  /// 入口获取目标键独占桶锁（两阶段锁）串行化同键读改写窗口，杜绝并发连接静默丢更新
  async fn zadd(
    &self,
    key: &[u8],
    score: f64,
    member: impl AsRef<[u8]>,
    options: ZAddOpt,
  ) -> Result<(usize, f64)>;

  /// 批量添加或更新有序集合成员 (ZADD key score member [score member ...])
  ///
  /// 入口获取目标键独占桶锁（两阶段锁）串行化同键读改写窗口，杜绝并发连接静默丢更新
  async fn zmadd<M: AsRef<[u8]>>(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = (f64, M)>,
    options: ZAddOpt,
  ) -> Result<usize>;

  /// 获取成员分数 (ZSCORE)
  async fn zscore(&self, key: &[u8], member: &[u8]) -> Result<Option<f64>>;

  /// 批量获取成员分数 (ZMSCORE)
  async fn zmscore(&self, key: &[u8], members: &[&[u8]]) -> Result<Vec<Option<f64>>>;

  /// 基于已加载元数据获取成员排名 (ZRANK，从低到高 0-based)
  fn zrank_with_meta(&self, meta: &MetaValue, member: &[u8]) -> Result<Option<usize>>;

  /// 获取成员排名 (ZRANK，从低到高 0-based)
  async fn zrank(&self, key: &[u8], member: &[u8]) -> Result<Option<usize>>;

  /// 获取成员逆序排名 (ZREVRANK，从高到低 0-based)
  async fn zrevrank(&self, key: &[u8], member: &[u8]) -> Result<Option<usize>>;

  /// 基于已加载元数据按排名范围获取元素（回调式零拷贝）
  ///
  /// `on_item(member, score)` 返回 false 提前终止扫描；成员切片仅在回调执行期内
  /// 有效（BfTree 扫描栈缓冲直出），调用方零 `to_vec` 即可消费
  fn zrange_with_meta_cb<F>(
    &self,
    meta: &MetaValue,
    start: isize,
    stop: isize,
    reverse: bool,
    on_item: F,
  ) -> Result<()>
  where
    F: FnMut(&[u8], f64) -> bool;

  /// 基于已加载元数据按排名范围获取元素
  fn zrange_with_meta(
    &self,
    meta: &MetaValue,
    start: isize,
    stop: isize,
    reverse: bool,
  ) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 按排名范围获取元素（回调式零拷贝，ZRANGE 语义）
  ///
  /// `on_item(member, score)` 返回 false 提前终止；成员切片仅在回调执行期内有效。
  /// 紧凑编码正向路径全程零堆分配直接消费载荷切片
  async fn zrange_cb<F>(
    &self,
    key: &[u8],
    start: isize,
    stop: isize,
    reverse: bool,
    on_item: F,
  ) -> Result<()>
  where
    F: FnMut(&[u8], f64) -> bool;

  /// 按排名范围获取元素 (ZRANGE)
  async fn zrange(
    &self,
    key: &[u8],
    start: isize,
    stop: isize,
    reverse: bool,
  ) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 基于已加载元数据按分数范围获取元素
  fn zrangebyscore_with_meta(
    &self,
    meta: &MetaValue,
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 按分数范围获取元素 (ZRANGEBYSCORE)
  async fn zrangebyscore(
    &self,
    key: &[u8],
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 获取元素总数 (ZCARD，O(1))
  async fn zcard(&self, key: &[u8]) -> Result<usize>;

  /// 统计指定分数区间元素数量 (ZCOUNT)
  async fn zcount(&self, key: &[u8], range: ScoreRange) -> Result<usize>;

  /// 分数递增 (ZINCRBY，独占桶锁串行化同键读改写窗口)
  async fn zincrby(&self, key: &[u8], increment: f64, member: &[u8]) -> Result<f64>;

  /// 移除成员 (ZREM，独占桶锁串行化同键读改写窗口)
  async fn zrem(&self, key: &[u8], members: &[&[u8]]) -> Result<usize>;

  /// 按排名范围删除成员 (ZREMRANGEBYRANK，独占桶锁串行化同键读改写窗口)
  async fn zremrangebyrank(&self, key: &[u8], start: isize, stop: isize) -> Result<usize>;

  /// 按分数范围删除成员 (ZREMRANGEBYSCORE，独占桶锁串行化同键读改写窗口)
  async fn zremrangebyscore(&self, key: &[u8], range: ScoreRange) -> Result<usize>;

  /// 弹出分数最小的成员 (ZPOPMIN，独占桶锁串行化同键读改写窗口)
  async fn zpopmin(&self, key: &[u8], count: usize) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 弹出分数最大的成员 (ZPOPMAX，独占桶锁串行化同键读改写窗口)
  async fn zpopmax(&self, key: &[u8], count: usize) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 游标扫描有序集合 (ZSCAN)
  async fn zscan(
    &self,
    key: &[u8],
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> Result<(usize, Vec<(Vec<u8>, f64)>)>;

  /// 随机获取有序集合中的成员 (ZRANDMEMBER)
  async fn zrandmember(&self, key: &[u8], count: isize) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 多有序集合并集 (ZUNION)
  async fn zunion(
    &self,
    keys: &[&[u8]],
    weights: &[f64],
    agg: AggregateType,
  ) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 多有序集合并集并存储 (ZUNIONSTORE)
  ///
  /// dest 与全部源键排序去重后整体获取独占桶锁（锁引擎内部再按桶下标全序加锁防死锁），
  /// 聚合计算、dest 清空与结果写入全程原子，杜绝 delete 与写入两步间被并发修改撕裂
  async fn zunionstore(
    &self,
    dest: &[u8],
    keys: &[&[u8]],
    weights: &[f64],
    agg: AggregateType,
  ) -> Result<usize>;

  /// 多有序集合交集 (ZINTER)
  async fn zinter(
    &self,
    keys: &[&[u8]],
    weights: &[f64],
    agg: AggregateType,
  ) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 多有序集合交集并存储 (ZINTERSTORE，dest + 源键整体独占桶锁保护，口径同 ZUNIONSTORE)
  async fn zinterstore(
    &self,
    dest: &[u8],
    keys: &[&[u8]],
    weights: &[f64],
    agg: AggregateType,
  ) -> Result<usize>;

  /// 多有序集合交集基数 (ZINTERCARD)
  ///
  /// 以基数最小的集合为驱动集逐成员探针，命中数达 LIMIT 立即返回，
  /// 避免物化完整交集（严格对标 Redis/Garnet LIMIT 语义）
  async fn zintercard(&self, keys: &[&[u8]], limit: usize) -> Result<usize>;

  /// 多有序集合求差集 (ZDIFF)
  ///
  /// 保序论证：zrange(0,-1,false) 输出即 (score 升序, member 升序) 的 BfTree/紧凑全序，
  /// 与旧实现的末尾 sort_by 比较器完全一致；因此对 first 原位保留过滤即天然保序，
  /// 无需 O(n log n) 重排（gxhash HashMap 为无序桶结构，不能依赖其遍历序）
  async fn zdiff(&self, keys: &[&[u8]]) -> Result<Vec<(Vec<u8>, f64)>>;

  /// 多有序集合求差集并存储 (ZDIFFSTORE，dest + 源键整体独占桶锁保护，口径同 ZUNIONSTORE)
  async fn zdiffstore(&self, dest: &[u8], keys: &[&[u8]]) -> Result<usize>;

  /// 字典序范围查询成员 (ZRANGEBYLEX)
  async fn zrangebylex(
    &self,
    key: &[u8],
    min: LexBound<'_>,
    max: LexBound<'_>,
    offset: usize,
    count: usize,
    reverse: bool,
  ) -> Result<Vec<Vec<u8>>>;

  /// 字典序范围元素计数 (ZLEXCOUNT)
  async fn zlexcount(&self, key: &[u8], min: LexBound<'_>, max: LexBound<'_>) -> Result<usize>;

  /// 字典序范围删除成员 (ZREMRANGEBYLEX，独占桶锁串行化同键读改写窗口)
  async fn zremrangebylex(&self, key: &[u8], min: LexBound<'_>, max: LexBound<'_>)
  -> Result<usize>;
}

pub(crate) async fn promote_zset_to_flattened<D: Device>(
  session: &StoreSession<D>,
  meta: &mut MetaValue,
  compact_buf: &[u8],
) -> Result<()> {
  for entry in CompactZSetCodec::iter_members(compact_buf) {
    let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, entry.member)?;
    let skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, entry.score, entry.member)?;
    session
      .store
      .bftree
      .insert(&skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
    session
      .store
      .bftree
      .insert(&mkey, &entry.score.to_be_bytes());
  }
  meta.set_encoding(StorageEncoding::Flattened);
  StoreSession::<D>::set_meta_chunk_info(&mut meta.reserved, 0, 0);
  Ok(())
}

impl<D: Device> ZSetCommands<D> for StoreSession<D> {
  /// 添加或更新有序集合成员 (ZADD)
  ///
  /// 入口获取目标键独占桶锁（两阶段锁）串行化同键读改写窗口，杜绝并发连接静默丢更新
  async fn zadd(
    &self,
    key: &[u8],
    score: f64,
    member: impl AsRef<[u8]>,
    options: ZAddOpt,
  ) -> Result<(usize, f64)> {
    options.validate(score)?;
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    zadd_unlocked(self, key, score, member, options).await
  }

  /// 批量添加或更新有序集合成员 (ZADD key score member [score member ...])
  ///
  /// 入口获取目标键独占桶锁（两阶段锁）串行化同键读改写窗口，杜绝并发连接静默丢更新
  async fn zmadd<M: AsRef<[u8]>>(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = (f64, M)>,
    options: ZAddOpt,
  ) -> Result<usize> {
    // 校验与收集合并为单遍，省去先 collect 再遍历校验的重复扫描；
    // size_hint 预分配消除批量写入的反复扩容
    let items = items.into_iter();
    let mut item_vec: Vec<(f64, M)> = Vec::with_capacity(items.size_hint().0);
    for (score, member) in items {
      options.validate(score)?;
      item_vec.push((score, member));
    }
    if item_vec.is_empty() {
      return Ok(0);
    }
    if options.incr && item_vec.len() != 1 {
      return Err(wedb_zset::Error::InvalidOpt.into());
    }
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    if options.incr {
      let (score, member) = &item_vec[0];
      return zadd_unlocked(self, key, *score, member.as_ref(), options)
        .await
        .map(|(count, _)| count);
    }
    zmadd_unlocked(self, key, item_vec, options).await
  }

  /// 获取成员分数 (ZSCORE)
  async fn zscore(&self, key: &[u8], member: &[u8]) -> Result<Option<f64>> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(None),
    };
    let meta = raw_read.meta;

    if meta.encoding() == StorageEncoding::Compact {
      let raw = raw_read.compact_payload().unwrap_or_default();
      return Ok(CompactZSetCodec::score_of(raw, member));
    }

    let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
    Ok(bftree_get_score(self, &mkey))
  }

  /// 批量获取成员分数 (ZMSCORE)
  async fn zmscore(&self, key: &[u8], members: &[&[u8]]) -> Result<Vec<Option<f64>>> {
    if members.is_empty() {
      return Ok(Vec::new());
    }
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(vec![None; members.len()]),
    };
    let meta = raw_read.meta;

    if meta.encoding() == StorageEncoding::Compact {
      let raw = raw_read.compact_payload().unwrap_or_default();
      let mut res = Vec::with_capacity(members.len());
      for &m in members {
        res.push(CompactZSetCodec::score_of(raw, m));
      }
      return Ok(res);
    }

    let mut res = Vec::with_capacity(members.len());
    for &member in members {
      let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
      res.push(bftree_get_score(self, &mkey));
    }
    Ok(res)
  }

  /// 基于已加载元数据获取成员排名 (ZRANK，从低到高 0-based)
  #[inline]
  fn zrank_with_meta(&self, meta: &MetaValue, member: &[u8]) -> Result<Option<usize>> {
    let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
    let score = match bftree_get_score(self, &mkey) {
      Some(s) => s,
      None => return Ok(None),
    };

    let skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, score, member)?;
    let prefix = zset_score_prefix(meta.key_id, meta.version);
    let mut rank = 0usize;
    self.store.bftree.scan_with_end_key_callback(
      &prefix,
      skey.as_slice(),
      wbftree::ScanReturnField::Key,
      |k, _| {
        if k < skey.as_slice() {
          rank += 1;
          true
        } else {
          false
        }
      },
    )?;
    Ok(Some(rank))
  }

  /// 获取成员排名 (ZRANK，从低到高 0-based)
  async fn zrank(&self, key: &[u8], member: &[u8]) -> Result<Option<usize>> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(None),
    };
    if raw_read.meta.encoding() == StorageEncoding::Compact {
      let raw = raw_read.compact_payload().unwrap_or_default();
      return Ok(CompactZSetCodec::rank_of(raw, member));
    }
    self.zrank_with_meta(&raw_read.meta, member)
  }

  /// 获取成员逆序排名 (ZREVRANK，从高到低 0-based)
  async fn zrevrank(&self, key: &[u8], member: &[u8]) -> Result<Option<usize>> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(None),
    };
    let size = raw_read.meta.size as usize;
    if raw_read.meta.encoding() == StorageEncoding::Compact {
      let raw = raw_read.compact_payload().unwrap_or_default();
      return Ok(
        CompactZSetCodec::rank_of(raw, member)
          .map(|rank| size.saturating_sub(1).saturating_sub(rank)),
      );
    }
    match self.zrank_with_meta(&raw_read.meta, member)? {
      Some(rank) => Ok(Some(size.saturating_sub(1).saturating_sub(rank))),
      None => Ok(None),
    }
  }

  /// 基于已加载元数据按排名范围获取元素（回调式零拷贝）
  ///
  /// `on_item(member, score)` 返回 false 提前终止扫描；成员切片仅在回调执行期内
  /// 有效（BfTree 扫描栈缓冲直出），调用方零 `to_vec` 即可消费
  fn zrange_with_meta_cb<F>(
    &self,
    meta: &MetaValue,
    start: isize,
    stop: isize,
    reverse: bool,
    mut on_item: F,
  ) -> Result<()>
  where
    F: FnMut(&[u8], f64) -> bool,
  {
    let Some((actual_start, count)) = normalize_zrange_range(meta.size as isize, start, stop)
    else {
      return Ok(());
    };

    let prefix = zset_score_prefix(meta.key_id, meta.version);
    // 右边界 const 定长数组栈上构造，彻底消除 ZSetSubKeyBuf 堆/搬运开销
    let mut end_buf = [0xffu8; 18];
    let mut end_len = 18usize;
    if let Some(next) = prefix_next_array(prefix) {
      end_buf[..prefix.len()].copy_from_slice(&next);
      end_len = prefix.len();
    }
    let prefix_end = &end_buf[..end_len];

    if !reverse {
      let mut current_idx = 0usize;
      let mut delivered = 0usize;
      self.store.bftree.scan_with_end_key_callback(
        &prefix,
        prefix_end,
        wbftree::ScanReturnField::Key,
        |k, _| {
          if current_idx >= actual_start
            && let Ok(sref) = ZSetSubKeyCodec::decode_score_key(k)
          {
            if !on_item(sref.member, sref.score) {
              return false;
            }
            delivered += 1;
            if delivered >= count {
              return false;
            }
          }
          current_idx += 1;
          true
        },
      )?;
      return Ok(());
    }

    // 逆序：BfTree 仅支持前向迭代，从逆序窗口起点正向收齐 count 项后倒序交付
    //（窗口恰为 [rev_start, rev_start + count)，成员物化成本与旧路径持平）
    let len = meta.size as usize;
    let rev_start = len - actual_start - count;
    let mut items: Vec<(Vec<u8>, f64)> = Vec::with_capacity(count);
    let mut current_idx = 0usize;
    self.store.bftree.scan_with_end_key_callback(
      &prefix,
      prefix_end,
      wbftree::ScanReturnField::Key,
      |k, _| {
        if current_idx >= rev_start
          && let Ok(sref) = ZSetSubKeyCodec::decode_score_key(k)
        {
          items.push((sref.member.to_vec(), sref.score));
          if items.len() >= count {
            return false;
          }
        }
        current_idx += 1;
        current_idx < rev_start + count
      },
    )?;
    for (member, score) in items.iter().rev() {
      if !on_item(member, *score) {
        break;
      }
    }
    Ok(())
  }

  /// 基于已加载元数据按排名范围获取元素
  fn zrange_with_meta(
    &self,
    meta: &MetaValue,
    start: isize,
    stop: isize,
    reverse: bool,
  ) -> Result<Vec<(Vec<u8>, f64)>> {
    let mut items = Vec::new();
    self.zrange_with_meta_cb(meta, start, stop, reverse, |member, score| {
      items.push((member.to_vec(), score));
      true
    })?;
    Ok(items)
  }

  /// 按排名范围获取元素（回调式零拷贝，ZRANGE 语义）
  ///
  /// `on_item(member, score)` 返回 false 提前终止；成员切片仅在回调执行期内有效。
  /// 紧凑编码正向路径全程零堆分配直接消费载荷切片
  async fn zrange_cb<F>(
    &self,
    key: &[u8],
    start: isize,
    stop: isize,
    reverse: bool,
    mut on_item: F,
  ) -> Result<()>
  where
    F: FnMut(&[u8], f64) -> bool,
  {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(()),
    };
    if raw_read.meta.encoding() != StorageEncoding::Compact {
      return self.zrange_with_meta_cb(&raw_read.meta, start, stop, reverse, on_item);
    }

    let Some((actual_start, count)) =
      normalize_zrange_range(raw_read.meta.size as isize, start, stop)
    else {
      return Ok(());
    };
    let raw = raw_read.compact_payload().unwrap_or_default();
    if !reverse {
      for e in CompactZSetCodec::iter_members(raw)
        .skip(actual_start)
        .take(count)
      {
        if !on_item(e.member, e.score) {
          break;
        }
      }
      return Ok(());
    }
    // 逆序：载荷切片随 raw_read 存活整个函数体，借用收集后倒序交付（零成员拷贝）
    let rev_skip = raw_read.meta.size as usize - actual_start - count;
    let borrowed: Vec<(&[u8], f64)> = CompactZSetCodec::iter_members(raw)
      .skip(rev_skip)
      .take(count)
      .map(|e| (e.member, e.score))
      .collect();
    for (member, score) in borrowed.iter().rev() {
      if !on_item(member, *score) {
        break;
      }
    }
    Ok(())
  }

  /// 按排名范围获取元素 (ZRANGE)
  async fn zrange(
    &self,
    key: &[u8],
    start: isize,
    stop: isize,
    reverse: bool,
  ) -> Result<Vec<(Vec<u8>, f64)>> {
    let mut items = Vec::new();
    self
      .zrange_cb(key, start, stop, reverse, |member, score| {
        items.push((member.to_vec(), score));
        true
      })
      .await?;
    Ok(items)
  }

  /// 基于已加载元数据按分数范围获取元素
  fn zrangebyscore_with_meta(
    &self,
    meta: &MetaValue,
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Result<Vec<(Vec<u8>, f64)>> {
    if meta.size == 0 || count == 0 {
      return Ok(Vec::new());
    }

    let (start_key, end_key) = zset_score_range_keys(meta.key_id, meta.version, range);
    if start_key.as_slice() >= end_key.as_slice() {
      return Ok(Vec::new());
    }

    if !reverse {
      let mut items = Vec::with_capacity(count.min(meta.size as usize));
      let mut skipped = 0usize;

      self.store.bftree.scan_with_end_key_callback(
        start_key.as_slice(),
        end_key.as_slice(),
        wbftree::ScanReturnField::Key,
        |k, _| {
          if k >= end_key.as_slice() {
            return false;
          }
          if let Ok(sref) = ZSetSubKeyCodec::decode_score_key(k) {
            // 升序扫描：当分数超过上限时立即提前短路终止整棵树扫描
            if (range.max_inclusive && sref.score > range.max)
              || (!range.max_inclusive && sref.score >= range.max)
            {
              return false;
            }

            let valid = if range.min_inclusive {
              sref.score >= range.min
            } else {
              sref.score > range.min
            };

            if valid {
              if skipped < offset {
                skipped += 1;
                return true;
              }
              items.push((sref.member.to_vec(), sref.score));
              if items.len() >= count {
                return false;
              }
            }
          }
          true
        },
      )?;
      Ok(items)
    } else {
      if offset >= meta.size as usize {
        return Ok(Vec::new());
      }
      let window_size = (meta.size as usize).min(offset.saturating_add(count));
      if window_size == 0 {
        return Ok(Vec::new());
      }
      let mut window: VecDeque<(Vec<u8>, f64)> = VecDeque::with_capacity(window_size.min(1024));

      self.store.bftree.scan_with_end_key_callback(
        start_key.as_slice(),
        end_key.as_slice(),
        wbftree::ScanReturnField::Key,
        |k, _| {
          if k >= end_key.as_slice() {
            return false;
          }
          if let Ok(sref) = ZSetSubKeyCodec::decode_score_key(k) {
            // 升序扫描：当分数超过上限时立即提前短路终止整棵树扫描
            if (range.max_inclusive && sref.score > range.max)
              || (!range.max_inclusive && sref.score >= range.max)
            {
              return false;
            }

            let valid = if range.min_inclusive {
              sref.score >= range.min
            } else {
              sref.score > range.min
            };
            if valid {
              if window.len() == window_size {
                if let Some((mut old_buf, _)) = window.pop_front() {
                  old_buf.clear();
                  old_buf.extend_from_slice(sref.member);
                  window.push_back((old_buf, sref.score));
                }
              } else {
                window.push_back((sref.member.to_vec(), sref.score));
              }
            }
          }
          true
        },
      )?;

      let available = window.len().saturating_sub(offset);
      let take_count = count.min(available);
      let start_idx = available.saturating_sub(take_count);
      let mut res = Vec::with_capacity(take_count);
      res.extend(window.drain(start_idx..available).rev());
      Ok(res)
    }
  }

  /// 按分数范围获取元素 (ZRANGEBYSCORE)
  async fn zrangebyscore(
    &self,
    key: &[u8],
    range: ScoreRange,
    reverse: bool,
    offset: usize,
    count: usize,
  ) -> Result<Vec<(Vec<u8>, f64)>> {
    if count == 0 {
      return Ok(Vec::new());
    }
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(Vec::new()),
    };
    if raw_read.meta.size == 0 {
      return Ok(Vec::new());
    }

    if raw_read.meta.encoding() == StorageEncoding::Compact {
      let raw = raw_read.compact_payload().unwrap_or_default();
      let iter = CompactZSetCodec::range_with_options(
        raw,
        range.min,
        range.min_inclusive,
        range.max,
        range.max_inclusive,
      );
      if !reverse {
        let mut items = Vec::with_capacity(count.min(raw_read.meta.size as usize));
        for e in iter.skip(offset).take(count) {
          items.push((e.member.to_vec(), e.score));
        }
        return Ok(items);
      } else {
        // 反向 + LIMIT：滑动窗口仅保留末尾 offset+count 条命中项，杜绝大区间全量物化
        let keep = offset.saturating_add(count);
        let mut tail: VecDeque<ZSetEntryRef<'_>> = VecDeque::with_capacity(keep.min(4096));
        for e in iter {
          if tail.len() == keep {
            tail.pop_front();
          }
          tail.push_back(e);
        }
        let mut items = Vec::with_capacity(count.min(tail.len()));
        for e in tail.into_iter().rev().skip(offset).take(count) {
          items.push((e.member.to_vec(), e.score));
        }
        return Ok(items);
      }
    }
    self.zrangebyscore_with_meta(&raw_read.meta, range, reverse, offset, count)
  }

  /// 获取元素总数 (ZCARD，O(1))
  async fn zcard(&self, key: &[u8]) -> Result<usize> {
    let meta = match self
      .load_collection_meta_read(key, CollectionType::ZSet)
      .await?
    {
      Some(m) => m,
      None => return Ok(0),
    };
    Ok(meta.size as usize)
  }

  /// 统计指定分数区间元素数量 (ZCOUNT)
  async fn zcount(&self, key: &[u8], range: ScoreRange) -> Result<usize> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(0),
    };
    let meta = raw_read.meta;
    if meta.size == 0 {
      return Ok(0);
    }

    if meta.encoding() == StorageEncoding::Compact {
      let raw = raw_read.compact_payload().unwrap_or_default();
      let count = CompactZSetCodec::count_score_range(
        raw,
        range.min,
        range.min_inclusive,
        range.max,
        range.max_inclusive,
      );
      return Ok(count);
    }

    let (start_key, end_key) = zset_score_range_keys(meta.key_id, meta.version, range);
    if start_key.as_slice() >= end_key.as_slice() {
      return Ok(0);
    }

    let mut count = 0usize;
    // 保序编码分值直接按大端字节序比较：min/max 仅编码一次，逐键零浮点解码零分配。
    // 端点先经 normalize_zero 归一（存储侧写入已统一 +0.0），杜绝查询端点为 -0.0 时
    // 字节序比较与浮点语义在 ±0 边界上的计数偏差
    let min_raw = u64::from_be_bytes(encode_order_preserving_f64(normalize_zero(range.min)));
    let max_raw = u64::from_be_bytes(encode_order_preserving_f64(normalize_zero(range.max)));
    self.store.bftree.scan_with_end_key_callback(
      start_key.as_slice(),
      end_key.as_slice(),
      wbftree::ScanReturnField::Key,
      |k, _| {
        if k >= end_key.as_slice() {
          return false;
        }
        // 分值键结构 [1B tag | 8B key_id | 8B version | 8B 保序分值 | member]，
        // 定长前缀保证 25 字节头部完整，try_into 恒成功
        let Some(score_bytes) = k.get(SCORE_KEY_HEADER_SIZE - 8..SCORE_KEY_HEADER_SIZE) else {
          return true;
        };
        let s = u64::from_be_bytes(score_bytes.try_into().unwrap());
        let valid = if range.min_inclusive {
          s >= min_raw
        } else {
          s > min_raw
        } && if range.max_inclusive {
          s <= max_raw
        } else {
          s < max_raw
        };
        if valid {
          count += 1;
        }
        true
      },
    )?;
    Ok(count)
  }

  /// 分数递增 (ZINCRBY，独占桶锁串行化同键读改写窗口)
  async fn zincrby(&self, key: &[u8], increment: f64, member: &[u8]) -> Result<f64> {
    if increment.is_nan() {
      return Err(wedb_zset::Error::InvalidScore.into());
    }
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    // -0.0 归一为 +0.0（normalize_zero 统一口径）
    let increment = normalize_zero(increment);
    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => {
        let key_id = self.store.next_key_id.fetch_add(1, Ordering::Relaxed);
        if member.len() <= ZSET_MAX_COMPACT_MEMBER && member.len() + 16 <= MAX_COMPACT_TOTAL_BYTES {
          let mut meta = MetaValue::new(key_id, CollectionType::ZSet, 1, 1);
          meta.set_encoding(StorageEncoding::Compact);
          // 预估容量：成员长 + 分值/长度前缀，避免插入时反复扩容
          let mut raw = Vec::with_capacity(member.len() + 16);
          CompactZSetCodec::insert(&mut raw, increment, member)?;
          self.save_compact_meta(key, &meta, &raw).await?;
          return Ok(increment);
        } else {
          let mut meta = MetaValue::new(key_id, CollectionType::ZSet, 1, 1);
          meta.set_encoding(StorageEncoding::Flattened);
          let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
          let skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, increment, member)?;
          self
            .store
            .bftree
            .insert(&skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
          self.store.bftree.insert(&mkey, &increment.to_be_bytes());
          self.save_meta(key, &meta).await?;
          return Ok(increment);
        }
      }
    };

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      let old_score_opt = CompactZSetCodec::score_of(&raw, member);
      let (old_score, is_new) = match old_score_opt {
        Some(s) => (s, false),
        None => (0.0, true),
      };
      let new_score = old_score + increment;
      if new_score.is_nan() {
        return Err(wedb_zset::Error::InvalidScore.into());
      }

      if is_new
        && (meta.size as usize >= ZSET_MAX_COMPACT_ENTRIES
          || member.len() > ZSET_MAX_COMPACT_MEMBER
          || (raw.len() + member.len() + 16 > MAX_COMPACT_TOTAL_BYTES))
      {
        promote_zset_to_flattened(self, &mut meta, &raw).await?;
        let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
        let skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, new_score, member)?;
        self
          .store
          .bftree
          .insert(&skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
        self.store.bftree.insert(&mkey, &new_score.to_be_bytes());
        meta.inc_size(1);
        self.save_meta(key, &meta).await?;
        return Ok(new_score);
      }

      CompactZSetCodec::insert(&mut raw, new_score, member)?;
      if is_new {
        meta.inc_size(1);
      }
      self.save_compact_meta(key, &meta, &raw).await?;
      return Ok(new_score);
    }

    let mkey = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member)?;
    let old_score_opt = bftree_get_score(self, &mkey);

    let (old_score, is_new) = match old_score_opt {
      Some(s) => (s, false),
      None => (0.0, true),
    };

    let new_score = old_score + increment;
    if new_score.is_nan() {
      return Err(wedb_zset::Error::InvalidScore.into());
    }

    if !is_new {
      let old_skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, old_score, member)?;
      self.store.bftree.delete(&old_skey);
    }

    // 先写数据后存元数据（与 zadd 崩溃一致性口径一致：中途崩溃至多 ZCARD 低估，绝不虚高）
    let new_skey = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, new_score, member)?;
    self
      .store
      .bftree
      .insert(&new_skey, ZSET_SCORE_ENTRY_PLACEHOLDER);
    self.store.bftree.insert(&mkey, &new_score.to_be_bytes());

    if is_new {
      meta.inc_size(1);
      self.save_meta(key, &meta).await?;
    }

    Ok(new_score)
  }

  /// 移除成员 (ZREM，独占桶锁串行化同键读改写窗口)
  async fn zrem(&self, key: &[u8], members: &[&[u8]]) -> Result<usize> {
    if members.is_empty() {
      return Ok(0);
    }
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    zrem_unlocked(self, key, members).await
  }

  /// 按排名范围删除成员 (ZREMRANGEBYRANK，独占桶锁串行化同键读改写窗口)
  async fn zremrangebyrank(&self, key: &[u8], start: isize, stop: isize) -> Result<usize> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(0),
    };
    if meta.size == 0 {
      return Ok(0);
    }

    let len = meta.size as isize;
    let actual_start = if start < 0 {
      len.saturating_add(start).max(0)
    } else {
      start
    };
    let actual_stop = if stop < 0 {
      len.saturating_add(stop)
    } else {
      stop
    };
    if actual_start >= len || actual_start > actual_stop {
      return Ok(0);
    }
    let actual_stop = actual_stop.min(len - 1);
    let count = (actual_stop - actual_start + 1) as usize;

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      let total_count = CompactZSetCodec::count(&raw).unwrap_or(0);
      if total_count == 0 {
        return Ok(0);
      }

      // meta.size 与载荷实际条目数可能漂移：以载荷实际条目数为上界收紧目标排名区间，
      // 保证目标区间恒落入真实条目范围（drain_start <= drain_end 恒成立，杜绝 drain panic）
      let target_start = (actual_start as usize).min(total_count - 1);
      let target_end = (actual_stop as usize).min(total_count - 1);

      let mut offset = 2;
      let mut drain_start = 2;
      let mut drain_end = 2;
      let mut removed = 0usize;

      for cur_rank in 0..total_count {
        if cur_rank == target_start {
          drain_start = offset;
        }
        let (entry_len, _) = CompactZSetCodec::parse_entry(&raw, offset)?;
        offset += entry_len;
        if cur_rank == target_end {
          drain_end = offset;
          // 按区间内实际命中的条目数计数（逐条 parse 确认存在），杜绝依赖虚高的预估算子
          removed = target_end - target_start + 1;
          break;
        }
      }

      if removed == 0 {
        return Ok(0);
      }

      raw.drain(drain_start..drain_end);
      let new_count = (total_count - removed) as u16;
      raw[0..2].copy_from_slice(&new_count.to_be_bytes());

      meta.dec_size(removed as u64);
      self.save_compact_meta(key, &meta, &raw).await?;
      return Ok(removed);
    }

    let mut remaining = count;
    let mut total_removed = 0usize;

    const BATCH_SIZE: usize = 512;
    while remaining > 0 {
      let batch_count = remaining.min(BATCH_SIZE);
      let batch_stop = actual_start
        .saturating_add(batch_count as isize)
        .saturating_sub(1);
      let batch = self.zrange_with_meta(&meta, actual_start, batch_stop, false)?;
      if batch.is_empty() {
        break;
      }
      let batch_len = batch.len();
      for (member, score) in &batch {
        if let Ok(mkey) = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member) {
          self.store.bftree.delete(&mkey);
        }
        if let Ok(skey) = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, *score, member) {
          self.store.bftree.delete(&skey);
        }
      }
      meta.dec_size(batch_len as u64);
      total_removed += batch_len;
      remaining = remaining.saturating_sub(batch_len);
      if batch_len < batch_count {
        break;
      }
    }

    if total_removed > 0 {
      if meta.size == 0 {
        // 崩溃一致性（先 meta 后数据）：先递增版本写 meta 判死（size==0 删元记录），
        // 后按旧版本清空 BfTree 双键；中途崩溃仅遗留读不可见的 BfTree 孤儿（磁盘泄漏可接受）
        let old_version = meta.version;
        meta.bump_version();
        self.save_meta(key, &meta).await?;
        self.clear_bftree_zset(meta.key_id, old_version)?;
      } else if meta.size <= AUTO_DEMOTE_MAX_SIZE {
        if !try_demote_zset(self, key, &mut meta).await? {
          self.save_meta(key, &meta).await?;
        }
      } else {
        self.save_meta(key, &meta).await?;
      }
    }
    Ok(total_removed)
  }

  /// 按分数范围删除成员 (ZREMRANGEBYSCORE，独占桶锁串行化同键读改写窗口)
  async fn zremrangebyscore(&self, key: &[u8], range: ScoreRange) -> Result<usize> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(0),
    };
    if meta.size == 0 {
      return Ok(0);
    }

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      let total_count = CompactZSetCodec::count(&raw).unwrap_or(0);
      if total_count == 0 {
        return Ok(0);
      }

      let mut offset = 2;
      let mut drain_start = None;
      let mut drain_end = 2;
      let mut removed_count = 0usize;

      for _ in 0..total_count {
        let entry_offset = offset;
        let (entry_len, entry) = CompactZSetCodec::parse_entry(&raw, offset)?;
        offset += entry_len;

        let valid = if range.min_inclusive {
          entry.score >= range.min
        } else {
          entry.score > range.min
        } && if range.max_inclusive {
          entry.score <= range.max
        } else {
          entry.score < range.max
        };

        if valid {
          if drain_start.is_none() {
            drain_start = Some(entry_offset);
          }
          drain_end = offset;
          removed_count += 1;
        } else if (range.max_inclusive && entry.score > range.max)
          || (!range.max_inclusive && entry.score >= range.max)
        {
          break;
        }
      }

      if let Some(start_pos) = drain_start {
        raw.drain(start_pos..drain_end);
        let new_count = total_count.saturating_sub(removed_count) as u16;
        raw[0..2].copy_from_slice(&new_count.to_be_bytes());

        meta.dec_size(removed_count as u64);
        self.save_compact_meta(key, &meta, &raw).await?;
      }

      return Ok(removed_count);
    }

    const BATCH_SIZE: usize = 512;
    let mut total_removed = 0usize;
    // 循环终止条件：只要集合中仍有存活元素且上批次已满，继续流式删除下一批
    while meta.size > 0 {
      let batch = self.zrangebyscore_with_meta(&meta, range, false, 0, BATCH_SIZE)?;
      if batch.is_empty() {
        break;
      }
      let batch_len = batch.len();
      for (member, score) in &batch {
        if let Ok(mkey) = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member) {
          self.store.bftree.delete(&mkey);
        }
        if let Ok(skey) = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, *score, member) {
          self.store.bftree.delete(&skey);
        }
      }
      meta.dec_size(batch_len as u64);
      total_removed += batch_len;
      if batch_len < BATCH_SIZE {
        break;
      }
    }

    if total_removed > 0 {
      if meta.size == 0 {
        // 崩溃一致性（先 meta 后数据）：先递增版本写 meta 判死（size==0 删元记录），
        // 后按旧版本清空 BfTree 双键；中途崩溃仅遗留读不可见的 BfTree 孤儿（磁盘泄漏可接受）
        let old_version = meta.version;
        meta.bump_version();
        self.save_meta(key, &meta).await?;
        self.clear_bftree_zset(meta.key_id, old_version)?;
      } else if meta.size <= AUTO_DEMOTE_MAX_SIZE {
        if !try_demote_zset(self, key, &mut meta).await? {
          self.save_meta(key, &meta).await?;
        }
      } else {
        self.save_meta(key, &meta).await?;
      }
    }
    Ok(total_removed)
  }

  /// 弹出分数最小的成员 (ZPOPMIN，独占桶锁串行化同键读改写窗口)
  async fn zpopmin(&self, key: &[u8], count: usize) -> Result<Vec<(Vec<u8>, f64)>> {
    if count == 0 {
      return Ok(Vec::new());
    }
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(Vec::new()),
    };
    if meta.size == 0 {
      return Ok(Vec::new());
    }

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      let total_count = CompactZSetCodec::count(&raw).unwrap_or(0);
      let pop_count = count.min(total_count);
      if pop_count == 0 {
        return Ok(Vec::new());
      }

      let mut candidates = Vec::with_capacity(pop_count);
      let mut offset = 2;
      for _ in 0..pop_count {
        let (entry_len, entry) = CompactZSetCodec::parse_entry(&raw, offset)?;
        candidates.push((entry.member.to_vec(), entry.score));
        offset += entry_len;
      }

      raw.drain(2..offset);
      let new_count = (total_count - pop_count) as u16;
      raw[0..2].copy_from_slice(&new_count.to_be_bytes());

      meta.dec_size(pop_count as u64);
      self.save_compact_meta(key, &meta, &raw).await?;
      return Ok(candidates);
    }

    let prefix = zset_score_prefix(meta.key_id, meta.version);
    let prefix_end = match prefix_next_array(prefix) {
      Some(end) => ZSetSubKeyBuf::from(&end[..]),
      None => ZSetSubKeyBuf::from(&[0xff; 18][..]),
    };

    let mut candidates = Vec::with_capacity(count.min(meta.size as usize));
    self.store.bftree.scan_with_end_key_callback(
      &prefix,
      prefix_end.as_slice(),
      wbftree::ScanReturnField::Key,
      |k, _| {
        if let Ok(sref) = ZSetSubKeyCodec::decode_score_key(k) {
          candidates.push((sref.member.to_vec(), sref.score));
          if candidates.len() >= count {
            return false;
          }
        }
        true
      },
    )?;

    if candidates.is_empty() {
      return Ok(Vec::new());
    }

    for (member, score) in &candidates {
      if let Ok(mkey) = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member) {
        self.store.bftree.delete(&mkey);
      }
      if let Ok(skey) = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, *score, member) {
        self.store.bftree.delete(&skey);
      }
    }

    meta.dec_size(candidates.len() as u64);
    if meta.size == 0 {
      // 崩溃一致性（先 meta 后数据）：先递增版本写 meta 判死（size==0 删元记录），
      // 后按旧版本清空 BfTree 双键；中途崩溃仅遗留读不可见的 BfTree 孤儿（磁盘泄漏可接受）
      let old_version = meta.version;
      meta.bump_version();
      self.save_meta(key, &meta).await?;
      self.clear_bftree_zset(meta.key_id, old_version)?;
    } else if meta.size <= AUTO_DEMOTE_MAX_SIZE {
      if !try_demote_zset(self, key, &mut meta).await? {
        self.save_meta(key, &meta).await?;
      }
    } else {
      self.save_meta(key, &meta).await?;
    }

    Ok(candidates)
  }

  /// 弹出分数最大的成员 (ZPOPMAX，独占桶锁串行化同键读改写窗口)
  async fn zpopmax(&self, key: &[u8], count: usize) -> Result<Vec<(Vec<u8>, f64)>> {
    if count == 0 {
      return Ok(Vec::new());
    }
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let (mut meta, raw_opt) = match self
      .load_collection_raw_write(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(Vec::new()),
    };
    if meta.size == 0 {
      return Ok(Vec::new());
    }

    if meta.encoding() == StorageEncoding::Compact {
      let mut raw = raw_opt.unwrap_or_default();
      let total = CompactZSetCodec::count(&raw).unwrap_or(0);
      let pop_count = count.min(total);
      if pop_count == 0 {
        return Ok(Vec::new());
      }
      let skip_count = total - pop_count;

      let mut offset = 2;
      for _ in 0..skip_count {
        let (entry_len, _) = CompactZSetCodec::parse_entry(&raw, offset)?;
        offset += entry_len;
      }

      let truncate_offset = offset;
      let mut candidates = Vec::with_capacity(pop_count);
      for _ in 0..pop_count {
        let (entry_len, entry) = CompactZSetCodec::parse_entry(&raw, offset)?;
        candidates.push((entry.member.to_vec(), entry.score));
        offset += entry_len;
      }
      // Redis ZPOPMAX 返回按分数降序排列
      candidates.reverse();

      raw.truncate(truncate_offset);
      let new_count = skip_count as u16;
      raw[0..2].copy_from_slice(&new_count.to_be_bytes());

      meta.dec_size(pop_count as u64);
      self.save_compact_meta(key, &meta, &raw).await?;
      return Ok(candidates);
    }

    let prefix = zset_score_prefix(meta.key_id, meta.version);
    let prefix_end = match prefix_next_array(prefix) {
      Some(end) => ZSetSubKeyBuf::from(&end[..]),
      None => ZSetSubKeyBuf::from(&[0xff; 18][..]),
    };

    let skip = (meta.size as usize).saturating_sub(count);
    let mut skipped = 0usize;
    // 容量以扫描剩余条目数为上界（min(count, size)），防御极端 count 值触发容量溢出中断
    let mut deque: VecDeque<(Vec<u8>, f64)> =
      VecDeque::with_capacity(count.min(meta.size as usize));

    self.store.bftree.scan_with_end_key_callback(
      &prefix,
      prefix_end.as_slice(),
      wbftree::ScanReturnField::Key,
      |k, _| {
        if skipped < skip {
          skipped += 1;
          return true;
        }
        if let Ok(sref) = ZSetSubKeyCodec::decode_score_key(k) {
          if deque.len() == count {
            deque.pop_front();
          }
          deque.push_back((sref.member.to_vec(), sref.score));
        }
        true
      },
    )?;

    if deque.is_empty() {
      return Ok(Vec::new());
    }

    let candidates: Vec<(Vec<u8>, f64)> = deque.into_iter().rev().collect();

    for (member, score) in &candidates {
      if let Ok(mkey) = ZSetSubKeyBuf::from_member(meta.key_id, meta.version, member) {
        self.store.bftree.delete(&mkey);
      }
      if let Ok(skey) = ZSetSubKeyBuf::from_score(meta.key_id, meta.version, *score, member) {
        self.store.bftree.delete(&skey);
      }
    }

    meta.dec_size(candidates.len() as u64);
    if meta.size == 0 {
      // 崩溃一致性（先 meta 后数据）：先递增版本写 meta 判死（size==0 删元记录），
      // 后按旧版本清空 BfTree 双键；中途崩溃仅遗留读不可见的 BfTree 孤儿（磁盘泄漏可接受）
      let old_version = meta.version;
      meta.bump_version();
      self.save_meta(key, &meta).await?;
      self.clear_bftree_zset(meta.key_id, old_version)?;
    } else if meta.size <= AUTO_DEMOTE_MAX_SIZE {
      if !try_demote_zset(self, key, &mut meta).await? {
        self.save_meta(key, &meta).await?;
      }
    } else {
      self.save_meta(key, &meta).await?;
    }

    Ok(candidates)
  }

  /// 游标扫描有序集合 (ZSCAN)
  async fn zscan(
    &self,
    key: &[u8],
    cursor: usize,
    count: usize,
    pattern: Option<&[u8]>,
  ) -> Result<(usize, Vec<(Vec<u8>, f64)>)> {
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok((0, Vec::new())),
    };
    let size = raw_read.meta.size as usize;
    if size == 0 || cursor >= size {
      return Ok((0, Vec::new()));
    }

    if raw_read.meta.encoding() == StorageEncoding::Compact {
      let raw = raw_read.compact_payload().unwrap_or_default();
      let batch_count = count.max(10);
      let mut filtered = Vec::with_capacity(batch_count.min(SCAN_RESERVE_CAP));
      let mut scanned = 0;

      for entry in CompactZSetCodec::iter_members(raw).skip(cursor) {
        if pattern.is_none_or(|pat| glob_match(pat, entry.member)) {
          filtered.push((entry.member.to_vec(), entry.score));
        }
        scanned += 1;
        if scanned >= batch_count {
          break;
        }
      }

      let next_cursor = if cursor + scanned >= size {
        0
      } else {
        cursor + scanned
      };

      return Ok((next_cursor, filtered));
    }

    let prefix = zset_score_prefix(raw_read.meta.key_id, raw_read.meta.version);
    let prefix_end = match prefix_next_array(prefix) {
      Some(end) => ZSetSubKeyBuf::from(&end[..]),
      None => ZSetSubKeyBuf::from(&[0xff; 18][..]),
    };

    let batch_count = count.max(10);
    let mut filtered = Vec::with_capacity(batch_count.min(SCAN_RESERVE_CAP));
    let mut scanned = 0;
    let mut skipped = 0;

    self.store.bftree.scan_with_end_key_callback(
      &prefix,
      prefix_end.as_slice(),
      wbftree::ScanReturnField::Key,
      |k, _| {
        if skipped < cursor {
          skipped += 1;
          return true;
        }
        if let Ok(sref) = ZSetSubKeyCodec::decode_score_key(k) {
          if pattern.is_none_or(|pat| glob_match(pat, sref.member)) {
            filtered.push((sref.member.to_vec(), sref.score));
          }
          scanned += 1;
          if scanned >= batch_count {
            return false;
          }
        }
        true
      },
    )?;

    let next_cursor = if cursor + scanned >= size {
      0
    } else {
      cursor + scanned
    };

    Ok((next_cursor, filtered))
  }

  /// 随机获取有序集合中的成员 (ZRANDMEMBER)
  async fn zrandmember(&self, key: &[u8], count: isize) -> Result<Vec<(Vec<u8>, f64)>> {
    if count == 0 {
      return Ok(Vec::new());
    }
    let raw_read = match self
      .load_collection_raw_read(key, CollectionType::ZSet)
      .await?
    {
      Some(res) => res,
      None => return Ok(Vec::new()),
    };
    let meta = raw_read.meta;
    if meta.size == 0 {
      return Ok(Vec::new());
    }

    if meta.encoding() == StorageEncoding::Compact {
      if let Some(raw) = raw_read.compact_payload() {
        if count == 1 || count == -1 {
          let rand_idx = fastrand::usize(0..meta.size as usize);
          if let Some(e) = CompactZSetCodec::entry_at_rank(raw, rand_idx) {
            return Ok(vec![(e.member.to_vec(), e.score)]);
          }
          return Ok(Vec::new());
        }
        if count > 0 {
          // 蓄水池单遍抽样：O(N) 时间 O(k) 空间，杜绝全量物化的 N 次 member 堆分配
          let k = (count as usize).min(meta.size as usize);
          let mut reservoir: Vec<(Vec<u8>, f64)> = Vec::with_capacity(k);
          for (i, e) in CompactZSetCodec::iter_members(raw).enumerate() {
            if i < k {
              reservoir.push((e.member.to_vec(), e.score));
            } else {
              let j = fastrand::usize(0..=i);
              if j < k {
                reservoir[j] = (e.member.to_vec(), e.score);
              }
            }
          }
          return Ok(reservoir);
        }
        // 负数 count 有放回采样：k 次随机 rank 直查，O(1) 额外内存
        let pick_count = count.unsigned_abs().min(SRANDMEMBER_MAX_SAMPLE);
        let mut result = Vec::with_capacity(pick_count.min(RAND_SAMPLE_RESERVE_CAP));
        for _ in 0..pick_count {
          if let Some(e) =
            CompactZSetCodec::entry_at_rank(raw, fastrand::usize(0..meta.size as usize))
          {
            result.push((e.member.to_vec(), e.score));
          }
        }
        return Ok(result);
      }
      return Ok(Vec::new());
    }

    // Flattened 模式（BfTree）：count.abs() == 1 单点随机 rank 检索（O(log N)，零千万级物化）
    if count == 1 || count == -1 {
      let rand_idx = fastrand::usize(0..meta.size as usize) as isize;
      return self.zrange_with_meta(&meta, rand_idx, rand_idx, false);
    }

    // 负数 count 小额采样：直接 rank 有放回随机采样，杜绝千万级全表物化 OOM
    if count < 0 && count.unsigned_abs() < (meta.size as usize / 4).min(64) {
      let pick_count = count.unsigned_abs().min(SRANDMEMBER_MAX_SAMPLE);
      let mut result = Vec::with_capacity(pick_count.min(RAND_SAMPLE_RESERVE_CAP));
      for _ in 0..pick_count {
        let rand_idx = fastrand::usize(0..meta.size as usize) as isize;
        let mut items = self.zrange_with_meta(&meta, rand_idx, rand_idx, false)?;
        if let Some(item) = items.pop() {
          result.push(item);
        }
      }
      return Ok(result);
    }

    // 多元素采样：对于小 count，直接按 rank 采样，无需物化全表
    if count > 0 && (count as usize) < (meta.size as usize / 4).min(64) {
      let k = (count as usize).min(meta.size as usize);
      let mut picked_indices = whasher::hash_set_with_capacity(k);
      while picked_indices.len() < k {
        picked_indices.insert(fastrand::usize(0..meta.size as usize) as isize);
      }
      let mut result = Vec::with_capacity(k);
      for idx in picked_indices {
        let mut items = self.zrange_with_meta(&meta, idx, idx, false)?;
        if let Some(item) = items.pop() {
          result.push(item);
        }
      }
      return Ok(result);
    }

    // 回退到 zrange_with_meta 获取全量（仅当请求数量接近全集时）
    let all = self.zrange_with_meta(&meta, 0, -1, false)?;
    pick_random_zset_from_slice(all, count)
  }

  /// 多有序集合并集 (ZUNION)
  async fn zunion(
    &self,
    keys: &[&[u8]],
    weights: &[f64],
    agg: AggregateType,
  ) -> Result<Vec<(Vec<u8>, f64)>> {
    if keys.is_empty() {
      return Ok(Vec::new());
    }
    let mut map = new_hash_map::<Vec<u8>, f64>();
    for (i, &k) in keys.iter().enumerate() {
      let weight = weights.get(i).copied().unwrap_or(1.0);
      let items = self.zrange(k, 0, -1, false).await?;
      for (member, score) in items {
        let w_score = score * weight;
        map
          .entry(member)
          .and_modify(|s| match agg {
            AggregateType::Sum => *s += w_score,
            AggregateType::Min => *s = s.min(w_score),
            AggregateType::Max => *s = s.max(w_score),
          })
          .or_insert(w_score);
      }
    }

    let mut result: Vec<(Vec<u8>, f64)> = map.into_iter().collect();
    result.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    Ok(result)
  }

  /// 多有序集合并集并存储 (ZUNIONSTORE)
  ///
  /// dest 与全部源键排序去重后整体获取独占桶锁（锁引擎内部再按桶下标全序加锁防死锁），
  /// 聚合计算、dest 清空与结果写入全程原子，杜绝 delete 与写入两步间被并发修改撕裂
  async fn zunionstore(
    &self,
    dest: &[u8],
    keys: &[&[u8]],
    weights: &[f64],
    agg: AggregateType,
  ) -> Result<usize> {
    let _key_lock = lock_keys_sorted(self, dest, keys)?;
    let result = self.zunion(keys, weights, agg).await?;
    zstore_apply_unlocked(self, dest, result).await
  }

  /// 多有序集合交集 (ZINTER)
  async fn zinter(
    &self,
    keys: &[&[u8]],
    weights: &[f64],
    agg: AggregateType,
  ) -> Result<Vec<(Vec<u8>, f64)>> {
    if keys.is_empty() {
      return Ok(Vec::new());
    }
    let first_weight = weights.first().copied().unwrap_or(1.0);
    let first_items = self.zrange(keys[0], 0, -1, false).await?;
    if first_items.is_empty() {
      return Ok(Vec::new());
    }

    let mut map = new_hash_map::<Vec<u8>, f64>();
    for (m, s) in first_items {
      map.insert(m, s * first_weight);
    }

    for (i, &k) in keys[1..].iter().enumerate() {
      let weight = weights.get(i + 1).copied().unwrap_or(1.0);
      let items = self.zrange(k, 0, -1, false).await?;
      let current_map: HashMap<Vec<u8>, f64> = items.into_iter().collect();

      map.retain(|m, s| {
        if let Some(&other_score) = current_map.get(m) {
          let w_score = other_score * weight;
          match agg {
            AggregateType::Sum => *s += w_score,
            AggregateType::Min => *s = s.min(w_score),
            AggregateType::Max => *s = s.max(w_score),
          }
          true
        } else {
          false
        }
      });

      if map.is_empty() {
        break;
      }
    }

    let mut result: Vec<(Vec<u8>, f64)> = map.into_iter().collect();
    result.sort_by(|a, b| a.1.total_cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
    Ok(result)
  }

  /// 多有序集合交集并存储 (ZINTERSTORE，dest + 源键整体独占桶锁保护，口径同 ZUNIONSTORE)
  async fn zinterstore(
    &self,
    dest: &[u8],
    keys: &[&[u8]],
    weights: &[f64],
    agg: AggregateType,
  ) -> Result<usize> {
    let _key_lock = lock_keys_sorted(self, dest, keys)?;
    let result = self.zinter(keys, weights, agg).await?;
    zstore_apply_unlocked(self, dest, result).await
  }

  /// 多有序集合交集基数 (ZINTERCARD)
  ///
  /// 以基数最小的集合为驱动集逐成员探针，命中数达 LIMIT 立即返回，
  /// 避免物化完整交集（严格对标 Redis/Garnet LIMIT 语义）
  async fn zintercard(&self, keys: &[&[u8]], limit: usize) -> Result<usize> {
    if keys.is_empty() {
      return Ok(0);
    }

    // 选取基数最小的集合作为驱动集，最小化成员探针总次数
    let mut driver_idx = 0;
    let mut driver_card = self.zcard(keys[0]).await?;
    for (i, &k) in keys.iter().enumerate().skip(1) {
      let card = self.zcard(k).await?;
      if card < driver_card {
        driver_card = card;
        driver_idx = i;
      }
    }
    if driver_card == 0 {
      return Ok(0);
    }

    // 预加载非驱动键元数据与紧凑载荷，任一键为空集则交集必为空
    let mut reads: Vec<Option<RawCollectionRead>> = Vec::with_capacity(keys.len());
    for (i, &k) in keys.iter().enumerate() {
      reads.push(if i == driver_idx {
        None
      } else {
        match self
          .load_collection_raw_read(k, CollectionType::ZSet)
          .await?
        {
          Some(rr) => Some(rr),
          None => return Ok(0),
        }
      });
    }

    // 物化探针：Compact 一次读出建成员哈希集（零拷贝借用切片），Flattened 复用 key_id/version
    let mut probes: Vec<ZSetProbe<'_>> = Vec::with_capacity(reads.len() - 1);
    for (i, read) in reads.iter().enumerate() {
      if i != driver_idx
        && let Some(rr) = read
      {
        probes.push(match rr.compact_payload() {
          Some(raw) => ZSetProbe::Compact(
            CompactZSetCodec::iter_members(raw)
              .map(|e| e.member)
              .collect(),
          ),
          None => ZSetProbe::Flattened {
            key_id: rr.meta.key_id,
            version: rr.meta.version,
          },
        });
      }
    }

    let mut count = 0usize;
    for (m, _) in self.zrange(keys[driver_idx], 0, -1, false).await? {
      let mut in_all = true;
      for probe in &probes {
        if !probe.contains(self, &m)? {
          in_all = false;
          break;
        }
      }
      if in_all {
        count += 1;
        if limit > 0 && count >= limit {
          return Ok(limit);
        }
      }
    }
    Ok(count)
  }

  /// 多有序集合求差集 (ZDIFF)
  ///
  /// 保序论证：zrange(0,-1,false) 输出即 (score 升序, member 升序) 的 BfTree/紧凑全序，
  /// 与旧实现的末尾 sort_by 比较器完全一致；因此对 first 原位保留过滤即天然保序，
  /// 无需 O(n log n) 重排（gxhash HashMap 为无序桶结构，不能依赖其遍历序）
  async fn zdiff(&self, keys: &[&[u8]]) -> Result<Vec<(Vec<u8>, f64)>> {
    if keys.is_empty() {
      return Ok(Vec::new());
    }
    let mut first = self.zrange(keys[0], 0, -1, false).await?;
    if first.is_empty() || keys.len() == 1 {
      return Ok(first);
    }

    // 逐键物化待剔除成员集并对 first 原位过滤（峰值内存 O(max|other|)，first 随过滤单调收缩）
    for &k in &keys[1..] {
      let items = self.zrange(k, 0, -1, false).await?;
      if items.is_empty() {
        continue;
      }
      let removed: HashSet<&[u8]> = items.iter().map(|(m, _)| m.as_slice()).collect();
      first.retain(|(m, _)| !removed.contains(m.as_slice()));
      if first.is_empty() {
        break;
      }
    }

    Ok(first)
  }

  /// 多有序集合求差集并存储 (ZDIFFSTORE，dest + 源键整体独占桶锁保护，口径同 ZUNIONSTORE)
  async fn zdiffstore(&self, dest: &[u8], keys: &[&[u8]]) -> Result<usize> {
    let _key_lock = lock_keys_sorted(self, dest, keys)?;
    let result = self.zdiff(keys).await?;
    zstore_apply_unlocked(self, dest, result).await
  }

  /// 字典序范围查询成员 (ZRANGEBYLEX)
  async fn zrangebylex(
    &self,
    key: &[u8],
    min: LexBound<'_>,
    max: LexBound<'_>,
    offset: usize,
    count: usize,
    reverse: bool,
  ) -> Result<Vec<Vec<u8>>> {
    if count == 0 {
      return Ok(Vec::new());
    }
    let all = self.zrange(key, 0, -1, reverse).await?;
    let filtered = all
      .into_iter()
      .filter(|(m, _)| min.matches_min(m) && max.matches_max(m))
      .skip(offset)
      .take(count)
      .map(|(m, _)| m)
      .collect();
    Ok(filtered)
  }

  /// 字典序范围元素计数 (ZLEXCOUNT)
  async fn zlexcount(&self, key: &[u8], min: LexBound<'_>, max: LexBound<'_>) -> Result<usize> {
    let all = self.zrange(key, 0, -1, false).await?;
    let count = all
      .into_iter()
      .filter(|(m, _)| min.matches_min(m) && max.matches_max(m))
      .count();
    Ok(count)
  }

  /// 字典序范围删除成员 (ZREMRANGEBYLEX，独占桶锁串行化同键读改写窗口)
  async fn zremrangebylex(
    &self,
    key: &[u8],
    min: LexBound<'_>,
    max: LexBound<'_>,
  ) -> Result<usize> {
    let _key_lock = self.store.index.acquire_keys_lock_exclusive(&[key])?;
    let all = self.zrange(key, 0, -1, false).await?;
    let to_remove: Vec<Vec<u8>> = all
      .into_iter()
      .filter(|(m, _)| min.matches_min(m) && max.matches_max(m))
      .map(|(m, _)| m)
      .collect();
    if to_remove.is_empty() {
      return Ok(0);
    }
    let refs: Vec<&[u8]> = to_remove.iter().map(|m| m.as_slice()).collect();
    // 已持本键独占锁，直接复用无锁内核避免桶锁重入自锁
    zrem_unlocked(self, key, &refs).await
  }
}
