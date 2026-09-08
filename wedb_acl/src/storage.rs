use std::{future::Future, pin::Pin};

use whasher::{GxPapayaMap, new_papaya_map};

use crate::{error::Result, user::User};

/// 异步堆分配 Future 别名，用于保障 AclStorage 具备 Dyn 兼容性（对象安全，支持 Arc<dyn AclStorage>）
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// ACL 数据库持久化存储接口
///
/// 对标 kvrocks engine::Storage 设计，支持千万级（10M+）高并发用户的单用户独立持久化。
/// 底层存储使用 bitcode 紧凑二进制格式，实现单用户 O(1) 读写与按需懒加载。
///
/// 键约定：所有方法接收 [`user_key`](crate::ns::user_key) 生成的
/// 「名字空间 × 用户名」复合主键字节（与数据键同源的 OPPV 前缀编码），
/// 实现方只需再拼接自己的表前缀（如 BfTag::AclUser）即可获得跨租户物理隔离，
/// 同名用户在不同名字空间互不冲突。
pub trait AclStorage: Send + Sync {
  /// 根据复合主键读取用户（内部通过 bitcode 紧凑二进制反序列化）
  fn get_user<'a>(&'a self, key: &'a [u8]) -> BoxFuture<'a, Result<Option<User>>>;

  /// 保存单个用户数据（内部通过 bitcode 紧凑二进制序列化，单点 O(1) 写入）
  fn put_user<'a>(&'a self, key: &'a [u8], user: &'a User) -> BoxFuture<'a, Result<()>>;

  /// 删除单个用户数据（单点 O(1) 删除）
  fn del_user<'a>(&'a self, key: &'a [u8]) -> BoxFuture<'a, Result<bool>>;

  /// 游标分批遍历用户复合主键（千万级用户场景下按页拉取，防单次响应 OOM）
  ///
  /// `cursor` 为上一页最后一条返回的复合主键：`None` 从头扫描，`Some(k)` 仅返回
  /// 字节序**严格大于** `k` 的键。相比 offset 分页每页 O(offset + limit) 的跳过成本，
  /// 游标分页每页恒 O(limit)，且并发写入下不会产生越页空洞。
  fn list_users_after<'a>(
    &'a self,
    cursor: Option<&'a [u8]>,
    limit: usize,
  ) -> BoxFuture<'a, Result<Vec<Vec<u8>>>>;

  /// 获取持久化存储中的用户总数
  fn user_count<'a>(&'a self) -> BoxFuture<'a, Result<usize>>;
}

/// 纯内存模拟的 AclStorage 实现（基于底层 bitcode 序列化存储字节），用于单元测试与基准评测
pub struct MemAclStorage {
  data: GxPapayaMap<Vec<u8>, Vec<u8>>,
}

impl Default for MemAclStorage {
  fn default() -> Self {
    Self {
      data: new_papaya_map(),
    }
  }
}

impl MemAclStorage {
  pub fn new() -> Self {
    Self::default()
  }
}

impl AclStorage for MemAclStorage {
  fn get_user<'a>(&'a self, key: &'a [u8]) -> BoxFuture<'a, Result<Option<User>>> {
    Box::pin(async move {
      let pin = self.data.pin();
      pin.get(key).map(|b| User::from_bitcode(b)).transpose()
    })
  }

  fn put_user<'a>(&'a self, key: &'a [u8], user: &'a User) -> BoxFuture<'a, Result<()>> {
    Box::pin(async move {
      let bytes = user.to_bitcode();
      self.data.pin().insert(key.to_vec(), bytes);
      Ok(())
    })
  }

  fn del_user<'a>(&'a self, key: &'a [u8]) -> BoxFuture<'a, Result<bool>> {
    Box::pin(async move { Ok(self.data.pin().remove(key).is_some()) })
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
      let pin = self.data.pin();
      let mut keys: Vec<Vec<u8>> = pin.keys().cloned().collect();
      keys.sort_unstable();
      let start = match cursor {
        // 键序二分定位「严格大于游标」的首个下标
        Some(c) => keys.partition_point(|k| k.as_slice() <= c),
        None => 0,
      };
      Ok(keys[start..].iter().take(limit).cloned().collect())
    })
  }

  fn user_count<'a>(&'a self) -> BoxFuture<'a, Result<usize>> {
    Box::pin(async move { Ok(self.data.pin().len()) })
  }
}
