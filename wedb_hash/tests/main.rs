use std::{thread::sleep, time::Duration};

use aok::{OK, Void};
use log::info;
use wedb_hash::{Error, ExpireOpt, ExpireResult, HashObject, MAX_RAND_SAMPLE_LIMIT, glob_match};
use whasher::HashSet;

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_basic_crud() -> Void {
  let mut hash = HashObject::new();
  assert!(hash.is_empty());
  assert_eq!(hash.len(), 0);

  // HSET
  assert!(hash.hset(b"f1".to_vec(), b"v1".to_vec()));
  assert_eq!(hash.len(), 1);
  assert_eq!(hash.hget(b"f1"), Some(b"v1".as_slice()));
  assert!(!hash.hset(b"f1".to_vec(), b"v1_new".to_vec()));
  assert_eq!(hash.hget(b"f1"), Some(b"v1_new".as_slice()));

  // HSETNX
  assert!(!hash.hsetnx(b"f1".to_vec(), b"v_other".to_vec()));
  assert!(hash.hsetnx(b"f2".to_vec(), b"v2".to_vec()));
  assert_eq!(hash.len(), 2);

  // HEXISTS & HSTRLEN
  assert!(hash.hexists(b"f1"));
  assert!(hash.hexists(b"f2"));
  assert!(!hash.hexists(b"f3"));
  assert_eq!(hash.hstrlen(b"f1"), 6); // "v1_new" length is 6
  assert_eq!(hash.hstrlen(b"nonexistent"), 0);

  // HMGET
  let res = hash.hmget(&[b"f1", b"f2", b"f3"]);
  assert_eq!(res[0], Some(b"v1_new".to_vec()));
  assert_eq!(res[1], Some(b"v2".to_vec()));
  assert_eq!(res[2], None);

  // HKEYS & HVALS & HGETALL
  let keys = hash.hkeys();
  assert_eq!(keys.len(), 2);
  let vals = hash.hvals();
  assert_eq!(vals.len(), 2);
  let all = hash.hgetall();
  assert_eq!(all.len(), 2);

  // HDEL
  assert_eq!(hash.hdel(&[b"f1", b"f3"]), 1);
  assert_eq!(hash.len(), 1);
  assert!(!hash.hexists(b"f1"));
  assert!(hash.hexists(b"f2"));

  info!("test_basic_crud passed");
  OK
}

#[test]
fn test_numeric_increments() -> Void {
  let mut hash = HashObject::new();

  // HINCRBY
  assert_eq!(hash.hincrby(b"count", 10)?, 10);
  assert_eq!(hash.hincrby(b"count", -3)?, 7);
  assert_eq!(hash.hget(b"count"), Some(b"7".as_slice()));

  // HINCRBYFLOAT
  let f = hash.hincrbyfloat(b"fcount", 10.5)?;
  assert!((f - 10.5).abs() < 1e-6);
  let f2 = hash.hincrbyfloat(b"fcount", -0.25)?;
  assert!((f2 - 10.25).abs() < 1e-6);

  info!("test_numeric_increments passed");
  OK
}

#[test]
fn test_random_fields() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"k1".to_vec(), b"v1".to_vec());
  hash.hset(b"k2".to_vec(), b"v2".to_vec());
  hash.hset(b"k3".to_vec(), b"v3".to_vec());

  let sampled = hash.hrandfield(2, true);
  assert_eq!(sampled.len(), 2);
  assert!(sampled[0].1.is_some());

  let dup_sampled = hash.hrandfield(-5, false);
  assert_eq!(dup_sampled.len(), 5);
  assert!(dup_sampled[0].1.is_none());

  info!("test_random_fields passed");
  OK
}

#[test]
fn test_field_expiration() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"f1".to_vec(), b"v1".to_vec());

  // HEXPIRE
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  let expire_at = now + 100; // 100ms
  assert_eq!(
    hash.hexpire(b"f1", expire_at, ExpireOpt::NONE),
    ExpireResult::Ok
  );
  assert!(hash.httl(b"f1") > 0);

  // 条件选项验证
  assert_eq!(
    hash.hexpire(
      b"f1",
      expire_at + 10,
      ExpireOpt {
        nx: true,
        ..Default::default()
      }
    ),
    ExpireResult::ExpireConditionNotMet
  );
  assert_eq!(
    hash.hexpire(
      b"f1",
      expire_at + 10,
      ExpireOpt {
        gt: true,
        ..Default::default()
      }
    ),
    ExpireResult::Ok
  );

  // HPERSIST
  assert_eq!(hash.hpersist(b"f1"), ExpireResult::Ok);
  assert_eq!(hash.httl(b"f1"), -1);
  assert_eq!(hash.hpersist(b"f1"), ExpireResult::NoExpirationSet);
  // 不存在的字段返回 -2 (对标 C# Persist 三态语义)
  assert_eq!(hash.hpersist(b"ghost"), ExpireResult::KeyNotFound);

  // 重新设置短暂过期并等待
  let short_expire = coarsetime::Clock::now_since_epoch().as_millis() + 50;
  assert_eq!(
    hash.hexpire(b"f1", short_expire, ExpireOpt::NONE),
    ExpireResult::Ok
  );
  sleep(Duration::from_millis(60));

  assert_eq!(hash.hget(b"f1"), None);
  assert_eq!(hash.httl(b"f1"), -2);
  assert_eq!(hash.len(), 0);

  info!("test_field_expiration passed");
  OK
}

