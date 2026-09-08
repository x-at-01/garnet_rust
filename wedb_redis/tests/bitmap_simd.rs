use std::sync::Arc;

use aok::{OK, Void};
use compio::runtime::Runtime;
use fastrand::Rng;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::{
  BitPosOffsetType, BitmapError, BitmapOp,
  bitmap_simd::{
    BIT_RANGE_MASK, OFFSET_TYPE_BIT, OFFSET_TYPE_BYTE, bit_index_count_byte, bitpos_bit_search,
    bitpos_bit_search_typed, bitpos_byte_search, bitpos_byte_search_scalar,
    bitpos_byte_search_scalar_typed, bitpos_byte_search_typed, bitpos_driver,
    process_negative_offset, simd_bit_count, simd_bit_count_range, simd_bitop, simd_bitop_binary,
    simd_bitop_not,
  },
  prelude::*,
};
use wkv::{StoreConfig, WedbStore};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 标量逐字节计算作为对比黄金参考（Ground Truth）
fn scalar_bitop_ref(op: BitmapOp, sources: &[&[u8]], dst: &mut [u8]) {
  if sources.is_empty() {
    return;
  }
  if op == BitmapOp::Not {
    for (d, &s) in dst.iter_mut().zip(sources[0]) {
      *d = !s;
    }
    return;
  }
  if sources.len() == 1 {
    dst[..sources[0].len()].copy_from_slice(sources[0]);
    return;
  }
  for (i, dst_byte) in dst.iter_mut().enumerate() {
    let mut val = sources[0].get(i).copied().unwrap_or(0);
    for src in &sources[1..] {
      let rhs = src.get(i).copied().unwrap_or(0);
      match op {
        BitmapOp::And => val &= rhs,
        BitmapOp::Or => val |= rhs,
        BitmapOp::Xor => val ^= rhs,
        BitmapOp::Diff => val &= !rhs,
        BitmapOp::Not => unreachable!(),
      }
    }
    *dst_byte = val;
  }
}

fn scalar_popcount_ref(bytes: &[u8]) -> usize {
  bytes.iter().map(|&b| b.count_ones() as usize).sum()
}

/// 标量逐 bit 遍历作为 bitpos 对比黄金参考（Ground Truth）
fn scalar_bitpos_ref(
  input: &[u8],
  start_offset: i64,
  end_offset: i64,
  search_for: u8,
  offset_type: u8,
) -> i64 {
  let len = input.len() as i64;
  if len == 0 {
    return if search_for == 0 && start_offset <= 0 && (end_offset == -1 || end_offset >= 0) {
      0
    } else {
      -1
    };
  }

  let (start_bit, end_bit, is_end_clamped) = if offset_type == 0 {
    let s = if start_offset < 0 {
      process_negative_offset(start_offset, len)
    } else {
      start_offset
    };
    let e = if end_offset < 0 {
      process_negative_offset(end_offset, len)
    } else {
      end_offset
    };
    if s >= len || s > e {
      return -1;
    }
    let clamped_e = if e >= len { len - 1 } else { e };
    (
      s * 8,
      (clamped_e + 1) * 8 - 1,
      end_offset >= len || end_offset == -1,
    )
  } else {
    let bit_len = len * 8;
    let s = if start_offset < 0 {
      process_negative_offset(start_offset, bit_len)
    } else {
      start_offset
    };
    let e = if end_offset < 0 {
      process_negative_offset(end_offset, bit_len)
    } else {
      end_offset
    };
    let s_byte = s >> 3;
    let e_byte = e >> 3;
    if s_byte >= len || s_byte > e_byte {
      return -1;
    }
    let clamped_e = if e_byte >= len { bit_len - 1 } else { e };
    (s, clamped_e, end_offset >= bit_len || end_offset == -1)
  };

  for bit_pos in start_bit..=end_bit {
    let byte_idx = (bit_pos >> 3) as usize;
    let bit_idx = 7 - (bit_pos & 7);
    let bit_val = (input[byte_idx] >> bit_idx) & 1;
    if bit_val == search_for {
      return bit_pos;
    }
  }

  if search_for == 0 && is_end_clamped {
    return len * 8;
  }

  -1
}

/// 单元测试 1: BITOP 正确性全算子与多尺度切片测试（`BitmapManagerBitOp`）
#[test]
fn test_bitop_simd_correctness() -> Void {
  let mut rng = Rng::with_seed(20260906);
  let test_lens = [
    0, 1, 2, 7, 8, 15, 16, 23, 31, 32, 47, 63, 64, 99, 127, 128, 199, 255, 256, 300, 511, 512,
    1024, 2048, 4097,
  ];

  for &len in &test_lens {
    if len == 0 {
      let mut dst = [];
      let written = simd_bitop(BitmapOp::And, &[], &mut dst)?;
      assert_eq!(written, 0);
      continue;
    }

    let mut s0 = vec![0u8; len];
    let mut s1 = vec![0u8; len];
    rng.fill(&mut s0);
    rng.fill(&mut s1);

    for op in [BitmapOp::And, BitmapOp::Or, BitmapOp::Xor, BitmapOp::Diff] {
      let mut actual = vec![0u8; len];
      let mut expected = vec![0u8; len];

      simd_bitop(op, &[&s0, &s1], &mut actual)?;
      scalar_bitop_ref(op, &[&s0, &s1], &mut expected);

      assert_eq!(
        actual, expected,
        "BITOP {op:?} 在长度 {len} 下运算结果与标量参考不一致"
      );
    }

    // 单源 NOT 测试
    let mut actual_not = vec![0u8; len];
    let mut expected_not = vec![0u8; len];
    simd_bitop(BitmapOp::Not, &[&s0], &mut actual_not)?;
    scalar_bitop_ref(BitmapOp::Not, &[&s0], &mut expected_not);
    assert_eq!(actual_not, expected_not, "BITOP NOT 在长度 {len} 下不一致");
  }

  OK
}

