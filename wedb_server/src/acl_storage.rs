use std::{
  io,
  sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
  },
};

use log::{debug, error, warn};
use wbftree::{
  BfTreeDeleteResult, BfTreeInsertResult, BfTreeReadResult, BfTreeService, ScanReturnField,
};
use wedb_acl::{AclStorage, BoxFuture, Result, User, decode_user_key};
use wval::BfTag;

/// 基于 BfTree 块级有序存储的 ACL 数据库持久化实现
///
/// 具备 10M+ 用户千万级存储与高并发检索能力：
/// 1. 单用户独立 bitcode 紧凑二进制点查/点写/点删 (O(log N) 索引, 零全量物化)
/// 2. 分页流式扫描 (list_users) 基于 BfTree 范围扫描，零 OOM 风险
/// 3. 用户计数原子缓存与元数据同步
///
/// 键约定：各方法入参为 [`wedb_acl::user_key`](wedb_acl::user_key) 生成的
/// 「名字空间 × 用户名」二进制安全复合主键，本实现再拼 `BfTag::AclUser` 表前缀落地
pub struct StoreAclStorage {
  bftree: Arc<BfTreeService>,
  cached_count: AtomicUsize,
}

impl StoreAclStorage {
  /// 创建新的数据库持久化 ACL 存储适配器
  pub fn new(bftree: Arc<BfTreeService>) -> Self {
    let mut initial_count = 0;
    let mut buf = [0u8; 8];
    let (res, len) = bftree.read_into(&BfTag::AclMeta.prefix(), &mut buf);
    if res == BfTreeReadResult::Found && len == 8 {
      initial_count = u64::from_be_bytes(buf) as usize;
    }
    Self {
      bftree,
      cached_count: AtomicUsize::new(initial_count),
    }
  }

  #[inline]
  fn with_row_key<R>(user_key: &[u8], f: impl FnOnce(&[u8]) -> R) -> R {
    BfTag::AclUser.with_key(user_key, f)
  }

  #[inline]
  fn sync_count_to_db(&self, count: usize) {
    let bytes = (count as u64).to_be_bytes();
    let res = self.bftree.insert(&BfTag::AclMeta.prefix(), &bytes);
    if res != BfTreeInsertResult::Success {
      warn!(target: "wedb::storage::acl", "持久化 ACL 用户计数元数据失败: res={res:?}");
    }
  }
}

/// 分页扫描单次最大预分配用户数（防御恶意超大 limit 导致内存膨胀）
const MAX_SCAN_PREALLOC_CAP: usize = 1024;

impl AclStorage for StoreAclStorage {
  fn get_user<'a>(&'a self, key: &'a [u8]) -> BoxFuture<'a, Result<Option<User>>> {
    Box::pin(async move {
      Self::with_row_key(key, |k| match self.bftree.read(k) {
        (BfTreeReadResult::Found, Some(bytes)) => Ok(Some(User::from_bitcode(&bytes)?)),
        _ => Ok(None),
      })
    })
  }

  fn put_user<'a>(&'a self, key: &'a [u8], user: &'a User) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
      let bytes = user.to_bitcode();
      Self::with_row_key(key, |k| {
        let (res, _) = self.bftree.read(k);
        let is_new = res != BfTreeReadResult::Found;

        let insert_res = self.bftree.insert(k, &bytes);
        if insert_res != BfTreeInsertResult::Success {
          error!(target: "wedb::storage::acl", "写入持久化 ACL 用户失败: user='{}', err={insert_res:?}", user.name);
          return Err(wedb_acl::Error::Io(io::Error::other(format!(
            "BfTree 写入 ACL 用户失败: {insert_res:?}"
          ))));
        }

        debug!(target: "wedb::storage::acl", "持久化 ACL 用户成功: user='{}', is_new={is_new}", user.name);

        if is_new {
          let new_count = self.cached_count.fetch_add(1, Ordering::Relaxed) + 1;
          self.sync_count_to_db(new_count);
        }
        Ok(())
      })
    })
  }

  fn del_user<'a>(&'a self, key: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
    Box::pin(async move {
      // 日志还原用户名片段（仅诊断用，二进制键不参与存储逻辑）
      let name = decode_user_key(key).map(|(_, n)| n).unwrap_or("<binary>");
      Self::with_row_key(key, |k| {
        let (res, _) = self.bftree.read(k);
        if res == BfTreeReadResult::Found {
          let del_res = self.bftree.delete(k);
          if del_res != BfTreeDeleteResult::Success {
            error!(target: "wedb::storage::acl", "从持久化存储删除 ACL 用户失败: user='{name}', err={del_res:?}");
            return Err(wedb_acl::Error::Io(io::Error::other(format!(
              "BfTree 删除 ACL 用户失败: {del_res:?}"
            ))));
          }
          debug!(target: "wedb::storage::acl", "删除持久化 ACL 用户成功: user='{name}'");
          let new_count = self
            .cached_count
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |c| {
              Some(c.saturating_sub(1))
            })
            .map(|old| old.saturating_sub(1))
            .unwrap_or(0);
          self.sync_count_to_db(new_count);
          Ok(true)
        } else {
          debug!(target: "wedb::storage::acl", "待删除 ACL 用户在持久化存储中不存在: user='{name}'");
          Ok(false)
        }
      })
    })
  }

  fn list_users_after<'a>(
    &'a self,
    cursor: Option<&'a [u8]>,
    limit: usize,
  ) -> BoxFuture<'a, Result<Vec<Vec<u8>>>> {
    Box::pin(async move {
      if limit == 0 {
        return Ok(Vec::new());
      }
      let mut keys = Vec::with_capacity(limit.min(MAX_SCAN_PREALLOC_CAP));
      let prefix = BfTag::AclUser.prefix();
      let mut start_buf = Vec::new();
      let start_key: &[u8] = match cursor {
        Some(c) => {
          start_buf.reserve_exact(prefix.len() + c.len());
          start_buf.extend_from_slice(&prefix);
          start_buf.extend_from_slice(c);
          &start_buf
        }
        None => &prefix,
      };

      let _ = self.bftree.scan_with_count_callback(
        start_key,
        limit.saturating_add(1),
        ScanReturnField::Key,
        |k, _| {
          let Some(user_key) = BfTag::AclUser.strip_prefix(k) else {
            return false;
          };
          if let Some(c) = cursor
            && user_key == c
          {
            return true;
          }
          keys.push(user_key.to_vec());
          keys.len() < limit
        },
      );

      Ok(keys)
    })
  }

  fn user_count<'a>(&'a self) -> BoxFuture<'a, Result<usize>> {
    Box::pin(async move { Ok(self.cached_count.load(Ordering::Relaxed)) })
  }
}