#[test]
fn test_serialization_roundtrip() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"key1".to_vec(), b"val1".to_vec());
  hash.hset(b"key2".to_vec(), b"val2".to_vec());

  let future_expire = coarsetime::Clock::now_since_epoch().as_millis() + 10000;
  assert_eq!(
    hash.hexpire(b"key2", future_expire, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  let mut buf = Vec::new();
  hash.serialize(&mut buf);

  let mut restored = HashObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 2);
  assert_eq!(restored.hget(b"key1"), Some(b"val1".as_slice()));
  assert_eq!(restored.hget(b"key2"), Some(b"val2".as_slice()));
  assert_eq!(restored.httl(b"key1"), -1);
  assert!(restored.httl(b"key2") > 0);

  info!("test_serialization_roundtrip passed");
  OK
}

#[test]
fn test_numeric_increment_errors() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"text".to_vec(), b"not_a_number".to_vec());

  // HINCRBY 解析非数字应返回 InvalidNumber
  assert_eq!(hash.hincrby(b"text", 1).unwrap_err(), Error::InvalidNumber);

  // HINCRBYFLOAT 解析非数字应返回 InvalidNumber
  assert_eq!(
    hash.hincrbyfloat(b"text", 1.5).unwrap_err(),
    Error::InvalidNumber
  );

  // HINCRBYFLOAT 传入 NaN 或 Infinity 应返回 InvalidNumber
  assert_eq!(
    hash.hincrbyfloat(b"f", f64::NAN).unwrap_err(),
    Error::InvalidNumber
  );
  assert_eq!(
    hash.hincrbyfloat(b"f", f64::INFINITY).unwrap_err(),
    Error::InvalidNumber
  );

  info!("test_numeric_increment_errors passed");
  OK
}

#[test]
fn test_hrandfield_edge_cases() -> Void {
  let mut hash = HashObject::new();
  // 空哈希对象
  assert!(hash.hrandfield(1, false).is_empty());
  assert!(hash.hrandfield(-5, true).is_empty());
  assert!(hash.hrandfield(0, false).is_empty());

  for i in 0..10 {
    hash.hset(format!("k_{i}").into_bytes(), format!("v_{i}").into_bytes());
  }

  // count = 1
  let one = hash.hrandfield(1, true);
  assert_eq!(one.len(), 1);
  assert!(one[0].1.is_some());

  // count == total
  let all = hash.hrandfield(10, false);
  assert_eq!(all.len(), 10);

  // count > total
  let more = hash.hrandfield(20, false);
  assert_eq!(more.len(), 10); // 至多返回 total 个不重复项

  // 负数 count：允许重复
  let dups = hash.hrandfield(-30, true);
  assert_eq!(dups.len(), 30);

  info!("test_hrandfield_edge_cases passed");
  OK
}

#[test]
fn test_large_serialization_roundtrip() -> Void {
  let mut hash = HashObject::new();
  let now = coarsetime::Clock::now_since_epoch().as_millis();

  for i in 0..500 {
    let k = format!("key_{i:04}").into_bytes();
    let v = format!("val_{i:04}").into_bytes();
    hash.hset(k.clone(), v);

    if i % 3 == 0 {
      // 1/3 的字段设置未来过期
      hash.hexpire(&k, now + 60_000, ExpireOpt::NONE);
    }
  }

  assert_eq!(hash.len(), 500);

  let mut buf = Vec::new();
  hash.serialize(&mut buf);

  let mut restored = HashObject::deserialize(&buf)?;
  assert_eq!(restored.len(), 500);

  for i in 0..500 {
    let k = format!("key_{i:04}").into_bytes();
    let expected_v = format!("val_{i:04}").into_bytes();
    assert_eq!(restored.hget(&k), Some(expected_v.as_slice()));

    if i % 3 == 0 {
      assert!(restored.httl(&k) > 0);
    } else {
      assert_eq!(restored.httl(&k), -1);
    }
  }

  info!("test_large_serialization_roundtrip passed");
  OK
}

#[test]
fn test_hscan_ref_expired_no_dead_loop() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"f1".to_vec(), b"v1".to_vec());
  hash.hset(b"f2".to_vec(), b"v2".to_vec());
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  // f1 expires immediately
  hash.hexpire(b"f1", now, ExpireOpt::NONE);

  // hscan_ref without delete_expired should scan non-expired f2 and terminate with next_cursor = 0
  let (next_cur, items) = hash.hscan_ref(0, 10, None);
  assert_eq!(next_cur, 0, "next_cursor must be 0 to terminate scan");
  assert_eq!(items.len(), 1);
  assert_eq!(items[0].0, b"f2");

  OK
}

