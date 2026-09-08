#[cfg(all(
  feature = "wedb_hash",
  feature = "wedb_zset",
  feature = "wedb_list",
  feature = "wedb_set"
))]
use std::sync::Arc;
#[cfg(any(feature = "wedb_hash", feature = "wedb_zset"))]
use std::{
  thread,
  time::{Duration, SystemTime, UNIX_EPOCH},
};

use aok::{OK, Void};
use log::info;
#[cfg(feature = "wedb_hash")]
use wedb_hash::{ExpireOpt, ExpireResult, HashObject};
#[cfg(feature = "wedb_list")]
use wedb_list::ListObject;
use wedb_object::{Error, GarnetObject, GarnetObjectType};
#[cfg(feature = "wedb_set")]
use wedb_set::SetObject;
#[cfg(feature = "wedb_zset")]
use wedb_zset::SortedSetObject;

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 当前毫秒时间戳
#[cfg(any(feature = "wedb_hash", feature = "wedb_zset"))]
fn now_ms() -> u64 {
  SystemTime::now()
    .duration_since(UNIX_EPOCH)
    .unwrap()
    .as_millis() as u64
}

#[test]
fn test_type_enum() -> Void {
  // 显式判别值即持久化类型字节（值永久固定，
  assert_eq!(GarnetObjectType::Null as u8, 0);
  assert_eq!(GarnetObjectType::SortedSet as u8, 1);
  assert_eq!(GarnetObjectType::List as u8, 2);
  assert_eq!(GarnetObjectType::Hash as u8, 3);
  assert_eq!(GarnetObjectType::Set as u8, 4);

  // 常量定义与保留边界
  assert_eq!(GarnetObjectType::LAST_OBJECT_TYPE, GarnetObjectType::Set);
  assert_eq!(GarnetObjectType::LAST_RESERVED_BUILTIN_TYPE, 0x3F);
  assert_eq!(GarnetObjectType::ALL, 0xFB);
  assert_eq!(GarnetObjectType::RESERVED_FORMAT_BYTE_START, 0xFC);

  for (byte, ty) in [
    (0, GarnetObjectType::Null),
    (1, GarnetObjectType::SortedSet),
    (2, GarnetObjectType::List),
    (3, GarnetObjectType::Hash),
    (4, GarnetObjectType::Set),
  ] {
    assert_eq!(GarnetObjectType::from_u8(byte)?, ty);
    assert_eq!(GarnetObjectType::try_from(byte)?, ty);
    assert_eq!(u8::from(ty), byte);
  }

  // Redis 规范类型名与 Display / AsRef<str> / AsRef<[u8]>
  assert_eq!(GarnetObjectType::Null.as_str(), "none");
  assert_eq!(GarnetObjectType::SortedSet.as_str(), "zset");
  assert_eq!(GarnetObjectType::List.as_str(), "list");
  assert_eq!(GarnetObjectType::Hash.as_str(), "hash");
  assert_eq!(GarnetObjectType::Set.as_str(), "set");

  assert_eq!(GarnetObjectType::Null.as_bytes(), b"none");
  assert_eq!(GarnetObjectType::SortedSet.as_bytes(), b"zset");
  assert_eq!(GarnetObjectType::List.as_bytes(), b"list");
  assert_eq!(GarnetObjectType::Hash.as_bytes(), b"hash");
  assert_eq!(GarnetObjectType::Set.as_bytes(), b"set");

  assert_eq!(format!("{}", GarnetObjectType::Hash), "hash");
  let zset_str: &str = GarnetObjectType::SortedSet.as_ref();
  assert_eq!(zset_str, "zset");
  let zset_bytes: &[u8] = GarnetObjectType::SortedSet.as_ref();
  assert_eq!(zset_bytes, b"zset");

  // 0..=255 全域 256 字节穷举测试：验证唯一正确分类与快速失败
  for b in 0u8..=255 {
    match b {
      0 => {
        assert_eq!(GarnetObjectType::from_u8(b)?, GarnetObjectType::Null);
        assert!(!GarnetObjectType::is_reserved(b));
      }
      1 => {
        assert_eq!(GarnetObjectType::from_u8(b)?, GarnetObjectType::SortedSet);
        assert!(!GarnetObjectType::is_reserved(b));
      }
      2 => {
        assert_eq!(GarnetObjectType::from_u8(b)?, GarnetObjectType::List);
        assert!(!GarnetObjectType::is_reserved(b));
      }
      3 => {
        assert_eq!(GarnetObjectType::from_u8(b)?, GarnetObjectType::Hash);
        assert!(!GarnetObjectType::is_reserved(b));
      }
      4 => {
        assert_eq!(GarnetObjectType::from_u8(b)?, GarnetObjectType::Set);
        assert!(!GarnetObjectType::is_reserved(b));
      }
      0xFC..=0xFF => {
        assert!(GarnetObjectType::is_reserved(b));
        assert!(matches!(
          GarnetObjectType::from_u8(b),
          Err(Error::UnsupportedFormatMarker(v)) if v == b
        ));
        assert!(matches!(
          GarnetObjectType::try_from(b),
          Err(Error::UnsupportedFormatMarker(v)) if v == b
        ));
      }
      5..=0xFB => {
        assert!(!GarnetObjectType::is_reserved(b));
        assert!(matches!(
          GarnetObjectType::from_u8(b),
          Err(Error::UnknownObjectType(v)) if v == b
        ));
        assert!(matches!(
          GarnetObjectType::try_from(b),
          Err(Error::UnknownObjectType(v)) if v == b
        ));
      }
    }
  }

  // FromStr 解析
  use std::str::FromStr;
  assert_eq!(GarnetObjectType::from_str("none")?, GarnetObjectType::Null);
  assert_eq!(
    GarnetObjectType::from_str("zset")?,
    GarnetObjectType::SortedSet
  );
  assert_eq!(GarnetObjectType::from_str("list")?, GarnetObjectType::List);
  assert_eq!(GarnetObjectType::from_str("hash")?, GarnetObjectType::Hash);
  assert_eq!(GarnetObjectType::from_str("set")?, GarnetObjectType::Set);
  assert!(GarnetObjectType::from_str("invalid").is_err());

  // is_collection 判定与 is_builtin_collection_byte 边界
  assert!(!GarnetObjectType::Null.is_collection());
  assert!(GarnetObjectType::SortedSet.is_collection());
  assert!(GarnetObjectType::List.is_collection());
  assert!(GarnetObjectType::Hash.is_collection());
  assert!(GarnetObjectType::Set.is_collection());

  assert!(!GarnetObjectType::is_builtin_collection_byte(0));
  for b in 1..=4 {
    assert!(GarnetObjectType::is_builtin_collection_byte(b));
  }
  assert!(!GarnetObjectType::is_builtin_collection_byte(5));
  assert!(!GarnetObjectType::is_builtin_collection_byte(0xFC));

  // 排序与全序特性
  assert!(GarnetObjectType::Null < GarnetObjectType::SortedSet);
  assert!(GarnetObjectType::SortedSet < GarnetObjectType::List);
  assert!(GarnetObjectType::List < GarnetObjectType::Hash);
  assert!(GarnetObjectType::Hash < GarnetObjectType::Set);

  info!("test_type_enum passed");
  OK
}

