#![cfg_attr(docsrs, feature(doc_cfg))]

mod codec;
mod compact;
mod error;
mod set;

pub use codec::{MemberChunkCodec, MemberChunkIter};
pub use compact::{CompactSet, CompactSetCodec, CompactSetIter};
pub use error::{Error, Result};
pub use set::{FORMAT_VERSION, IntoIter, Iter, MAX_RAND_SAMPLE_LIMIT, SetObject};
pub use wrecord::glob_match;
