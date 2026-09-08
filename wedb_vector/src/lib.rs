//! wedb_vector: 向量集数据类型 (对标 Garnet libs/server/Resp/Vector 与 Redis V* 命令族)
//!
//! 纯 Rust 实现的 Vamana 图 ANN 索引（DiskANN 风格）+ 元素属性存储，
//! [`manager::VectorManager`] 管理键空间；序列化与 AOF/复制接入由宿主负责。

#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
mod manager;
mod set;
mod vamana;

pub use error::{Error, Result};
pub use manager::VectorManager;
pub use set::VectorSet;
pub use vamana::{Vamana, VamanaParams};
