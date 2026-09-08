use std::{
  collections::BTreeMap,
  hash::{DefaultHasher, Hash, Hasher},
  time::Instant,
};

use aok::{OK, Void};
use log::info;
use wedb_resp::{
  Error, SHA1_HEX_LEN, ScriptHashKey,
  script_hash::{scalar_eq_40, simd_eq_40_kernel},
};
use whasher::{new_hash_map, new_hash_set};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

fn calc_hash<T: Hash>(t: &T) -> u64 {
  let mut s = DefaultHasher::new();
  t.hash(&mut s);
  s.finish()
}

#[test]
fn test_script_hash_equality_differences() -> Void {
  let base_raw = b"0123456789abcdef0123456789abcdef01234567";
  let base = ScriptHashKey::from_bytes(base_raw)?;

  // 1. 完全相同比对
  let same = ScriptHashKey::from_bytes(base_raw)?;
  assert_eq!(base, same);
  assert!(base.eq_simd(&same));
  assert!(base.eq_scalar(&same));
  assert!(scalar_eq_40(base.as_bytes(), same.as_bytes()));

  // 2. 遍历全部 0..40 字节单一位置差异测试（覆盖头部、中间、尾部及第 39 字节）
  for i in 0..SHA1_HEX_LEN {
    let mut modified = *base_raw;
    // 将原字符替换为不同十六进制字符
    modified[i] = if modified[i] == b'a' { b'b' } else { b'a' };
    let other = ScriptHashKey::from_bytes(&modified)?;

    assert_ne!(
      base, other,
      "第 {i} 字节不同时比对必须返回 false (头部/中间/尾部)"
    );
    assert!(!base.eq_simd(&other));
    assert!(!base.eq_scalar(&other));
    assert!(!scalar_eq_40(base.as_bytes(), other.as_bytes()));
  }

  // 3. 特殊边界位置显式验证
  // 头部差异（第 0 字节）
  let mut head_diff = *base_raw;
  head_diff[0] = b'f';
  assert_ne!(base, ScriptHashKey::from_bytes(&head_diff)?);

  // 中间差异（第 15, 16, 20, 24 字节）
  for &idx in &[15, 16, 20, 24] {
    let mut mid_diff = *base_raw;
    mid_diff[idx] = if mid_diff[idx] == b'0' { b'1' } else { b'0' };
    assert_ne!(base, ScriptHashKey::from_bytes(&mid_diff)?);
  }

  // 向量重叠分界点差异（第 8 字节与第 32 字节）
  let mut boundary_diff8 = *base_raw;
  boundary_diff8[8] = b'9';
  assert_ne!(base, ScriptHashKey::from_bytes(&boundary_diff8)?);

  let mut boundary_diff32 = *base_raw;
  boundary_diff32[32] = b'9';
  assert_ne!(base, ScriptHashKey::from_bytes(&boundary_diff32)?);

  // 尾部差异（第 38 与第 39 字节）
  let mut tail_diff38 = *base_raw;
  tail_diff38[38] = b'0';
  assert_ne!(base, ScriptHashKey::from_bytes(&tail_diff38)?);

  let mut tail_diff39 = *base_raw;
  tail_diff39[39] = b'0';
  assert_ne!(base, ScriptHashKey::from_bytes(&tail_diff39)?);

  info!("ScriptHashKey 全位置差异与 SIMD 零分支比对测试通过");
  OK
}