/// 单元测试 2: 多源（3~7 个源）与不同长度交叉位操作测试
#[test]
fn test_bitop_multi_sources_unequal_lengths() -> Void {
  let mut rng = Rng::with_seed(987654321);

  let lengths = [17, 128, 256, 400, 1024];
  let sources_data: Vec<Vec<u8>> = lengths
    .iter()
    .map(|&l| {
      let mut buf = vec![0u8; l];
      rng.fill(&mut buf);
      buf
    })
    .collect();

  let max_len = *lengths.iter().max().unwrap();
  let sources: Vec<&[u8]> = sources_data.iter().map(|b| b.as_slice()).collect();

  for op in [BitmapOp::And, BitmapOp::Or, BitmapOp::Xor, BitmapOp::Diff] {
    let mut actual = vec![0u8; max_len];
    let mut expected = vec![0u8; max_len];

    let written = simd_bitop(op, &sources, &mut actual)?;
    assert_eq!(written, max_len);

    scalar_bitop_ref(op, &sources, &mut expected);
    assert_eq!(
      actual, expected,
      "多源多长度 BITOP {op:?} 结果与标量参考不一致"
    );
  }

  // 校验不同长度下 2 个源的尾部处理（例如 s0 较短，s1 较长）
  let s_short = &sources_data[0]; // 17 字节
  let s_long = &sources_data[4]; // 1024 字节
  for op in [BitmapOp::And, BitmapOp::Or, BitmapOp::Xor, BitmapOp::Diff] {
    let mut actual = vec![0u8; s_long.len()];
    let mut expected = vec![0u8; s_long.len()];

    simd_bitop(op, &[s_short, s_long], &mut actual)?;
    scalar_bitop_ref(op, &[s_short, s_long], &mut expected);
    assert_eq!(actual, expected, "短前长后 2 源 BITOP {op:?} 失败");

    // 反转顺序：长前短后
    simd_bitop(op, &[s_long, s_short], &mut actual)?;
    scalar_bitop_ref(op, &[s_long, s_short], &mut expected);
    assert_eq!(actual, expected, "长前短后 2 源 BITOP {op:?} 失败");
  }

  OK
}

/// 单元测试 3: 边界异常参数与容错测试
#[test]
fn test_bitop_edge_cases() -> Void {
  let buf1 = [1u8, 2, 3];
  let buf2 = [4u8, 5];
  let mut dst = [0u8; 3];

  // 1. NOT 传入多个源应报错
  let err_not = simd_bitop(BitmapOp::Not, &[&buf1, &buf2], &mut dst);
  assert_eq!(err_not, Err(BitmapError::NotRequiresSingleSource));

  // 2. DIFF 仅传入 1 个源应报错
  let err_diff = simd_bitop(BitmapOp::Diff, &[&buf1], &mut dst);
  assert_eq!(err_diff, Err(BitmapError::DiffRequiresMultipleSources));

  // 3. 目标缓冲区过小报错
  let mut small_dst = [0u8; 2];
  let err_small = simd_bitop(BitmapOp::Or, &[&buf1, &buf2], &mut small_dst);
  assert_eq!(err_small, Err(BitmapError::DestinationBufferTooSmall));

  // 4. 空源集合返回 0
  let mut any_dst = [0u8; 10];
  let empty_written = simd_bitop(BitmapOp::And, &[], &mut any_dst)?;
  assert_eq!(empty_written, 0);

  OK
}

/// 单元测试 4: SIMD 位计数全尺寸正确性测试（`BitmapManagerBitCount`）
#[test]
fn test_simd_bitcount_correctness() -> Void {
  let mut rng = Rng::with_seed(123456789);

  // 1. 空切片
  assert_eq!(simd_bit_count(&[]), 0);

  // 2. 所有单字节 0..=255
  for b in 0..=255u8 {
    let slice = [b];
    assert_eq!(simd_bit_count(&slice), b.count_ones() as usize);
  }

  // 3. 特殊固定模式（全 0、全 1、0xAA、0x55）在不同长度下的正确性
  let patterns = [0x00u8, 0xFF, 0xAA, 0x55, 0x01, 0x80];
  let lens = [
    1, 2, 7, 8, 15, 16, 31, 32, 63, 64, 127, 128, 129, 255, 256, 257, 512, 1024, 4096,
  ];

  for &pat in &patterns {
    for &len in &lens {
      let buf = vec![pat; len];
      let expected = (pat.count_ones() as usize) * len;
      let actual = simd_bit_count(&buf);
      assert_eq!(
        actual, expected,
        "模式 {pat:#04x} 在长度 {len} 下的 bitcount 结果错误"
      );
    }
  }

  // 4. 随机内容任意切片测试
  for &len in &lens {
    let mut buf = vec![0u8; len];
    rng.fill(&mut buf);
    let expected = scalar_popcount_ref(&buf);
    let actual = simd_bit_count(&buf);
    assert_eq!(
      actual, expected,
      "随机数据在长度 {len} 下的 bitcount 与标量不匹配"
    );
  }

  OK
}