#[test]
fn test_hexpire_option_precedence_over_past_timestamp() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"f1".to_vec(), b"v1".to_vec());
  hash.hset(b"f2".to_vec(), b"v2".to_vec());

  let now = coarsetime::Clock::now_since_epoch().as_millis();
  // f1 设置了未来过期
  assert_eq!(
    hash.hexpire(b"f1", now + 60_000, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  // 1. NX 选项：仅在未设置过期时允许。若已设置过期，哪怕传入过去时间戳，也必须先被 NX 拦截，绝不能误删字段
  assert_eq!(
    hash.hexpire(
      b"f1",
      now.saturating_sub(1000),
      ExpireOpt {
        nx: true,
        ..Default::default()
      }
    ),
    ExpireResult::ExpireConditionNotMet
  );
  assert!(hash.hexists(b"f1")); // 字段必须依然存在

  // 2. XX 选项：仅在已设置过期时允许。f2 未设置过期，传入过去时间戳应被 XX 拦截，绝不能误删字段
  assert_eq!(
    hash.hexpire(
      b"f2",
      now.saturating_sub(1000),
      ExpireOpt {
        xx: true,
        ..Default::default()
      }
    ),
    ExpireResult::ExpireConditionNotMet
  );
  assert!(hash.hexists(b"f2")); // 字段必须依然存在

  // 3. 条件满足且时间戳在过去时，正确删除字段并返回 KeyAlreadyExpired
  assert_eq!(
    hash.hexpire(
      b"f1",
      now.saturating_sub(1000),
      ExpireOpt {
        xx: true,
        ..Default::default()
      }
    ),
    ExpireResult::KeyAlreadyExpired
  );
  assert!(!hash.hexists(b"f1"));

  info!("test_hexpire_option_precedence_over_past_timestamp passed");
  OK
}

#[test]
fn test_readonly_methods_and_purge_expired() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"f1".to_vec(), b"v1".to_vec());
  hash.hset(b"f2".to_vec(), b"v2".to_vec());
  hash.hset(b"f3".to_vec(), b"v3".to_vec());

  let now = coarsetime::Clock::now_since_epoch().as_millis();
  hash.hexpire(b"f1", now + 60_000, ExpireOpt::NONE);
  hash.hexpire(b"f2", now + 20, ExpireOpt::NONE);
  sleep(Duration::from_millis(30));

  // 只读 httl_ref
  assert!(hash.httl_ref(b"f1") > 0);
  assert_eq!(hash.httl_ref(b"f2"), -2); // 已过期
  assert_eq!(hash.httl_ref(b"f3"), -1); // 未设置过期
  assert_eq!(hash.httl_ref(b"nonexistent"), -2);

  // 只读 hmget_ref (零拷贝切片借用)
  let vals = hash.hmget_ref(&[b"f1", b"f2", b"f3", b"not_found"]);
  assert_eq!(vals[0], Some(b"v1".as_slice()));
  assert_eq!(vals[1], None); // 已过期
  assert_eq!(vals[2], Some(b"v3".as_slice()));
  assert_eq!(vals[3], None);

  // purge_expired 物理清理，返回清理的过期字段数
  let purged_count = hash.purge_expired();
  assert_eq!(purged_count, 1);
  assert_eq!(hash.len(), 2);
  assert_eq!(hash.purge_expired(), 0); // 再次清理为 0

  info!("test_readonly_methods_and_purge_expired passed");
  OK
}

#[test]
fn test_hincrbyfloat_zero_normalization_and_defense() -> Void {
  let mut hash = HashObject::new();

  // -0.5 + 0.5 正常产生 0.0 而非 -0.0，且整数值以整数格式存储 (对齐 Redis ld2string)
  hash.hset(b"f".to_vec(), b"-0.5".to_vec());
  let res = hash.hincrbyfloat(b"f", 0.5)?;
  assert_eq!(res, 0.0);
  assert_eq!(hash.hget(b"f"), Some(b"0".as_slice()));

  // 现有字段字符串为非有限数（如 nan / inf）时防御拦截
  hash.hset(b"bad".to_vec(), b"inf".to_vec());
  assert_eq!(
    hash.hincrbyfloat(b"bad", 1.0).unwrap_err(),
    Error::InvalidNumber
  );

  hash.hset(b"bad_nan".to_vec(), b"nan".to_vec());
  assert_eq!(
    hash.hincrbyfloat(b"bad_nan", 1.0).unwrap_err(),
    Error::InvalidNumber
  );

  info!("test_hincrbyfloat_zero_normalization_and_defense passed");
  OK
}

#[test]
fn test_glob_match_escapes_and_brackets() -> Void {
  // 转义字符支持
  assert!(glob_match(br"\*", b"*"));
  assert!(!glob_match(br"\*", b"a"));
  assert!(glob_match(br"\?", b"?"));
  assert!(!glob_match(br"\?", b"a"));
  assert!(glob_match(br"\[a\]", b"[a]"));

  // 连续 * 折叠与性能保障
  assert!(glob_match(b"***a***b***", b"a_mid_b"));
  assert!(glob_match(b"***", b"anything"));

  // 中括号内转义与范围（'!' 为字面集合成员，对齐 C# GlobUtils L49）
  assert!(glob_match(b"[\\]]", b"]"));
  assert!(glob_match(b"[a-z]", b"k"));
  assert!(!glob_match(b"[a-z]", b"K"));
  assert!(!glob_match(b"[!0-9]", b"x"));
  assert!(glob_match(b"[!0-9]", b"!"));
  assert!(glob_match(b"[!0-9]", b"5"));

  info!("test_glob_match_escapes_and_brackets passed");
  OK
}