#[test]
fn test_script_hash_case_normalization() -> Void {
  let upper_raw = b"A1B2C3D4E5F678901234567890ABCDEF12345678";
  let lower_raw = b"a1b2c3d4e5f678901234567890abcdef12345678";
  let mixed_raw = b"A1b2C3d4E5f678901234567890aBcDeF12345678";

  let key_upper = ScriptHashKey::from_bytes(upper_raw)?;
  let key_lower = ScriptHashKey::from_bytes(lower_raw)?;
  let key_mixed = ScriptHashKey::from_bytes(mixed_raw)?;

  // 验证内部规范化后全为小写字符串
  assert_eq!(
    key_upper.as_str(),
    "a1b2c3d4e5f678901234567890abcdef12345678"
  );
  assert_eq!(
    key_mixed.as_str(),
    "a1b2c3d4e5f678901234567890abcdef12345678"
  );

  // 验证各形式构造出的实例均完全等价
  assert_eq!(key_upper, key_lower);
  assert_eq!(key_mixed, key_lower);
  assert_eq!(key_upper, key_mixed);

  // 验证前缀哈希与 Hasher 状态一致性
  assert_eq!(key_upper.hash_prefix(), key_lower.hash_prefix());
  assert_eq!(calc_hash(&key_upper), calc_hash(&key_lower));
  assert_eq!(calc_hash(&key_mixed), calc_hash(&key_lower));

  info!("ScriptHashKey 大小写规范化测试通过");
  OK
}

#[test]
fn test_script_hash_invalid_inputs() -> Void {
  // 1. 非法长度拦截
  let invalid_lens = [
    &b""[..],
    &b"1"[..],
    &b"a1b2c3d4e5f678901234567890abcdef1234567"[..], // 39 字节
    &b"a1b2c3d4e5f678901234567890abcdef123456789"[..], // 41 字节
    &[b'a'; 100][..],
  ];
  for bytes in invalid_lens {
    assert_eq!(
      ScriptHashKey::from_bytes(bytes),
      Err(Error::InvalidScriptHash),
      "长度为 {} 必须被拦截",
      bytes.len()
    );
  }

  // 2. 包含非法十六进制字符拦截（分别测试头、中、尾）
  let invalid_char_buf = *b"0123456789abcdef0123456789abcdef01234567";

  let bad_positions = [0, 10, 20, 30, 39];
  let bad_chars = [b'g', b'z', b'G', b'Z', b' ', b'\n', b'-', 0xFF, b'!'];

  for &pos in &bad_positions {
    for &bad in &bad_chars {
      let mut cur = invalid_char_buf;
      cur[pos] = bad;
      assert_eq!(
        ScriptHashKey::from_bytes(&cur),
        Err(Error::InvalidScriptHash),
        "位置 {pos} 包含非法字符 {bad} 必须被拦截"
      );
    }
  }

  info!("ScriptHashKey 非法长度与非十六进制字符拦截测试通过");
  OK
}

