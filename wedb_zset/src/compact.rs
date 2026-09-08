//! 紧凑有序集合直接复用并导出 wrecord 的实现
pub use wrecord::{CompactZSet, CompactZSetCodec, CompactZSetIter, ZSetEntryRef};

use crate::{
  error::{Error, Result},
  zset::{SortedSetObject, ZAddOpt},
};

/// 紧凑条目引用类型别名
pub type CompactEntryRef<'a> = ZSetEntryRef<'a>;

/// 为 CompactZSet 扩展与 SortedSetObject 互转及带有 ZAddOpt 的写入方法
pub trait CompactZSetExt {
  /// 插入或更新成员 (兼容 ZAddOpt 语义)
  fn zadd(&mut self, score: f64, member: &[u8], options: ZAddOpt) -> Result<(usize, f64)>;
  /// 转换为标准跳表版 SortedSetObject
  fn to_sorted_set(&self) -> Result<SortedSetObject>;
  /// 从标准跳表版 SortedSetObject 转换
  fn from_sorted_set(zset: &SortedSetObject) -> Result<CompactZSet>;
}

impl CompactZSetExt for CompactZSet {
  fn zadd(&mut self, score: f64, member: &[u8], options: ZAddOpt) -> Result<(usize, f64)> {
    // 与 SortedSetObject::zadd 共用同一校验入口：NaN 分数与选项互斥组合
    options.validate(score)?;
    // 紧凑编码长度字段为 u16，超长成员直接拒绝
    if member.len() > u16::MAX as usize {
      return Err(Error::InvalidOpt);
    }

    let old_score = self.score_of(member);
    if let Some(old_s) = old_score {
      let new_score = if options.incr {
        let s = old_s + score;
        if s.is_nan() {
          return Err(Error::InvalidScore);
        }
        s
      } else {
        score
      };

      if options.nx {
        return Ok((0, old_s));
      }
      if options.gt && new_score <= old_s {
        return Ok((0, old_s));
      }
      if options.lt && new_score >= old_s {
        return Ok((0, old_s));
      }

      self.insert(new_score, member)?;
      // 与 SortedSetObject::zadd 返回值语义一致：CH 计数变更，INCR 变更同样计 1
      let changed = if (options.ch || options.incr) && new_score != old_s {
        1
      } else {
        0
      };
      Ok((changed, new_score))
    } else {
      // 未命中契约与 SortedSetObject::zadd 一致：XX 返回 (0, 增量)，上层写 nil
      if options.xx {
        return Ok((0, score));
      }
      self.insert(score, member)?;
      Ok((1, score))
    }
  }

  fn to_sorted_set(&self) -> Result<SortedSetObject> {
    let mut zset = SortedSetObject::with_capacity(self.len());
    for entry in self.iter_members() {
      zset.zadd(entry.score, entry.member.to_vec(), Default::default())?;
    }
    Ok(zset)
  }

  fn from_sorted_set(zset: &SortedSetObject) -> Result<CompactZSet> {
    let mut compact = CompactZSet::with_capacity(zset.len_ref());
    for (m, s) in zset.zrange_ref(0, -1, false) {
      compact.insert(s, &m)?;
    }
    Ok(compact)
  }
}
