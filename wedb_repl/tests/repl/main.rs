#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

mod backlog_and_replayer;
mod cluster_replication;
mod diskless_sync;
mod handshake_and_psync;
mod support;
