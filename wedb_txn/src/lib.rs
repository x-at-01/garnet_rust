#![cfg_attr(docsrs, feature(doc_cfg))]

//! 高并发乐观锁事务引擎
//!
//! 实现 Redis 语义的 WATCH / MULTI / EXEC / DISCARD / UNWATCH 事务生命周期管理、
//! 死锁防御全局哈希排序加锁，以及无锁常数时间复杂度全局版本号乐观并发控制。
//!
//! # 键契约（名字空间隔离 + 二进制安全）
//!
//! 本 crate 对键字节完全透明：所有 API（[`WatchedKeysContainer::watch`]、
//! [`QueuedCommand`] 的 args、[`TransactionManager::save_key_entry_to_lock`] 等）
//! 均要求传入"名字空间限定后的完整键字节"。WATCH 失效检测以键字节 64 位哈希
//! 寻址全局版本表，若两个名字空间传入相同原始键字节，将共享同一版本槽位——
//! 跨名字空间写入只会引发保守的多余失效（绝不漏失效），但为满足名字空间
//! 相互隔离的语义，调用方必须保证不同名字空间的键字节互异（如内嵌名字空间
//! 前缀的复合键）。
//!
//! # 失效通知接口约定
//!
//! - 事务写路径：[`TransactionManager::commit`] 统一对全部排他锁键自增
//!   [`WatchVersionMap`] 版本，使其他会话的 WATCH 整体失效；事务内排队的
//!   FLUSHDB / FLUSHALL 全局变异则对版本表全表自增，广播失效所有监视键；
//! - 非事务写路径（单命令即时执行，含过期删除、逐出等隐式变异）：存储层
//!   修改键后须自行调用 [`WatchVersionMap::bump_version_key`]，对标 Garnet
//!   存储函数内的 `watchVersionMap.IncrementVersion` 钩子。
//!
//! # 加锁顺序与可线性化前提
//!
//! - 乐观一站式：[`TransactionManager::exec_prepare`]（内含版本校验）→ 执行 →
//!   提交。校验与提交之间存在窗口，该流程仅在命令执行被外部串行化时
//!   （如 compio 单线程事件循环内同线程会话逐个推进）才是可线性化的；
//!   执行器必须保证不重入 [`TransactionManager`]。
//! - 跨核并发（多会话并行执行）：必须使用拆分流程闭合"校验-加锁"竞态窗口：
//!   [`TransactionManager::prepare_lockset`] → 按 [`TransactionManager::key_entries`]
//!   顺序加物理锁 → [`TransactionManager::validate_watches`] →
//!   [`TransactionManager::begin_run`] → 执行 → [`TransactionManager::commit`]。
//!   校验冲突时锁条目集合被刻意保留：调用方须按原集合释放物理锁后，再经
//!   [`TransactionManager::key_entries_mut`] 调用 [`TxnKeyEntries::unlock_all_keys`]
//!   清理（对标 Garnet `UnlockAllKeys` 先于条目清空的释放顺序）。

mod error;
mod key_entry;
mod manager;
mod queued_cmd;
mod state;
mod version_map;
mod watched_keys;

pub use error::{Error, Result};
pub use key_entry::{LockType, TxnKeyComparison, TxnKeyEntries, TxnKeyEntry};
pub use manager::{ExecPreparation, ExecResult, TransactionGuard, TransactionManager};
pub use queued_cmd::QueuedCommand;
pub use state::TxnState;
pub use version_map::{DEFAULT_VERSION_MAP_CAPACITY, WatchVersionMap};
pub use watched_keys::{WatchedKeyEntry, WatchedKeysContainer};
