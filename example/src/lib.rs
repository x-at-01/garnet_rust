//! 嵌入式场景数据结构使用示例集
//!
//! 工作区内各集合结构均为纯内存实现，不依赖网络服务与存储后端，
//! 可作为嵌入式引擎的内嵌组件直接实例化（类比 SQLite 之于 MySQL）：
//!
//! | 结构 | crate | 对标 Redis 能力 |
//! |---|---|---|
//! | `HashObject` | wedb_hash | HSET/HGET/HINCRBY/HEXPIRE 字段级过期 |
//! | `ListObject` / `LinkedPage` | wedb_list | LPUSH/RPOP/LMOVE 双端队列 |
//! | `SetObject` / `CompactSet` | wedb_set | SADD/SINTER/SDIFF/SSCAN |
//! | `SortedSetObject` / `SkipList` / `CompactZSet` | wedb_zset | ZADD/ZRANK/ZRANGEBYLEX/GEO |
//! | `HyperLogLog` | wedb_hll | PFADD/PFCOUNT/PFMERGE |
//! | `LightEpoch` | wepoch | 无锁纪元保护与延迟回收 |
//! | `fast_hash` / `StreamHasher` | whasher | gxhash 硬件加速哈希与校验和 |
//!
//! 每个主题对应 `examples/` 下一个独立演示文件，逐个运行：
//!
//! ```bash
//! cargo run -p wedb_example --example hash
//! ```

#![cfg_attr(docsrs, feature(doc_cfg))]
