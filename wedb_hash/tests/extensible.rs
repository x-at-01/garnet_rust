use aok::{OK, Void};
use coarsetime::Clock;
use log::info;
use wedb_hash::{EXPIRATION_BIT_MASK, ExpireOpt, ExpireResult, HashEntryBitcode, HashObject};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_empty_hash_serialization() -> Void {
  let mut hash = HashObject::new();
  let mut buf = Vec::new();
  hash.serialize(&mut buf);

  // 1 字节 version (1) + 4 字节 count (0) = 5 字节
  assert_eq!(buf.len(), 5);
  assert_eq!(buf[0], 1); // version 1
  assert_eq!(&buf[1..5], &[0, 0, 0, 0]);

  let restored = HashObject::deserialize(&buf)?;
  assert_eq!(restored.len_ref(), 0);
  assert!(restored.is_empty_ref());
  OK
}

#[test]
fn test_hash_extensible_serialization_roundtrip() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"name".to_vec(), b"wedb".to_vec());
  hash.hset(b"version".to_vec(), b"2.0".to_vec());
  hash.hset(b"desc".to_vec(), b"embed engine".to_vec());

  let future = Clock::now_since_epoch().as_millis() + 50000;
  assert_eq!(
    hash.hexpire(b"version", future, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  let mut buf = Vec::new();
  hash.serialize(&mut buf);

  assert_eq!(buf[0], 1); // version 1

  let mut restored = HashObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 3);
  assert_eq!(restored.hget(b"name"), Some(b"wedb".as_slice()));
  assert_eq!(restored.hget(b"version"), Some(b"2.0".as_slice()));
  assert_eq!(restored.hget(b"desc"), Some(b"embed engine".as_slice()));
  assert_eq!(restored.httl(b"name"), -1);
  assert!(restored.httl(b"version") > 0);

  info!("test_hash_extensible_serialization_roundtrip passed");
  OK
}

#[test]
fn test_unsupported_version_rejection() {
  let buf = vec![99, 0, 0, 0, 0]; // version 99
  let res = HashObject::deserialize(&buf);
  assert!(res.is_err());
}

#[test]
fn test_corrupted_safeguards() {
  assert!(HashObject::deserialize(&[]).is_err());
  assert!(HashObject::deserialize(&[1, 0]).is_err()); // too short header
  assert!(HashObject::deserialize(&[1, 1, 0, 0, 0]).is_err()); // count=1 but no data

  // 伪造超大 count 头：每条目至少 8 字节，提前拒绝而非按 count 循环
  let mut forged = Vec::new();
  forged.push(1u8);
  forged.extend_from_slice(&u32::MAX.to_le_bytes());
  forged.extend_from_slice(&[0u8; 16]);
  assert!(matches!(
    HashObject::deserialize(&forged),
    Err(wedb_hash::Error::CorruptedData)
  ));

  // count 头虚报 2 条：尾部不足以容纳第 2 条完整条目，条目级边界校验报 BufferTooShort 而非 panic
  let mut buf = Vec::new();
  buf.push(1u8);
  buf.extend_from_slice(&2u32.to_le_bytes());
  // 条目 1: 空 key + 空 val (最小 8 字节)
  buf.extend_from_slice(&0u32.to_le_bytes());
  buf.extend_from_slice(&0u32.to_le_bytes());
  // 条目 2: 声明 val_len=4 但仅携带 2 字节即截断
  buf.extend_from_slice(&0u32.to_le_bytes());
  buf.extend_from_slice(&4u32.to_le_bytes());
  buf.extend_from_slice(&[0xAA, 0xBB]);
  assert!(matches!(
    HashObject::deserialize(&buf),
    Err(wedb_hash::Error::BufferTooShort)
  ));
}

