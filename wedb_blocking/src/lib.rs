#![cfg_attr(docsrs, feature(doc_cfg))]
#![warn(clippy::absolute_paths)]

mod broker;
mod error;
mod observer;
mod provider;
mod result;

pub use broker::CollectionItemBroker;
pub use error::{Error, Result};
pub use observer::{CollectionItemObserver, ObserverRx, ObserverStatus, ObserverTx};
pub use provider::{CollectionProvider, Direction, MemoryCollectionStore};
pub use result::CollectionItemResult;
