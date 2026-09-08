//! 紧凑集合 CompactSet、扁平化分块 MemberChunkCodec 与 ExtensibleSet 编解码测试

use aok::{OK, Void};
use log::info;
use wedb_set::{CompactSet, CompactSetCodec, MemberChunkCodec, SetObject};

/// CompactSet 基础增删查与有序字典序
/// 对标 Garnet SetObject 紧凑小集合优化
#[test]
fn test_compact_set_basic() -> Void {
  let mut compact = CompactSet::new();
  assert!(compact.is_empty());
  assert_eq!(compact.len(), 0);

  // 有序插入与去重
  assert!(compact.insert(b"banana")?);
  assert!(compact.insert(b"apple")?);
  assert!(compact.insert(b"cherry")?);
  assert!(!compact.insert(b"banana")?);
  assert_eq!(compact.len(), 3);

  // 内部字典序单调递增
  let members: Vec<&[u8]> = compact.iter_members().collect();
  assert_eq!(members, [&b"apple"[..], &b"banana"[..], &b"cherry"[..]]);

  // contains 查重
  assert!(compact.contains(b"apple"));
  assert!(compact.contains(b"banana"));
  assert!(compact.contains(b"cherry"));
  assert!(!compact.contains(b"durian"));

  // binary_search 二分查找
  assert_eq!(compact.binary_search(b"apple")?, Ok(0));
  assert_eq!(compact.binary_search(b"banana")?, Ok(1));
  assert_eq!(compact.binary_search(b"cherry")?, Ok(2));
  assert_eq!(compact.binary_search(b"blueberry")?, Err(2));

  // 删除
  assert!(compact.remove(b"banana")?);
  assert!(!compact.remove(b"banana")?);
  assert_eq!(compact.len(), 2);
  assert!(!compact.contains(b"banana"));

  // 批量 encode
  let encoded = CompactSetCodec::encode([&b"zebra"[..], &b"ant"[..], &b"cat"[..]])?;
  let decoded = CompactSet::from_vec(encoded)?;
  assert_eq!(decoded.len(), 3);
  let decoded_mems: Vec<&[u8]> = decoded.iter_members().collect();
  assert_eq!(decoded_mems, [&b"ant"[..], &b"cat"[..], &b"zebra"[..]]);

  info!("test_compact_set_basic 通过");
  OK
}

/// 紧凑编码损坏输入绝不 panic：解析/查找/插入/删除/迭代全部安全失败
/// 对标 Garnet 边界安全防护
#[test]
fn test_compact_corruption_never_panics() -> Void {
  // count 头谎报 5 个成员但仅 1 个真实条目
  let lying_count = {
    let mut b = vec![0u8, 5];
    b.extend_from_slice(&3u16.to_be_bytes());
    b.extend_from_slice(b"abc");
    b
  };
  assert_eq!(CompactSetCodec::count(&lying_count)?, 5);
  assert!(CompactSetCodec::validate(&lying_count).is_err());
  assert!(CompactSetCodec::contains(&lying_count, b"abc"));
  assert!(!CompactSetCodec::contains(&lying_count, b"zzz"));
  assert_eq!(
    CompactSetCodec::remove(&mut lying_count.clone(), b"abc"),
    Ok(true)
  );
  assert_eq!(CompactSetCodec::iter_members(&lying_count).count(), 1);
  let mut copy = lying_count.clone();
  assert!(CompactSetCodec::insert(&mut copy, b"zzz").is_err());

  // 条目长度越过缓冲区末端
  let overrun = [0u8, 1, 0, 40, b'x'];
  assert!(CompactSetCodec::validate(&overrun).is_err());
  assert!(!CompactSetCodec::contains(&overrun, b"x"));

  // 字典序逆序被拒绝
  let descending = {
    let mut b = vec![0u8, 2];
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(b'z');
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(b'a');
    b
  };
  assert!(CompactSetCodec::validate(&descending).is_err());

  // 重复条目被拒绝
  let duplicated = {
    let mut b = vec![0u8, 2];
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(b'a');
    b.extend_from_slice(&1u16.to_be_bytes());
    b.push(b'a');
    b
  };
  assert!(CompactSetCodec::validate(&duplicated).is_err());

  // 尾部冗余字节必须被拒绝
  let mut trailing = vec![0u8, 1];
  trailing.extend_from_slice(&1u16.to_be_bytes());
  trailing.push(b'a');
  trailing.push(0);
  assert!(CompactSetCodec::validate(&trailing).is_err());

  // 空切片安全失败
  assert!(CompactSetCodec::count(&[]).is_err());
  assert_eq!(CompactSetCodec::binary_search(&[], b"a")?, Err(0));
  assert!(!CompactSetCodec::contains(&[], b"a"));
  assert_eq!(CompactSetCodec::remove(&mut Vec::new(), b"a"), Ok(false));
  let mut empty = Vec::new();
  assert_eq!(CompactSetCodec::insert(&mut empty, b"a"), Ok(true));
  assert_eq!(CompactSetCodec::count(&empty)?, 1);

  // 迭代器对损坏切片安全短路
  let bad = [0u8, 1, 0];
  assert_eq!(CompactSetCodec::iter_members(&bad).count(), 0);

  info!("test_compact_corruption_never_panics 通过");
  OK
}

