//! `WedbServer::run()` 事件驱动停机等待的行为测试：
//! 正常停机（stop）应经唤醒通道即时返回，旁路翻转 `context.is_running` 应经兜底超时感知退出。

use std::{
  path::Path,
  sync::{Arc, OnceLock},
  time::{Duration, Instant},
};

use aok::{OK, Void};
use compio::{runtime::spawn, time::sleep};
use log::info;
use tempfile::tempdir;
use wedb_server::{ServerArgs, WedbServer};
use wkv::{AUTO_COMPACTION_FREQ_SECS, DEFAULT_COMPACTION_MAX_SEEK_BYTES};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 停机触发前的延迟：给 `run()` 留出完成启动并进入事件等待挂起点的时间
const STOP_DELAY_MS: u64 = 100;
/// 与 `src/lib.rs` 私有常量 `RUN_FALLBACK_TIMEOUT` 同步的兜底等待周期（毫秒，未导出故此处镜像）
const RUN_FALLBACK_MS: u64 = 1000;
/// 即时唤醒路径阈值（毫秒）：stop 发起 → run() 退出必须小于该值。
/// 唤醒通道若失效，兜底路径的 stop→exit 耗时存在理论下界 `RUN_FALLBACK_MS - STOP_DELAY_MS`
/// （发起停机最早发生在 run() 进入等待 STOP_DELAY_MS 之后，之后还须等满整个兜底 tick 方能超时退出），
/// 阈值恰取该下界即可确定性区分两条路径：通道唤醒毫秒级通过；通道回归破坏则必走兜底、耗时越界失败
const WAKE_EXIT_LIMIT_MS: u64 = RUN_FALLBACK_MS - STOP_DELAY_MS;
/// 兜底路径下界（毫秒）：退出至少须等满一个兜底 tick，扣 100ms 容忍计时器精度与调度抖动；
/// 仍远高于即时唤醒的毫秒级，防止旁路停机被唤醒通道「误救」而失去测试价值
const FALLBACK_EXIT_MIN_MS: u64 = RUN_FALLBACK_MS - 100;
/// 兜底路径上界（毫秒）：翻转停机时首个兜底 tick 早已挂起，至多再等一个 tick 必被感知，超过即异常
const FALLBACK_EXIT_LIMIT_MS: u64 = 2 * RUN_FALLBACK_MS;
/// run() 退出总耗时上限（毫秒）：仅拦截「退出极慢」的退化情形，放宽到 5s 防 CI 抖动误报；
/// 注意 cargo test 并无内建全局超时，若 run() 真正挂死本测试将永不结束，
/// 只能由外部执行环境（如 CI 任务级超时）暴露，而非此断言
const RUN_EXIT_LIMIT_MS: u64 = 5000;

/// 构造最小化测试服务参数（port 0 由内核分配，quiet 关闭控制台输出）
fn test_args(dir: &Path) -> ServerArgs {
  ServerArgs {
    port: 0,
    dir: dir.to_string_lossy().to_string(),
    quiet: true,
    ..Default::default()
  }
}

#[compio::test]
async fn test_run_exits_immediately_on_stop() -> Void {
  let dir = tempdir()?;
  let server = Arc::new(WedbServer::new(test_args(dir.path())).await?);
  let runner = Arc::clone(&server);
  let started = Instant::now();
  let run_task = spawn(async move { runner.run().await });

  // stopper 侧记录「发起 stop」时刻（必然早于唤醒信号写入通道）：
  // 断言 stop→exit 显著小于兜底周期，从而区分「通道即时唤醒」与「兜底超时」两条退出路径
  let stop_at = Arc::new(OnceLock::<Instant>::new());
  let flag = Arc::clone(&stop_at);
  let stopper = Arc::clone(&server);
  spawn(async move {
    sleep(Duration::from_millis(STOP_DELAY_MS)).await;
    let _ = flag.set(Instant::now());
    stopper.stop().await
  })
  .detach();

  run_task.await.expect("run 任务不应被取消或 panic")?;
  assert!(server.is_disposed(), "stop 后 run() 返回时服务应已停机");
  let exited = Instant::now();
  // run() 只能因停机而退出，而停机必先经过 stop 发起点，此刻记录必已就绪（100% 确定）
  let wake_elapsed = exited.duration_since(*stop_at.get().expect("stop 发起时刻必已记录"));
  assert!(
    wake_elapsed < Duration::from_millis(WAKE_EXIT_LIMIT_MS),
    "run() 应经通道唤醒即时退出, stop→exit 实测 {wake_elapsed:?}, 越界疑似退化为兜底超时路径"
  );
  let total = started.elapsed();
  assert!(
    total < Duration::from_millis(RUN_EXIT_LIMIT_MS),
    "run() 退出总耗时异常, 实测 {total:?}"
  );
  info!("run() 正常停机唤醒测试通过, stop→exit {wake_elapsed:?}, 总耗时 {total:?}");
  OK
}

#[compio::test]
async fn test_run_exits_on_context_bypass_stop() -> Void {
  // 旁路停机：绕过 stop()/close() 直接翻转公开的 context.is_running，
  // 唤醒通道无法 hook，验证兜底 tick 能感知并退出。
  // 耗时收敛于 [一个兜底 tick 的下界, 2×tick)：下界排除被唤醒通道「即时误救」，
  // 上界排除多 tick 空转（STOP_DELAY_MS 须大于启动耗时，保证 run() 已先进入等待挂起点）
  let dir = tempdir()?;
  let server = Arc::new(WedbServer::new(test_args(dir.path())).await?);
  let runner = Arc::clone(&server);
  let started = Instant::now();
  let run_task = spawn(async move { runner.run().await });

  let ctx = Arc::clone(&server.context);
  spawn(async move {
    sleep(Duration::from_millis(STOP_DELAY_MS)).await;
    ctx.stop();
  })
  .detach();

  run_task.await.expect("run 任务不应被取消或 panic")?;
  assert!(server.is_disposed(), "旁路停机后 run() 返回时服务应已停机");
  let elapsed = started.elapsed();
  assert!(
    elapsed >= Duration::from_millis(FALLBACK_EXIT_MIN_MS),
    "run() 应经兜底 tick 感知旁路停机, 实测耗时 {elapsed:?} 过短, 疑似被唤醒通道误唤醒"
  );
  assert!(
    elapsed < Duration::from_millis(FALLBACK_EXIT_LIMIT_MS),
    "run() 应在至多两个兜底 tick 内感知旁路停机, 实测耗时 {elapsed:?}"
  );
  info!("run() 旁路停机兜底测试通过, 耗时 {elapsed:?}");
  OK
}

/// 周期自动紧缩配置透传断言：server 级不重复暴露配置项，参数唯一来源为底层
/// StoreConfig（auto() 自适应默认 60s / 1GiB，start_threaded 据此挂载周期任务）
#[compio::test]
async fn test_compaction_config_passthrough() -> Void {
  let dir = tempdir()?;
  let server = WedbServer::new(test_args(dir.path())).await?;

  assert_eq!(
    server.context.store.config.compaction_freq_secs, AUTO_COMPACTION_FREQ_SECS,
    "server 全新开库必须继承 auto() 的周期紧缩频率"
  );
  assert_eq!(
    server.context.store.config.compaction_max_seek_bytes, DEFAULT_COMPACTION_MAX_SEEK_BYTES,
    "server 全新开库必须继承 auto() 的单轮推进上限"
  );
  info!("周期自动紧缩配置透传断言通过");
  OK
}