#[test]
fn test_error_handling_and_predicates() -> Void {
  // 错误类型判定方法
  let eof_err = Error::UnexpectedEof;
  assert!(eof_err.is_wrong_type());
  assert!(eof_err.is_unexpected_eof());
  assert!(!eof_err.is_null());

  let null_err = Error::NullObject;
  assert!(null_err.is_wrong_type());
  assert!(!null_err.is_unexpected_eof());
  assert!(null_err.is_null());

  let wrong_err = Error::WrongType;
  assert!(wrong_err.is_wrong_type());
  assert!(!wrong_err.is_unexpected_eof());
  assert!(!wrong_err.is_null());

  // Null type 0
  let err = GarnetObject::deserialize(&[0]).unwrap_err();
  assert!(err.is_null());
  assert!(matches!(err, Error::NullObject));

  // Unknown type 5
  assert!(matches!(
    GarnetObject::deserialize(&[5]),
    Err(Error::UnknownObjectType(5))
  ));

  // C# All (0xFB) 不是合法持久化类型
  assert!(matches!(
    GarnetObject::deserialize(&[0xFB]),
    Err(Error::UnknownObjectType(0xFB))
  ));

  // Reserved markers 0xFC..=0xFF
  for rsv in 0xFCu8..=0xFF {
    assert!(matches!(
      GarnetObject::deserialize(&[rsv]),
      Err(Error::UnsupportedFormatMarker(v)) if v == rsv
    ));
  }

  // Truncated empty input
  let empty_err = GarnetObject::deserialize(&[]).unwrap_err();
  assert!(empty_err.is_unexpected_eof());

  info!("test_error_handling_and_predicates passed");
  OK
}