/// 单元测试 5: 单字节与切片局部位计数（`BitIndexCount`）
#[test]
fn test_bit_index_count() -> Void {
  // 1. 单字节 bit 计数测试
  // 0b1000_0000 (0x80): 最高位 bit 0 为 1
  assert_eq!(bit_index_count_byte(0x80, 0, 1), 1);
  assert_eq!(bit_index_count_byte(0x80, 1, 8), 0);

  // 0b0000_0001 (0x01): 最低位 bit 7 为 1
  assert_eq!(bit_index_count_byte(0x01, 0, 7), 0);
  assert_eq!(bit_index_count_byte(0x01, 7, 8), 1);

  // 0b1111_0000 (0xF0): bit 0..4 全为 1
  assert_eq!(bit_index_count_byte(0xF0, 0, 4), 4);
  assert_eq!(bit_index_count_byte(0xF0, 4, 8), 0);
  assert_eq!(bit_index_count_byte(0xF0, 2, 6), 2);

  // 2. 字节范围 (BYTE) 与位范围 (BIT) 测试
  let data = [0xFFu8, 0x00, 0xF0, 0x0F, 0xAA];
  // 索引对应二进制: // 0: 11111111 (8)
  // 1: 00000000 (0) // 2: 11110000 (4)
  // 3: 00001111 (4) // 4: 10101010 (4)
  // 全量 1 总数: 20 // 全区间 byte
  assert_eq!(simd_bit_count_range(&data, 0, -1, false), 20);
  // 单字节 byte
  assert_eq!(simd_bit_count_range(&data, 0, 0, false), 8);
  assert_eq!(simd_bit_count_range(&data, 1, 1, false), 0);
  assert_eq!(simd_bit_count_range(&data, 2, 3, false), 8);
  // 负向索引 byte
  assert_eq!(simd_bit_count_range(&data, -3, -2, false), 8);

  // 位范围 (BIT) // 统计 bit 0..=7 (第 0 字节，全部 8 个 1)
  assert_eq!(simd_bit_count_range(&data, 0, 7, true), 8);
  // 统计 bit 0..=15 (第 0 与第 1 字节)
  assert_eq!(simd_bit_count_range(&data, 0, 15, true), 8);
  // 统计跨字节局部 bit: data[2] 的 bit 4..7 (0b0000 为 0) 与 data[3] 的 bit 0..3 (0b0000 为 0) // data[2] bit offset: 16..23. data[3] bit offset: 24..31.
  assert_eq!(simd_bit_count_range(&data, 20, 27, true), 0);
  // data[3] 的 28..31 (4 个 1) 与 data[4] 的 32..35 (1010 -> 2 个 1) -> 6
  assert_eq!(simd_bit_count_range(&data, 28, 35, true), 6);

  OK
}

/// 单元测试 6: 对 bit_index_count_byte 进行 256 种字节与全部有效区间的 100% 穷举形式化验证
#[test]
fn test_bit_index_count_exhaustive() -> Void {
  for b in 0..=255u8 {
    for s in 0..=8 {
      for e in s..=8 {
        let mut truth = 0;
        for bit in s..e {
          if (b >> (7 - bit)) & 1 == 1 {
            truth += 1;
          }
        }
        let actual = bit_index_count_byte(b, s, e);
        assert_eq!(
          actual, truth,
          "字节 {b:#04x} 在区间 [{s}, {e}) 计算错误: 期望 {truth}, 实际 {actual}"
        );
      }
    }
  }
  OK
}

/// 单元测试 7: 验证 simd_bitop_not 与 simd_bitop_binary 零分配接口
#[test]
fn test_simd_bitop_specialized_apis() -> Void {
  let s0 = [0xAAu8; 100];
  let s1 = [0x55u8; 150];
  let mut dst = [0u8; 150];

  // NOT

  simd_bitop_not(&s0, &mut dst[..100])?;
  assert_eq!(&dst[..100], &[0x55u8; 100]);

  // OR

  simd_bitop_binary(BitmapOp::Or, &s0, &s1, &mut dst)?;
  assert_eq!(&dst[..100], &[0xFFu8; 100]);
  assert_eq!(&dst[100..150], &[0x55u8; 50]);

  // AND

  simd_bitop_binary(BitmapOp::And, &s0, &s1, &mut dst)?;
  assert_eq!(&dst[..100], &[0x00u8; 100]);
  assert_eq!(&dst[100..150], &[0x00u8; 50]);

  // XOR

  simd_bitop_binary(BitmapOp::Xor, &s0, &s1, &mut dst)?;
  assert_eq!(&dst[..100], &[0xFFu8; 100]);
  assert_eq!(&dst[100..150], &[0x55u8; 50]);

  // DIFF

  simd_bitop_binary(BitmapOp::Diff, &s0, &s1, &mut dst)?;
  assert_eq!(&dst[..100], &[0xAAu8; 100]);
  assert_eq!(&dst[100..150], &[0x00u8; 50]);

  OK
}

/// 单元测试 8: 验证非 8 字节/非 Cache Line 对齐切片的剥离逻辑与位计数正确性
#[test]
fn test_unaligned_slice_peeling() -> Void {
  let mut rng = Rng::with_seed(55555);
  let mut buffer = vec![0u8; 1024];
  rng.fill(&mut buffer);

  // 针对不同的非对齐起始偏移（0..8）与不同的长度进行全面覆盖
  for align_offset in 0..8 {
    for len in [0, 1, 7, 8, 15, 16, 31, 33, 63, 65, 127, 129, 255, 300] {
      if align_offset + len <= buffer.len() {
        let slice = &buffer[align_offset..align_offset + len];
        let expected = scalar_popcount_ref(slice);
        let actual = simd_bit_count(slice);
        assert_eq!(
          actual, expected,
          "非对齐偏移 {align_offset} 长度 {len} 下 bitcount 不匹配"
        );
      }
    }
  }

  OK
}