#[test]
fn test_large_scale_roundtrip() -> Void {
  let mut hash = HashObject::with_capacity(10000);
  for i in 0..10000u32 {
    let k = i.to_le_bytes();
    let v = (i * 2).to_le_bytes();
    hash.hset(k.to_vec(), v.to_vec());
  }

  let mut buf = Vec::new();
  hash.serialize(&mut buf);

  let mut restored = HashObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 10000);

  for i in (0..10000u32).step_by(100) {
    let k = i.to_le_bytes();
    let expected_v = (i * 2).to_le_bytes();
    assert_eq!(restored.hget(&k), Some(expected_v.as_slice()));
  }

  OK
}

#[test]
fn test_expire_types_bitcode_roundtrip() -> Void {
  let opt = ExpireOpt {
    nx: true,
    xx: false,
    gt: true,
    lt: false,
  };
  let encoded_opt = bitcode::encode(&opt);
  let decoded_opt: ExpireOpt = bitcode::decode(&encoded_opt)?;
  assert_eq!(decoded_opt, opt);

  let res = ExpireResult::ExpireConditionNotMet;
  let encoded_res = bitcode::encode(&res);
  let decoded_res: ExpireResult = bitcode::decode(&encoded_res)?;
  assert_eq!(decoded_res, res);

  OK
}

#[test]
fn test_hash_bitcode_roundtrip() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"k1".to_vec(), b"v1".to_vec());
  hash.hset(b"k2".to_vec(), b"v2".to_vec());
  hash.hset(b"k3".to_vec(), b"v3".to_vec());

  let future = Clock::now_since_epoch().as_millis() + 60_000;
  assert_eq!(
    hash.hexpire(b"k2", future, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  let encoded = hash.to_bitcode()?;
  assert!(!encoded.is_empty());

  let mut restored = HashObject::from_bitcode(&encoded)?;
  assert_eq!(restored.len(), 3);
  assert_eq!(restored.hget(b"k1"), Some(b"v1".as_slice()));
  assert_eq!(restored.hget(b"k2"), Some(b"v2".as_slice()));
  assert_eq!(restored.hget(b"k3"), Some(b"v3".as_slice()));
  assert_eq!(restored.httl(b"k1"), -1);
  // 两次查询毫秒级 TTL 接近
  let ttl1 = restored.httl(b"k2");
  let ttl2 = restored.httl(b"k2");
  assert!(ttl1 > 0 && ttl2 > 0);
  assert!((ttl1 - ttl2).abs() <= 10);

  // 空哈希 bitcode 往返
  let mut empty_hash = HashObject::new();
  let empty_encoded = empty_hash.to_bitcode()?;
  let restored_empty = HashObject::from_bitcode(&empty_encoded)?;
  assert_eq!(restored_empty.len_ref(), 0);
  assert!(restored_empty.is_empty_ref());

  OK
}

#[test]
fn test_hash_bitcode_expired_purging() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"alive".to_vec(), b"yes".to_vec());
  hash.hset(b"dead".to_vec(), b"no".to_vec());

  let now = Clock::now_since_epoch().as_millis();
  // 注入已过去的时间戳（通过直接构造带过期时间戳的 bitcode 序列化条目）
  let past_entry = wedb_hash::HashEntryBitcode {
    field: b"dead".to_vec(),
    val: b"no".to_vec(),
    expire_at: Some(now.saturating_sub(1000)),
  };
  let alive_entry = wedb_hash::HashEntryBitcode {
    field: b"alive".to_vec(),
    val: b"yes".to_vec(),
    expire_at: None,
  };
  let encoded = bitcode::encode(&vec![past_entry, alive_entry]);

  let mut restored = HashObject::from_bitcode(&encoded)?;
  // 验证 dead 字段在反序列化阶段直接被过滤丢弃
  assert_eq!(restored.len(), 1);
  assert_eq!(restored.hget(b"alive"), Some(b"yes".as_slice()));
  assert_eq!(restored.hget(b"dead"), None);
  assert_eq!(restored.httl(b"dead"), -2);

  OK
}

