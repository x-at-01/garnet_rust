use std::collections::BTreeSet;

use aok::{OK, Void};
use log::info;
use wedb_hll::{
  Error, HLL_DENSE, HLL_DENSE_SIZE, HLL_MAGIC, HLL_MAX_COUNT, HLL_REGISTERS, HLL_SPARSE,
  HyperLogLog, pack_3bytes, unpack_3bytes,
};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// splitmix64 确定性伪随机（对标 C# 测试的 `Random(674386)` 固定种子语义）
struct Rng(u64);

impl Rng {
  fn next(&mut self) -> u64 {
    self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = self.0;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
  }
}

/// 验证 3 字节与 4 个 6-bit 寄存器打解包的全面正确性
#[test]
fn test_pack_unpack_full() -> Void {
  // 测试边界值与多种组合
  let test_vals = [0u8, 1, 2, 7, 15, 31, 42, 62, 63];
  for &r0 in &test_vals {
    for &r1 in &test_vals {
      for &r2 in &test_vals {
        for &r3 in &test_vals {
          let (b0, b1, b2) = pack_3bytes(r0, r1, r2, r3);
          let (u0, u1, u2, u3) = unpack_3bytes(b0, b1, b2);
          assert_eq!(
            (r0, r1, r2, r3),
            (u0, u1, u2, u3),
            "打解包不匹配: input=({r0},{r1},{r2},{r3}), unpacked=({u0},{u1},{u2},{u3})"
          );
        }
      }
    }
  }
  OK
}

/// 验证全量 16384 个 6-bit 寄存器的单寄存器读写隔离性与边界覆盖
#[test]
fn test_all_registers_isolation() -> Void {
  let mut hll = HyperLogLog::new();

  // 初始全为 0 (Sparse 零段路径)
  for i in 0..HLL_REGISTERS {
    assert_eq!(hll.get_register(i), 0);
  }

  // 为每个寄存器写入特定 6-bit 模式 ((i * 7 + 13) % 64)，首次写入触发 sparse→dense 升级
  for i in 0..HLL_REGISTERS {
    let expected = ((i * 7 + 13) % 64) as u8;
    hll.set_register(i, expected);
  }

  // 校验每个寄存器读出值完全符合预期，无相邻位破坏
  for i in 0..HLL_REGISTERS {
    let expected = ((i * 7 + 13) % 64) as u8;
    assert_eq!(hll.get_register(i), expected, "寄存器 {i} 读写隔离校验失败");
  }

  // 测试最后一个寄存器 (索引 16383)
  hll.set_register(16383, 63);
  assert_eq!(hll.get_register(16383), 63);

  OK
}

/// 验证不同数量级基数统计准确性 (10, 100, 1000, 10_000, 100_000)
#[test]
fn test_cardinality_accuracy_scales() -> Void {
  let scales = [10, 100, 1000, 10_000, 100_000];

  for &scale in &scales {
    let mut hll = HyperLogLog::new();
    for i in 0..scale {
      let key = format!("scale_{scale}_elem_{i}");
      hll.add(key.as_bytes());
    }

    let est = hll.count();
    let err = (est as f64 - scale as f64).abs() / scale as f64;
    info!(
      "Scale {scale:>6} => Estimated: {est:>6}, Error: {:.4}%",
      err * 100.0
    );

    // 小基数在 σ 修正下精度极高，高基数在标准误差 1.04/sqrt(16384) ≈ 0.81% 范围内
    let max_allowed_err = match scale {
      10 => 0.15,       // 允许 1~2 个误差
      100 => 0.05,      // 5% 以内
      1000 => 0.03,     // 3% 以内
      10_000 => 0.025,  // 2.5% 约 3-sigma 范围内
      100_000 => 0.025, // 2.5% 约 3-sigma 范围内
      _ => 0.05,
    };

    assert!(
      err <= max_allowed_err,
      "基数 {scale} 误差超标: 估计值={est}, 实际误差={:.4}%, 阈值={:.4}%",
      err * 100.0,
      max_allowed_err * 100.0
    );
  }

  OK
}

