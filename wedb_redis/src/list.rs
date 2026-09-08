use wdev::Device;
use wedb_list::{InsertPosition, ListObject};
use wedb_object::GarnetObject;
use wkv::StoreSession;

use super::*;
use crate::error::Result;

pub trait ListCommands<D: Device> {
  /// 左推入列表元素 (LPUSH)
  async fn lpush(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = impl Into<Vec<u8>>>,
  ) -> Result<usize>;

  /// 右推入列表元素 (RPUSH)
  async fn rpush(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = impl Into<Vec<u8>>>,
  ) -> Result<usize>;

  /// 仅当列表存在时左推入元素 (LPUSHX)
  async fn lpushx(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = impl Into<Vec<u8>>>,
  ) -> Result<usize>;

  /// 仅当列表存在时右推入元素 (RPUSHX)
  async fn rpushx(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = impl Into<Vec<u8>>>,
  ) -> Result<usize>;

  /// 左侧弹出元素 (LPOP)
  async fn lpop(&self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>>;

  /// 右侧弹出元素 (RPOP)
  async fn rpop(&self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>>;

  /// 从源列表弹出并推入目标列表 (LMOVE)
  ///
  /// 单遍收敛：一次 load 源/目标富对象后复用对象层 `ListObject` 单遍转移，
  /// 每个对象至多一次 `save_object` 持久化（对比 lpop+lpush 模拟的两次完整
  /// load/save 往返）。同键退化为页级 `rotate`，O(1) 零元素搬移。
  /// 类型校验先于任何修改 (Redis 口径)：目标 WRONGTYPE 时源保持原样。
  async fn lmove(
    &self,
    src: &[u8],
    dst: &[u8],
    wherefrom_left: bool,
    whereto_left: bool,
  ) -> Result<Option<Vec<u8>>>;

  /// 右端弹出并左端推入 (RPOPLPUSH)
  async fn rpoplpush(&self, src: &[u8], dst: &[u8]) -> Result<Option<Vec<u8>>>;

  /// 依次遍历多个键并在首个非空列表中弹出 (LMPOP)
  async fn lmpop(
    &self,
    keys: &[&[u8]],
    from_left: bool,
    count: usize,
  ) -> Result<Option<(Vec<u8>, Vec<Vec<u8>>)>>;

  /// 获取列表长度 (LLEN)
  async fn llen(&self, key: &[u8]) -> Result<usize>;

  /// 获取范围切片 (LRANGE)
  async fn lrange(&self, key: &[u8], start: isize, stop: isize) -> Result<Vec<Vec<u8>>>;

  /// 按索引查询元素 (LINDEX)
  async fn lindex(&self, key: &[u8], index: isize) -> Result<Option<Vec<u8>>>;

  /// 修改指定索引处的元素 (LSET)
  async fn lset(&self, key: &[u8], index: isize, value: impl Into<Vec<u8>>) -> Result<bool>;

  /// 列表裁剪 (LTRIM)
  async fn ltrim(&self, key: &[u8], start: isize, stop: isize) -> Result<()>;

  /// 移除指定数量的匹配元素 (LREM)
  async fn lrem(&self, key: &[u8], count: isize, value: &[u8]) -> Result<usize>;

  /// 插入元素 (LINSERT)
  async fn linsert(
    &self,
    key: &[u8],
    pivot: &[u8],
    val: impl Into<Vec<u8>>,
    pos: InsertPosition,
  ) -> Result<isize>;

  /// 查找元素匹配位置 (LPOS)
  async fn lpos(
    &self,
    key: &[u8],
    element: &[u8],
    rank: isize,
    count: Option<usize>,
    maxlen: usize,
  ) -> Result<Vec<usize>>;
}

impl<D: Device> ListCommands<D> for StoreSession<D> {
  /// 左推入列表元素 (LPUSH)
  async fn lpush(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = impl Into<Vec<u8>>>,
  ) -> Result<usize> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => GarnetObject::List(ListObject::new()),
    };
    let count = {
      let list = obj.as_list_mut()?;
      list.lpush(items)
    };
    self.save_object(key, obj).await?;
    Ok(count)
  }