#[test]
fn test_hash_bitcode_large_scale() -> Void {
  let mut hash = HashObject::with_capacity(5000);
  for i in 0..5000u32 {
    let k = i.to_le_bytes();
    let v = (i * 3).to_le_bytes();
    hash.hset(k.to_vec(), v.to_vec());
  }

  let encoded = hash.to_bitcode()?;
  let mut restored = HashObject::from_bitcode(&encoded)?;
  assert_eq!(restored.len(), 5000);

  for i in (0..5000u32).step_by(50) {
    let k = i.to_le_bytes();
    let expected = (i * 3).to_le_bytes();
    assert_eq!(restored.hget(&k), Some(expected.as_slice()));
  }

  OK
}

#[test]
fn test_to_bitcode_ref_and_corrupted_robustness() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"alpha".to_vec(), b"one".to_vec());
  hash.hset(b"beta".to_vec(), b"two".to_vec());

  let future = Clock::now_since_epoch().as_millis() + 60_000;
  assert_eq!(
    hash.hexpire(b"alpha", future, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  // 1. 只读序列化 to_bitcode_ref
  let encoded_ref = hash.to_bitcode_ref()?;
  let mut restored = HashObject::from_bitcode(&encoded_ref)?;
  assert_eq!(restored.len(), 2);
  assert_eq!(restored.hget(b"alpha"), Some(b"one".as_slice()));
  assert_eq!(restored.hget(b"beta"), Some(b"two".as_slice()));
  assert!(restored.httl(b"alpha") > 0);
  assert_eq!(restored.httl(b"beta"), -1);

  // 2. 恶意截断与损坏数据防御
  assert!(HashObject::from_bitcode(&[]).is_err());
  assert!(HashObject::from_bitcode(&[0x01]).is_err());
  assert!(HashObject::from_bitcode(&[0xFF, 0xFF, 0xFF]).is_err());

  // 3. 重复键且混合过期时间戳的数据鲁棒性
  let now = Clock::now_since_epoch().as_millis();
  let dup_entries = vec![
    wedb_hash::HashEntryBitcode {
      field: b"dup".to_vec(),
      val: b"first".to_vec(),
      expire_at: Some(now + 10_000),
    },
    wedb_hash::HashEntryBitcode {
      field: b"dup".to_vec(),
      val: b"second".to_vec(),
      expire_at: None, // 覆盖并持久化
    },
  ];
  let dup_encoded = bitcode::encode(&dup_entries);
  let mut restored_dup = HashObject::from_bitcode(&dup_encoded)?;
  assert_eq!(restored_dup.len(), 1);
  assert_eq!(restored_dup.hget(b"dup"), Some(b"second".as_slice()));
  assert_eq!(restored_dup.httl(b"dup"), -1); // 成功持久化无残留过期时间

  OK
}

#[test]
fn test_deserialize_duplicate_key_ttl_override() -> Void {
  // 手工构造重复键缓冲区，验证「后到条目覆盖一切」恢复语义 (与 from_bitcode 对齐)：
  // 1. 先带未来 TTL，再无 TTL 覆盖 → 字段持久化，无残留脏 TTL
  let mut buf = Vec::new();
  buf.push(1u8); // FORMAT_VERSION
  buf.extend_from_slice(&2u32.to_le_bytes());
  // 条目 1: key=dup, val=first, 含未来过期时间戳
  buf.extend_from_slice(&(3u32 | EXPIRATION_BIT_MASK).to_le_bytes());
  buf.extend_from_slice(b"dup");
  buf.extend_from_slice(&5u32.to_le_bytes());
  buf.extend_from_slice(b"first");
  buf.extend_from_slice(&(Clock::now_since_epoch().as_millis() + 60_000).to_le_bytes());
  // 条目 2: key=dup, val=second, 无过期
  buf.extend_from_slice(&3u32.to_le_bytes());
  buf.extend_from_slice(b"dup");
  buf.extend_from_slice(&6u32.to_le_bytes());
  buf.extend_from_slice(b"second");

  let restored = HashObject::deserialize(&buf)?;
  assert_eq!(restored.len_ref(), 1);
  assert_eq!(restored.hget_ref(b"dup"), Some(b"second".as_slice()));
  assert_eq!(restored.httl_ref(b"dup"), -1);

  // 2. 先无 TTL，再已过期 TTL 覆盖 → 字段被后到记录删除
  let now = Clock::now_since_epoch().as_millis();
  let mut buf2 = Vec::new();
  buf2.push(1u8);
  buf2.extend_from_slice(&2u32.to_le_bytes());
  buf2.extend_from_slice(&3u32.to_le_bytes());
  buf2.extend_from_slice(b"dup");
  buf2.extend_from_slice(&5u32.to_le_bytes());
  buf2.extend_from_slice(b"alive");
  buf2.extend_from_slice(&(3u32 | EXPIRATION_BIT_MASK).to_le_bytes());
  buf2.extend_from_slice(b"dup");
  buf2.extend_from_slice(&4u32.to_le_bytes());
  buf2.extend_from_slice(b"dead");
  buf2.extend_from_slice(&now.saturating_sub(1000).to_le_bytes());

  let restored2 = HashObject::deserialize(&buf2)?;
  assert_eq!(restored2.len_ref(), 0);
  assert_eq!(restored2.hget_ref(b"dup"), None);

  OK
}

/// from_bitcode 与 deserialize 的「后到已过期条目覆盖删除」语义必须一致
#[test]
fn test_bitcode_expired_override_matches_deserialize() -> Void {
  let now = Clock::now_since_epoch().as_millis();

  // 场景：先无 TTL 存活条目，再后到一条同名已过期条目 → 字段被彻底删除
  let entries = vec![
    HashEntryBitcode {
      field: b"dup".to_vec(),
      val: b"alive".to_vec(),
      expire_at: None,
    },
    HashEntryBitcode {
      field: b"dup".to_vec(),
      val: b"dead".to_vec(),
      expire_at: Some(now.saturating_sub(1000)),
    },
  ];
  let encoded = bitcode::encode(&entries);
  let restored = HashObject::from_bitcode(&encoded)?;
  assert_eq!(restored.len_ref(), 0);
  assert_eq!(restored.hget_ref(b"dup"), None);

  // 场景：先带未来 TTL，再后到一条同名已过期条目 → 字段连同 TTL 记录一并删除
  let entries2 = vec![
    HashEntryBitcode {
      field: b"dup".to_vec(),
      val: b"with_ttl".to_vec(),
      expire_at: Some(now + 60_000),
    },
    HashEntryBitcode {
      field: b"dup".to_vec(),
      val: b"dead".to_vec(),
      expire_at: Some(now.saturating_sub(1000)),
    },
  ];
  let encoded2 = bitcode::encode(&entries2);
  let restored2 = HashObject::from_bitcode(&encoded2)?;
  assert_eq!(restored2.len_ref(), 0);
  assert_eq!(restored2.hget_ref(b"dup"), None);
  assert_eq!(restored2.httl_ref(b"dup"), -2);

  // 场景：先带未来 TTL，再后到同名无 TTL 条目 → 覆盖为持久化且无残留脏 TTL
  let entries3 = vec![
    HashEntryBitcode {
      field: b"dup".to_vec(),
      val: b"with_ttl".to_vec(),
      expire_at: Some(now + 60_000),
    },
    HashEntryBitcode {
      field: b"dup".to_vec(),
      val: b"persistent".to_vec(),
      expire_at: None,
    },
  ];
  let encoded3 = bitcode::encode(&entries3);
  let restored3 = HashObject::from_bitcode(&encoded3)?;
  assert_eq!(restored3.len_ref(), 1);
  assert_eq!(restored3.hget_ref(b"dup"), Some(b"persistent".as_slice()));
  assert_eq!(restored3.httl_ref(b"dup"), -1);

  OK
}