#[cfg(feature = "wedb_hash")]
#[test]
fn test_hash_object() -> Void {
  let mut hash = HashObject::default();
  hash.hset(b"name", b"alice");
  hash.hset(b"city", b"beijing");

  let mut obj = GarnetObject::from(hash);
  assert_eq!(obj.object_type(), GarnetObjectType::Hash);
  assert_eq!(obj.type_name(), "hash");
  assert_eq!(obj.len(), 2);
  assert_eq!(obj.len_ref(), 2);
  assert!(!obj.is_empty());
  assert!(!obj.is_empty_ref());

  let bytes = obj.to_vec();
  assert_eq!(bytes[0], 3); // 3 == Hash

  let mut de = GarnetObject::deserialize(&bytes)?;
  assert_eq!(de.object_type(), GarnetObjectType::Hash);
  assert_eq!(de.len(), 2);
  let h = de.as_hash_mut()?;
  assert_eq!(h.hget(b"name"), Some(b"alice".as_slice()));
  assert_eq!(h.hget(b"city"), Some(b"beijing".as_slice()));

  // 截断载荷与损坏版本号
  assert!(matches!(
    GarnetObject::deserialize(&[3]),
    Err(Error::Hash(_))
  ));
  assert!(matches!(
    GarnetObject::deserialize(&[3, 9]),
    Err(Error::Hash(_))
  ));

  info!("test_hash_object passed");
  OK
}

#[cfg(feature = "wedb_zset")]
#[test]
fn test_zset_object() -> Void {
  let mut zset = SortedSetObject::default();
  zset.zadd(10.5, b"player1", Default::default())?;
  zset.zadd(20.0, b"player2", Default::default())?;

  let mut obj = GarnetObject::from(zset);
  assert_eq!(obj.object_type(), GarnetObjectType::SortedSet);
  assert_eq!(obj.type_name(), "zset");
  assert_eq!(obj.len(), 2);
  assert_eq!(obj.len_ref(), 2);
  assert!(!obj.is_empty());
  assert!(!obj.is_empty_ref());

  let bytes = obj.to_vec();
  assert_eq!(bytes[0], 1); // 1 == SortedSet

  let mut de = GarnetObject::deserialize(&bytes)?;
  assert_eq!(de.len(), 2);
  let z = de.as_sorted_set_mut()?;
  assert_eq!(z.zscore(b"player1"), Some(10.5));
  assert_eq!(z.zscore(b"player2"), Some(20.0));

  assert!(matches!(
    GarnetObject::deserialize(&[1]),
    Err(Error::ZSet(_))
  ));

  info!("test_zset_object passed");
  OK
}

#[cfg(feature = "wedb_list")]
#[test]
fn test_list_object() -> Void {
  let mut list = ListObject::default();
  list.rpush([b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]);

  let mut obj = GarnetObject::from(list);
  assert_eq!(obj.object_type(), GarnetObjectType::List);
  assert_eq!(obj.type_name(), "list");
  assert_eq!(obj.len(), 3);
  assert_eq!(obj.len_ref(), 3);
  assert!(!obj.is_empty());
  assert!(!obj.is_empty_ref());

  let bytes = obj.to_vec();
  assert_eq!(bytes[0], 2); // 2 == List

  let de = GarnetObject::deserialize(&bytes)?;
  assert_eq!(de.len_ref(), 3);
  let l = de.as_list()?;
  assert_eq!(l.len(), 3);
  assert_eq!(
    l.lrange(0, -1),
    vec![b"a".to_vec(), b"b".to_vec(), b"c".to_vec()]
  );

  assert!(matches!(
    GarnetObject::deserialize(&[2]),
    Err(Error::List(_))
  ));

  info!("test_list_object passed");
  OK
}

