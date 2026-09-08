mod blocking;
mod support;

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}