/// 验证多 HLL 合并、原始切片合并与多路联合统计的完全等价性与联合去重
#[test]
fn test_merge_and_count_multiple_equivalence() -> Void {
  let mut hll_a = HyperLogLog::new();
  let mut hll_b = HyperLogLog::new();
  let mut hll_c = HyperLogLog::new();

  // A: 0..6000
  for i in 0..6000 {
    hll_a.add(format!("item_{i}").as_bytes());
  }

  // B: 4000..10000
  for i in 4000..10000 {
    hll_b.add(format!("item_{i}").as_bytes());
  }

  // C: 8000..15000
  for i in 8000..15000 {
    hll_c.add(format!("item_{i}").as_bytes());
  }

  // 1. 多路联合统计
  let multi_ab = HyperLogLog::count_multiple(&[&hll_a, &hll_b]);
  let multi_abc = HyperLogLog::count_multiple(&[&hll_a, &hll_b, &hll_c]);

  // 2. 双路 merge 对比
  let mut merged_ab = hll_a.clone();
  merged_ab.merge(&hll_b);
  let count_ab = merged_ab.count();
  assert_eq!(multi_ab, count_ab, "双路联合统计与 merge 结果必须严格等价");

  // 3. 三路 merge 对比
  let mut merged_abc = merged_ab.clone();
  merged_abc.merge(&hll_c);
  let count_abc = merged_abc.count();
  assert_eq!(
    multi_abc, count_abc,
    "三路联合统计与连续 merge 结果必须严格等价"
  );

  // 4. 原始字节合并 merge_raw 对比
  let mut raw_merged = hll_a.clone();
  raw_merged.merge_raw(hll_b.as_bytes())?;
  raw_merged.merge_raw(hll_c.as_bytes())?;
  assert_eq!(
    raw_merged.count(),
    count_abc,
    "merge_raw 与 merge 结果必须完全一致"
  );

  // 5. 总体基数误差验证 (真实并集基数为 15000)
  let total_err = (count_abc as f64 - 15000.0).abs() / 15000.0;
  info!(
    "Total union 15000 => Estimated: {count_abc}, Error: {:.4}%",
    total_err * 100.0
  );
  assert!(total_err < 0.02, "联合去重误差超标: count={count_abc}");

  OK
}

/// 验证基数缓存的命中、失效与只读语义 (Sparse 表示路径)
#[test]
fn test_cache_hit_and_invalidation() -> Void {
  let mut hll = HyperLogLog::new();
  assert_eq!(hll.as_bytes()[4], HLL_SPARSE);
  assert_eq!(hll.get_cached_count(), None);

  // 只读估算不建立缓存
  let ro_count = hll.count_readonly();
  assert_eq!(ro_count, 0);
  assert_eq!(hll.get_cached_count(), None);

  // count() 建立缓存
  let c1 = hll.count();
  assert_eq!(c1, 0);
  assert_eq!(hll.get_cached_count(), Some(0));

  // 添加元素发生更新，缓存立即失效
  let updated = hll.add(b"new_element_1");
  assert!(updated);
  assert_eq!(hll.get_cached_count(), None);

  // 再次 count() 重建缓存
  let c2 = hll.count();
  assert_eq!(c2, 1);
  assert_eq!(hll.get_cached_count(), Some(1));

  // 添加已存在的元素，不发生寄存器更新，缓存保持有效
  let updated_again = hll.add(b"new_element_1");
  assert!(!updated_again);
  assert_eq!(hll.get_cached_count(), Some(1));

  // 合并相同/更小的 HLL，未发生更新，缓存保持有效
  let empty_hll = HyperLogLog::new();
  hll.merge(&empty_hll);
  assert_eq!(hll.get_cached_count(), Some(1));

  // 合并具有更大值的 HLL，发生更新，缓存失效
  let mut other_hll = HyperLogLog::new();
  other_hll.add(b"other_element");
  hll.merge(&other_hll);
  assert_eq!(hll.get_cached_count(), None);

  // set_register 主动使缓存失效 (并升级 Dense)
  hll.count();
  assert!(hll.get_cached_count().is_some());
  hll.set_register(10, 5);
  assert_eq!(hll.get_cached_count(), None);

  // as_bytes_mut 主动使缓存失效
  hll.count();
  assert!(hll.get_cached_count().is_some());
  let _ = hll.as_bytes_mut();
  assert_eq!(hll.get_cached_count(), None);

  // 手动失效测试
  hll.count();
  assert!(hll.get_cached_count().is_some());
  hll.invalidate_cache();
  assert_eq!(hll.get_cached_count(), None);

  OK
}

