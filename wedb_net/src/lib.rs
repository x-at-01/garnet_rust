#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(clippy::absolute_paths)]

mod buffer;
mod config;
mod connection;
mod error;
mod listener;
mod pool;
mod server;
mod session;

pub use buffer::{PooledReceiveBuffer, SendBuffer};
pub use config::{
  DEFAULT_INITIAL_RECV_BUF_SIZE, DEFAULT_MAX_CONNECTIONS, DEFAULT_MAX_RECV_BUF_SIZE, DEFAULT_PORT,
  DEFAULT_SEND_BUF_SIZE, NetConfig,
};
pub use connection::{NetConnection, ServerContext};
pub use error::{Error, Result};
pub use listener::NetListener;
pub use pool::LimitedFixedBufferPool;
pub use server::WedbServer;
pub use session::NetSession;
