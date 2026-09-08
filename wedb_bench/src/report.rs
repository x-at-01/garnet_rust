//! Markdown 评测报告生成（中英双语，基于 i18n 模板）

use std::{
  fmt::{Display, Formatter, Result as FmtResult, Write},
  str::from_utf8_unchecked,
};

use crate::{
  i18n::{I18nConfig, get_i18n_en, get_i18n_zh},
  stats::{BYTES_IN_MB, GbVal, MbVal, SpaceAmpVal, calc_space_amp, push_f64_precise},
  suite::{BenchmarkReport, ComparisonItem},
};

/// 加粗判定中视为相等的浮点容差
const EPS: f64 = 1e-6;

/// 零堆分配操作数千分位格式化显示包装类型
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpsVal(pub f64);

impl Display for OpsVal {
  fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
    let val = self.0;
    if val.is_nan() || val <= 0.0 {
      return f.write_str("0");
    }
    let int_val = val.round() as u64;
    let mut itoa_buf = itoa::Buffer::new();
    let s = itoa_buf.format(int_val);
    let bytes = s.as_bytes();
    let len = bytes.len();
    if len <= 3 {
      return f.write_str(s);
    }
    let commas = (len - 1) / 3;
    let mut buf = [0u8; 32];
    let prefix = if len.is_multiple_of(3) { 3 } else { len % 3 };
    buf[..prefix].copy_from_slice(&bytes[..prefix]);
    let mut out_idx = prefix;
    for chunk in bytes[prefix..].as_chunks::<3>().0 {
      buf[out_idx] = b',';
      buf[out_idx + 1..out_idx + 4].copy_from_slice(chunk);
      out_idx += 4;
    }
    // 安全：buf 仅写入 ASCII 字节和逗号，长度为 len + commas
    let formatted = unsafe { from_utf8_unchecked(&buf[..len + commas]) };
    f.write_str(formatted)
  }
}

/// 操作数千分位格式化
#[inline]
pub(crate) fn format_ops(val: f64) -> String {
  OpsVal(val).to_string()
}

/// 两位小数 μs 单元格
#[inline]
fn cell_us(v: f64) -> String {
  let mut s = String::with_capacity(16);
  push_f64_precise(&mut s, v, 2);
  s
}

/// 倍率单元格（两位小数 + x 后缀）
#[inline]
fn cell_speedup(v: f64) -> String {
  let mut s = String::with_capacity(16);
  push_f64_precise(&mut s, v, 2);
  s.push('x');
  s
}

/// 四引擎指标单元格：求最优值并对最优单元格加粗
///
/// `max_best` 为 true 时取最大值（吞吐类），否则过滤零值/NaN 后取最小值（延迟类）
fn metric_cells(vals: [f64; 4], max_best: bool, fmt: impl Fn(f64) -> String) -> [String; 4] {
  let best = if max_best {
    vals.iter().copied().fold(f64::NEG_INFINITY, f64::max)
  } else {
    vals
      .iter()
      .copied()
      .filter(|&v| v > 0.0 && !v.is_nan())
      .fold(f64::INFINITY, f64::min)
  };
  vals.map(|v| {
    let cell = fmt(v);
    let hit = if max_best {
      best > 0.0 && (v - best).abs() < EPS
    } else {
      v > 0.0 && (v - best).abs() < EPS
    };
    if hit { format!("**{cell}**") } else { cell }
  })
}

/// 写入一行五列 Markdown 表格（指标名 + 四引擎单元格）
fn write_row(out: &mut String, label: &str, cells: &[String; 4]) {
  let _ = writeln!(
    out,
    "| {} | {} | {} | {} | {} |",
    label, cells[0], cells[1], cells[2], cells[3]
  );
}

/// 单占位符模板渲染
#[inline]
fn write_template_1(out: &mut String, template: &str, arg: impl Display) {
  if let Some((prefix, suffix)) = template.split_once("{}") {
    let _ = writeln!(out, "- {prefix}{arg}{suffix}\n");
  } else {
    let _ = writeln!(out, "- {template}\n");
  }
}

/// 双占位符模板渲染
#[inline]
fn write_template_2(out: &mut String, template: &str, arg1: impl Display, arg2: impl Display) {
  if let Some((part1, rest)) = template.split_once("{}")
    && let Some((part2, part3)) = rest.split_once("{}")
  {
    let _ = writeln!(out, "- {part1}{arg1}{part2}{arg2}{part3}\n");
  } else {
    let _ = writeln!(out, "- {template}\n");
  }
}