#[test]
fn test_script_hash_collections_and_ordering() -> Void {
  let h1 = ScriptHashKey::from_bytes(b"0000000000000000000000000000000000000001")?;
  let h2 = ScriptHashKey::from_bytes(b"0000000000000000000000000000000000000002")?;
  let h3 = ScriptHashKey::from_bytes(b"ffffffffffffffffffffffffffffffffffffffff")?;

  // 1. HashMap 测试
  let mut map = new_hash_map();
  map.insert(h1, "first");
  map.insert(h2, "second");
  map.insert(h3, "third");

  assert_eq!(map.get(&h1), Some(&"first"));
  assert_eq!(map.get(&h2), Some(&"second"));
  assert_eq!(map.get(&h3), Some(&"third"));
  assert_eq!(map.len(), 3);

  // 2. HashSet 去重测试
  let mut set = new_hash_set();
  set.insert(h1);
  set.insert(h1); // 重复插入
  set.insert(h2);
  assert_eq!(set.len(), 2);
  assert!(set.contains(&h1));
  assert!(set.contains(&h2));
  assert!(!set.contains(&h3));

  // 3. BTreeMap 排序测试（验证 Ord / PartialOrd）
  let mut btree = BTreeMap::new();
  btree.insert(h3, 3);
  btree.insert(h1, 1);
  btree.insert(h2, 2);

  let ordered_keys: Vec<_> = btree.keys().copied().collect();
  assert_eq!(ordered_keys, vec![h1, h2, h3]);
  assert!(h1 < h2);
  assert!(h2 < h3);

  // 4. Deref, AsRef, Display, Debug 与 TryFrom 测试
  assert_eq!(h1.len(), 40);
  assert_eq!(&h1[..], b"0000000000000000000000000000000000000001");
  assert_eq!(format!("{h1}"), "0000000000000000000000000000000000000001");
  assert!(format!("{h1:?}").contains("ScriptHashKey"));

  let parsed: ScriptHashKey = "0000000000000000000000000000000000000001".parse()?;
  assert_eq!(parsed, h1);

  // 5. CopyTo：copy_to_slice 验证
  let mut dst_buf = [0u8; 48];
  h1.copy_to_slice(&mut dst_buf);
  assert_eq!(&dst_buf[..40], h1.as_bytes());
  assert_eq!(&dst_buf[40..], &[0u8; 8]); // 确保无缓冲区溢出越界写入

  // 6. 对称 PartialEq 验证（双向支持 &[u8], [u8; 40], &[u8; 40], &str, str）
  let raw_arr: [u8; 40] = *b"0000000000000000000000000000000000000001";
  let raw_arr_ref: &[u8; 40] = &raw_arr;
  let raw_slice: &[u8] = &raw_arr[..];
  let raw_str: &str = "0000000000000000000000000000000000000001";

  assert!(h1 == raw_arr);
  assert!(raw_arr == h1);
  assert!(h1 == raw_arr_ref);
  assert!(raw_arr_ref == h1);
  assert!(h1 == raw_slice);
  assert!(raw_slice == h1);
  assert!(h1 == raw_str);
  assert!(raw_str == h1);
  assert!(h1 == *raw_str);
  assert!(*raw_str == h1);

  // 7. 编译期 const 计算验证
  const CONST_KEY: ScriptHashKey =
    ScriptHashKey::from_raw(*b"0000000000000000000000000000000000000001");
  const CONST_STR: &str = CONST_KEY.as_str();
  const CONST_PREFIX: u64 = CONST_KEY.hash_prefix();
  const CONST_SUFFIX: u64 = CONST_KEY.hash_suffix();
  assert_eq!(CONST_STR, "0000000000000000000000000000000000000001");
  assert_eq!(CONST_PREFIX, h1.hash_prefix());
  assert_eq!(CONST_SUFFIX, h1.hash_suffix());

  info!("ScriptHashKey 集合存储、排序与特性转换测试通过");
  OK
}

#[test]
fn test_script_hash_random_equivalence_10000() -> Void {
  const HEX_CHARS: &[u8; 16] = b"0123456789abcdef";

  let mut rng = fastrand::Rng::with_seed(20260906);

  let gen_random_hash = |rng: &mut fastrand::Rng| -> [u8; 40] {
    let mut buf = [0u8; 40];
    for b in &mut buf {
      let idx = rng.usize(..16);
      *b = HEX_CHARS[idx];
    }
    buf
  };

  let level = fearless_simd::Level::new();

  for iteration in 0..10_000 {
    let a_bytes = gen_random_hash(&mut rng);
    let key_a = ScriptHashKey::from_raw(a_bytes);

    let roll = rng.usize(..100);
    let (b_bytes, expected_equal) = if roll < 30 {
      // 30% 完全相同
      (a_bytes, true)
    } else if roll < 70 {
      // 40% 随机改变 1 个字节
      let mut b = a_bytes;
      let pos = rng.usize(..40);
      let mut new_idx = rng.usize(..16);
      while HEX_CHARS[new_idx] == b[pos] {
        new_idx = rng.usize(..16);
      }
      b[pos] = HEX_CHARS[new_idx];
      (b, false)
    } else {
      // 30% 全随机
      let b = gen_random_hash(&mut rng);
      let eq = b == a_bytes;
      (b, eq)
    };

    let key_b = ScriptHashKey::from_raw(b_bytes);

    // 1. 标准逐字节比对
    let std_eq = a_bytes == b_bytes;
    assert_eq!(std_eq, expected_equal);

    // 2. SIMD 比对（通过 PartialEq ==）
    let op_eq = key_a == key_b;
    assert_eq!(
      op_eq, std_eq,
      "迭代 {iteration}: PartialEq == 结果与标准比对不一致"
    );

    // 3. SIMD 内核直接调用比对
    let simd_kernel_res =
      fearless_simd::dispatch!(level, simd => simd_eq_40_kernel(simd, &a_bytes, &b_bytes));
    assert_eq!(
      simd_kernel_res, std_eq,
      "迭代 {iteration}: simd_eq_40_kernel 结果与标准比对不一致"
    );

    // 4. 标量 5x u64 比对
    let scalar_res = scalar_eq_40(&a_bytes, &b_bytes);
    assert_eq!(
      scalar_res, std_eq,
      "迭代 {iteration}: scalar_eq_40 结果与标准比对不一致"
    );

    // 5. 辅助方法比对
    assert_eq!(key_a.eq_simd(&key_b), std_eq);
    assert_eq!(key_a.eq_scalar(&key_b), std_eq);
  }

  info!("10,000 次随机哈希 SIMD 与标量比对等价性验证全部通过");
  OK
}