#[test]
fn test_container_convergence_on_empty() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"k1".to_vec(), b"v1".to_vec());
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  // 设置短暂过期并等待过期
  hash.hexpire(b"k1", now + 20, ExpireOpt::NONE);
  sleep(Duration::from_millis(30));

  // 所有字段均已过期
  assert_eq!(hash.len_ref(), 0);
  assert!(hash.is_empty_ref());

  // 清理后结构彻底收敛
  assert_eq!(hash.delete_expired(), 1);
  assert_eq!(hash.len(), 0);
  assert!(hash.is_empty());

  // HDEL 清空容器后结构彻底收敛
  hash.hset(b"k2".to_vec(), b"v2".to_vec());
  hash.hexpire(b"k2", now + 50_000, ExpireOpt::NONE);
  assert_eq!(hash.hdel(&[b"k2"]), 1);
  assert_eq!(hash.len(), 0);
  assert!(hash.is_empty());

  OK
}

#[test]
fn test_httl_and_hexpire_semantics() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"field1".to_vec(), b"value1".to_vec());
  hash.hset(b"field2".to_vec(), b"value2".to_vec());

  let now = coarsetime::Clock::now_since_epoch().as_millis();
  // 1. 无过期设置时：-1
  assert_eq!(hash.httl(b"field1"), -1);
  assert_eq!(hash.httl_ref(b"field1"), -1);

  // 2. 字段不存在时：-2
  assert_eq!(hash.httl(b"no_such_field"), -2);
  assert_eq!(hash.httl_ref(b"no_such_field"), -2);

  // 3. 设置毫秒级过期时间戳
  let exp_target = now + 10_000;
  assert_eq!(
    hash.hexpire(b"field1", exp_target, ExpireOpt::NONE),
    ExpireResult::Ok
  );
  assert!(hash.httl(b"field1") > 0);
  assert!(hash.httl_ref(b"field1") > 0);

  // 4. hexpire 条件约束 (XX/NX/GT/LT)
  let opt_nx = ExpireOpt {
    nx: true,
    ..Default::default()
  };
  // 已有过期时间，NX 失败
  assert_eq!(
    hash.hexpire(b"field1", exp_target + 5000, opt_nx),
    ExpireResult::ExpireConditionNotMet
  );

  let opt_xx = ExpireOpt {
    xx: true,
    ..Default::default()
  };
  // 已有过期时间，XX 成功
  assert_eq!(
    hash.hexpire(b"field1", exp_target + 5000, opt_xx),
    ExpireResult::Ok
  );

  // 5. 过期后查询返回 -2
  let past = now.saturating_sub(100);
  assert_eq!(
    hash.hexpire(b"field2", past, ExpireOpt::NONE),
    ExpireResult::KeyAlreadyExpired
  );
  assert_eq!(hash.httl(b"field2"), -2);
  assert_eq!(hash.httl_ref(b"field2"), -2);

  OK
}

#[test]
fn test_zero_copy_views_and_iterators() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"alpha".to_vec(), b"1".to_vec());
  hash.hset(b"beta".to_vec(), b"2".to_vec());
  hash.hset(b"gamma".to_vec(), b"3".to_vec());

  let now = coarsetime::Clock::now_since_epoch().as_millis();
  hash.hexpire(b"gamma", now.saturating_sub(10), ExpireOpt::NONE);

  // 1. hkeys_iter 只迭代未过期的 alpha 和 beta
  let keys: Vec<&[u8]> = hash.hkeys_iter().collect();
  assert_eq!(keys.len(), 2);
  assert!(keys.contains(&b"alpha".as_slice()));
  assert!(keys.contains(&b"beta".as_slice()));

  // 2. hvals_iter
  let vals: Vec<&[u8]> = hash.hvals_iter().collect();
  assert_eq!(vals.len(), 2);
  assert!(vals.contains(&b"1".as_slice()));
  assert!(vals.contains(&b"2".as_slice()));

  // 3. iter_valid (HGETALL 零拷贝遍历)
  let kvs: Vec<(&[u8], &[u8])> = hash.iter_valid().collect();
  assert_eq!(kvs.len(), 2);

  // 4. len 与 len_ref
  assert_eq!(hash.len_ref(), 2);
  assert_eq!(hash.len(), 2);

  OK
}

#[test]
fn test_hrandfield_ref_zero_copy() -> Void {
  let mut hash = HashObject::new();
  for i in 0..10u8 {
    let k = vec![b'k', i];
    let v = vec![b'v', i];
    hash.hset(k, v);
  }

  // 1. 单个随机采样
  let one = hash.hrandfield_ref(1, true);
  assert_eq!(one.len(), 1);
  assert!(one[0].0.starts_with(b"k"));
  assert!(one[0].1.is_some());

  // 2. 多个不重复随机采样 (count = 5)
  let five = hash.hrandfield_ref(5, false);
  assert_eq!(five.len(), 5);
  for item in &five {
    assert!(item.1.is_none());
  }

  // 3. 有放回随机采样 (count = -8)
  let eight = hash.hrandfield_ref(-8, true);
  assert_eq!(eight.len(), 8);

  // 4. 边界采样 count = 0
  let zero = hash.hrandfield_ref(0, true);
  assert!(zero.is_empty());

  OK
}

