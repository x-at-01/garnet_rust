#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
mod list;
mod page;

pub use error::{Error, Result};
pub use list::{InsertPosition, ListObject};
pub use page::{DEFAULT_PAGE_CAPACITY, LinkedPage};
