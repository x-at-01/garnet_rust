//! 报告文案的 i18n 资源模型（zh.yml / en.yml 编译期内嵌）

use std::sync::LazyLock;

use serde::Deserialize;

/// 十项基准测试的展示标题（顺序与 suite 结果项一一对应）
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Items {
  pub seq_insert: String,
  pub rand_insert: String,
  pub seq_read: String,
  pub rand_read: String,
  pub task_a: String,
  pub task_b: String,
  pub task_c: String,
  pub task_d: String,
  pub range_query: String,
  pub concurrency: String,
}

impl Items {
  /// 按结果项下标取展示标题（越界返回空串）
  #[inline]
  pub fn get_by_index(&self, idx: usize) -> &str {
    match idx {
      0 => &self.seq_insert,
      1 => &self.rand_insert,
      2 => &self.seq_read,
      3 => &self.rand_read,
      4 => &self.task_a,
      5 => &self.task_b,
      6 => &self.task_c,
      7 => &self.task_d,
      8 => &self.range_query,
      9 => &self.concurrency,
      _ => "",
    }
  }
}

/// 表格表头文案
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Headers {
  pub metric: String,
  pub throughput: String,
  pub ops_rate: String,
  pub p50_lat: String,
  pub p99_lat: String,
  pub speedup_vs_rocksdb: String,
  pub engine: String,
  pub geomean_tp: String,
  pub geomean_qps: String,
  pub relative_ratio: String,
  pub raw_dataset_size: String,
  pub disk_usage: String,
  pub space_amp: String,
  pub disk_features: String,
  pub mem_budget: String,
  pub mem_usage: String,
  pub mem_ratio: String,
  pub mem_features: String,
}

/// 四引擎展示名
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EngineNames {
  pub wedb: String,
  pub bftree: String,
  pub rocksdb: String,
  pub fjall: String,
}

/// 四引擎磁盘/内存特征说明
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct EngineSummaries {
  pub wedb_disk_features: String,
  pub wedb_mem_features: String,
  pub bftree_disk_features: String,
  pub bftree_mem_features: String,
  pub rocksdb_disk_features: String,
  pub rocksdb_mem_features: String,
  pub fjall_disk_features: String,
  pub fjall_mem_features: String,
}

/// 完整报告文案配置
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct I18nConfig {
  pub title: String,
  pub environment_title: String,
  pub env_write_ops: String,
  pub env_read_ops: String,
  pub env_dataset_size: String,
  pub env_total_ops: String,
  pub env_val_size: String,
  pub env_budget: String,
  pub env_peak_rss: String,
  pub env_mode: String,

  pub arch_title: String,
  pub arch_items: Vec<String>,

  pub benchmark_title: String,

  pub summary_tp_title: String,
  pub summary_disk_title: String,
  pub summary_mem_title: String,

  pub items: Items,
  pub headers: Headers,
  pub engine_names: EngineNames,
  pub engine_summaries: EngineSummaries,

  pub insights_title: String,
  pub insights: Vec<String>,
}

/// 中文文案（编译期内嵌，首次访问时解析）
static I18N_ZH: LazyLock<I18nConfig> = LazyLock::new(|| {
  serde_yaml_ng::from_str(include_str!("../i18n/zh.yml")).expect("zh.yml 格式必须正确")
});

/// 英文文案（编译期内嵌，首次访问时解析）
static I18N_EN: LazyLock<I18nConfig> = LazyLock::new(|| {
  serde_yaml_ng::from_str(include_str!("../i18n/en.yml")).expect("en.yml 格式必须正确")
});

#[inline]
pub(crate) fn get_i18n_zh() -> &'static I18nConfig {
  &I18N_ZH
}

#[inline]
pub(crate) fn get_i18n_en() -> &'static I18nConfig {
  &I18N_EN
}