#[test]
fn test_script_hash_perf_benchmark() -> Void {
  let key_a = ScriptHashKey::from_bytes(b"0123456789abcdef0123456789abcdef01234567")?;
  let key_b = ScriptHashKey::from_bytes(b"0123456789abcdef0123456789abcdef01234567")?;
  let key_c = ScriptHashKey::from_bytes(b"0123456789abcdef0123456789abcdef01234568")?;

  const ITERS: usize = 100_000;

  // 1. 测试 SIMD 比对性能
  let start = Instant::now();
  let mut dummy_simd = 0usize;
  for _ in 0..ITERS {
    if key_a.eq_simd(&key_b) {
      dummy_simd += 1;
    }
    if !key_a.eq_simd(&key_c) {
      dummy_simd += 1;
    }
  }
  let dur_simd = start.elapsed();

  // 2. 测试标量 5x64 异或累积比对性能
  let start = Instant::now();
  let mut dummy_scalar = 0usize;
  for _ in 0..ITERS {
    if key_a.eq_scalar(&key_b) {
      dummy_scalar += 1;
    }
    if !key_a.eq_scalar(&key_c) {
      dummy_scalar += 1;
    }
  }
  let dur_scalar = start.elapsed();

  // 3. 测试 O(1) 哈希性能
  let start = Instant::now();
  let mut dummy_hash = 0u64;
  for _ in 0..ITERS {
    dummy_hash = dummy_hash.wrapping_add(calc_hash(&key_a));
  }
  let dur_hash = start.elapsed();

  // 4. 测试无分支查表构造与规范化性能
  let raw = b"0123456789ABCDEF0123456789ABCDEF01234567";
  let start = Instant::now();
  let mut dummy_from = 0usize;
  for _ in 0..ITERS {
    if let Ok(k) = ScriptHashKey::from_bytes(raw) {
      dummy_from += k.len();
    }
  }
  let dur_from = start.elapsed();

  assert_eq!(dummy_simd, ITERS * 2);
  assert_eq!(dummy_scalar, ITERS * 2);
  assert_ne!(dummy_hash, 0);
  assert_eq!(dummy_from, ITERS * 40);

  info!(
    "【ScriptHashKey 性能基准 ({} 万次迭代)】SIMD比对: {:?} ({:.2} ns/op) | 标量比对: {:?} ({:.2} ns/op) | O(1)哈希: {:?} ({:.2} ns/op) | 查表规范化: {:?} ({:.2} ns/op)",
    ITERS / 10_000,
    dur_simd,
    (dur_simd.as_nanos() as f64) / (ITERS as f64 * 2.0),
    dur_scalar,
    (dur_scalar.as_nanos() as f64) / (ITERS as f64 * 2.0),
    dur_hash,
    (dur_hash.as_nanos() as f64) / (ITERS as f64),
    dur_from,
    (dur_from.as_nanos() as f64) / (ITERS as f64),
  );

  OK
}
