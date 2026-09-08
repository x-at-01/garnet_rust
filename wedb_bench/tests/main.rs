use aok::{OK, Void};
use log::info;
use mimalloc::MiMalloc;
use wedb_bench::{
  BfTreeHarness, ComparisonItem, FjallHarness, RocksDbHarness, WedbHarness, get_process_rss_bytes,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

#[test]
fn test_harness_smoke() -> Void {
  let wedb = WedbHarness::new(1024, 64 * 1024, 16)?;
  let bftree = BfTreeHarness::new()?;
  let rocksdb = RocksDbHarness::new()?;
  let fjall = FjallHarness::new()?;

  let pairs = [
    (b"k1".as_slice(), b"v1".as_slice()),
    (b"k2".as_slice(), b"v2".as_slice()),
  ];

  // 1. 点写
  let w_stats = wedb.bench_upsert_batch("smoke_write", &pairs)?;
  assert_eq!(w_stats.total_ops, 2);

  let b_stats = bftree.bench_upsert_batch("smoke_write", &pairs)?;
  assert_eq!(b_stats.total_ops, 2);

  let r_stats = rocksdb.bench_upsert_batch("smoke_write", &pairs)?;
  assert_eq!(r_stats.total_ops, 2);

  let f_stats = fjall.bench_upsert_batch("smoke_write", &pairs)?;
  assert_eq!(f_stats.total_ops, 2);

  // 2. 点读
  let keys = [b"k1".as_slice(), b"k2".as_slice()];
  let w_rstats = wedb.bench_read_batch("smoke_read", &keys)?;
  assert_eq!(w_rstats.total_ops, 2);

  let b_rstats = bftree.bench_read_batch("smoke_read", &keys)?;
  assert_eq!(b_rstats.total_ops, 2);

  let r_rstats = rocksdb.bench_read_batch("smoke_read", &keys)?;
  assert_eq!(r_rstats.total_ops, 2);

  let f_rstats = fjall.bench_read_batch("smoke_read", &keys)?;
  assert_eq!(f_rstats.total_ops, 2);

  // 3. 点删除
  let del_keys = [b"k1".as_slice()];
  let w_dstats = wedb.bench_delete_batch("smoke_delete", &del_keys)?;
  assert_eq!(w_dstats.total_ops, 1);

  let b_dstats = bftree.bench_delete_batch("smoke_delete", &del_keys)?;
  assert_eq!(b_dstats.total_ops, 1);

  let r_dstats = rocksdb.bench_delete_batch("smoke_delete", &del_keys)?;
  assert_eq!(r_dstats.total_ops, 1);

  let f_dstats = fjall.bench_delete_batch("smoke_delete", &del_keys)?;
  assert_eq!(f_dstats.total_ops, 1);

  // 4. 范围切片查询
  let zset_key = b"smoke_zset";
  let z_items = [(10.0, b"k1".as_slice()), (20.0, b"k2".as_slice())];
  wedb.zadd_elements(zset_key, &z_items)?;
  let z_stats = wedb.bench_zrange_queries("smoke_zrange", zset_key, &[(0, 1)])?;
  assert_eq!(z_stats.total_ops, 1);

  let ranges = [(b"k1".as_slice(), b"k2".as_slice())];
  let br_stats = bftree.bench_range_queries("smoke_range", &ranges)?;
  assert_eq!(br_stats.total_ops, 1);

  let rr_stats = rocksdb.bench_range_queries("smoke_range", &ranges)?;
  assert_eq!(rr_stats.total_ops, 1);

  let fr_stats = fjall.bench_range_queries("smoke_range", &ranges)?;
  assert_eq!(fr_stats.total_ops, 1);

  // 5. 报表与加速比验证
  let item = ComparisonItem {
    wedb: w_stats,
    bftree: b_stats,
    rocksdb: r_stats,
    fjall: f_stats,
  };
  assert!(item.speedup_vs_rocksdb() >= 0.0);
  assert!(item.speedup_vs_fjall() >= 0.0);
  assert!(item.speedup_vs_bftree() >= 0.0);
  let report = wedb_bench::BenchmarkReport {
    options: wedb_bench::BenchOpt::default(),
    items: vec![item],
    total_dataset_bytes: 1024,
    total_unique_records: 4,
    wedb_disk_bytes: wedb.disk_usage(),
    bftree_disk_bytes: bftree.disk_usage(),
    rocksdb_disk_bytes: rocksdb.disk_usage(),
    fjall_disk_bytes: fjall.disk_usage(),
    wedb_mem_bytes: wedb.memory_usage(),
    bftree_mem_bytes: bftree.memory_usage(),
    rocksdb_mem_bytes: rocksdb.memory_usage(),
    fjall_mem_bytes: fjall.memory_usage(),
    process_rss_bytes: get_process_rss_bytes(),
  };
  let md_zh = wedb_bench::format_markdown_report_zh(&report);
  assert!(md_zh.contains("WeDB") && md_zh.contains("WeDb-BfTree"));
  let md_en = wedb_bench::format_markdown_report_en(&report);
  assert!(md_en.contains("WeDB") && md_en.contains("WeDb-BfTree"));

  info!("评测包装 Harness 冒烟测试（包含 WeDB、BfTree、RocksDB、Fjall）通过");
  OK
}

#[test]
fn test_bench_options_and_geomean() -> Void {
  let config = wkv::StoreConfig::new(1024, 64 * 1024, 16, 0.5)?;
  let wedb = WedbHarness::new_with_config(config)?;
  let zset_key = b"smoke_zset_pure";
  let items = [(1.0, b"a".as_slice()), (2.0, b"b".as_slice())];
  wedb.zadd_elements(zset_key, &items)?;

  let res = wedb.bench_zrange_queries("smoke_zrange", zset_key, &[(0, 1)])?;
  assert_eq!(res.total_ops, 1);

  // 验证 BenchOpt 与加权几何平均数计算
  let options = wedb_bench::BenchOpt::from_profile("quick");
  assert_eq!(options.seq_count, 30_000);
  assert_eq!(
    options.memory_budget_mb,
    wedb_bench::DEFAULT_MEMORY_BUDGET_MB
  );
  assert_eq!(
    wedb_bench::DEFAULT_MEMORY_BUDGET_BYTES,
    (wedb_bench::DEFAULT_MEMORY_BUDGET_MB * wedb_bench::MB_BYTES) as u64
  );
  assert_eq!(
    wedb_bench::DEFAULT_LSM_CACHE_BYTES + wedb_bench::DEFAULT_LSM_MEMTABLE_BYTES,
    wedb_bench::DEFAULT_MEMORY_BUDGET_BYTES
  );

  let wedb_default = WedbHarness::default_budget(10_000)?;
  assert_eq!(wedb_default.disk_usage(), 0);

  let rep = wedb_bench::BenchmarkReport {
    options,
    items: vec![ComparisonItem {
      wedb: res.clone(),
      bftree: res.clone(),
      rocksdb: res.clone(),
      fjall: res,
    }],
    total_dataset_bytes: 1024,
    total_unique_records: 4,
    wedb_disk_bytes: 1024,
    bftree_disk_bytes: 1024,
    rocksdb_disk_bytes: 1024,
    fjall_disk_bytes: 1024,
    wedb_mem_bytes: 1024,
    bftree_mem_bytes: 1024,
    rocksdb_mem_bytes: 1024,
    fjall_mem_bytes: 1024,
    process_rss_bytes: 1024,
  };
  let (w_geo, b_geo, r_geo, f_geo) = rep.weighted_geomean();
  assert!(w_geo > 0.0);
  assert!(b_geo > 0.0);
  assert!(r_geo > 0.0);
  assert!(f_geo > 0.0);

  info!("纯存储基准配置与4引擎加权几何平均数计算验证通过");
  OK
}
