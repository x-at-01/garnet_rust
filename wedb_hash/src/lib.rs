#![cfg_attr(docsrs, feature(doc_cfg))]

mod codec;
mod error;
mod expire;
mod hash;

pub use codec::{
  EXPIRE_HEADER_LEN, FIELD_VALUE_STACK_CAP, FieldChunkCodec, FieldChunkIter, FieldValueBuf,
  FieldValueCodec, NO_EXPIRE_HEADER_LEN, TAG_NO_EXPIRE, TAG_WITH_EXPIRE,
};
pub use error::{Error, Result};
pub use expire::{ExpireOpt, ExpireResult};
pub use hash::{
  EXPIRATION_BIT_MASK, FORMAT_VERSION, HScanBorrowedResult, HScanResult, HashEntry,
  HashEntryBitcode, HashObject, MAX_RAND_SAMPLE_LIMIT, glob_match,
};