#[test]
fn test_hscan_borrowed_zero_copy() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"prefix:1".to_vec(), b"v1".to_vec());
  hash.hset(b"prefix:2".to_vec(), b"v2".to_vec());
  hash.hset(b"other:3".to_vec(), b"v3".to_vec());

  // 只匹配 prefix:*
  let (cur, items) = hash.hscan_borrowed(0, 10, Some(b"prefix:*"));
  assert_eq!(cur, 0); // 扫描完毕，游标为 0
  assert_eq!(items.len(), 2);
  assert!(items.iter().all(|(k, _)| k.starts_with(b"prefix:")));

  // 分页流式扫描
  let (next_cur, items1) = hash.hscan_borrowed(0, 1, None);
  assert_eq!(items1.len(), 1);
  assert!(next_cur > 0);

  let (final_cur, items2) = hash.hscan_borrowed(next_cur, 10, None);
  assert_eq!(final_cur, 0);
  assert_eq!(items2.len(), 2);

  OK
}

#[test]
fn test_shrink_to_fit_and_large_empty_convergence() -> Void {
  let mut hash = HashObject::with_capacity(500);
  for i in 0..300u32 {
    let k = i.to_le_bytes().to_vec();
    let v = (i * 2).to_le_bytes().to_vec();
    hash.hset(k, v);
  }
  assert_eq!(hash.len(), 300);

  hash.shrink_to_fit();
  assert_eq!(hash.len(), 300);

  // 清空所有字段
  for i in 0..300u32 {
    let k = i.to_le_bytes();
    hash.hdel(&[&k]);
  }
  assert_eq!(hash.len(), 0);
  assert!(hash.is_empty());

  // 手动再次收缩
  hash.shrink_to_fit();
  assert_eq!(hash.len_ref(), 0);

  OK
}

#[test]
fn test_complex_glob_match_exhaustive() {
  // 1. 转义字符
  assert!(glob_match(b"\\*", b"*"));
  assert!(!glob_match(b"\\*", b"a"));
  assert!(glob_match(b"\\[abc\\]", b"[abc]"));
  assert!(glob_match(b"a\\\\b", b"a\\b"));
  assert!(glob_match(b"\\?", b"?"));

  // 2. 复杂字符集与取反（仅 '^' 取反；'!' 属字面成员，集合 {'!'} ∪ a-z，大小写敏感故不命中 'A'）
  assert!(glob_match(b"[^0-9]", b"a"));
  assert!(!glob_match(b"[^0-9]", b"5"));
  assert!(!glob_match(b"[!a-z]", b"A"));
  assert!(glob_match(b"[!a-z]", b"!"));
  assert!(glob_match(b"[!a-z]", b"c"));
  assert!(glob_match(b"[a-cx-z]", b"b"));
  assert!(glob_match(b"[a-cx-z]", b"y"));
  assert!(!glob_match(b"[a-cx-z]", b"m"));

  // 3. 范围反转与区间右端点 ']'（C# L75-95：'a-]' 消费为区间，']' 不终止集合）
  assert!(glob_match(b"[z-a]", b"m")); // low=a, high=z 自动归一化
  assert!(!glob_match(b"[a-]", b"-")); // 'a-]' 的 ']' 为区间上界，范围为 93(']')..=97('a')
  assert!(glob_match(b"[a-]", b"^")); // 94 在 93..=97 之间
  assert!(glob_match(b"[-a]", b"-"));

  // 4. 未闭合中括号
  assert!(glob_match(b"[abc", b"a"));
  assert!(glob_match(b"[abc", b"b"));
  assert!(!glob_match(b"[abc", b"d"));

  // 5. 连续星号与空匹配（目标为空仅空模式命中，C# GlobUtils L19/L158）
  assert!(glob_match(b"***", b"anything"));
  assert!(glob_match(b"***a***b***", b"---a---b---"));
  assert!(glob_match(b"", b""));
  assert!(!glob_match(b"", b"non_empty"));
  assert!(!glob_match(b"*", b""));
}