/// 单元测试 9: 大位图（1MB）SIMD 极限吞吐与位操作全量回归
#[test]
fn test_large_bitmap_simd() -> Void {
  let mut rng = Rng::with_seed(10101010);
  let size = 1024 * 1024; // 1MB 大位图

  let mut b0 = vec![0u8; size];
  let mut b1 = vec![0u8; size];
  rng.fill(&mut b0);
  rng.fill(&mut b1);

  // 1. 大位图位计数正确性验证
  let expected_cnt0 = scalar_popcount_ref(&b0);
  let actual_cnt0 = simd_bit_count(&b0);
  assert_eq!(actual_cnt0, expected_cnt0, "1MB 大位图位计数不一致");

  // 2. 大位图全位操作与标量黄金参考全量比对
  for op in [
    BitmapOp::And,
    BitmapOp::Or,
    BitmapOp::Xor,
    BitmapOp::Diff,
    BitmapOp::Not,
  ] {
    let mut actual = vec![0u8; size];
    let mut expected = vec![0u8; size];

    if op == BitmapOp::Not {
      simd_bitop(op, &[&b0], &mut actual)?;
      scalar_bitop_ref(op, &[&b0], &mut expected);
    } else {
      simd_bitop(op, &[&b0, &b1], &mut actual)?;
      scalar_bitop_ref(op, &[&b0, &b1], &mut expected);
    }

    assert_eq!(
      actual, expected,
      "1MB 大位图在算子 {op:?} 下与标量黄金参考完全一致"
    );
  }

  OK
}