/// 验证 Bitcode 序列化、反序列化及损坏切片拦截
#[test]
fn test_bitcode_serde_and_validation() -> Void {
  let mut hll = HyperLogLog::new();
  for i in 0..500 {
    hll.add(format!("bitcode_test_{i}").as_bytes());
  }
  let original_count = hll.count();

  // 序列化
  let encoded = hll.to_bitcode();
  assert!(!encoded.is_empty());

  // 反序列化成功
  let decoded = HyperLogLog::from_bitcode(&encoded)?;
  assert_eq!(decoded, hll);
  assert_eq!(decoded.count_readonly(), original_count);

  // 拦截损坏的 Bitcode 数据
  assert!(HyperLogLog::from_bitcode(&[]).is_err());
  assert!(HyperLogLog::from_bitcode(&[0xFF; 10]).is_err());

  // 拦截损坏的内部切片 (如魔数破坏)
  let mut corrupted_bytes = hll.as_bytes().to_vec();
  corrupted_bytes[0] = b'X'; // 破坏魔数
  let bad_encoded = bitcode::encode(&corrupted_bytes);
  assert_eq!(
    HyperLogLog::from_bitcode(&bad_encoded),
    Err(Error::InvalidHyperLogLog)
  );

  OK
}

/// 验证针对非法输入与损坏数据的全方位安全拦截
#[test]
fn test_invalid_input_rejection() -> Void {
  // 长度不足以容纳任何表示
  assert_eq!(
    HyperLogLog::from_bytes(&[0u8; 100]),
    Err(Error::InvalidHyperLogLog)
  );
  assert_eq!(
    HyperLogLog::from_bytes(&[0u8; HLL_DENSE_SIZE - 1]),
    Err(Error::InvalidHyperLogLog)
  );
  assert_eq!(
    HyperLogLog::from_bytes(&[0u8; HLL_DENSE_SIZE + 1]),
    Err(Error::InvalidHyperLogLog)
  );

  // 魔数错误
  let mut valid_like = vec![0u8; HLL_DENSE_SIZE];
  valid_like[0..4].copy_from_slice(b"FAIL");
  valid_like[4] = 1;
  assert_eq!(
    HyperLogLog::from_bytes(&valid_like),
    Err(Error::InvalidHyperLogLog)
  );

  // 编码标识非法
  let mut wrong_encoding = vec![0u8; HLL_DENSE_SIZE];
  wrong_encoding[0..4].copy_from_slice(HLL_MAGIC);
  wrong_encoding[4] = 2; // 非法编码
  assert_eq!(
    HyperLogLog::from_bytes(&wrong_encoding),
    Err(Error::InvalidHyperLogLog)
  );

  // merge_raw 拦截非法切片
  let mut hll = HyperLogLog::new();
  assert_eq!(hll.merge_raw(&[]), Err(Error::InvalidHyperLogLog));
  assert_eq!(
    hll.merge_raw(&wrong_encoding),
    Err(Error::InvalidHyperLogLog)
  );

  OK
}

