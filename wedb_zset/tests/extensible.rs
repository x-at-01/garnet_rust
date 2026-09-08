use aok::{OK, Void};
use coarsetime::Clock;
use log::info;
use wedb_zset::{
  ExpireOpt, ExpireResult, ScoreRange, SortedSetObject, ZAddOpt, decode_sortable_f64,
  encode_sortable_f64,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_sortable_float_ordering() -> Void {
  let values = [
    f64::NEG_INFINITY,
    -1000.5,
    -100.0,
    -1.0,
    -0.0001,
    -0.0,
    0.0,
    0.0001,
    1.0,
    100.0,
    1000.5,
    f64::INFINITY,
  ];

  let encoded: Vec<[u8; 8]> = values.iter().map(|&v| encode_sortable_f64(v)).collect();

  // 校验解码还原无损
  for (i, &v) in values.iter().enumerate() {
    let decoded = decode_sortable_f64(encoded[i]);
    if v == 0.0 || v == -0.0 {
      assert_eq!(decoded, 0.0);
    } else {
      assert_eq!(decoded, v);
    }
  }

  // 校验大端字节序天然满足单调递增
  for i in 0..encoded.len() - 1 {
    // -0.0 与 +0.0 规范化为相等，其余严格递增
    if (values[i] == 0.0 && values[i + 1] == -0.0) || (values[i] == -0.0 && values[i + 1] == 0.0) {
      assert_eq!(encoded[i], encoded[i + 1]);
    } else {
      assert!(
        encoded[i] < encoded[i + 1],
        "values[{}]={} 应小于 values[{}]={}",
        i,
        values[i],
        i + 1,
        values[i + 1]
      );
    }
  }

  OK
}

#[test]
fn test_empty_zset_serialization() -> Void {
  let mut zset = SortedSetObject::new();
  let mut buf = Vec::new();
  zset.serialize(&mut buf);

  // 1 字节 version (1) + 4 字节 count (0) = 5 字节
  assert_eq!(buf.len(), 5);
  assert_eq!(buf[0], 1); // version 1
  assert_eq!(&buf[1..5], &[0, 0, 0, 0]);

  let mut restored = SortedSetObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 0);
  assert!(restored.is_empty());
  OK
}

#[test]
fn test_zset_extensible_serialization_roundtrip() -> Void {
  let mut zset = SortedSetObject::new();
  zset.zadd(10.5, b"user1".to_vec(), ZAddOpt::default())?;
  zset.zadd(-5.0, b"user2".to_vec(), ZAddOpt::default())?;
  zset.zadd(100.0, b"user3".to_vec(), ZAddOpt::default())?;

  let future = Clock::now_since_epoch().as_millis() + 50000;
  assert_eq!(
    zset.zexpire(b"user2", future, ExpireOpt::default()),
    ExpireResult::Ok
  );

  let mut buf = Vec::new();
  zset.serialize(&mut buf);

  assert_eq!(buf[0], 1); // version 1

  let mut restored = SortedSetObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 3);
  assert_eq!(restored.zscore(b"user1"), Some(10.5));
  assert_eq!(restored.zscore(b"user2"), Some(-5.0));
  assert_eq!(restored.zscore(b"user3"), Some(100.0));

  // 校验排序准确
  let range = restored.zrangebyscore(ScoreRange::new(-10.0, true, 50.0, true), false, 0, 10);
  let members: Vec<Vec<u8>> = range.into_iter().map(|(m, _)| m).collect();
  assert_eq!(members, vec![b"user2".to_vec(), b"user1".to_vec()]);

  assert_eq!(restored.zttl(b"user1"), -1);
  assert!(restored.zttl(b"user2") > 0);

  info!("test_zset_extensible_serialization_roundtrip passed");
  OK
}

#[test]
fn test_unsupported_version_rejection() {
  let buf = vec![99, 0, 0, 0, 0];
  let res = SortedSetObject::deserialize(&buf);
  assert!(res.is_err());
}

#[test]
fn test_corrupted_safeguards() {
  assert!(SortedSetObject::deserialize(&[]).is_err());
  assert!(SortedSetObject::deserialize(&[1, 0]).is_err());
  assert!(SortedSetObject::deserialize(&[1, 1, 0, 0, 0]).is_err());

  // 空集合正常 5 字节带尾部多余垃圾字节必须报错 CorruptedData
  let mut trailing_junk = vec![1, 0, 0, 0, 0];
  trailing_junk.push(0xAA);
  assert_eq!(
    SortedSetObject::deserialize(&trailing_junk).err(),
    Some(wedb_zset::Error::CorruptedData)
  );
}

#[test]
fn test_large_scale_roundtrip() -> Void {
  let mut zset = SortedSetObject::with_capacity(5000);
  for i in 0..5000u32 {
    let score = (i as f64) * 1.5;
    zset.zadd(score, i.to_le_bytes().to_vec(), ZAddOpt::default())?;
  }

  let mut buf = Vec::new();
  zset.serialize(&mut buf);

  let mut restored = SortedSetObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 5000);

  for i in (0..5000u32).step_by(100) {
    let expected_score = (i as f64) * 1.5;
    assert_eq!(restored.zscore(&i.to_le_bytes()), Some(expected_score));
  }

  OK
}