  /// 右推入列表元素 (RPUSH)
  async fn rpush(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = impl Into<Vec<u8>>>,
  ) -> Result<usize> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => GarnetObject::List(ListObject::new()),
    };
    let count = {
      let list = obj.as_list_mut()?;
      list.rpush(items)
    };
    self.save_object(key, obj).await?;
    Ok(count)
  }

  /// 仅当列表存在时左推入元素 (LPUSHX)
  async fn lpushx(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = impl Into<Vec<u8>>>,
  ) -> Result<usize> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(0),
    };
    let count = {
      let list = obj.as_list_mut()?;
      list.lpush(items)
    };
    self.save_object(key, obj).await?;
    Ok(count)
  }

  /// 仅当列表存在时右推入元素 (RPUSHX)
  async fn rpushx(
    &self,
    key: &[u8],
    items: impl IntoIterator<Item = impl Into<Vec<u8>>>,
  ) -> Result<usize> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(0),
    };
    let count = {
      let list = obj.as_list_mut()?;
      list.rpush(items)
    };
    self.save_object(key, obj).await?;
    Ok(count)
  }

  /// 左侧弹出元素 (LPOP)
  async fn lpop(&self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(Vec::new()),
    };
    let items = {
      let list = obj.as_list_mut()?;
      list.lpop(count)
    };
    self.save_object(key, obj).await?;
    Ok(items)
  }

  /// 右侧弹出元素 (RPOP)
  async fn rpop(&self, key: &[u8], count: usize) -> Result<Vec<Vec<u8>>> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(Vec::new()),
    };
    let items = {
      let list = obj.as_list_mut()?;
      list.rpop(count)
    };
    self.save_object(key, obj).await?;
    Ok(items)
  }

  /// 从源列表弹出并推入目标列表 (LMOVE)
  ///
  /// 单遍收敛：一次 load 源/目标富对象后复用对象层 `ListObject` 单遍转移，
  /// 每个对象至多一次 `save_object` 持久化（对比 lpop+lpush 模拟的两次完整
  /// load/save 往返）。同键退化为页级 `rotate`，O(1) 零元素搬移。
  /// 类型校验先于任何修改 (Redis 口径)：目标 WRONGTYPE 时源保持原样。
  async fn lmove(
    &self,
    src: &[u8],
    dst: &[u8],
    wherefrom_left: bool,
    whereto_left: bool,
  ) -> Result<Option<Vec<u8>>> {
    // 源类型校验在前：源不存在直接返回 nil（不校验目标类型、不创建目标键）
    let mut src_obj = match self.load_object(src).await? {
      Some(obj) => obj,
      None => return Ok(None),
    };
    let same_key = src == dst;
    // 目标类型校验先于弹出：跨类型 WRONGTYPE 时源不被修改
    let dst_obj = if same_key {
      None
    } else {
      self.load_object(dst).await?
    };

    if same_key {
      // 同键 LMOVE：页级首尾旋转，弹出元素即旋转后目标端新端点
      let Some(item) = src_obj
        .as_list_mut()?
        .rotate(wherefrom_left, whereto_left)
        .map(Vec::from)
      else {
        return Ok(None);
      };
      self.save_object(src, src_obj).await?;
      return Ok(Some(item));
    }

    let mut dst_obj = dst_obj.unwrap_or_else(|| GarnetObject::List(ListObject::new()));
    let item = {
      let src_list = src_obj.as_list_mut()?;
      let dst_list = dst_obj.as_list_mut()?;
      ListObject::lmove(src_list, dst_list, wherefrom_left, whereto_left)
    };
    let Some(item) = item else {
      // 源为空列表（空集合不落盘的不变量下不可达）：无修改直接返回 nil
      return Ok(None);
    };
    self.save_object(src, src_obj).await?;
    self.save_object(dst, dst_obj).await?;
    Ok(Some(item))
  }

  /// 右端弹出并左端推入 (RPOPLPUSH)
  #[inline]
  async fn rpoplpush(&self, src: &[u8], dst: &[u8]) -> Result<Option<Vec<u8>>> {
    self.lmove(src, dst, false, true).await
  }

  /// 依次遍历多个键并在首个非空列表中弹出 (LMPOP)
  async fn lmpop(
    &self,
    keys: &[&[u8]],
    from_left: bool,
    count: usize,
  ) -> Result<Option<(Vec<u8>, Vec<Vec<u8>>)>> {
    for &k in keys {
      let popped = if from_left {
        self.lpop(k, count).await?
      } else {
        self.rpop(k, count).await?
      };
      if !popped.is_empty() {
        return Ok(Some((k.to_vec(), popped)));
      }
    }
    Ok(None)
  }

  /// 获取列表长度 (LLEN)
  async fn llen(&self, key: &[u8]) -> Result<usize> {
    let obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(0),
    };
    let list = obj.as_list()?;
    Ok(list.len())
  }

  /// 获取范围切片 (LRANGE)
  async fn lrange(&self, key: &[u8], start: isize, stop: isize) -> Result<Vec<Vec<u8>>> {
    let obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(Vec::new()),
    };
    let list = obj.as_list()?;
    Ok(list.lrange(start, stop))
  }

  /// 按索引查询元素 (LINDEX)
  async fn lindex(&self, key: &[u8], index: isize) -> Result<Option<Vec<u8>>> {
    let obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(None),
    };
    let list = obj.as_list()?;
    Ok(list.lindex(index).map(|s| s.to_vec()))
  }

  /// 修改指定索引处的元素 (LSET)
  async fn lset(&self, key: &[u8], index: isize, value: impl Into<Vec<u8>>) -> Result<bool> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(false),
    };
    {
      let list = obj.as_list_mut()?;
      list.lset(index, value)?;
    }
    self.save_object(key, obj).await?;
    Ok(true)
  }

  /// 列表裁剪 (LTRIM)
  async fn ltrim(&self, key: &[u8], start: isize, stop: isize) -> Result<()> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(()),
    };
    {
      let list = obj.as_list_mut()?;
      list.ltrim(start, stop);
    }
    self.save_object(key, obj).await?;
    Ok(())
  }

  /// 移除指定数量的匹配元素 (LREM)
  async fn lrem(&self, key: &[u8], count: isize, value: &[u8]) -> Result<usize> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(0),
    };
    let removed = {
      let list = obj.as_list_mut()?;
      list.lrem(count, value)
    };
    self.save_object(key, obj).await?;
    Ok(removed)
  }

  /// 插入元素 (LINSERT)
  async fn linsert(
    &self,
    key: &[u8],
    pivot: &[u8],
    val: impl Into<Vec<u8>>,
    pos: InsertPosition,
  ) -> Result<isize> {
    let mut obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(0),
    };
    let res = {
      let list = obj.as_list_mut()?;
      list.linsert(pivot, val, pos)
    };
    if res > 0 {
      self.save_object(key, obj).await?;
    }
    Ok(res)
  }

  /// 查找元素匹配位置 (LPOS)
  async fn lpos(
    &self,
    key: &[u8],
    element: &[u8],
    rank: isize,
    count: Option<usize>,
    maxlen: usize,
  ) -> Result<Vec<usize>> {
    let obj = match self.load_object(key).await? {
      Some(obj) => obj,
      None => return Ok(Vec::new()),
    };
    let list = obj.as_list()?;
    Ok(list.lpos(element, rank, count, maxlen))
  }
}