#[test]
fn test_ttl_repeated_in_place_updates_and_heap_hygiene() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"dyn_field".to_vec(), b"val".to_vec());

  let now = coarsetime::Clock::now_since_epoch().as_millis();

  // 1. 首次设置过期：now + 100ms
  let t1 = now + 100;
  assert_eq!(
    hash.hexpire(b"dyn_field", t1, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  // 2. 原地多次更新过期时间戳（GT 提升至 now + 500ms）
  let t2 = now + 500;
  assert_eq!(
    hash.hexpire(
      b"dyn_field",
      t2,
      ExpireOpt {
        gt: true,
        ..Default::default()
      }
    ),
    ExpireResult::Ok
  );

  // 3. 再次原地更新（LT 降低至 now + 300ms）
  let t3 = now + 300;
  assert_eq!(
    hash.hexpire(
      b"dyn_field",
      t3,
      ExpireOpt {
        lt: true,
        ..Default::default()
      }
    ),
    ExpireResult::Ok
  );

  // 模拟大量原地更新产生堆幽灵条目，验证 shrink_to_fit 清理幽灵条目
  for i in 1..=20 {
    let t_tmp = t3 + i * 10;
    assert_eq!(
      hash.hexpire(
        b"dyn_field",
        t_tmp,
        ExpireOpt {
          gt: true,
          ..Default::default()
        }
      ),
      ExpireResult::Ok
    );
  }
  let final_target_exp = t3 + 200; // t3 + 20*10

  // 堆中已有 > 20 个历史废弃条目，触发 shrink_to_fit 堆重构
  hash.shrink_to_fit();

  // 等待最初的 t1 (100ms) 过去
  sleep(Duration::from_millis(120));

  // 绝不能因为残留的 t1 堆条目误删更新后的字段！
  assert_eq!(hash.delete_expired(), 0);
  assert!(hash.hexists(b"dyn_field"));
  assert!(hash.httl(b"dyn_field") > 0);
  assert_eq!(hash.hget(b"dyn_field"), Some(b"val".as_slice()));

  // 等待最终目标时间过去后，字段正常被清理
  let current_now = coarsetime::Clock::now_since_epoch().as_millis();
  if final_target_exp > current_now {
    sleep(Duration::from_millis((final_target_exp - current_now) + 50));
  }
  assert_eq!(hash.delete_expired(), 1);
  assert!(!hash.hexists(b"dyn_field"));
  assert_eq!(hash.httl(b"dyn_field"), -2);

  OK
}

#[test]
fn test_numeric_increment_boundary_overflows() -> Void {
  let mut hash = HashObject::new();

  // 1. i64 上溢出拦截
  hash.hset(b"max_i64".to_vec(), i64::MAX.to_string().into_bytes());
  assert_eq!(
    hash.hincrby(b"max_i64", 1).unwrap_err(),
    Error::InvalidNumber
  );
  // 保持原有值未被破坏
  assert_eq!(hash.hget(b"max_i64"), Some(i64::MAX.to_string().as_bytes()));

  // 2. i64 下溢出拦截
  hash.hset(b"min_i64".to_vec(), i64::MIN.to_string().into_bytes());
  assert_eq!(
    hash.hincrby(b"min_i64", -1).unwrap_err(),
    Error::InvalidNumber
  );
  assert_eq!(hash.hget(b"min_i64"), Some(i64::MIN.to_string().as_bytes()));

  // 3. f64 加法导致超出有限数范围 (溢出至 Infinity)
  hash.hset(b"huge_f64".to_vec(), f64::MAX.to_string().into_bytes());
  assert_eq!(
    hash.hincrbyfloat(b"huge_f64", f64::MAX).unwrap_err(),
    Error::InvalidNumber
  );

  // 4. -1.0 + 1.0 产生的 0 规整为 +0.0 并以整数格式存储
  hash.hset(b"zero_test".to_vec(), b"-1.0".to_vec());
  let res = hash.hincrbyfloat(b"zero_test", 1.0)?;
  assert_eq!(res, 0.0);
  assert_eq!(hash.hget(b"zero_test"), Some(b"0".as_slice()));

  OK
}

#[test]
fn test_hexpireat_and_hpexpireat() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"f_sec".to_vec(), b"v_sec".to_vec());
  hash.hset(b"f_ms".to_vec(), b"v_ms".to_vec());

  let now = coarsetime::Clock::now_since_epoch().as_millis();
  let now_sec = now / 1000;

  // 1. HEXPIREAT 设置秒级时间戳 (未来 60 秒)
  let exp_sec = now_sec + 60;
  assert_eq!(
    hash.hexpireat(b"f_sec", exp_sec, ExpireOpt::NONE),
    ExpireResult::Ok
  );
  assert!(hash.httl(b"f_sec") > 0);

  // 2. HPEXPIREAT 设置毫秒级时间戳 (未来 60000 毫秒)
  let exp_ms = now + 60_000;
  assert_eq!(
    hash.hpexpireat(b"f_ms", exp_ms, ExpireOpt::NONE),
    ExpireResult::Ok
  );
  assert!(hash.httl(b"f_ms") > 0);

  // 3. HEXPIREAT 传入过去时间戳删除
  assert_eq!(
    hash.hexpireat(b"f_sec", now_sec.saturating_sub(10), ExpireOpt::NONE),
    ExpireResult::KeyAlreadyExpired
  );
  assert!(!hash.hexists(b"f_sec"));
  assert_eq!(hash.httl(b"f_sec"), -2);

  // 4. HPEXPIREAT 传入过去时间戳删除
  assert_eq!(
    hash.hpexpireat(b"f_ms", now.saturating_sub(100), ExpireOpt::NONE),
    ExpireResult::KeyAlreadyExpired
  );
  assert!(!hash.hexists(b"f_ms"));
  assert_eq!(hash.httl(b"f_ms"), -2);

  OK
}

