#![cfg_attr(docsrs, feature(doc_cfg))]

mod bftree_harness;
mod data_gen;
mod driver;
mod error;
mod fjall_harness;
mod i18n;
mod report;
mod rocksdb_harness;
mod stats;
mod suite;
mod wedb_harness;

pub use bftree_harness::BfTreeHarness;
pub use error::{Error, Result};
pub use fjall_harness::FjallHarness;
pub use report::{format_markdown_report_en, format_markdown_report_zh};
pub use rocksdb_harness::RocksDbHarness;
pub use stats::{BenchStats, MixedOp, get_process_rss_bytes};
pub use suite::{
  BenchOpt, BenchmarkReport, ComparisonItem, DEFAULT_LSM_CACHE_BYTES, DEFAULT_LSM_MEMTABLE_BYTES,
  DEFAULT_MEMORY_BUDGET_BYTES, DEFAULT_MEMORY_BUDGET_MB, LSM_MEMTABLE_DIVISOR, MB_BYTES,
  calc_lsm_budget_split, num_key, run_comparative_benchmarks,
};
pub use wedb_harness::WedbHarness;
