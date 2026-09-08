use std::hint::black_box;

use criterion::{Criterion, criterion_group, criterion_main};
use mimalloc::MiMalloc;
use wedb_bench::{FjallHarness, WedbHarness, num_key};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

fn bench_point_operations(c: &mut Criterion) {
  let mut group = c.benchmark_group("point_operations");

  let count = 1000usize;
  // 连续栈数组存储 10 字节键，避免 1000 次小堆分配
  let keys: Vec<[u8; 10]> = (0..count).map(|i| num_key::<10>(b"k:", i)).collect();
  let val = vec![b'v'; 100];
  let pairs: Vec<(&[u8], &[u8])> = keys
    .iter()
    .map(|k| (k.as_slice(), val.as_slice()))
    .collect();

  // 1. WeDB 点写评测
  group.bench_function("wedb_upsert", |b| {
    let wedb = WedbHarness::new(16384, 64 * 1024, 16).unwrap();
    b.iter(|| {
      let _ = wedb.bench_upsert_batch("iter", black_box(&pairs[..100]));
    });
  });

  // 2. Fjall 点写评测
  group.bench_function("fjall_insert", |b| {
    let fjall = FjallHarness::new().unwrap();
    b.iter(|| {
      let _ = fjall.bench_upsert_batch("iter", black_box(&pairs[..100]));
    });
  });

  // 3. WeDB 点读评测
  group.bench_function("wedb_read", |b| {
    let wedb = WedbHarness::new(16384, 64 * 1024, 16).unwrap();
    wedb.bench_upsert_batch("init", &pairs).unwrap();
    let sample_keys: Vec<&[u8]> = keys[..100].iter().map(|k| k.as_slice()).collect();
    b.iter(|| {
      let _ = wedb.bench_read_batch("iter", black_box(&sample_keys));
    });
  });

  // 4. Fjall 点读评测
  group.bench_function("fjall_get", |b| {
    let fjall = FjallHarness::new().unwrap();
    fjall.bench_upsert_batch("init", &pairs).unwrap();
    let sample_keys: Vec<&[u8]> = keys[..100].iter().map(|k| k.as_slice()).collect();
    b.iter(|| {
      let _ = fjall.bench_read_batch("iter", black_box(&sample_keys));
    });
  });

  group.finish();
}

fn bench_range_operations(c: &mut Criterion) {
  let mut group = c.benchmark_group("range_operations");

  let count = 500usize;
  let sorted_keys: Vec<[u8; 10]> = (0..count).map(|i| num_key::<10>(b"order:", i)).collect();
  let val = vec![b'v'; 64];

  let zset_key = b"bench_zset";
  let zset_init: Vec<(f64, &[u8])> = sorted_keys
    .iter()
    .enumerate()
    .map(|(i, k)| (i as f64 * 10.0, k.as_slice()))
    .collect();

  let fjall_init: Vec<(&[u8], &[u8])> = sorted_keys
    .iter()
    .map(|k| (k.as_slice(), val.as_slice()))
    .collect();

  // 1. WeDB 范围切片查询评测 (ZSet SkipList slice [10..60])
  group.bench_function("wedb_zrange_50_items", |b| {
    let wedb = WedbHarness::new(16384, 64 * 1024, 64).unwrap();
    wedb.zadd_elements(zset_key, &zset_init).unwrap();
    let queries = [(10isize, 59isize)];
    b.iter(|| {
      let _ = wedb.bench_zrange_queries("iter", zset_key, black_box(&queries));
    });
  });

  // 2. Fjall LSM 范围扫描评测 (Range scan [10..60])
  group.bench_function("fjall_range_50_items", |b| {
    let fjall = FjallHarness::new().unwrap();
    fjall.bench_upsert_batch("init", &fjall_init).unwrap();
    let ranges = [(sorted_keys[10].as_slice(), sorted_keys[59].as_slice())];
    b.iter(|| {
      let _ = fjall.bench_range_queries("iter", black_box(&ranges));
    });
  });

  group.finish();
}

criterion_group!(benches, bench_point_operations, bench_range_operations);
criterion_main!(benches);