#[cfg(feature = "wedb_set")]
#[test]
fn test_set_object() -> Void {
  let mut set = SetObject::default();
  set.sadd([b"apple".to_vec(), b"banana".to_vec()]);

  let mut obj = GarnetObject::from(set);
  assert_eq!(obj.object_type(), GarnetObjectType::Set);
  assert_eq!(obj.type_name(), "set");
  assert_eq!(obj.len(), 2);
  assert_eq!(obj.len_ref(), 2);
  assert!(!obj.is_empty());
  assert!(!obj.is_empty_ref());

  let bytes = obj.to_vec();
  assert_eq!(bytes[0], 4); // 4 == Set

  let de = GarnetObject::deserialize(&bytes)?;
  assert_eq!(de.len_ref(), 2);
  let s = de.as_set()?;
  assert_eq!(s.len(), 2);
  assert!(s.sismember(b"apple"));
  assert!(s.sismember(b"banana"));

  assert!(matches!(
    GarnetObject::deserialize(&[4]),
    Err(Error::Set(_))
  ));

  info!("test_set_object passed");
  OK
}

#[cfg(feature = "wedb_hash")]
#[test]
fn test_hash_expire_recycle() -> Void {
  let mut hash = HashObject::default();
  hash.hset(b"session", b"alice");
  assert_eq!(
    hash.hexpire(b"session", now_ms() + 30, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  // 未过期时序列化：过期时间戳随载荷往返保留
  let mut obj = GarnetObject::from(hash);
  let bytes = obj.to_vec();
  let de = GarnetObject::deserialize(&bytes)?;
  assert!(de.as_hash()?.httl_ref(b"session") > 0);

  // 等待过期后再序列化：serialize 内部回收过期条目，对象变空
  thread::sleep(Duration::from_millis(60));
  assert!(obj.is_empty_ref());
  assert!(obj.is_empty());
  assert_eq!(obj.len(), 0);

  let bytes = obj.to_vec();
  assert_eq!(bytes[0], 3); // 类型字节不变

  // 空对象恢复后 is_empty 为真，下游 save_object 据此删除键（过期回收闭环）
  let mut de = GarnetObject::deserialize(&bytes)?;
  assert_eq!(de.object_type(), GarnetObjectType::Hash);
  assert!(de.is_empty());
  assert!(de.is_empty_ref());

  info!("test_hash_expire_recycle passed");
  OK
}

#[cfg(feature = "wedb_zset")]
#[test]
fn test_zset_expire_recycle() -> Void {
  let mut zset = SortedSetObject::default();
  zset.zadd(100.0, b"token", Default::default())?;
  assert_eq!(
    zset.zexpire(b"token", now_ms() + 30, Default::default()),
    wedb_zset::ExpireResult::Ok
  );

  let mut obj = GarnetObject::from(zset);
  assert!(!obj.is_empty());
  assert_eq!(obj.len(), 1);

  // 未过期时序列化：过期时间戳随载荷往返保留（与 hash 用例口径对齐）
  let bytes0 = obj.to_vec();
  let de0 = GarnetObject::deserialize(&bytes0)?;
  assert!(de0.as_sorted_set()?.zttl_ref(b"token") > 0);

  // 等待过期
  thread::sleep(Duration::from_millis(60));
  assert!(obj.is_empty_ref());
  assert!(obj.is_empty());
  assert_eq!(obj.len(), 0);

  // 序列化为字节并反序列化回对象，验证空对象行为闭环
  let bytes = obj.to_vec();
  assert_eq!(bytes[0], 1); // 1 == SortedSet
  let mut de = GarnetObject::deserialize(&bytes)?;
  assert!(de.is_empty());
  assert!(de.is_empty_ref());
  assert_eq!(de.len(), 0);

  info!("test_zset_expire_recycle passed");
  OK
}

#[cfg(all(
  feature = "wedb_hash",
  feature = "wedb_zset",
  feature = "wedb_list",
  feature = "wedb_set"
))]
#[test]
fn test_type_conversions_and_try_from() -> Void {
  let mut obj = GarnetObject::from(SetObject::default());
  obj.as_set_mut()?.sadd([b"apple".to_vec()]);
  assert!(!obj.is_empty());

  // as_* 只接受匹配类型
  assert!(obj.as_set().is_ok());
  assert!(obj.as_set_mut().is_ok());
  assert!(matches!(obj.as_hash(), Err(Error::WrongType)));
  assert!(matches!(obj.as_hash_mut(), Err(Error::WrongType)));
  assert!(matches!(obj.as_list(), Err(Error::WrongType)));
  assert!(matches!(obj.as_sorted_set(), Err(Error::WrongType)));

  // into_* 消耗所有权并校验类型
  assert!(matches!(obj.clone().into_hash(), Err(Error::WrongType)));
  assert!(matches!(obj.clone().into_list(), Err(Error::WrongType)));
  assert!(matches!(
    obj.clone().into_sorted_set(),
    Err(Error::WrongType)
  ));
  let set = obj.into_set()?;
  assert!(set.sismember(b"apple"));

  // TryFrom 实现验证
  let hash_obj = GarnetObject::from(HashObject::default());
  let h_res: Result<HashObject, _> = HashObject::try_from(hash_obj.clone());
  assert!(h_res.is_ok());
  let l_res: Result<ListObject, _> = ListObject::try_from(hash_obj);
  assert!(matches!(l_res, Err(Error::WrongType)));

  let list_obj = GarnetObject::from(ListObject::default());
  assert!(ListObject::try_from(list_obj.clone()).is_ok());
  assert!(SortedSetObject::try_from(list_obj).is_err());

  let zset_obj = GarnetObject::from(SortedSetObject::default());
  assert!(SortedSetObject::try_from(zset_obj.clone()).is_ok());
  assert!(SetObject::try_from(zset_obj).is_err());

  let set_obj = GarnetObject::from(SetObject::default());
  assert!(SetObject::try_from(set_obj.clone()).is_ok());
  assert!(HashObject::try_from(set_obj).is_err());

  // WRONGTYPE 判定
  let e = GarnetObject::from(SetObject::default())
    .into_hash()
    .unwrap_err();
  assert!(e.is_wrong_type());

  // 借用引用 TryFrom 转换验证 (零拷贝借用转换)
  let mut hash_for_ref = GarnetObject::from(HashObject::default());
  let h_ref: Result<&HashObject, _> = (&hash_for_ref).try_into();
  assert!(h_ref.is_ok());
  let h_ref_mut: Result<&mut HashObject, _> = (&mut hash_for_ref).try_into();
  assert!(h_ref_mut.is_ok());
  let wrong_list_ref: Result<&ListObject, _> = (&hash_for_ref).try_into();
  assert!(matches!(wrong_list_ref, Err(Error::WrongType)));

  let mut list_for_ref = GarnetObject::from(ListObject::default());
  let l_ref: Result<&ListObject, _> = (&list_for_ref).try_into();
  assert!(l_ref.is_ok());
  let l_ref_mut: Result<&mut ListObject, _> = (&mut list_for_ref).try_into();
  assert!(l_ref_mut.is_ok());

  let mut zset_for_ref = GarnetObject::from(SortedSetObject::default());
  let z_ref: Result<&SortedSetObject, _> = (&zset_for_ref).try_into();
  assert!(z_ref.is_ok());
  let z_ref_mut: Result<&mut SortedSetObject, _> = (&mut zset_for_ref).try_into();
  assert!(z_ref_mut.is_ok());

  let mut set_for_ref = GarnetObject::from(SetObject::default());
  let s_ref: Result<&SetObject, _> = (&set_for_ref).try_into();
  assert!(s_ref.is_ok());
  let s_ref_mut: Result<&mut SetObject, _> = (&mut set_for_ref).try_into();
  assert!(s_ref_mut.is_ok());

  info!("test_type_conversions_and_try_from passed");
  OK
}