/// 官方用例：PFADD/PFCOUNT 重复添加与去重 (C# SimpleHyperLogLogAddCount / SimpleHyperLogLogMerge)
#[test]
fn test_garnet_parity_simple_add_and_merge() -> Void {
  let mut hll = HyperLogLog::new();
  let data = [b"a".as_slice(), b"b", b"c", b"d", b"e", b"f"];

  // 首次添加全部触发更新
  for &d in &data {
    assert!(hll.add(d), "首次添加应该更新寄存器");
  }

  // 重复添加全部不触发更新
  for &d in &data {
    assert!(!hll.add(d), "重复添加不应更新寄存器");
  }

  // 基数估算完全精确
  assert_eq!(hll.count(), 6);

  // Merge: X = ["h","e","l","l","o"], Y = ["w","o","r","l","d"]
  let mut hll_x = HyperLogLog::new();
  for &ch in &[b"h".as_slice(), b"e", b"l", b"l", b"o"] {
    hll_x.add(ch);
  }
  assert_eq!(hll_x.count(), 4); // "h","e","l","o" 去重后为 4

  let mut hll_y = HyperLogLog::new();
  for &ch in &[b"w".as_slice(), b"o", b"r", b"l", b"d"] {
    hll_y.add(ch);
  }
  assert_eq!(hll_y.count(), 5);

  let mut hll_w = HyperLogLog::new();
  hll_w.merge(&hll_x);
  assert_eq!(hll_w.count(), 4);

  hll_w.merge(&hll_y);
  assert_eq!(hll_w.count(), 7); // 并集为 'h','e','l','o','w','r','d'，共 7 个

  OK
}

/// 验证超大基数饱和校正：所有寄存器填入最大 6-bit 值 63 时，绝对防 NaN、防塌缩为 0，稳定饱和为 HLL_MAX_COUNT
#[test]
fn test_large_cardinality_saturation_no_nan() -> Void {
  let mut hll = HyperLogLog::new();
  for i in 0..HLL_REGISTERS {
    hll.set_register(i, 63);
  }
  let count = hll.count();
  assert_eq!(
    count, HLL_MAX_COUNT,
    "极端全满输入下必须饱和为 HLL_MAX_COUNT"
  );

  // 只读估算与缓存估算严格一致
  assert_eq!(hll.count_readonly(), HLL_MAX_COUNT);

  // 验证多路估算在全满实例下同样稳定饱和
  let empty = HyperLogLog::new();
  let multi_sat = HyperLogLog::count_multiple(&[&hll, &empty]);
  assert_eq!(multi_sat, HLL_MAX_COUNT);
  OK
}

/// 验证公开寄存器读写 API 在越界索引时的受控 panic 安全隔离
#[test]
#[should_panic(expected = "out of bounds")]
fn test_get_register_out_of_bounds() {
  let hll = HyperLogLog::new();
  hll.get_register(HLL_REGISTERS);
}

#[test]
#[should_panic(expected = "out of bounds")]
fn test_set_register_out_of_bounds() {
  let mut hll = HyperLogLog::new();
  hll.set_register(HLL_REGISTERS, 10);
}

#[test]
#[should_panic(expected = "out of bounds")]
fn test_update_register_out_of_bounds() {
  let mut hll = HyperLogLog::new();
  hll.update_register(HLL_REGISTERS + 100, 10);
}

/// 验证 update_register 在旧值更大或相等时的快速提前退出与缓存保持
#[test]
fn test_update_register_early_exit_behavior() -> Void {
  let mut hll = HyperLogLog::new();
  hll.set_register(42, 20);
  let c1 = hll.count();
  assert!(hll.get_cached_count().is_some());

  // 尝试用更小的值更新 -> 返回 false，缓存不失效
  assert!(!hll.update_register(42, 10));
  assert_eq!(hll.get_cached_count(), Some(c1));

  // 尝试用相等的值更新 -> 返回 false，缓存不失效
  assert!(!hll.update_register(42, 20));
  assert_eq!(hll.get_cached_count(), Some(c1));

  // 尝试用更大的值更新 -> 返回 true，缓存失效
  assert!(hll.update_register(42, 25));
  assert_eq!(hll.get_cached_count(), None);
  OK
}

/// 验证 Sparse 表示下的 update_register 语义：零段拆分、原地改写与超界升级 Dense
#[test]
fn test_update_register_sparse_paths() -> Void {
  let mut hll = HyperLogLog::new();
  assert_eq!(hll.as_bytes()[4], HLL_SPARSE);

  // 零段拆分写入
  assert!(hll.update_register(5, 3));
  assert_eq!(hll.get_register(5), 3);
  // 同寄存器更小 / 相等值不更新
  assert!(!hll.update_register(5, 2));
  assert!(!hll.update_register(5, 3));
  // 值 opcode 原地改写 (上限 qbit+1)
  assert!(hll.update_register(5, 51));
  assert_eq!(hll.get_register(5), 51);
  // 新寄存器写入
  assert!(hll.update_register(100, 7));
  assert_eq!(hll.get_register(100), 7);
  assert_eq!(hll.count(), 2);

  // 值超过 Sparse opcode 表达范围 (52..=63)：升级 Dense 后写入
  assert!(hll.update_register(5, 60));
  assert_eq!(hll.as_bytes()[4], HLL_DENSE);
  assert_eq!(hll.get_register(5), 60);
  assert_eq!(hll.get_register(100), 7);
  OK
}

