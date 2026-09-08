//! wedb_set 核心端到端冒烟测试套件

use aok::{OK, Void};
use log::info;
use wedb_set::{CompactSet, MemberChunkCodec, SetObject, glob_match};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 端到端基础 CRUD 冒烟（对标 C# RespSetTest.cs: CandDoSaddBasic, CanRemoveField, CanCheckIfMemberExistsInSet）
#[test]
fn test_smoke_basic_crud() -> Void {
  let mut set = SetObject::new();
  assert!(set.is_empty());
  assert_eq!(set.len(), 0);

  // SADD: 批量添加与去重
  assert_eq!(set.sadd([&b"apple"[..], &b"banana"[..], &b"cherry"[..]]), 3);
  assert_eq!(set.sadd([&b"banana"[..], &b"durian"[..]]), 1);
  assert_eq!(set.len(), 4);

  // SISMEMBER & SMISMEMBER
  assert!(set.sismember(b"apple"));
  assert!(!set.sismember(b"orange"));
  assert_eq!(
    set.smismember(&[b"apple", b"orange", b"durian"]),
    [true, false, true]
  );

  // SREM
  assert_eq!(set.srem(&[b"apple", b"orange"]), 1);
  assert_eq!(set.len(), 3);
  assert!(!set.sismember(b"apple"));

  info!("test_smoke_basic_crud 通过");
  OK
}

/// 端到端集合运算冒烟 (SUNION, SINTER, SDIFF, SINTERCARD)（对标 C# RespSetTest.cs: CanDoSetUnion, CanDoSetInter, CanDoSdiff, CanDoSinterCard）
#[test]
fn test_smoke_set_algebra() -> Void {
  let mut s1 = SetObject::new();
  s1.sadd([b"a", b"b", b"c"]);

  let mut s2 = SetObject::new();
  s2.sadd([b"b", b"c", b"d"]);

  // UNION: {a, b, c, d}
  let union = s1.union(&[&s2]);
  assert_eq!(union.len(), 4);

  // INTER: {b, c}
  let inter = s1.inter(&[&s2]);
  assert_eq!(inter.len(), 2);
  assert!(inter.sismember(b"b"));
  assert!(inter.sismember(b"c"));

  // DIFF: s1 \ s2 = {a}
  let diff = s1.diff(&[&s2]);
  assert_eq!(diff.len(), 1);
  assert!(diff.sismember(b"a"));

  // SINTERCARD
  assert_eq!(s1.intercard(&[&s2], 0), 2);
  assert_eq!(s1.intercard(&[&s2], 1), 1);

  info!("test_smoke_set_algebra 通过");
  OK
}

/// 端到端随机采样、弹出与移动冒烟 (SRANDMEMBER, SPOP, SMOVE)（对标 C# RespSetTest.cs: CanDoSRANDMEMBERWithCountCommandSE, CanDoSPOPCommandLC, CanDoSmoveBasic）
#[test]
fn test_smoke_pop_and_rand() -> Void {
  let mut src = SetObject::with_capacity(10);
  src.sadd((0..10).map(|i| {
    let mut b = itoa::Buffer::new();
    let s = b.format(i);
    let mut v = Vec::with_capacity(5 + s.len());
    v.extend_from_slice(b"item_");
    v.extend_from_slice(s.as_bytes());
    v
  }));

  // SRANDMEMBER
  let rand_samples = src.srandmember(3);
  assert_eq!(rand_samples.len(), 3);
  assert_eq!(src.len(), 10);

  // SMOVE
  let mut dst = SetObject::new();
  assert!(src.smove(&mut dst, b"item_0"));
  assert!(!src.sismember(b"item_0"));
  assert!(dst.sismember(b"item_0"));
  assert_eq!(src.len(), 9);

  // SPOP
  let popped = src.spop(2);
  assert_eq!(popped.len(), 2);
  assert_eq!(src.len(), 7);
  for p in &popped {
    assert!(!src.sismember(p));
  }

  info!("test_smoke_pop_and_rand 通过");
  OK
}

/// 端到端序列化与 bitcode 往返冒烟（对标 Garnet SetObject 持久化与状态传输）
#[test]
fn test_smoke_serialization_and_bitcode() -> Void {
  let mut set = SetObject::new();
  set.sadd([b"k1", b"k2", b"k3"]);

  // 1. ExtensibleSet 二进制序列化
  let mut buf = Vec::new();
  set.serialize(&mut buf);
  let restored = SetObject::deserialize(&buf)?;
  assert_eq!(restored, set);

  // 2. bitcode 极速编解码
  let encoded = set.encode_bitcode();
  let decoded = SetObject::decode_bitcode(&encoded)?;
  assert_eq!(decoded, set);

  info!("test_smoke_serialization_and_bitcode 通过");
  OK
}

/// 端到端 CompactSet 与 MemberChunkCodec 冒烟（对标 Garnet 紧凑编码与分块传输）
#[test]
fn test_smoke_compact_and_chunk() -> Void {
  // CompactSet
  let mut compact = CompactSet::new();
  compact.insert(b"zeta")?;
  compact.insert(b"alpha")?;
  compact.insert(b"beta")?;
  assert_eq!(compact.len(), 3);
  let members: Vec<&[u8]> = compact.iter_members().collect();
  assert_eq!(members, [&b"alpha"[..], &b"beta"[..], &b"zeta"[..]]);

  // MemberChunkCodec
  let members_chunk: [&[u8]; 2] = [b"chunk_a", b"chunk_b"];
  let chunk_buf = MemberChunkCodec::encode_to_vec(&members_chunk);
  let chunk_iter = MemberChunkCodec::iter(&chunk_buf)?;
  let decoded_chunk: Vec<&[u8]> = chunk_iter.collect();
  assert_eq!(decoded_chunk, members_chunk);

  info!("test_smoke_compact_and_chunk 通过");
  OK
}

/// 端到端 SSCAN 与 Glob 匹配冒烟（对标 C# RespSetTest.cs: CanDoSScanWithCursor 与 GlobUtils.Match）
#[test]
fn test_smoke_sscan_and_glob() -> Void {
  let mut set = SetObject::with_capacity(20);
  set.sadd((0..20).map(|i| {
    let mut b = itoa::Buffer::new();
    let s = b.format(i);
    let mut v = Vec::with_capacity(7);
    v.extend_from_slice(b"user:");
    if i < 10 {
      v.push(b'0');
    }
    v.extend_from_slice(s.as_bytes());
    v
  }));

  // 游标遍历 + 模式过滤
  let (cur, matched) = set.sscan(0, 50, Some(b"user:1*"));
  assert_eq!(cur, 0);
  assert_eq!(matched.len(), 10);
  for item in &matched {
    assert!(glob_match(b"user:1*", item));
  }

  info!("test_smoke_sscan_and_glob 通过");
  OK
}