/// MemberChunkCodec 打包、解包与流式零拷贝迭代
/// 对标 Garnet 分块传输与存储协议
#[test]
fn test_member_chunk_codec_roundtrip() -> Void {
  let members: [&[u8]; 4] = [b"uid_10001", b"uid_10002", b"uid_10003", b"uid_10004"];

  let mut buf = Vec::new();
  MemberChunkCodec::encode(&members, &mut buf);

  // 解码遍历
  let decoded: Vec<&[u8]> = MemberChunkCodec::iter(&buf)?.collect();
  assert_eq!(decoded, members);

  // 追加成员
  MemberChunkCodec::append(b"uid_99999", &mut buf)?;
  let decoded_after_append: Vec<&[u8]> = MemberChunkCodec::iter(&buf)?.collect();
  assert_eq!(
    decoded_after_append,
    [
      &b"uid_10001"[..],
      &b"uid_10002"[..],
      &b"uid_10003"[..],
      &b"uid_10004"[..],
      &b"uid_99999"[..]
    ]
  );

  // 空成员 chunk
  let mut empty_buf = Vec::new();
  MemberChunkCodec::encode(&[], &mut empty_buf);
  let empty_decoded: Vec<&[u8]> = MemberChunkCodec::iter(&empty_buf)?.collect();
  assert!(empty_decoded.is_empty());

  info!("test_member_chunk_codec_roundtrip 通过");
  OK
}

/// 分块编解码损坏输入严格拦截防崩溃
/// 对标 Garnet 协议截断防护
#[test]
fn test_member_chunk_corruption() -> Void {
  let members: [&[u8]; 2] = [b"aaa", b"bb"];
  let mut buf = Vec::new();
  MemberChunkCodec::encode(&members, &mut buf);

  // 截断检验
  for end in 1..buf.len() {
    if let Ok(iter) = MemberChunkCodec::iter(&buf[..end]) {
      for (i, m) in iter.enumerate() {
        assert_eq!(m, members[i], "截断至 {end} 字节的成员须为原前缀");
      }
    }
  }

  // 长度域谎报超出剩余载荷
  let mut lying = Vec::new();
  lying.extend_from_slice(&100u32.to_be_bytes());
  lying.extend_from_slice(b"aaa");
  assert!(MemberChunkCodec::iter(&lying).is_err());

  // 尾部残留半截长度域
  let mut partial_tail = buf.clone();
  partial_tail.extend_from_slice(&[0, 0, 5]);
  assert!(MemberChunkCodec::iter(&partial_tail).is_err());

  // 合法空缓冲区迭代为空
  let decoded_empty: Vec<&[u8]> = MemberChunkCodec::iter(&[])?.collect();
  assert!(decoded_empty.is_empty());

  info!("test_member_chunk_corruption 通过");
  OK
}

/// MemberChunkCodec encode_to_vec 快捷包装
/// 对标 Garnet 编码包装
#[test]
fn test_member_chunk_encode_to_vec() -> Void {
  let members: [&[u8]; 3] = [b"chunk1", b"chunk2", b"chunk3"];
  let buf = MemberChunkCodec::encode_to_vec(&members);

  let iter = MemberChunkCodec::iter(&buf)?;
  assert_eq!(iter.len(), 3);
  let decoded: Vec<&[u8]> = iter.collect();
  assert_eq!(decoded, members);

  info!("test_member_chunk_encode_to_vec 通过");
  OK
}

