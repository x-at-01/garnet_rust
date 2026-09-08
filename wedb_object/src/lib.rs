//! Garnet 统一堆对象聚合层（对标 C# `GarnetObjectType` / `GarnetObjectSerializer`）
//!
//! 序列化布局 `[type_byte][payload]`：首字节为持久化的对象类型标识（值永久固定），
//! 0xFC..=0xFF 为未来格式版本保留段，永不作为类型字节写入。
//!
//! # 线程模型
//!
//! 对象本身非线程安全，不包含任何内部锁：服务端采用 compio 每核单线程模型，
//! 同一 session（同一 worker 线程）独占对象的可变访问，热路径无锁、无原子开销。
//! 跨线程边界仅在存储层异步往返：`wkv::load_object`/`save_object` 持有
//! 对象跨 `.await` 传递，`Future` 须 `Send`；下方编译期 Send/Sync 断言锁定该契约
//! （Sync 为更强不变式：类型无任何内部可变性），防止引入 `RefCell`/裸指针类字段静默破坏。

#![cfg_attr(docsrs, feature(doc_cfg))]

mod error;
mod object;

pub use error::{Error, Result};
pub use object::{GarnetObject, GarnetObjectType};

// 编译期契约锁：`GarnetObject`/`Error` 在下游 wedb_store 的 async fn（load_object/save_object）
// 中跨 .await 持有传递，Future 须 Send；Sync 为更强不变式（无内部可变性）。
// 任一类型失去 Send/Sync 立即在编译期失败。
const _: () = {
  const fn assert_send_sync<T: Send + Sync>() {}
  assert_send_sync::<GarnetObject>();
  assert_send_sync::<GarnetObjectType>();
  assert_send_sync::<Error>();
};