#[test]
fn test_hexpiretime_and_hpexpiretime() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"f_persist".to_vec(), b"val1".to_vec());
  hash.hset(b"f_exp".to_vec(), b"val2".to_vec());

  // 未设置过期时间的字段
  assert_eq!(hash.hexpiretime(b"f_persist"), -1);
  assert_eq!(hash.hexpiretime_ref(b"f_persist"), -1);
  assert_eq!(hash.hpexpiretime(b"f_persist"), -1);
  assert_eq!(hash.hpexpiretime_ref(b"f_persist"), -1);

  // 不存在的字段
  assert_eq!(hash.hexpiretime(b"not_found"), -2);
  assert_eq!(hash.hexpiretime_ref(b"not_found"), -2);
  assert_eq!(hash.hpexpiretime(b"not_found"), -2);
  assert_eq!(hash.hpexpiretime_ref(b"not_found"), -2);

  // 设置毫秒级过期时间戳
  let exp_target_ms = 1_900_000_000_000u64;
  assert_eq!(
    hash.hexpire(b"f_exp", exp_target_ms, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  // hexpiretime 返回秒级时间戳
  assert_eq!(hash.hexpiretime(b"f_exp"), (exp_target_ms / 1000) as i64);
  assert_eq!(
    hash.hexpiretime_ref(b"f_exp"),
    (exp_target_ms / 1000) as i64
  );

  // hpexpiretime 返回毫秒级时间戳
  assert_eq!(hash.hpexpiretime(b"f_exp"), exp_target_ms as i64);
  assert_eq!(hash.hpexpiretime_ref(b"f_exp"), exp_target_ms as i64);

  // 过去时间戳判定
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  hash.hset(b"f_past".to_vec(), b"val".to_vec());
  hash.hexpire(b"f_past", now + 20, ExpireOpt::NONE);
  sleep(Duration::from_millis(30));

  assert_eq!(hash.hexpiretime(b"f_past"), -2);
  assert_eq!(hash.hexpiretime_ref(b"f_past"), -2);
  assert_eq!(hash.hpexpiretime(b"f_past"), -2);
  assert_eq!(hash.hpexpiretime_ref(b"f_past"), -2);

  OK
}

#[test]
fn test_serialize_ref() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"a".to_vec(), b"1".to_vec());
  hash.hset(b"b".to_vec(), b"2".to_vec());
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  hash.hexpire(b"b", now + 60_000, ExpireOpt::NONE);

  // 只读序列化
  let mut buf = Vec::new();
  hash.serialize_ref(&mut buf);
  assert!(!buf.is_empty());

  // 反序列化还原
  let restored = HashObject::deserialize(&buf)?;
  assert_eq!(restored.len_ref(), 2);
  assert_eq!(restored.hget_ref(b"a"), Some(b"1".as_slice()));
  assert_eq!(restored.hget_ref(b"b"), Some(b"2".as_slice()));
  assert_eq!(restored.httl_ref(b"a"), -1);
  assert!(restored.httl_ref(b"b") > 0);

  OK
}

/// 对标 C# CanSetWithExpireAndRemoveExpireByCallingSetAgain：HSET 覆盖存活字段清除其 TTL
#[test]
fn test_hset_overwrite_clears_ttl() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"f".to_vec(), b"v1".to_vec());
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  assert_eq!(
    hash.hexpire(b"f", now + 50, ExpireOpt::NONE),
    ExpireResult::Ok
  );

  // 覆盖写入后字段转为持久化
  assert!(!hash.hset(b"f".to_vec(), b"v2".to_vec()));
  assert_eq!(hash.httl(b"f"), -1);
  assert_eq!(hash.hget(b"f"), Some(b"v2".as_slice()));

  // 等待原过期时刻过去，字段必须依然存活
  sleep(Duration::from_millis(60));
  assert_eq!(hash.hget(b"f"), Some(b"v2".as_slice()));

  OK
}

/// 对标 C# CanDoHIncrByWithExpire：HINCRBY/HINCRBYFLOAT 保留字段既有 TTL
#[test]
fn test_hincrby_preserves_ttl() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"n".to_vec(), b"1".to_vec());
  hash.hset(b"fl".to_vec(), b"1.5".to_vec());
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  hash.hexpire(b"n", now + 60, ExpireOpt::NONE);
  hash.hexpire(b"fl", now + 60, ExpireOpt::NONE);

  assert_eq!(hash.hincrby(b"n", -4)?, -3);
  assert!(hash.httl(b"n") > 0, "HINCRBY 不得清除字段 TTL");

  assert!((hash.hincrbyfloat(b"fl", 2.0)? - 3.5).abs() < 1e-9);
  assert!(hash.httl(b"fl") > 0, "HINCRBYFLOAT 不得清除字段 TTL");

  // 等待 TTL 到期后，字段整体消失，自增从增量重新起算
  sleep(Duration::from_millis(70));
  assert_eq!(hash.hincrby(b"n", -1)?, -1);
  assert_eq!(hash.httl(b"n"), -1);

  OK
}