#[cfg(all(
  feature = "wedb_hash",
  feature = "wedb_zset",
  feature = "wedb_list",
  feature = "wedb_set"
))]
#[test]
fn test_trailing_garbage_payload_tolerance() -> Void {
  // 底层契约验证：
  // Hash 与 List 的反序列化在读满声明字段数后容忍尾部多余字节；
  // Set 与 ZSet 对切片边界进行严格校验，末尾存在未声明字节时按数据损坏拒绝。
  let mut hash = HashObject::default();
  hash.hset(b"key1", b"val1");
  let mut bytes = GarnetObject::from(hash).to_vec();
  bytes.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);

  let mut de = GarnetObject::deserialize(&bytes)?;
  assert_eq!(de.as_hash_mut()?.hget(b"key1"), Some(b"val1".as_slice()));

  let mut list = ListObject::default();
  list.rpush([b"elem".to_vec()]);
  let mut bytes = GarnetObject::from(list).to_vec();
  bytes.extend_from_slice(&[0x01, 0x02, 0x03]);
  let de = GarnetObject::deserialize(&bytes)?;
  assert_eq!(de.as_list()?.len(), 1);

  // Set 尾部有冗余垃圾数据时，底层严格校验并拒绝
  let mut set = SetObject::default();
  set.sadd([b"s1".to_vec()]);
  let mut bytes = GarnetObject::from(set).to_vec();
  bytes.extend_from_slice(&[0xAA, 0xBB]);
  assert!(GarnetObject::deserialize(&bytes).is_err());

  // ZSet 尾部有冗余垃圾数据时，底层严格校验并拒绝
  let mut zset = SortedSetObject::default();
  zset.zadd(99.0, b"m1", Default::default())?;
  let mut bytes = GarnetObject::from(zset).to_vec();
  bytes.extend_from_slice(&[0xFF, 0x00]);
  assert!(GarnetObject::deserialize(&bytes).is_err());

  info!("test_trailing_garbage_payload_tolerance passed");
  OK
}