/// 空集合序列化
/// 对标 Garnet SetObject 空实例持久化
#[test]
fn test_empty_set_serialization() -> Void {
  let set = SetObject::new();
  let mut buf = Vec::new();
  set.serialize(&mut buf);

  // 1 字节 version (1) + 4 字节 count (0) = 5 字节
  assert_eq!(buf.len(), 5);
  assert_eq!(buf[0], 1);
  assert_eq!(&buf[1..5], &[0, 0, 0, 0]);

  let restored = SetObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 0);
  assert!(restored.is_empty());

  info!("test_empty_set_serialization 通过");
  OK
}

/// ExtensibleSet 序列化与反序列化往返
/// 对标 Garnet SetObject 二进制持久化规范
#[test]
fn test_set_extensible_serialization_roundtrip() -> Void {
  let mut set = SetObject::new();
  set.sadd([
    b"member1".as_slice(),
    b"member2".as_slice(),
    b"member3".as_slice(),
  ]);

  let mut buf = Vec::new();
  set.serialize(&mut buf);
  assert_eq!(buf[0], 1); // version 1

  let restored = SetObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 3);
  assert!(restored.sismember(b"member1"));
  assert!(restored.sismember(b"member2"));
  assert!(restored.sismember(b"member3"));
  assert!(!restored.sismember(b"member4"));

  info!("test_set_extensible_serialization_roundtrip 通过");
  OK
}

/// 不支持的序列化版本拒绝
/// 对标 Garnet 版本向前兼容安全防护
#[test]
fn test_unsupported_version_rejection() -> Void {
  let buf = [99u8, 0, 0, 0, 0];
  let res = SetObject::deserialize(&buf);
  assert!(res.is_err());
  info!("test_unsupported_version_rejection 通过");
  OK
}

/// 损坏序列化输入防护
/// 对标 Garnet 数据校验安全防崩溃
#[test]
fn test_corrupted_safeguards() -> Void {
  assert!(SetObject::deserialize(&[]).is_err());
  assert!(SetObject::deserialize(&[1, 0]).is_err());
  assert!(SetObject::deserialize(&[1, 1, 0, 0, 0]).is_err());
  info!("test_corrupted_safeguards 通过");
  OK
}

/// 大规模集合 (10,000 成员) 序列化往返
/// 对标 Garnet 大规模实例持久化稳定性
#[test]
fn test_large_scale_roundtrip() -> Void {
  let mut set = SetObject::with_capacity(10000);
  set.sadd((0..10000u32).map(|i| i.to_le_bytes()));

  let mut buf = Vec::new();
  set.serialize(&mut buf);

  let restored = SetObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 10000);

  for i in (0..10000u32).step_by(100) {
    assert!(restored.sismember(&i.to_le_bytes()));
  }

  info!("test_large_scale_roundtrip 通过");
  OK
}

/// 恶意/损坏序列化输入绝不 panic，且分配受输入长度约束
/// 对标 Garnet 协议攻击防御与防 OOM
#[test]
fn test_deserialize_corruption_never_panics() -> Void {
  use fastrand::Rng;

  let mut set = SetObject::new();
  set.sadd([&b"apple"[..], &b"banana"[..], &b"cherry"[..]]);
  let mut buf = Vec::new();
  set.serialize(&mut buf);

  // 截断输入一律报错
  for end in 0..buf.len() {
    assert!(
      SetObject::deserialize(&buf[..end]).is_err(),
      "截断至 {end} 字节应返回 Err"
    );
  }

  // 谎报超大 count 必须被拒绝（防 OOM，栈数组避免分配）
  let huge_count = [1u8, 0xFF, 0xFF, 0xFF, 0xFF];
  assert!(SetObject::deserialize(&huge_count).is_err());

  // 谎报超大单成员长度必须被拒绝
  let huge_len = [1u8, 1, 0, 0, 0, 0xFF, 0xFF, 0xFF, 0xFF];
  assert!(SetObject::deserialize(&huge_len).is_err());

  // 确定性随机字节翻转
  let mut rng = Rng::with_seed(42);
  for _ in 0..512 {
    let mut corrupted = buf.clone();
    let pos = rng.usize(0..corrupted.len());
    corrupted[pos] = rng.u8(..);
    if let Ok(restored) = SetObject::deserialize(&corrupted) {
      assert!(restored.len() <= buf.len());
    }
  }

  info!("test_deserialize_corruption_never_panics 通过");
  OK
}

