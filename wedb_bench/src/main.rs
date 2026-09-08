use std::{env::current_dir, fs, path::PathBuf};

use clap::Parser;
use mimalloc::MiMalloc;
use wedb_bench::{
  BenchOpt, DEFAULT_MEMORY_BUDGET_MB, Result, format_markdown_report_en, format_markdown_report_zh,
  run_comparative_benchmarks,
};

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

/// 项目根目录 README.mdt 模板（仅缺失时补齐，不覆写已有内容）
const README_MDT: &str = "[English](#en) | [中文](#zh)\n\n---\n\n<a name=\"en\"></a>\n\n<+ ./readme/en/readme.md >\n<+ ./readme/en/bench.md >\n\n---\n\n<a name=\"zh\"></a>\n\n<+ ./readme/zh/readme.md >\n<+ ./readme/zh/bench.md >\n";

/// WeDB vs WeDb-BfTree vs RocksDB vs Fjall 深度性能评测与回归基准工具
#[derive(Parser, Debug)]
#[command(
  name = "wedb_bench",
  about = "WeDB (Garnet Tsavorite) vs WeDb-BfTree vs RocksDB vs Fjall 深度性能评测与基准工具",
  version
)]
pub struct CliArgs {
  /// 测试规格预设 profile ("quick", "standard")
  #[arg(short, long, default_value = "standard", value_parser = ["quick", "standard"])]
  pub profile: String,

  /// 单项测试总操作数 (同时覆盖 seq/rand/read 操作数)
  #[arg(short, long)]
  pub ops: Option<usize>,

  /// 顺序写入操作次数 (独立覆盖)
  #[arg(long)]
  pub seq_ops: Option<usize>,

  /// 随机写入操作次数 (独立覆盖)
  #[arg(long)]
  pub rand_ops: Option<usize>,

  /// 点读操作次数 (独立覆盖)
  #[arg(long)]
  pub read_ops: Option<usize>,

  /// Value 字节大小 (覆盖 profile 默认值)
  #[arg(short, long)]
  pub val_size: Option<usize>,

  /// 各引擎对齐的物理内存预算 (MB)
  #[arg(short, long, default_value_t = DEFAULT_MEMORY_BUDGET_MB)]
  pub memory_budget_mb: usize,

  /// 关闭评测报告自动保存到项目根目录 readme/zh/bench.md 与 readme/en/bench.md (默认开启)
  #[arg(long)]
  pub no_save_readme: bool,
}

/// 自下而上查找包含 [workspace] 的项目根目录
fn find_project_root() -> Option<PathBuf> {
  let mut cur = current_dir().ok()?;
  loop {
    let cargo_toml = cur.join("Cargo.toml");
    if cargo_toml.is_file()
      && let Ok(content) = fs::read_to_string(&cargo_toml)
      && content.contains("[workspace]")
    {
      return Some(cur);
    }
    if !cur.pop() {
      break;
    }
  }
  None
}

fn main() -> Result<()> {
  let args = CliArgs::parse();

  let mut options = BenchOpt::from_profile(&args.profile);
  options.memory_budget_mb = args.memory_budget_mb;

  if let Some(ops) = args.ops {
    options.seq_count = ops;
    options.rand_count = ops;
    options.read_count = ops;
  }
  if let Some(seq) = args.seq_ops {
    options.seq_count = seq;
  }
  if let Some(rand) = args.rand_ops {
    options.rand_count = rand;
  }
  if let Some(read) = args.read_ops {
    options.read_count = read;
  }
  if let Some(val_size) = args.val_size {
    options.val_len = val_size;
  }

  println!("============================================================");
  println!("   WeDB vs WeDb-BfTree vs RocksDB vs Fjall 深度性能评测");
  println!("============================================================");
  println!("评测配置参数:");
  println!("  - Profile 规格预设: {}", args.profile);
  println!(
    "  - 写入操作数: 顺序 {} ops / 随机 {} ops",
    options.seq_count, options.rand_count
  );
  println!("  - 点读操作数: {} ops", options.read_count);
  println!("  - Value 载荷大小: {} B", options.val_len);
  println!("  - 对齐物理内存预算: {} MB", options.memory_budget_mb);
  println!("  - 评测模式: 纯存储引擎端到端公平对决 (无内存侧边缓存)");
  println!("============================================================\n");

  let results = run_comparative_benchmarks(&options)?;
  let report_zh = format_markdown_report_zh(&results);
  let report_en = format_markdown_report_en(&results);

  println!("\n{report_zh}");

  if args.no_save_readme {
    return Ok(());
  }

  let Some(root) = find_project_root() else {
    eprintln!("未找到包含 [workspace] 的项目根目录，跳过保存 readme");
    return Ok(());
  };

  let zh_bench = root.join("readme").join("zh").join("bench.md");
  let en_bench = root.join("readme").join("en").join("bench.md");
  if let Some(p) = zh_bench.parent() {
    fs::create_dir_all(p)?;
  }
  if let Some(p) = en_bench.parent() {
    fs::create_dir_all(p)?;
  }
  fs::write(&zh_bench, &report_zh)?;
  fs::write(&en_bench, &report_en)?;
  println!("\n>>> 评测报告已保存至:");
  println!("    - {}", zh_bench.display());
  println!("    - {}", en_bench.display());

  // 首次运行补齐项目根目录 README.mdt 聚合模板（不覆写已有内容）
  let mdt_path = root.join("README.mdt");
  if !mdt_path.exists() {
    fs::write(&mdt_path, README_MDT)?;
  }

  Ok(())
}