/// 验证 4 路以上多实例联合估算 (包含空 HLL、同源 HLL、独立 HLL 的联合去重)
#[test]
fn test_count_multiple_four_instances() -> Void {
  let mut h1 = HyperLogLog::new();
  let mut h2 = HyperLogLog::new();
  let mut h3 = HyperLogLog::new();
  let h4_empty = HyperLogLog::new();

  for i in 0..1000 {
    h1.add(format!("elem_{i}").as_bytes());
  }
  for i in 500..1500 {
    h2.add(format!("elem_{i}").as_bytes());
  }
  for i in 1200..2000 {
    h3.add(format!("elem_{i}").as_bytes());
  }

  // 4 路联合统计 (包含空实例)
  let multi = HyperLogLog::count_multiple(&[&h1, &h2, &h3, &h4_empty]);

  // 逐次合并作为对拍基准
  let mut merged = h1.clone();
  merged.merge(&h2);
  merged.merge(&h3);
  merged.merge(&h4_empty);
  let expected = merged.count();

  assert_eq!(multi, expected, "4 路联合统计与逐次合并结果必须完全一致");
  let err = (multi as f64 - 2000.0).abs() / 2000.0;
  assert!(err < 0.03, "4 路联合基数误差超标: multi={multi}");

  // 空列表联合统计
  assert_eq!(HyperLogLog::count_multiple(&[]), 0);
  // 单实例联合统计
  assert_eq!(HyperLogLog::count_multiple(&[&h1]), h1.count_readonly());
  OK
}

/// C# HyperLogLogMultiCountTest 完全对齐：
/// A: ["h","e","l","l","o"] (4), B: ["w","o","r","l","d"] (5), C: ["a","b","c","d","e","f"] (6)
/// 联合统计 A + B + C 严格等于 11
#[test]
fn test_garnet_multi_count_exact_parity() -> Void {
  let mut hll_a = HyperLogLog::new();
  let mut hll_b = HyperLogLog::new();
  let mut hll_c = HyperLogLog::new();

  for item in [b"h".as_slice(), b"e", b"l", b"l", b"o"] {
    hll_a.add(item);
  }
  for item in [b"w".as_slice(), b"o", b"r", b"l", b"d"] {
    hll_b.add(item);
  }
  for item in [b"a".as_slice(), b"b", b"c", b"d", b"e", b"f"] {
    hll_c.add(item);
  }

  assert_eq!(hll_a.count(), 4, "KeyA count 必须为 4");
  assert_eq!(hll_b.count(), 5, "KeyB count 必须为 5");
  assert_eq!(hll_c.count(), 6, "KeyC count 必须为 6");

  // 多键联合统计 (对标 ClassicAssert.AreEqual(11, totalCount))
  let total_count = HyperLogLog::count_multiple(&[&hll_a, &hll_b, &hll_c]);
  assert_eq!(total_count, 11, "A+B+C 联合统计必须精确等于 11");

  OK
}