/// 尾部脏数据严格拦截防篡改
/// 对标 Garnet 存储完整性校验
#[test]
fn test_deserialize_trailing_garbage() -> Void {
  let mut set = SetObject::new();
  set.sadd([&b"alpha"[..], &b"beta"[..]]);
  let mut bytes = set.to_vec();

  assert!(SetObject::deserialize(&bytes).is_ok());

  // 尾部追加 1 字节脏数据必须返回 Err
  bytes.push(0xFF);
  assert!(matches!(
    SetObject::deserialize(&bytes),
    Err(wedb_set::Error::CorruptedData)
  ));

  info!("test_deserialize_trailing_garbage 通过");
  OK
}

/// bitcode 极速序列化往返一致性
/// 对标 Garnet 高性能网络传输与状态保存
#[test]
fn test_bitcode_roundtrip() -> Void {
  // 空集合
  let empty = SetObject::new();
  let decoded_empty = SetObject::decode_bitcode(&empty.encode_bitcode())?;
  assert_eq!(decoded_empty, empty);

  // 非空集合
  let mut set = SetObject::new();
  set.sadd([&b"alpha"[..], &b"beta"[..], &b"gamma"[..]]);
  let decoded = SetObject::decode_bitcode(&set.encode_bitcode())?;
  assert_eq!(decoded, set);
  assert_eq!(decoded.len(), 3);
  assert!(decoded.sismember(b"beta"));

  // 大集合往返
  let mut large = SetObject::with_capacity(1000);
  large.sadd((0..1000u32).map(|i| i.to_le_bytes()));
  let decoded_large = SetObject::decode_bitcode(&large.encode_bitcode())?;
  assert_eq!(decoded_large, large);

  info!("test_bitcode_roundtrip 通过");
  OK
}

/// bitcode 损坏输入防护
/// 对标 Garnet 防恶意载荷注入
#[test]
fn test_bitcode_corruption_never_panics() -> Void {
  use fastrand::Rng;

  let mut set = SetObject::new();
  set.sadd([&b"m1"[..], &b"m2"[..], &b"m3"[..]]);
  let encoded = set.encode_bitcode();

  // 空输入必须报错
  assert!(SetObject::decode_bitcode(&[]).is_err());

  // 截断输入一律报错
  for end in 0..encoded.len() {
    assert!(
      SetObject::decode_bitcode(&encoded[..end]).is_err(),
      "bitcode 截断至 {end} 字节应返回 Err"
    );
  }

  // 确定性随机损坏
  let mut rng = Rng::with_seed(7);
  for _ in 0..512 {
    let mut corrupted = encoded.clone();
    let pos = rng.usize(0..corrupted.len());
    corrupted[pos] = rng.u8(..);
    if let Ok(restored) = SetObject::decode_bitcode(&corrupted) {
      assert!(restored.len() <= encoded.len());
    }
  }

  // 尾部追加冗余字节必须报错
  let mut padded = encoded.clone();
  padded.push(0);
  assert!(SetObject::decode_bitcode(&padded).is_err());

  info!("test_bitcode_corruption_never_panics 通过");
  OK
}

/// MemberChunkCodec 与 SetObject 反序列化不完整块残余安全拦截
/// 对标 Garnet 协议残余分块截断拦截
#[test]
fn test_codec_and_deserialize_partial_chunk_safeguards() -> Void {
  // 1. MemberChunkCodec 残留 1, 2, 3 字节无法构成 4 字节头部
  for remainder in 1..=3 {
    let mut bad_buf = MemberChunkCodec::encode_to_vec(&[b"alpha", b"beta"]);
    bad_buf.resize(bad_buf.len() + remainder, 0xAA);
    assert!(
      matches!(
        MemberChunkCodec::iter(&bad_buf),
        Err(wrecord::Error::BufferTooShort { .. })
      ),
      "残留 {remainder} 字节应返回 BufferTooShort"
    );
  }

  // 2. SetObject::deserialize 残留 1..=3 字节脏数据
  let mut valid_set = SetObject::new();
  valid_set.sadd([&b"one"[..], &b"two"[..]]);
  let valid_bytes = valid_set.to_vec();

  for remainder in 1..=3 {
    let mut bad_bytes = valid_bytes.clone();
    bad_bytes.resize(bad_bytes.len() + remainder, 0xBB);
    assert!(
      matches!(
        SetObject::deserialize(&bad_bytes),
        Err(wedb_set::Error::CorruptedData)
      ),
      "反序列化残留 {remainder} 字节应返回 CorruptedData"
    );
  }

  info!("test_codec_and_deserialize_partial_chunk_safeguards 通过");
  OK
}