/// 输出中文 Markdown 评测报告（完全基于 zh.yml）
pub fn format_markdown_report_zh(report: &BenchmarkReport) -> String {
  format_markdown_report_with_i18n(report, get_i18n_zh())
}

/// 输出英文 Markdown 评测报告（完全基于 en.yml）
pub fn format_markdown_report_en(report: &BenchmarkReport) -> String {
  format_markdown_report_with_i18n(report, get_i18n_en())
}

/// 核心通用 Markdown 报告生成器
pub(crate) fn format_markdown_report_with_i18n(
  report: &BenchmarkReport,
  i18n: &I18nConfig,
) -> String {
  let mut out = String::with_capacity(16384);
  let _ = writeln!(out, "# {}\n\n", i18n.title.as_str());

  // 1. 环境与配置
  let _ = writeln!(out, "## {}\n", i18n.environment_title.as_str());
  write_template_2(
    &mut out,
    &i18n.env_write_ops,
    OpsVal(report.options.seq_count as f64),
    OpsVal(report.options.rand_count as f64),
  );
  write_template_1(
    &mut out,
    &i18n.env_read_ops,
    OpsVal(report.options.read_count as f64),
  );
  let total_dataset_mb = (report.total_dataset_bytes as f64) / BYTES_IN_MB;
  write_template_2(
    &mut out,
    &i18n.env_dataset_size,
    format_args!("{total_dataset_mb:.1}"),
    OpsVal(report.total_unique_records as f64),
  );
  let cumulative_ops: usize = report.items.iter().map(|it| it.wedb.total_ops).sum();
  write_template_1(&mut out, &i18n.env_total_ops, OpsVal(cumulative_ops as f64));
  write_template_1(&mut out, &i18n.env_val_size, report.options.val_len);
  write_template_1(&mut out, &i18n.env_budget, report.options.memory_budget_mb);
  if report.process_rss_bytes > 0 {
    let rss_mb = (report.process_rss_bytes as f64) / BYTES_IN_MB;
    write_template_1(&mut out, &i18n.env_peak_rss, format_args!("{rss_mb:.2}"));
  }
  let _ = writeln!(out, "- {}\n\n", i18n.env_mode.as_str());

  // 2. 架构对比 (列表)
  let _ = writeln!(out, "## {}\n", i18n.arch_title.as_str());
  for item in &i18n.arch_items {
    let _ = writeln!(out, "- {}\n", item.as_str());
  }
  out.push('\n');

  // 3. 性能评测 (每个测试单独一个表格，数据库作为列)
  let _ = writeln!(out, "## {}\n", i18n.benchmark_title.as_str());
  let engine_names = [
    i18n.engine_names.wedb.as_str(),
    i18n.engine_names.bftree.as_str(),
    i18n.engine_names.rocksdb.as_str(),
    i18n.engine_names.fjall.as_str(),
  ];
  for (idx, item) in report.items.iter().enumerate() {
    let title = i18n.items.get_by_index(idx);
    let _ = writeln!(out, "### {}\n", title);

    let tps = [
      item.wedb.gb_per_sec,
      item.bftree.gb_per_sec,
      item.rocksdb.gb_per_sec,
      item.fjall.gb_per_sec,
    ];
    let max_tp = tps.iter().cloned().fold(f64::NEG_INFINITY, f64::max);

    // 表头与吞吐行：仅最快列加粗
    let head_cells: Vec<String> = engine_names
      .iter()
      .enumerate()
      .map(|(i, n)| {
        if max_tp > 0.0 && (tps[i] - max_tp).abs() < EPS {
          format!("**{n}**")
        } else {
          (*n).to_string()
        }
      })
      .collect();
    let _ = writeln!(
      out,
      "| {} | {} | {} | {} | {} |",
      i18n.headers.metric.as_str(),
      head_cells[0],
      head_cells[1],
      head_cells[2],
      head_cells[3]
    );
    out.push_str("| :--- | :---: | :---: | :---: | :---: |\n");

    // 行 1: 吞吐 (GB/s)
    write_row(
      &mut out,
      i18n.headers.throughput.as_str(),
      &metric_cells(tps, true, |v| GbVal(v).to_string()),
    );

    // 行 2: 速率 (ops/s)
    let ops = [
      item.wedb.ops_per_sec,
      item.bftree.ops_per_sec,
      item.rocksdb.ops_per_sec,
      item.fjall.ops_per_sec,
    ];
    write_row(
      &mut out,
      i18n.headers.ops_rate.as_str(),
      &metric_cells(ops, true, |v| OpsVal(v).to_string()),
    );

    // 行 3/4: P50 / P99 延迟 (μs) - 最低加粗
    let p50s = [
      item.wedb.p50_us,
      item.bftree.p50_us,
      item.rocksdb.p50_us,
      item.fjall.p50_us,
    ];
    let p99s = [
      item.wedb.p99_us,
      item.bftree.p99_us,
      item.rocksdb.p99_us,
      item.fjall.p99_us,
    ];
    write_row(
      &mut out,
      i18n.headers.p50_lat.as_str(),
      &metric_cells(p50s, false, cell_us),
    );
    write_row(
      &mut out,
      i18n.headers.p99_lat.as_str(),
      &metric_cells(p99s, false, cell_us),
    );

    // 行 5: 相对 RocksDB - 最大加粗
    let sps = [
      item.speedup_vs_rocksdb(),
      ComparisonItem::calc_speedup(item.bftree.gb_per_sec, item.rocksdb.gb_per_sec),
      1.00,
      ComparisonItem::calc_speedup(item.fjall.gb_per_sec, item.rocksdb.gb_per_sec),
    ];
    write_row(
      &mut out,
      i18n.headers.speedup_vs_rocksdb.as_str(),
      &metric_cells(sps, true, cell_speedup),
    );
    out.push('\n');
  }

  // 综合性能对比 - 最快加粗
  let (w_gb, b_gb, r_gb, f_gb) = report.weighted_geomean_gb();
  let (w_qps, b_qps, r_qps, f_qps) = report.weighted_geomean();
  let max_geomean_gb = w_gb.max(b_gb).max(r_gb).max(f_gb);

  let _ = writeln!(out, "### {}\n", i18n.summary_tp_title.as_str());
  let _ = writeln!(
    out,
    "| {} | {} | {} | {} |",
    i18n.headers.engine.as_str(),
    i18n.headers.geomean_tp.as_str(),
    i18n.headers.geomean_qps.as_str(),
    i18n.headers.relative_ratio.as_str()
  );
  out.push_str("| :--- | :---: | :---: | :---: |\n");

  let format_summary_row = |out: &mut String, name: &str, gb: f64, qps: f64, sp: f64| {
    if (gb - max_geomean_gb).abs() < EPS && max_geomean_gb > 0.0 {
      let _ = writeln!(
        out,
        "| **{}** | **{}** | **{} ops/s** | **{:.2}x** |",
        name,
        GbVal(gb),
        OpsVal(qps),
        sp
      );
    } else {
      let _ = writeln!(
        out,
        "| {} | {} | {} ops/s | {:.2}x |",
        name,
        GbVal(gb),
        OpsVal(qps),
        sp
      );
    }
  };

  format_summary_row(
    &mut out,
    i18n.engine_names.wedb.as_str(),
    w_gb,
    w_qps,
    ComparisonItem::calc_speedup(w_gb, r_gb),
  );
  format_summary_row(
    &mut out,
    i18n.engine_names.bftree.as_str(),
    b_gb,
    b_qps,
    ComparisonItem::calc_speedup(b_gb, r_gb),
  );
  format_summary_row(
    &mut out,
    i18n.engine_names.rocksdb.as_str(),
    r_gb,
    r_qps,
    1.00,
  );
  format_summary_row(
    &mut out,
    i18n.engine_names.fjall.as_str(),
    f_gb,
    f_qps,
    ComparisonItem::calc_speedup(f_gb, r_gb),
  );
  out.push_str("\n\n");

  // 存储空间对比
  let dataset_mb = (report.total_dataset_bytes as f64) / BYTES_IN_MB;
  let w_disk_mb = (report.wedb_disk_bytes as f64) / BYTES_IN_MB;
  let b_disk_mb = (report.bftree_disk_bytes as f64) / BYTES_IN_MB;
  let r_disk_mb = (report.rocksdb_disk_bytes as f64) / BYTES_IN_MB;
  let f_disk_mb = (report.fjall_disk_bytes as f64) / BYTES_IN_MB;

  let sa_wedb = calc_space_amp(report.wedb_disk_bytes, report.total_dataset_bytes);
  let sa_bftree = calc_space_amp(report.bftree_disk_bytes, report.total_dataset_bytes);
  let sa_rocks = calc_space_amp(report.rocksdb_disk_bytes, report.total_dataset_bytes);
  let sa_fjall = calc_space_amp(report.fjall_disk_bytes, report.total_dataset_bytes);
  let min_sa = sa_wedb.min(sa_bftree).min(sa_rocks).min(sa_fjall);

  let _ = writeln!(out, "### {}\n", i18n.summary_disk_title.as_str());
  let _ = writeln!(
    out,
    "- {}: {:.2} MB\n",
    i18n.headers.raw_dataset_size, dataset_mb
  );
  let _ = writeln!(
    out,
    "| {} | {} | {} | {} |",
    i18n.headers.engine.as_str(),
    i18n.headers.disk_usage.as_str(),
    i18n.headers.space_amp.as_str(),
    i18n.headers.disk_features.as_str()
  );
  out.push_str("| :--- | :---: | :---: | :--- |\n");

  let format_disk_row = |out: &mut String, name: &str, disk_mb: f64, sa: f64, feat: &str| {
    if (sa - min_sa).abs() < 1e-4 && sa > 0.0 {
      let _ = writeln!(
        out,
        "| **{}** | {:.2} MB | **{}** | {} |",
        name,
        disk_mb,
        SpaceAmpVal(sa),
        feat
      );
    } else {
      let _ = writeln!(
        out,
        "| {} | {:.2} MB | {} | {} |",
        name,
        disk_mb,
        SpaceAmpVal(sa),
        feat
      );
    }
  };

  format_disk_row(
    &mut out,
    i18n.engine_names.wedb.as_str(),
    w_disk_mb,
    sa_wedb,
    &i18n.engine_summaries.wedb_disk_features,
  );
  format_disk_row(
    &mut out,
    i18n.engine_names.bftree.as_str(),
    b_disk_mb,
    sa_bftree,
    &i18n.engine_summaries.bftree_disk_features,
  );
  format_disk_row(
    &mut out,
    i18n.engine_names.rocksdb.as_str(),
    r_disk_mb,
    sa_rocks,
    &i18n.engine_summaries.rocksdb_disk_features,
  );
  format_disk_row(
    &mut out,
    i18n.engine_names.fjall.as_str(),
    f_disk_mb,
    sa_fjall,
    &i18n.engine_summaries.fjall_disk_features,
  );
  out.push_str("\n\n");

  // 内存占用对比
  let budget_mb = report.options.memory_budget_mb as f64;
  let w_mem_mb = (report.wedb_mem_bytes as f64) / BYTES_IN_MB;
  let b_mem_mb = (report.bftree_mem_bytes as f64) / BYTES_IN_MB;
  let r_mem_mb = (report.rocksdb_mem_bytes as f64) / BYTES_IN_MB;
  let f_mem_mb = (report.fjall_mem_bytes as f64) / BYTES_IN_MB;

  let calc_ratio = |mem_mb: f64| -> String {
    if budget_mb > 0.0 {
      let mut s = String::with_capacity(16);
      push_f64_precise(&mut s, (mem_mb / budget_mb) * 100.0, 1);
      s.push('%');
      s
    } else {
      "-".to_string()
    }
  };

  let _ = writeln!(out, "### {}\n", i18n.summary_mem_title.as_str());
  let _ = writeln!(out, "- {}: {:.2} MB\n", i18n.headers.mem_budget, budget_mb);
  let _ = writeln!(
    out,
    "| {} | {} | {} | {} |",
    i18n.headers.engine.as_str(),
    i18n.headers.mem_usage.as_str(),
    i18n.headers.mem_ratio.as_str(),
    i18n.headers.mem_features.as_str()
  );
  out.push_str("| :--- | :---: | :---: | :--- |\n");
  let _ = writeln!(
    out,
    "| {} | {} | {} | {} |",
    i18n.engine_names.wedb,
    MbVal(w_mem_mb),
    calc_ratio(w_mem_mb),
    i18n.engine_summaries.wedb_mem_features
  );
  let _ = writeln!(
    out,
    "| {} | {} | {} | {} |",
    i18n.engine_names.bftree,
    MbVal(b_mem_mb),
    calc_ratio(b_mem_mb),
    i18n.engine_summaries.bftree_mem_features
  );
  let _ = writeln!(
    out,
    "| {} | {} | {} | {} |",
    i18n.engine_names.rocksdb,
    MbVal(r_mem_mb),
    calc_ratio(r_mem_mb),
    i18n.engine_summaries.rocksdb_mem_features
  );
  let _ = writeln!(
    out,
    "| {} | {} | {} | {} |\n\n",
    i18n.engine_names.fjall,
    MbVal(f_mem_mb),
    calc_ratio(f_mem_mb),
    i18n.engine_summaries.fjall_mem_features
  );

  // 4. 技术分析
  let _ = writeln!(out, "## {}\n", i18n.insights_title.as_str());
  for (i, text) in i18n.insights.iter().enumerate() {
    let _ = writeln!(out, "{}. {}\n\n", i + 1, text.as_str());
  }

  markdown_table_formatter::format_tables(&out)
}
