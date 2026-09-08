use std::io::{self, Result as IoResult};

use compio::{
  runtime::{Runtime, spawn},
  signal,
};
use crossfire::mpsc::bounded_async;
use log::{error, info};
use wedb_server::{Result, ServerArgs, WedbServer, config::cmd};

/// glibc 信号编号:SIGTERM(`kill <pid>` 的默认信号,docker stop 与 systemd stop 均发送它)
const SIGTERM_NUM: i32 = 15;

/// 竞速等待首个停机信号到达,返回信号名供日志展示
///
/// - unix:[`compio::signal::ctrl_c`] 与 [`compio::signal::unix::signal`] 监听的 SIGTERM 并驱竞速,
///   Ctrl+C 与 docker stop / systemd stop 一样走优雅刷盘路径;
/// - windows:仅 CTRL_C_EVENT(SIGTERM 不存在于 windows)。
///
/// 任一监听任务注册失败仅记录日志并让出其发送端;全部失败时通道断开,本函数返回错误。
///
/// 返回后未触发的监听任务被取消:任务取消会 drop 内部的 SignalListener 并将信号恢复为默认处置,
/// 因此优雅停机刷盘期间再次到来的 SIGINT/SIGTERM 会按默认行为立即终止进程(刷盘挂死时的强杀逃生通道)。
async fn wait_shutdown_signal() -> IoResult<&'static str> {
  const SIGINT_LABEL: &str = "SIGINT (Ctrl+C)";
  const SIGTERM_LABEL: &str = "SIGTERM";

  let (tx, rx) = bounded_async::<&'static str>(1);
  let int_task = spawn({
    let tx = tx.clone();
    async move {
      if let Err(e) = signal::ctrl_c().await {
        error!("注册 SIGINT 监听失败: {e}");
        return;
      }
      let _ = tx.try_send(SIGINT_LABEL);
    }
  });

  #[cfg(unix)]
  let term_task = spawn(async move {
    use compio::signal::unix::signal as unix_signal;
    if let Err(e) = unix_signal(SIGTERM_NUM).await {
      error!("注册 SIGTERM 监听失败: {e}");
      return;
    }
    let _ = tx.try_send(SIGTERM_LABEL);
  });
  #[cfg(windows)]
  // windows 无 SIGTERM:立即释放原发送端,防止监听注册失败后 recv 永久悬等
  drop(tx);

  let label = rx
    .recv()
    .await
    .map_err(|e| io::Error::other(format!("等待停机信号的通道已断开: {e}")))?;

  // 取消未触发的监听任务,恢复信号默认处置(见函数文档的强杀逃生通道语义)
  drop(int_task);
  #[cfg(unix)]
  drop(term_task);

  Ok(label)
}

/// WeDB 服务端可执行程序主入口
fn main() -> Result<()> {
  let Some(matches) = clap_args::parse!(cmd) else {
    return Ok(());
  };
  let args = ServerArgs::from_matches(&matches);
  let rt = Runtime::new()?;
  rt.block_on(async move {
    let server = WedbServer::new(args).await?;
    let local_addr = server.start_threaded(None).await?;
    info!("WeDB 服务成功启动并就绪，监听地址: {}", local_addr);

    let signal_label = wait_shutdown_signal().await?;
    info!("捕获停机信号 ({signal_label}), 正在执行优雅停机...");
    server.stop().await?;
    info!("WeDB 守护进程退出成功.");
    Ok(())
  })
}