#[cfg(all(feature = "wedb_hash", feature = "wedb_list"))]
#[test]
fn test_large_scale_and_binary_data() -> Void {
  // 包含空字节、零值二进制数据的复杂 Hash
  let mut hash = HashObject::default();
  for i in 0..200 {
    let key = format!("k\0ey_{i}").into_bytes();
    let val = [i as u8; 64];
    hash.hset(&key[..], &val[..]);
  }
  let mut obj = GarnetObject::from(hash);
  assert_eq!(obj.len(), 200);
  assert_eq!(obj.len_ref(), 200);

  let bytes = obj.to_vec();
  let mut de = GarnetObject::deserialize(&bytes)?;
  assert_eq!(de.len(), 200);
  let h = de.as_hash_mut()?;
  for i in 0..200 {
    let key = format!("k\0ey_{i}").into_bytes();
    let expected = [i as u8; 64];
    assert_eq!(h.hget(&key), Some(expected.as_slice()));
  }

  // 大规模 List
  let mut list = ListObject::default();
  let items: Vec<Vec<u8>> = (0..500).map(|i| format!("item_{i}").into_bytes()).collect();
  list.rpush(items.clone());
  let mut obj = GarnetObject::from(list);
  assert_eq!(obj.len(), 500);
  let bytes = obj.to_vec();
  let de = GarnetObject::deserialize(&bytes)?;
  let l = de.as_list()?;
  assert_eq!(l.len(), 500);
  assert_eq!(l.lrange(0, 499), items);

  info!("test_large_scale_and_binary_data passed");
  OK
}

#[cfg(all(
  feature = "wedb_hash",
  feature = "wedb_zset",
  feature = "wedb_list",
  feature = "wedb_set"
))]
#[test]
fn test_concurrent_deserialize() -> Void {
  // 构造四类对象字节，多线程并发反序列化共享缓冲，验证只读恢复路径线程安全
  let mut hash = HashObject::default();
  hash.hset(b"name", b"alice");
  let mut zset = SortedSetObject::default();
  zset.zadd(1.5, b"m", Default::default())?;
  let mut list = ListObject::default();
  list.rpush([b"x".to_vec()]);
  let mut set = SetObject::default();
  set.sadd([b"s".to_vec()]);

  let bufs = Arc::new(vec![
    GarnetObject::from(hash).to_vec(),
    GarnetObject::from(zset).to_vec(),
    GarnetObject::from(list).to_vec(),
    GarnetObject::from(set).to_vec(),
  ]);

  let handles: Vec<_> = (0..8)
    .map(|_| {
      let bufs = bufs.clone();
      thread::spawn(move || {
        for bytes in bufs.iter() {
          let obj = GarnetObject::deserialize(bytes).unwrap();
          assert_eq!(obj.object_type() as u8, bytes[0]);
        }
      })
    })
    .collect();
  for h in handles {
    h.join().unwrap();
  }

  info!("test_concurrent_deserialize passed");
  OK
}