/// C# HyperLogLogDumpVariantCoverage_SparseAndDenseRepresentations 对齐：
/// 新建 HLL 为 Sparse (编码 0)，持续添加后自动升级为 Dense (编码 1)，且升级不可逆
#[test]
fn test_sparse_init_and_dense_upgrade() -> Void {
  let mut hll = HyperLogLog::new();

  // 初始 Sparse：18 字节头 + 128 个 0xFF 零段 = 146 字节
  let bytes = hll.as_bytes();
  assert_eq!(bytes[4], HLL_SPARSE);
  assert_eq!(bytes.len(), 146);

  // 小基数阶段保持 Sparse
  for i in 0..64 {
    hll.add(format!("s_{i}").as_bytes());
  }
  assert_eq!(hll.as_bytes()[4], HLL_SPARSE);

  // 持续添加直至 RLE 流越过 4096 字节上限触发自动升级
  let mut i = 64;
  while hll.as_bytes()[4] == HLL_SPARSE {
    hll.add(format!("upgrade_{i}").as_bytes());
    i += 1;
    assert!(i < 100_000, "未在合理元素数内触发 sparse→dense 升级");
  }
  assert_eq!(hll.as_bytes()[4], HLL_DENSE);
  assert_eq!(hll.as_bytes().len(), HLL_DENSE_SIZE);

  // 升级后基数估计与元素总数保持一致 (寄存器逐位保留)
  let est = hll.count();
  let err = (est as f64 - i as f64).abs() / i as f64;
  assert!(err < 0.05, "升级后基数漂移过大: est={est}, n={i}");

  OK
}

/// C# HyperLogLogValidatorRejectsMalformedSparsePayload / SparseStreamCoverageMismatch 对齐：
/// 拦截 RLE 长度不符、非零值越界、寄存器覆盖缺失与超长 Sparse 负载
#[test]
fn test_sparse_validation_rejects_malformed_payloads() -> Void {
  // 合法 Sparse 基准
  let mut hll = HyperLogLog::new();
  for i in 0..20 {
    hll.add(format!("v_{i}").as_bytes());
  }
  assert!(HyperLogLog::from_bytes(hll.as_bytes()).is_ok());

  // 1. RLE 长度字段与实际负载不符 (C# HyperLogLogValidatorRejectsMalformedSparsePayload)
  let mut bad_rle = hll.as_bytes().to_vec();
  let rle = u16::from_le_bytes([bad_rle[16], bad_rle[17]]);
  bad_rle[16..18].copy_from_slice(&(rle + 100).to_le_bytes());
  assert_eq!(
    HyperLogLog::from_bytes(&bad_rle),
    Err(Error::InvalidHyperLogLog)
  );

  // 2. 非零 opcode 值越界 (> qbit+1)：手工构造恰好覆盖 16384 个寄存器的流
  //    [值 51] + [零段 128] * 127 + [零段 127] => 1 + 16256 + 127 = 16384
  let mut craft = vec![0u8; 18 + 129];
  craft[0..4].copy_from_slice(HLL_MAGIC);
  craft[4] = HLL_SPARSE;
  craft[8..16].copy_from_slice(&i64::MIN.to_le_bytes()); // 缓存失效
  craft[16..18].copy_from_slice(&129u16.to_le_bytes());
  craft[18] = 50; // 值 51，合法
  for b in &mut craft[19..18 + 128] {
    *b = 0xFF; // 零段 128
  }
  craft[18 + 128] = 0xFE; // 零段 127
  assert!(HyperLogLog::from_bytes(&craft).is_ok(), "合法流不应被拦截");

  let mut bad_val = craft.clone();
  bad_val[18] = 51; // 值 52 > qbit+1，非法
  assert_eq!(
    HyperLogLog::from_bytes(&bad_val),
    Err(Error::InvalidHyperLogLog)
  );

  // 3. 覆盖缺失 (C# HyperLogLogValidatorRejectsSparseStreamCoverageMismatch)：
  //    截断末尾零段后覆盖数不足 16384
  let mut truncated = craft.clone();
  truncated.pop();
  truncated[16..18].copy_from_slice(&128u16.to_le_bytes());
  assert_eq!(
    HyperLogLog::from_bytes(&truncated),
    Err(Error::InvalidHyperLogLog)
  );

  // 4. 超过 Sparse 4096 字节上限
  let mut oversize = vec![0u8; 4097];
  oversize[0..4].copy_from_slice(HLL_MAGIC);
  oversize[4] = HLL_SPARSE;
  assert_eq!(
    HyperLogLog::from_bytes(&oversize),
    Err(Error::InvalidHyperLogLog)
  );

  OK
}