/// HINCRBYFLOAT 结果为整数值时以整数格式写回 (对齐 Redis ld2string / C# TryFormat)
#[test]
fn test_hincrbyfloat_integer_format() -> Void {
  let mut hash = HashObject::new();

  // 5.0 → "5"，可被后续 HINCRBY 继续操作
  hash.hincrbyfloat(b"a", 5.0)?;
  assert_eq!(hash.hget(b"a"), Some(b"5".as_slice()));
  assert_eq!(hash.hincrby(b"a", 3)?, 8);

  // 大整数不落科学计数法
  hash.hincrbyfloat(b"b", 1e18)?;
  assert_eq!(hash.hget(b"b"), Some(b"1000000000000000000".as_slice()));

  // 小数保持最短往返格式
  hash.hincrbyfloat(b"c", 10.25)?;
  assert_eq!(hash.hget(b"c"), Some(b"10.25".as_slice()));

  // 超出 i64 范围的整数值保留浮点格式，且仍可精确读回
  let huge = hash.hincrbyfloat(b"d", 1e19)?;
  assert_eq!(huge, 1e19);

  OK
}

/// HDEL 对已过期字段视作不存在：物理清除但不计入删除数
#[test]
fn test_hdel_expired_field_not_counted() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"alive".to_vec(), b"v1".to_vec());
  hash.hset(b"dead".to_vec(), b"v2".to_vec());
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  hash.hexpire(b"dead", now.saturating_sub(10), ExpireOpt::NONE);

  // 只统计存活字段 dead 不计入
  assert_eq!(hash.hdel(&[b"alive", b"dead", b"ghost"]), 1);
  assert_eq!(hash.len(), 0);
  assert!(!hash.hexists(b"dead"));

  OK
}

/// HSCAN 大字段表多页扫描：游标严格单调推进至 0 终止，全字段恰好覆盖一次 (回绕/终止保证)
#[test]
fn test_hscan_multipage_full_coverage() -> Void {
  let mut hash = HashObject::new();
  let total = 137u32;
  for i in 0..total {
    hash.hset(format!("f{i:03}").into_bytes(), vec![b'v']);
  }

  // 无 pattern：COUNT 7 分页，全量覆盖且无重复
  let (mut cur, mut seen) = (0usize, HashSet::default());
  let mut pages = 0;
  loop {
    let (next, items) = hash.hscan(cur, 7, None);
    assert!(items.len() <= 7);
    for (k, _) in items {
      assert!(seen.insert(k.clone()), "字段被重复返回: {k:?}");
    }
    pages += 1;
    assert_ne!(next, cur, "游标必须严格推进或归零，不得原地打转");
    cur = next;
    if cur == 0 {
      break;
    }
    assert!(pages < total, "游标未在有限页内终止");
  }
  assert_eq!(seen.len(), total as usize, "多页扫描必须覆盖全部字段");

  // MATCH + COUNT 组合：只返回命中项，游标仍对全部存活条目计数推进至终止
  for i in 0..50u32 {
    hash.hset(format!("batch:{i:03}").into_bytes(), vec![b'b']);
  }
  let (mut cur, mut matched) = (0usize, HashSet::default());
  loop {
    let (next, items) = hash.hscan(cur, 3, Some(b"batch:*"));
    assert!(items.iter().all(|(k, _)| k.starts_with(b"batch:")));
    for (k, _) in items {
      assert!(matched.insert(k));
    }
    cur = next;
    if cur == 0 {
      break;
    }
  }
  assert_eq!(matched.len(), 50);
  // 未命中 pattern 的原字段不受扫描影响
  assert_eq!(hash.len(), (total + 50) as usize);

  OK
}

/// HINCRBYFLOAT 作用于已过期字段：等价于从 0.0 起算 (字段重生无残留 TTL)，与 Redis 对齐
#[test]
fn test_hincrbyfloat_on_expired_field() -> Void {
  let mut hash = HashObject::new();
  hash.hset(b"fl".to_vec(), b"99.5".to_vec());
  let now = coarsetime::Clock::now_since_epoch().as_millis();
  hash.hexpire(b"fl", now.saturating_sub(10), ExpireOpt::NONE);

  // 已过期字段自增：旧值 99.5 不参与计算，从增量起算
  assert!((hash.hincrbyfloat(b"fl", 2.5)? - 2.5).abs() < 1e-9);
  assert_eq!(hash.hget(b"fl"), Some(b"2.5".as_slice()));
  // 重生字段不残留旧 TTL
  assert_eq!(hash.httl(b"fl"), -1);

  OK
}

/// HRANDFIELD 超大 count 受上限保护，mut 与只读引用版本语义一致
#[test]
fn test_hrandfield_huge_count_guard() -> Void {
  let mut hash = HashObject::new();
  for i in 0..3u8 {
    hash.hset(vec![b'k', i], vec![b'v', i]);
  }

  // 正向超大 count：饱和至字段总数
  let big = hash.hrandfield(isize::MAX, true);
  assert_eq!(big.len(), 3);

  // 负向超大 count：受 MAX_RAND_SAMPLE_LIMIT 截断而非内存耗尽
  let neg_big = hash.hrandfield(isize::MIN, false);
  assert_eq!(neg_big.len(), MAX_RAND_SAMPLE_LIMIT);
  assert!(neg_big.iter().all(|(k, _)| k.starts_with(b"k")));

  // 只读引用版本同样受保护
  let ref_big = hash.hrandfield_ref(isize::MAX, false);
  assert_eq!(ref_big.len(), 3);
  let ref_neg = hash.hrandfield_ref(isize::MIN, true);
  assert_eq!(ref_neg.len(), MAX_RAND_SAMPLE_LIMIT);

  OK
}
