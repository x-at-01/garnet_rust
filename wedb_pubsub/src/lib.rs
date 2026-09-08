#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(clippy::absolute_paths)]

mod broker;
mod channel;
mod error;
mod glob;
mod message;
mod pattern;
mod registry;
mod session;

pub use broker::SubscribeBroker;
pub use channel::ChannelManager;
pub use error::{Error, Result};
pub use glob::{glob_match, match_glob, match_glob_nocase};
pub use message::PubSubMessage;
pub use pattern::PatternManager;
pub use session::{AsyncRx, AsyncTx, SendStatus, SessionHandle, SubscriberSession, create_session};
