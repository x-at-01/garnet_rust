//! 测试辅助方法与测试夹具（对标 Garnet 测试辅助套件）

use wedb_set::SetObject;

/// 构造包含指定成员切片的集合
pub fn make_set(members: &[&[u8]]) -> SetObject {
  let mut set = SetObject::with_capacity(members.len());
  set.sadd(members.iter().copied());
  set
}

/// 构造包含指定数量序列成员的集合 (前缀 + 序号)
pub fn make_seq_set(prefix: &str, count: usize) -> SetObject {
  let mut set = SetObject::with_capacity(count);
  set.sadd((0..count).map(|i| {
    let mut b = itoa::Buffer::new();
    let s = b.format(i);
    let mut v = Vec::with_capacity(prefix.len() + s.len());
    v.extend_from_slice(prefix.as_bytes());
    v.extend_from_slice(s.as_bytes());
    v
  }));
  set
}

/// 获取集合内所有成员的字典序切片排序列表，便于断言比较（零拷贝避免分配内部成员）
pub fn sorted_members(set: &SetObject) -> Vec<&[u8]> {
  let mut members = set.members_ref();
  members.sort_unstable();
  members
}
