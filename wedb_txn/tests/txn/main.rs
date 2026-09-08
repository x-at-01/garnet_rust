#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

mod concurrency_and_deadlock;
mod support;
mod transactions;
mod watch_semantics;
