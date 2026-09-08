use std::result;

use thiserror::Error;

#[derive(Error, Debug, PartialEq, Eq)]
pub enum Error {
  #[error("WRONGTYPE Key is not a valid HyperLogLog string value.")]
  InvalidHyperLogLog,
}

pub type Result<T> = result::Result<T, Error>;