/// C# HyperLogLogTestPFMERGE_SparseToSparseV2 / SparseToDenseV2 / DenseToDenseV2 对齐：
/// 四种表示组合的合并语义与基数正确性
#[test]
fn test_pfmerge_representation_combinations() -> Void {
  // 1. sparse + sparse (C# SparseToSparseV2)：范围重叠 16..32
  let mut sparse_a = HyperLogLog::new();
  let mut sparse_b = HyperLogLog::new();
  for i in 0..32 {
    sparse_a.add(format!("m_{i}").as_bytes());
  }
  for i in 16..48 {
    sparse_b.add(format!("m_{i}").as_bytes());
  }
  assert_eq!(sparse_a.as_bytes()[4], HLL_SPARSE);
  sparse_a.merge(&sparse_b);
  let union_a = sparse_a.count();
  let err = (union_a as f64 - 48.0).abs() / 48.0;
  assert!(err < 0.05, "sparse+sparse 合并基数漂移: {union_a}");

  // 2. sparse 目标 + dense 源：目标升级 Dense (C# MergeGrow → DenseBytes)
  let mut sparse_dst = HyperLogLog::new();
  for i in 0..32 {
    sparse_dst.add(format!("m_{i}").as_bytes());
  }
  let mut dense_src = HyperLogLog::new();
  for i in 0..8192 {
    dense_src.add(format!("m_{i}").as_bytes());
  }
  assert_eq!(dense_src.as_bytes()[4], HLL_DENSE);
  sparse_dst.merge(&dense_src);
  assert_eq!(
    sparse_dst.as_bytes()[4],
    HLL_DENSE,
    "密集源合并后目标必须升级为 Dense"
  );
  let err = (sparse_dst.count() as f64 - 8192.0).abs() / 8192.0;
  assert!(err < 0.02, "sparse+dense 合并基数漂移");

  // 3. dense 目标 + sparse 源：目标保持 Dense (C# SparseToDense)
  let mut dense_dst = HyperLogLog::new();
  for i in 0..8192 {
    dense_dst.add(format!("m_{i}").as_bytes());
  }
  let cached = dense_dst.count();
  dense_dst.merge(&sparse_b); // sparse_b 的 16..48 全部被 dense 覆盖，无更新
  assert_eq!(dense_dst.as_bytes()[4], HLL_DENSE);
  assert_eq!(
    dense_dst.get_cached_count(),
    Some(cached),
    "无更新不应失效缓存"
  );

  // 4. sparse + sparse 合并后越过上限：自动升级 Dense 且寄存器内容正确
  let mut big_a = HyperLogLog::new();
  let mut big_b = HyperLogLog::new();
  for i in 0..2500 {
    big_a.add(format!("big_{i}").as_bytes());
  }
  for i in 2500..5000 {
    big_b.add(format!("big_{i}").as_bytes());
  }
  big_a.merge(&big_b);
  let est = big_a.count();
  let err = (est as f64 - 5000.0).abs() / 5000.0;
  assert!(err < 0.03, "大基数 sparse 合并漂移: est={est}");

  OK
}

/// 验证 Sparse / Dense 混合实例的 count_multiple 与逐一 merge 严格等价
#[test]
fn test_count_multiple_mixed_representations() -> Void {
  let mut sparse_a = HyperLogLog::new();
  let mut sparse_b = HyperLogLog::new();
  let mut dense_c = HyperLogLog::new();
  let empty = HyperLogLog::new();

  for i in 0..32 {
    sparse_a.add(format!("x_{i}").as_bytes());
  }
  for i in 16..48 {
    sparse_b.add(format!("x_{i}").as_bytes());
  }
  for i in 0..8192 {
    dense_c.add(format!("x_{i}").as_bytes());
  }

  // 混合表示多路联合统计
  let multi = HyperLogLog::count_multiple(&[&sparse_a, &sparse_b, &dense_c, &empty]);

  // 逐一合并对拍
  let mut merged = sparse_a.clone();
  merged.merge(&sparse_b);
  merged.merge(&dense_c);
  merged.merge(&empty);
  assert_eq!(
    multi,
    merged.count(),
    "混合表示联合统计与逐次合并必须严格等价"
  );

  // 真实并集为 0..8192 = 8192
  let err = (multi as f64 - 8192.0).abs() / 8192.0;
  assert!(err < 0.02, "混合联合基数漂移: multi={multi}");
  OK
}