#[cfg(all(
  feature = "wedb_hash",
  feature = "wedb_zset",
  feature = "wedb_list",
  feature = "wedb_set"
))]
#[test]
fn test_serialized_len_hint_and_capacity() -> Void {
  let mut hash = HashObject::default();
  hash.hset(b"k1", b"v1");
  let mut obj = GarnetObject::from(hash);
  let hint = obj.serialized_len_hint();
  let vec = obj.to_vec();
  assert!(hint > 0);
  assert!(obj.capacity() >= 1);
  obj.shrink_to_fit();
  assert_eq!(vec[0], GarnetObjectType::Hash as u8);

  let mut zset = SortedSetObject::default();
  zset.zadd(1.0, b"m1", Default::default())?;
  let mut obj = GarnetObject::from(zset);
  let hint = obj.serialized_len_hint();
  let vec = obj.to_vec();
  assert!(hint > 0);
  assert!(obj.capacity() >= 1);
  obj.shrink_to_fit();
  assert_eq!(vec[0], GarnetObjectType::SortedSet as u8);

  let mut list = ListObject::default();
  list.rpush([b"elem1".to_vec()]);
  let mut obj = GarnetObject::from(list);
  let hint = obj.serialized_len_hint();
  let vec = obj.to_vec();
  assert!(hint > 0);
  assert!(obj.capacity() >= 1);
  obj.shrink_to_fit();
  assert_eq!(vec[0], GarnetObjectType::List as u8);

  let mut set = SetObject::default();
  set.sadd([b"member1".to_vec()]);
  let mut obj = GarnetObject::from(set);
  let hint = obj.serialized_len_hint();
  let vec = obj.to_vec();
  assert!(hint > 0);
  assert!(obj.capacity() >= 1);
  obj.shrink_to_fit();
  assert_eq!(vec[0], GarnetObjectType::Set as u8);

  // 追加语义：serialize 写入既有缓冲区尾部而非覆盖（下游复用缓冲区依赖此契约）
  let mut shared = b"prefix".to_vec();
  obj.serialize(&mut shared);
  assert_eq!(&shared[..6], b"prefix");
  assert_eq!(shared[6], GarnetObjectType::Set as u8);

  info!("test_serialized_len_hint_and_capacity passed");
  OK
}

#[cfg(all(
  feature = "wedb_hash",
  feature = "wedb_zset",
  feature = "wedb_list",
  feature = "wedb_set"
))]
#[test]
fn test_purge_expired_coordination() -> Void {
  let now = now_ms();

  // 1. Hash 过期回收
  // 时序余量：过期窗口取 500ms，"未到期"断言紧跟构造之后，全量并发负载下
  // 调度抖动超过 500ms 才会误判（原先 20ms 窗口在 CI 高负载下必 flake）
  let mut hash = HashObject::default();
  hash.hset(b"live", b"val");
  hash.hset(b"exp", b"val");
  hash.hexpire(b"exp", now + 500, ExpireOpt::NONE);
  let mut h_obj = GarnetObject::from(hash);
  assert_eq!(h_obj.purge_expired(), 0); // 未到期，清理 0
  thread::sleep(Duration::from_millis(550));
  assert_eq!(h_obj.purge_expired(), 1); // 到期物理清理 1 个字段
  assert_eq!(h_obj.len(), 1);

  // 2. ZSet 过期回收
  let mut zset = SortedSetObject::default();
  zset.zadd(1.0, b"m_live", Default::default())?;
  zset.zadd(2.0, b"m_exp", Default::default())?;
  let znow = now_ms();
  assert_eq!(
    zset.zexpire(b"m_exp", znow + 500, Default::default()),
    wedb_zset::ExpireResult::Ok
  );
  let mut z_obj = GarnetObject::from(zset);
  assert_eq!(z_obj.purge_expired(), 0);
  thread::sleep(Duration::from_millis(550));
  assert_eq!(z_obj.purge_expired(), 1);
  assert_eq!(z_obj.len(), 1);

  // 3. List 和 Set 无逐元素 TTL，purge_expired 返回 0
  let mut list_obj = GarnetObject::from(ListObject::default());
  assert_eq!(list_obj.purge_expired(), 0);

  let mut set_obj = GarnetObject::from(SetObject::default());
  assert_eq!(set_obj.purge_expired(), 0);

  info!("test_purge_expired_coordination passed");
  OK
}

