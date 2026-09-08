#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

mod cluster_forward;
mod consistency;
mod glob_pattern;
mod resp_pubsub;
mod session_lifecycle;
mod support;
