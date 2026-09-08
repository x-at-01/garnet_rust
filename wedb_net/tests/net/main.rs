#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

pub mod support;

mod buffer_pool;
mod network_lifecycle;
mod pipelining;
mod protocol_robustness;
mod session_commands;
mod transaction_semantics;