#[test]
fn test_garnet_object_new_factory() -> Void {
  #[cfg(feature = "wedb_hash")]
  {
    let mut obj = GarnetObject::new(GarnetObjectType::Hash)?;
    assert_eq!(obj.object_type(), GarnetObjectType::Hash);
    assert_eq!(obj.len(), 0);
    assert!(obj.is_empty());
    assert!(obj.is_empty_ref());
  }
  #[cfg(feature = "wedb_zset")]
  {
    let mut obj = GarnetObject::new(GarnetObjectType::SortedSet)?;
    assert_eq!(obj.object_type(), GarnetObjectType::SortedSet);
    assert_eq!(obj.len(), 0);
    assert!(obj.is_empty());
    assert!(obj.is_empty_ref());
  }
  #[cfg(feature = "wedb_list")]
  {
    let mut obj = GarnetObject::new(GarnetObjectType::List)?;
    assert_eq!(obj.object_type(), GarnetObjectType::List);
    assert_eq!(obj.len(), 0);
    assert!(obj.is_empty());
    assert!(obj.is_empty_ref());
  }
  #[cfg(feature = "wedb_set")]
  {
    let mut obj = GarnetObject::new(GarnetObjectType::Set)?;
    assert_eq!(obj.object_type(), GarnetObjectType::Set);
    assert_eq!(obj.len(), 0);
    assert!(obj.is_empty());
    assert!(obj.is_empty_ref());
  }

  // Null 类型不能创建实体对象
  assert!(matches!(
    GarnetObject::new(GarnetObjectType::Null),
    Err(Error::NullObject)
  ));

  info!("test_garnet_object_new_factory passed");
  OK
}

#[cfg(all(
  feature = "wedb_hash",
  feature = "wedb_zset",
  feature = "wedb_list",
  feature = "wedb_set"
))]
#[test]
fn test_empty_objects_roundtrip() -> Void {
  for ty in [
    GarnetObjectType::Hash,
    GarnetObjectType::SortedSet,
    GarnetObjectType::List,
    GarnetObjectType::Set,
  ] {
    let mut original = GarnetObject::new(ty)?;
    assert!(original.is_empty());
    assert_eq!(original.len(), 0);
    let bytes = original.to_vec();
    assert_eq!(bytes[0], ty as u8);

    let mut restored = GarnetObject::deserialize(&bytes)?;
    assert_eq!(restored.object_type(), ty);
    assert!(restored.is_empty());
    assert!(restored.is_empty_ref());
    assert_eq!(restored.len(), 0);
    assert_eq!(restored.len_ref(), 0);
  }

  info!("test_empty_objects_roundtrip passed");
  OK
}

#[test]
fn test_corrupted_payload_penetration() -> Void {
  // 1. 只有类型字节而无任何 payload
  for &ty_byte in &[1u8, 2, 3, 4] {
    let res = GarnetObject::deserialize(&[ty_byte]);
    assert!(
      res.is_err(),
      "single byte payload for type {ty_byte} must fail"
    );
  }

  // 2. 伪造保留段字节（0xFC..=0xFF），带任意尾随数据
  for rsv in 0xFCu8..=0xFF {
    let data = [rsv, 0x01, 0x02, 0x03, 0x04];
    assert!(matches!(
      GarnetObject::deserialize(&data),
      Err(Error::UnsupportedFormatMarker(v)) if v == rsv
    ));
  }

  // 3. 畸变 payload 无法引发 panic，优雅返回 Err
  let fuzzed: &[&[u8]] = &[
    &[1, 0xFF, 0xFF, 0xFF, 0xFF], // SortedSet 损坏长度
    &[2, 0xFF, 0xFF, 0xFF, 0xFF], // List 损坏长度
    &[3, 0xFF, 0xFF, 0xFF, 0xFF], // Hash 损坏长度
    &[4, 0xFF, 0xFF, 0xFF, 0xFF], // Set 损坏长度
    &[1, 0x00],
    &[2, 0x00],
    &[3, 0x00],
    &[4, 0x00],
  ];
  for payload in fuzzed {
    let _ = GarnetObject::deserialize(payload); // 必须 safe 且不 panic
  }

  info!("test_corrupted_payload_penetration passed");
  OK
}