/// 验证 Sparse 合并升级的精确边界（C# `MergeGrow` 仅在扩容需求越限时才转 Dense）：
/// 手工构造恰好 4096 字节（= 上限）的合法 Sparse blob ——
/// 1 个非零 opcode（值 5，覆盖寄存器 0）+ 4077 个零段（4076×4 + 79 = 16383）恰好覆盖全部寄存器
#[test]
fn test_merge_sparse_cap_boundary() -> Void {
  let mut craft = vec![0u8; 4096];
  craft[0..4].copy_from_slice(HLL_MAGIC);
  craft[4] = HLL_SPARSE;
  craft[8..16].copy_from_slice(&i64::MIN.to_le_bytes()); // 缓存失效
  craft[16..18].copy_from_slice(&4078u16.to_le_bytes()); // RLE 长度 = 4096 - 18
  craft[18] = 4; // 非零值 5
  for b in &mut craft[19..19 + 4076] {
    *b = 0x83; // 零段 4
  }
  craft[19 + 4076] = 0xCE; // 零段 79

  let mut cap_hll = HyperLogLog::from_bytes(&craft)?;
  assert_eq!(cap_hll.as_bytes().len(), 4096);
  assert_eq!(cap_hll.as_bytes()[4], HLL_SPARSE);
  assert_eq!(cap_hll.count(), 1);

  // 1. 与空 HLL 合并：无寄存器更新 → 保持 Sparse 且缓存不失效
  let mut dst = cap_hll.clone();
  dst.merge(&HyperLogLog::new());
  assert_eq!(dst.as_bytes()[4], HLL_SPARSE, "无更新不得触发 Dense 升级");
  assert_eq!(dst.get_cached_count(), Some(1));

  // 2. 与内容完全相同的源合并：同样无更新，保持 Sparse
  let mut dst2 = cap_hll.clone();
  dst2.merge(&cap_hll);
  assert_eq!(dst2.as_bytes()[4], HLL_SPARSE);
  assert_eq!(dst2.get_cached_count(), Some(1));

  // 3. 合并带来更大寄存器值的 Sparse 源：发生更新且长度越限 → 升级 Dense
  let mut bigger = HyperLogLog::new();
  assert!(bigger.update_register(0, 9));
  assert_eq!(
    bigger.as_bytes()[4],
    HLL_SPARSE,
    "值 9 在 Sparse 表达范围内"
  );
  let mut dst3 = cap_hll.clone();
  dst3.merge(&bigger);
  assert_eq!(dst3.as_bytes()[4], HLL_DENSE, "更新后越限必须升级 Dense");
  assert_eq!(dst3.get_register(0), 9);
  assert_eq!(dst3.count(), 1);

  OK
}

/// 验证 PFADD 128 个随机 32 字节元素的更新返回与估算误差 (C# HyperLogLogUpdateReturnTest 语义)：
/// Sparse 自动升级实例的更新返回必须与 Dense 参照实例逐步一致
#[test]
fn test_pfadd_random_updates_and_accuracy() -> Void {
  let mut rng = Rng(674_386);
  let mut target = HyperLogLog::new();
  // Dense 参照实例 (C# 以 InitDense 的参照 HLL 逐步对拍 expectedUpdated)
  let mut reference = HyperLogLog::new();
  reference.set_register(0, 0);
  assert_eq!(reference.as_bytes()[4], HLL_DENSE);
  let mut distinct = BTreeSet::new();

  for round in 0..4 {
    for _ in 0..128 {
      let mut value = [0u8; 32];
      for b in &mut value {
        *b = (rng.next() & 0xFF) as u8;
      }
      let updated = target.add(&value);
      let expected = reference.add(&value);
      assert_eq!(
        updated, expected,
        "Sparse 实例更新返回必须与 Dense 参照一致 (round={round})"
      );
      distinct.insert(value);
    }

    let est = target.count();
    let n = distinct.len();
    let err = (est as f64 - n as f64).abs() / n as f64;
    assert!(
      err < 0.04,
      "C# EstimationError < 4.0 阈值超标: est={est}, n={n}"
    );
  }
  OK
}