/// 单元测试 10: WeDB StoreSession 中 bitop 与 bitcount 实际业务集成测试
#[test]
fn test_redis_session_bitop_and_bitcount() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("redis_bitmap_test.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
    let config = StoreConfig::new(2048, 64 * 1024, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    // 1. SETBIT / GETBIT 基础集成
    let bm_key = b"bm:test:1";
    let old = session.setbit(bm_key, 0, 1).await?;
    assert_eq!(old, 0);
    let old2 = session.setbit(bm_key, 7, 1).await?;
    assert_eq!(old2, 0);
    let bit0 = session.getbit(bm_key, 0).await?;
    assert_eq!(bit0, 1);
    let bit7 = session.getbit(bm_key, 7).await?;
    assert_eq!(bit7, 1);
    let bit1 = session.getbit(bm_key, 1).await?;
    assert_eq!(bit1, 0);

    // 当前 bm:test:1 的字节应为 0b1000_0001 (0x81)
    let cnt = session.bitcount(bm_key, None).await?;
    assert_eq!(cnt, 2);

    // 2. 准备第二个位图 bm:test:2 (写入 0b0100_0010, 0x42)
    let bm_key2 = b"bm:test:2";
    session.setbit(bm_key2, 1, 1).await?;
    session.setbit(bm_key2, 6, 1).await?;
    let cnt2 = session.bitcount(bm_key2, None).await?;
    assert_eq!(cnt2, 2);

    // 3. BITOP OR -> dest_or (0x81 | 0x42 = 0b1100_0011, count = 4)
    let dest_or = b"bm:dest:or";
    let written = session
      .bitop(BitmapOp::Or, dest_or, &[bm_key, bm_key2])
      .await?;
    assert_eq!(written, 1);
    assert_eq!(session.bitcount(dest_or, None).await?, 4);

    // 4. BITOP AND -> dest_and (0x81 & 0x42 = 0x00, count = 0)
    let dest_and = b"bm:dest:and";
    session
      .bitop(BitmapOp::And, dest_and, &[bm_key, bm_key2])
      .await?;
    assert_eq!(session.bitcount(dest_and, None).await?, 0);

    // 5. BITOP XOR -> dest_xor (0x81 ^ 0x42 = 0xC3, count = 4)
    let dest_xor = b"bm:dest:xor";
    session
      .bitop(BitmapOp::Xor, dest_xor, &[bm_key, bm_key2])
      .await?;
    assert_eq!(session.bitcount(dest_xor, None).await?, 4);

    // 6. BITOP DIFF -> dest_diff (0x81 & ~0x42 = 0x81, count = 2)
    let dest_diff = b"bm:dest:diff";
    session
      .bitop(BitmapOp::Diff, dest_diff, &[bm_key, bm_key2])
      .await?;
    assert_eq!(session.bitcount(dest_diff, None).await?, 2);

    // 7. BITOP NOT -> dest_not (~0x81 = 0x7E, count = 6)
    let dest_not = b"bm:dest:not";
    session.bitop(BitmapOp::Not, dest_not, &[bm_key]).await?;
    assert_eq!(session.bitcount(dest_not, None).await?, 6);

    // 8. 原位覆写：目标键也是源键 (dest == src)

    session.bitop(BitmapOp::Not, bm_key, &[bm_key]).await?;
    assert_eq!(session.bitcount(bm_key, None).await?, 6);

    // 9. 不存在键的容错：空键 BITOP 导致目标键被删除且返回 0
    let empty_k1 = b"bm:nonexistent:1";
    let empty_k2 = b"bm:nonexistent:2";
    let dest_empty = b"bm:dest:empty";
    session.upsert(dest_empty, b"temp").await?;
    let len_res = session
      .bitop(BitmapOp::And, dest_empty, &[empty_k1, empty_k2])
      .await?;
    assert_eq!(len_res, 0);
    assert_eq!(session.read(dest_empty).await?, None);

    // 10. bitcount_range 模式校验
    let multi_byte_k = b"bm:multi";
    session
      .upsert(multi_byte_k, &[0xFF, 0x00, 0xF0, 0x0F])
      .await?;
    // BYTE 范围 [0, 1] -> 0xFF + 0x00 -> 8
    assert_eq!(session.bitcount_range(multi_byte_k, 0, 1, false).await?, 8);
    // BIT 范围 [0, 7] -> 8
    assert_eq!(session.bitcount_range(multi_byte_k, 0, 7, true).await?, 8);
    // BIT 范围 [16, 19] -> data[2] 的最高 4 位 (0xF0 中的 1111) -> 4
    assert_eq!(session.bitcount_range(multi_byte_k, 16, 19, true).await?, 4);

    // 11. session.bitpos 集成校验
    // bm:test:1 经前面的 NOT 覆写后值为 0x7E (0b0111_1110)
    // 首个 1 出现在 bit 1，首个 0 出现在 bit 0
    assert_eq!(session.bitpos(bm_key, 1, None, None, false).await?, 1);
    assert_eq!(session.bitpos(bm_key, 0, None, None, false).await?, 0);
    // 从 bit 2 开始搜 0：bit 7 为 0
    assert_eq!(session.bitpos(bm_key, 0, Some(2), None, true).await?, 7);
    // 不存在键：找 1 返回 -1，找 0 返回 0
    let non_key = b"bm:not:exist";
    assert_eq!(session.bitpos(non_key, 1, None, None, false).await?, -1);
    assert_eq!(session.bitpos(non_key, 0, None, None, false).await?, 0);

    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 单元测试 11: 1:1 `BitmapBitPosFixedTests` BYTE 模式基础搜索测试
#[test]
fn test_bitpos_garnet_byte_mode_tests() -> Void {
  let value = [0x00u8, 0xFF, 0xF0];

  // 1. pos = db.StringBitPosition(key, true, 0) -> 8
  let pos = bitpos_driver(&value, 0, -1, 1, OFFSET_TYPE_BYTE);
  assert_eq!(pos, 8);
  assert_eq!(
    bitpos_driver(&value, 0, -1, 1, BitPosOffsetType::Byte as u8),
    8
  );
  // 无效偏移类型容错
  assert_eq!(bitpos_driver(&value, 0, -1, 1, 99), -1);

  // 2. pos = db.StringBitPosition(key, true, 2, -1, StringIndexType.Byte) -> 16
  let pos2 = bitpos_driver(&value, 2, -1, 1, OFFSET_TYPE_BYTE);
  assert_eq!(pos2, 16);

  // 3. pos = db.StringBitPosition(key, true, 0, 0, StringIndexType.Byte) -> -1
  let pos3 = bitpos_driver(&value, 0, 0, 1, OFFSET_TYPE_BYTE);
  assert_eq!(pos3, -1);

  // 4. pos = db.StringBitPosition(key, false, 0, 0, StringIndexType.Byte) -> 0
  let pos4 = bitpos_driver(&value, 0, 0, 0, OFFSET_TYPE_BYTE);
  assert_eq!(pos4, 0);

  // 5. 从字节 1 开始找 0：第 1 字节 0xFF 全为 1，第 2 字节 0xF0 为 11110000，第一个 0 在 bit 20
  let pos5 = bitpos_driver(&value, 1, -1, 0, OFFSET_TYPE_BYTE);
  assert_eq!(pos5, 20);

  // 6. 字节 1..=1 找 0：第 1 字节全为 1，且范围被严格限制在 [1, 1]，应返回 -1
  let pos6 = bitpos_driver(&value, 1, 1, 0, OFFSET_TYPE_BYTE);
  assert_eq!(pos6, -1);

  // 7. 全 0 找 1 返回 -1
  let zeros = [0x00u8; 16];
  assert_eq!(bitpos_driver(&zeros, 0, -1, 1, OFFSET_TYPE_BYTE), -1);

  // 8. 全 1 找 0，未指定 end (-1)，返回末尾补充 0 位 (16 * 8 = 128)
  let ones = [0xFFu8; 16];
  assert_eq!(bitpos_driver(&ones, 0, -1, 0, OFFSET_TYPE_BYTE), 128);

  OK
}

/// 单元测试 12: 1:1 `BitmapBitPosFixedTests` BIT 模式位级搜索测试
#[test]
fn test_bitpos_garnet_bit_mode_tests() -> Void {
  let value = [0x00u8, 0xFF, 0xF0];

  // 1. pos = db.StringBitPosition(key, true, 7, 15, StringIndexType.Bit) -> 8
  let pos1 = bitpos_driver(&value, 7, 15, 1, OFFSET_TYPE_BIT);
  assert_eq!(pos1, 8);

  // 2. value = [0xf8, 0x6f, 0xf0] // 0xF8 = 11111000
  // 0x6F = 01101111 // 0xF0 = 11110000
  let value2 = [0xF8u8, 0x6F, 0xF0];

  // pos = db.StringBitPosition(key, true, 5, 17, StringIndexType.Bit) -> 9 // bit 5..7: 000; bit 8..15: 01101111 (bit 8 是 0, bit 9 是 1) -> 9
  let pos2 = bitpos_driver(&value2, 5, 17, 1, OFFSET_TYPE_BIT);
  assert_eq!(pos2, 9);

  // pos = db.StringBitPosition(key, true, 10, 12, StringIndexType.Bit) -> 10
  let pos3 = bitpos_driver(&value2, 10, 12, 1, OFFSET_TYPE_BIT);
  assert_eq!(pos3, 10);

  // pos = db.StringBitPosition(key, true, 20, 25, StringIndexType.Bit) -> -1 // bit 20..23 是 0000，bit 24..25 超出长度或为 0 -> -1
  let pos4 = bitpos_driver(&value2, 20, 25, 1, OFFSET_TYPE_BIT);
  assert_eq!(pos4, -1);

  // 3. value = [0xff, 0x7f, 0xf0] // pos = db.StringBitPosition(key, false, 7, 15, StringIndexType.Bit) -> 8
  // 0xFF (bit 0..7 为 1), 0x7F = 01111111 (bit 8 是 0) -> 8
  let value3 = [0xFFu8, 0x7F, 0xF0];
  let pos5 = bitpos_driver(&value3, 7, 15, 0, OFFSET_TYPE_BIT);
  assert_eq!(pos5, 8);

  // 4. 单字节内部局部搜索掩码校验
  // 0b0010_1000 (0x28): bit 2 为 1, bit 4 为 1
  let single = [0x28u8];
  assert_eq!(bitpos_driver(&single, 0, 1, 1, OFFSET_TYPE_BIT), -1);
  assert_eq!(bitpos_driver(&single, 0, 2, 1, OFFSET_TYPE_BIT), 2);
  assert_eq!(bitpos_driver(&single, 3, 3, 1, OFFSET_TYPE_BIT), -1);
  assert_eq!(bitpos_driver(&single, 3, 4, 1, OFFSET_TYPE_BIT), 4);
  assert_eq!(bitpos_driver(&single, 5, 7, 1, OFFSET_TYPE_BIT), -1);

  OK
}

/// 单元测试 13: 负偏移量回绕计算与越界防错（`ProcessNegativeOffset`）
#[test]
fn test_bitpos_negative_offsets() -> Void {
  let value = [0x00u8, 0x00, 0xFF, 0x00]; // 4 字节
  let len = value.len() as i64;

  // 1. process_negative_offset 函数本身计算验证
  assert_eq!(process_negative_offset(-1, len), 3);
  assert_eq!(process_negative_offset(-2, len), 2);
  assert_eq!(process_negative_offset(-4, len), 4);
  assert_eq!(process_negative_offset(-5, len), 3);
  assert_eq!(process_negative_offset(-1, 0), 0);

  // 2. BYTE 模式下负偏移搜索
  // 查找第 2 字节（即 -2..=-2 范围）的 1，位置应为 16
  assert_eq!(bitpos_driver(&value, -2, -2, 1, OFFSET_TYPE_BYTE), 16);
  // -1..=-1（最后 1 字节 0x00）找 1 -> -1
  assert_eq!(bitpos_driver(&value, -1, -1, 1, OFFSET_TYPE_BYTE), -1);

  // 3. 越界情况：start > end 返回 -1
  assert_eq!(bitpos_driver(&value, 3, 2, 1, OFFSET_TYPE_BYTE), -1);
  // start >= len 返回 -1
  assert_eq!(bitpos_driver(&value, 4, 4, 1, OFFSET_TYPE_BYTE), -1);
  assert_eq!(bitpos_driver(&value, 100, 200, 1, OFFSET_TYPE_BYTE), -1);

  // 4. BIT 模式下越界与负偏移
  let _bit_len = len * 8; // 32
  // -16 指向倒数第 16 位即 bit 16
  assert_eq!(bitpos_driver(&value, -16, -1, 1, OFFSET_TYPE_BIT), 16);
  // start > end
  assert_eq!(bitpos_driver(&value, 20, 10, 1, OFFSET_TYPE_BIT), -1);
  // start >= bit_len
  assert_eq!(bitpos_driver(&value, 32, 40, 1, OFFSET_TYPE_BIT), -1);

  OK
}

/// 单元测试 14: 防内存越界读取（Canary Byte Test，回归修复测试）
#[test]
fn test_bitpos_out_of_bounds_canary() -> Void {
  // 1. BYTE 模式 Canary 测试
  // 构造长度为 8 字节的全 0 真实数据，随后紧跟 1 字节全 1 的 Canary 哨兵
  let mut buf = vec![0x00u8; 8];
  buf.push(0xFF); // Canary
  let real_slice = &buf[..8];

  // 当显式指定超出范围的 end_offset（例如 1_000_000）搜索 1 时，驱动绝不可读入 Canary
  let pos_byte = bitpos_driver(real_slice, 0, 1_000_000, 1, OFFSET_TYPE_BYTE);
  assert_eq!(pos_byte, -1, "BYTE 模式绝对禁止越界读入 Canary 字节");

  // 2. BIT 模式 Canary 测试
  let mut buf_bit = vec![0x00u8; 3];
  buf_bit.push(0xFF); // Canary
  let real_slice_bit = &buf_bit[..3];

  let pos_bit = bitpos_driver(real_slice_bit, 0, 1_000_000, 1, OFFSET_TYPE_BIT);
  assert_eq!(pos_bit, -1, "BIT 模式绝对禁止越界读入 Canary 字节");

  OK
}

/// 单元测试 15: 找 0 时尾部补充虚拟 0 位全边界覆盖验证（对齐 Redis/Garnet 语义）
#[test]
fn test_bitpos_clear_bit_trailing_padding() -> Void {
  // 1. 空切片：找 0 返回 0，找 1 返回 -1
  assert_eq!(bitpos_driver(&[], 0, -1, 0, OFFSET_TYPE_BYTE), 0);
  assert_eq!(bitpos_driver(&[], 0, -1, 1, OFFSET_TYPE_BYTE), -1);
  assert_eq!(bitpos_driver(&[], 1, 2, 0, OFFSET_TYPE_BYTE), -1);

  // 2. 1 字节全 1 (0xFF)
  let b1 = [0xFFu8];
  // 未指定 end (-1)，首个 0 位在位图之外的 bit 8
  assert_eq!(bitpos_driver(&b1, 0, -1, 0, OFFSET_TYPE_BYTE), 8);
  // 指定   end = 0 (限制在 byte 0)，无 0，返回 -1
  assert_eq!(bitpos_driver(&b1, 0, 0, 0, OFFSET_TYPE_BYTE), -1);
  // 指定 end >= len (包含补充字节)，返回 8
  assert_eq!(bitpos_driver(&b1, 0, 1, 0, OFFSET_TYPE_BYTE), 8);
  assert_eq!(bitpos_driver(&b1, 0, 100, 0, OFFSET_TYPE_BYTE), 8);

  // 3. BIT 模式下 1 字节全 1
  assert_eq!(bitpos_driver(&b1, 0, -1, 0, OFFSET_TYPE_BIT), 8);
  assert_eq!(bitpos_driver(&b1, 0, 7, 0, OFFSET_TYPE_BIT), -1);
  assert_eq!(bitpos_driver(&b1, 0, 8, 0, OFFSET_TYPE_BIT), 8);
  assert_eq!(bitpos_driver(&b1, 0, 100, 0, OFFSET_TYPE_BIT), 8);

  // 4. 3 字节全 1 [0xFF, 0xFF, 0xFF]
  let b3 = [0xFFu8; 3];
  assert_eq!(bitpos_driver(&b3, 0, -1, 0, OFFSET_TYPE_BYTE), 24);
  assert_eq!(bitpos_driver(&b3, 0, 2, 0, OFFSET_TYPE_BYTE), -1);
  assert_eq!(bitpos_driver(&b3, 0, 3, 0, OFFSET_TYPE_BYTE), 24);

  OK
}

/// 单元测试 16: 多尺度随机切片 SIMD 与标量黄金参考 100% 等价性测试（覆盖全跨度）
#[test]
fn test_bitpos_simd_random_exhaustive_equiv() -> Void {
  let mut rng = Rng::with_seed(20260907);
  let test_lens = [
    1, 2, 7, 8, 15, 16, 31, 32, 33, 63, 64, 65, 127, 128, 255, 256, 512, 1024,
  ];

  for &len in &test_lens {
    let mut buf = vec![0u8; len];
    rng.fill(&mut buf);

    for _ in 0..20 {
      let start_byte = rng.usize(0..len);
      let end_byte = rng.usize(start_byte..len);

      for search_for in [0u8, 1] {
        // BYTE 模式
        let actual_byte = bitpos_driver(
          &buf,
          start_byte as i64,
          end_byte as i64,
          search_for,
          OFFSET_TYPE_BYTE,
        );
        let expect_byte = scalar_bitpos_ref(
          &buf,
          start_byte as i64,
          end_byte as i64,
          search_for,
          OFFSET_TYPE_BYTE,
        );
        assert_eq!(
          actual_byte, expect_byte,
          "BYTE 模式在 len={len}, range=[{start_byte}, {end_byte}], search={search_for} 下不匹配"
        );

        // BIT 模式
        let bit_len = len * 8;
        let start_bit = rng.usize(0..bit_len);
        let end_bit = rng.usize(start_bit..bit_len);
        let actual_bit = bitpos_driver(
          &buf,
          start_bit as i64,
          end_bit as i64,
          search_for,
          OFFSET_TYPE_BIT,
        );
        let expect_bit = scalar_bitpos_ref(
          &buf,
          start_bit as i64,
          end_bit as i64,
          search_for,
          OFFSET_TYPE_BIT,
        );
        assert_eq!(
          actual_bit, expect_bit,
          "BIT 模式在 len={len}, range=[{start_bit}, {end_bit}], search={search_for} 下不匹配"
        );
      }
    }
  }

  OK
}

/// 单元测试 17: 1MB 大位图 SIMD 极限跳过与稀疏位高速定位测试
#[test]
fn test_bitpos_large_bitmap_simd() -> Void {
  let size = 1024 * 1024; // 1MB 大位图
  let mut buf = vec![0x00u8; size];

  // 1. 全 0 中搜 1，末尾没有 1 返回 -1
  let pos = bitpos_driver(&buf, 0, -1, 1, OFFSET_TYPE_BYTE);
  assert_eq!(pos, -1);

  // 2. 在第 500,000 字节的第 3 位设置 1（0b0010_0000 = 0x20）
  let target_byte = 500_000;
  buf[target_byte] = 0x20;
  let expected_bit = (target_byte as i64 * 8) + 2; // 0x20 从左向右第 3 位，位移 2
  let actual_bit = bitpos_driver(&buf, 0, -1, 1, OFFSET_TYPE_BYTE);
  assert_eq!(actual_bit, expected_bit, "1MB 大位图 SIMD 查找 1 失败");

  // 3. 在 BIT 模式下跨大区间搜索
  let actual_bit_mode = bitpos_driver(&buf, 0, (size as i64 * 8) - 1, 1, OFFSET_TYPE_BIT);
  assert_eq!(
    actual_bit_mode, expected_bit,
    "1MB 大位图 BIT 模式查找 1 失败"
  );

  // 4. 全 1 中搜 0：稀疏 0 位快速定位
  let mut buf_ones = vec![0xFFu8; size];
  let target_zero_byte = 750_000;
  buf_ones[target_zero_byte] = 0xDF; // 0b1101_1111，第 3 位为 0 (位移 2)
  let expected_zero_bit = (target_zero_byte as i64 * 8) + 2;
  let actual_zero_bit = bitpos_driver(&buf_ones, 0, -1, 0, OFFSET_TYPE_BYTE);
  assert_eq!(
    actual_zero_bit, expected_zero_bit,
    "1MB 大位图 SIMD 查找 0 失败"
  );

  OK
}

/// 单元测试 18: 预计算 BIT_RANGE_MASK 2D LUT 81 种组合形式化数学等价性验证
#[test]
fn test_bit_range_mask_exhaustive() -> Void {
  for (left, row) in BIT_RANGE_MASK.iter().enumerate() {
    for (right, &actual) in row.iter().enumerate() {
      let m_left = (0xFFu16 >> left) as u8;
      let m_right = (0xFFu16 >> right) as u8;
      let expected = m_left ^ m_right;
      assert_eq!(
        actual, expected,
        "BIT_RANGE_MASK[{left}][{right}] 与数学定义不一致"
      );

      if left < right {
        for bit in 0..8 {
          let is_set = (actual & (1 << (7 - bit))) != 0;
          if (left..right).contains(&bit) {
            assert!(
              is_set,
              "BIT_RANGE_MASK[{left}][{right}] 在 bit {bit} 处应为 1"
            );
          } else {
            assert!(
              !is_set,
              "BIT_RANGE_MASK[{left}][{right}] 在 bit {bit} 处应为 0"
            );
          }
        }
      }
    }
  }

  OK
}

/// 单元测试 19: 标量阶梯匹配算法（8/4/2/1 字节无分支梯级）全尺寸与单比特位置穷举验证
#[test]
fn test_bitpos_byte_search_scalar_exhaustive() -> Void {
  // 覆盖 0..=40 字节所有长度下的标量梯级
  for len in 0..=40 {
    let mut zeros = vec![0x00u8; len];
    let mut ones = vec![0xFFu8; len];

    if len > 0 {
      // 1. 全 0 中找 1 返回 -1
      assert_eq!(
        bitpos_byte_search_scalar_typed::<true>(&zeros, 0, len - 1),
        -1
      );
      assert_eq!(bitpos_byte_search_scalar(&zeros, 0, len - 1, 1), -1);

      // 2. 全 1 中找 0 返回 -1
      assert_eq!(
        bitpos_byte_search_scalar_typed::<false>(&ones, 0, len - 1),
        -1
      );
      assert_eq!(bitpos_byte_search_scalar(&ones, 0, len - 1, 0), -1);

      // 3. 在每一个可能的比特位置设置单个 1，验证查找定位百分之百精准
      for target_bit in 0..(len * 8) {
        let byte_idx = target_bit / 8;
        let bit_in_byte = 7 - (target_bit % 8);
        zeros[byte_idx] |= 1 << bit_in_byte;

        let found_pos = bitpos_byte_search_scalar_typed::<true>(&zeros, 0, len - 1);
        assert_eq!(
          found_pos, target_bit as i64,
          "长度 {len} 下在 bit {target_bit} 处搜 1 失败"
        );

        let found_dyn = bitpos_byte_search_scalar(&zeros, 0, len - 1, 1);
        assert_eq!(found_dyn, target_bit as i64);

        zeros[byte_idx] &= !(1 << bit_in_byte); // 恢复
      }

      // 4. 在每一个可能的比特位置清除单个 0，验证找 0 定位百分之百精准
      for target_bit in 0..(len * 8) {
        let byte_idx = target_bit / 8;
        let bit_in_byte = 7 - (target_bit % 8);
        ones[byte_idx] &= !(1 << bit_in_byte);

        let found_pos = bitpos_byte_search_scalar_typed::<false>(&ones, 0, len - 1);
        assert_eq!(
          found_pos, target_bit as i64,
          "长度 {len} 下在 bit {target_bit} 处搜 0 失败"
        );

        let found_dyn = bitpos_byte_search_scalar(&ones, 0, len - 1, 0);
        assert_eq!(found_dyn, target_bit as i64);

        ones[byte_idx] |= 1 << bit_in_byte; // 恢复
      }
    } else {
      assert_eq!(bitpos_byte_search_scalar_typed::<true>(&zeros, 0, 0), -1);
      assert_eq!(bitpos_byte_search_scalar_typed::<false>(&ones, 0, 0), -1);
    }
  }

  OK
}

/// 单元测试 20: bitpos_bit_search 与 bitpos_byte_search 边界、空切片与越界契约安全测试
#[test]
fn test_bitpos_bit_search_exhaustive() -> Void {
  // 1. 空切片安全契约：禁止 panic，必须直接返回 -1
  assert_eq!(bitpos_bit_search(&[], 0, 0, 1), -1);
  assert_eq!(bitpos_bit_search(&[], 0, 0, 0), -1);
  assert_eq!(bitpos_bit_search_typed::<true>(&[], 0, 0), -1);
  assert_eq!(bitpos_bit_search_typed::<false>(&[], 0, 0), -1);
  assert_eq!(bitpos_byte_search(&[], 0, 0, 1), -1);
  assert_eq!(bitpos_byte_search_typed::<true>(&[], 0, 0), -1);

  // 2. 起始偏移越界契约
  let data = [0x55u8, 0xAA, 0xF0]; // 3 字节 = 24 位
  assert_eq!(bitpos_bit_search(&data, 24, 30, 1), -1);
  assert_eq!(bitpos_bit_search(&data, -1, 10, 1), -1);
  assert_eq!(bitpos_bit_search(&data, 10, 5, 1), -1);

  // 3. 结束偏移越界自动安全剪裁（Canary 隔离防越界读取测试）
  let mut canary_buf = vec![0x00u8; 8];
  canary_buf.push(0xFF); // 哨兵 Canary 字节
  let safe_slice = &canary_buf[..8];

  // 显式传入极大 end_bit_offset，验证底层实现绝对不读取超出 safe_slice 的 Canary
  assert_eq!(bitpos_bit_search(safe_slice, 0, 100_000, 1), -1);
  assert_eq!(bitpos_bit_search_typed::<true>(safe_slice, 0, 100_000), -1);

  // 4. 对齐与非对齐起始/结束区间的随机交叉验证
  let mut rng = Rng::with_seed(333444555);
  for len in [1, 2, 5, 8, 17, 33, 64, 128] {
    let mut buf = vec![0u8; len];
    rng.fill(&mut buf);
    let bit_len = (len * 8) as i64;

    for _ in 0..50 {
      let start_bit = rng.i64(0..bit_len);
      let end_bit = rng.i64(start_bit..bit_len);

      for search_for in [0u8, 1] {
        let actual = bitpos_bit_search(&buf, start_bit, end_bit, search_for);
        let expected = if search_for == 1 {
          let mut found = -1;
          for bit in start_bit..=end_bit {
            let b_idx = (bit >> 3) as usize;
            let bit_in_b = 7 - (bit & 7);
            if ((buf[b_idx] >> bit_in_b) & 1) == 1 {
              found = bit;
              break;
            }
          }
          found
        } else {
          let mut found = -1;
          for bit in start_bit..=end_bit {
            let b_idx = (bit >> 3) as usize;
            let bit_in_b = 7 - (bit & 7);
            if ((buf[b_idx] >> bit_in_b) & 1) == 0 {
              found = bit;
              break;
            }
          }
          found
        };

        assert_eq!(
          actual, expected,
          "len={len}, range=[{start_bit}, {end_bit}], search_for={search_for} 结果不一致"
        );
      }
    }
  }

  OK
}
