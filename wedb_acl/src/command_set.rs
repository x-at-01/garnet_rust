use core::str::from_utf8_unchecked;

use wedb_resp::{FIRST_NO_AUTH, LAST_NO_AUTH, RespCommand};
use whasher::HashSet;

use crate::error::{Error, Result};

pub const CAT_ALL: [u64; 16] = [u64::MAX; 16];
pub const CAT_NONE: [u64; 16] = [0; 16];
pub const CAT_ADMIN: [u64; 16] = [
  0x0000000000400000,
  0x0000000200000000,
  0x0000000000000000,
  0xf8c0000000000000,
  0xee67800fd003f980,
  0x00001cffebd6ffdf,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_BITMAP: [u64; 16] = [
  0x0000000000000004,
  0x0e08000000000800,
  0x0000000000000100,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_BLOCKING: [u64; 16] = [
  0x0007c00000000038,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_CONNECTION: [u64; 16] = [
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0ff7004000000000,
  0x000007e000000060,
  0x0000e00000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_CUSTOM: [u64; 16] = [
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x00000000003c0000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_DANGEROUS: [u64; 16] = [
  0x0008000000006000,
  0x0000000008000004,
  0x0000000000400000,
  0xf8c0000000000000,
  0xee67800fd03edb80,
  0x00001cff82d2ffdf,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_FAST: [u64; 16] = [
  0x43c0071fffb818f2,
  0xa40059f9bb8081da,
  0xff2145422f133b81,
  0x00070841ebe218f9,
  0x0000000000004c6d,
  0x0000e00000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_GARNET: [u64; 16] = [
  0x6000000000400000,
  0x100000fa00000002,
  0x0000000000000000,
  0x80000007e3c00000,
  0x80100000003d2010,
  0x00001c9a6b9674cf,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_GEO: [u64; 16] = [
  0x0000000000078000,
  0x0000000000000000,
  0x000000000000007e,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_HASH: [u64; 16] = [
  0x00000003ffc00000,
  0x0000000000000000,
  0x000000000f3ff000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_HYPERLOGLOG: [u64; 16] = [
  0x0600000000000000,
  0x0000000000000000,
  0x0000008000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_KEYSPACE: [u64; 16] = [
  0x11c8000000007900,
  0xe00000001800000c,
  0x0300037c00400001,
  0x0000000000000000,
  0x0000200000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_LIST: [u64; 16] = [
  0x0007ffe000000000,
  0x00000000000000f0,
  0x00000000f0000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_PUBSUB: [u64; 16] = [
  0x0000000000000000,
  0x0000000000000000,
  0x0012000000000000,
  0x0000ff0000000000,
  0x0000000000000000,
  0x0000000060000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_READ: [u64; 16] = [
  0x0000000000000000,
  0xfe00000000000000,
  0xffffffffffffffff,
  0x00000007ffffffff,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_SCRIPTING: [u64; 16] = [
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000001800000000,
  0x0000000003800000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_SET: [u64; 16] = [
  0x0000000000000000,
  0x0000000007c00300,
  0x008dfc0000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_SLOW: [u64; 16] = [
  0x3c3ff8e00007e50c,
  0x5a0fa60444671e24,
  0x00debabdd0ecc47e,
  0x7ff8f7be141de706,
  0xfffffffffffe9382,
  0x00001ffdf7ffffff,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_SORTEDSET: [u64; 16] = [
  0x0000000000000038,
  0x0007ffff00000000,
  0x0000000000000000,
  0x000000001fffffc0,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_STREAM: [u64; 16] = [
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_STRING: [u64; 16] = [
  0x0830001c003804c2,
  0x0000000000279400,
  0x0060000200800e80,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_TRANSACTION: [u64; 16] = [
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000038,
  0x000000000000001f,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_VECTOR: [u64; 16] = [
  0x0000000000000000,
  0x00000000e0000000,
  0xfc00000000000000,
  0x0000000000000007,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];
pub const CAT_WRITE: [u64; 16] = [
  0x7ffffffffffffdfe,
  0x000fffffffe79ffe,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
  0x0000000000000000,
];

/// 免认证命令掩码（AUTH, HELLO, QUIT 保护不可清除）
///
/// 由 `wedb_resp` 的 `FIRST_NO_AUTH..=LAST_NO_AUTH` 判别值范围在编译期生成，
/// 单一事实来源：wedb_resp 枚举增删免认证命令时自动跟随，杜绝双份手抄失配
pub const NO_AUTH_MASK: [u64; 16] = build_no_auth_mask();

const fn build_no_auth_mask() -> [u64; 16] {
  let mut mask = [0u64; 16];
  let mut cmd = FIRST_NO_AUTH;
  while cmd <= LAST_NO_AUTH {
    mask[(cmd >> 6) as usize] |= 1u64 << (cmd & 63);
    cmd += 1;
  }
  mask
}

/// 全部可用命令分类名称与位图常量的单一定义点（宏同时生成名称列表与查询表）
macro_rules! define_categories {
  ($(($name:literal, $cat:ident)),+ $(,)?) => {
    /// 全部可用命令分类列表（与 CATEGORY_TABLE 严格同源）
    pub const ALL_CATEGORIES: &[&str] = &[$($name),+];

    /// 分类查询表：（规范小写名, 位图）单一事实来源（对标 C# ACLParser.categoryNames）
    static CATEGORY_TABLE: &[(&str, [u64; 16])] = &[$(($name, $cat)),+];
  };
}

define_categories!(
  ("admin", CAT_ADMIN),
  ("all", CAT_ALL),
  ("bitmap", CAT_BITMAP),
  ("blocking", CAT_BLOCKING),
  ("connection", CAT_CONNECTION),
  ("custom", CAT_CUSTOM),
  ("dangerous", CAT_DANGEROUS),
  ("fast", CAT_FAST),
  ("garnet", CAT_GARNET),
  ("geo", CAT_GEO),
  ("hash", CAT_HASH),
  ("hyperloglog", CAT_HYPERLOGLOG),
  ("keyspace", CAT_KEYSPACE),
  ("list", CAT_LIST),
  ("pubsub", CAT_PUBSUB),
  ("read", CAT_READ),
  ("scripting", CAT_SCRIPTING),
  ("set", CAT_SET),
  ("slow", CAT_SLOW),
  ("sortedset", CAT_SORTEDSET),
  ("stream", CAT_STREAM),
  ("string", CAT_STRING),
  ("transaction", CAT_TRANSACTION),
  ("vector", CAT_VECTOR),
  ("write", CAT_WRITE),
);

/// 按名称（忽略大小写）查询分类，返回 (规范名, 位图)，零堆分配
#[inline]
pub fn lookup_category(cat: &str) -> Option<(&'static str, [u64; 16])> {
  CATEGORY_TABLE
    .iter()
    .find(|(name, _)| name.eq_ignore_ascii_case(cat))
    .map(|(name, bits)| (*name, *bits))
}

/// 根据命令分类名称获取对应位图（零堆分配匹配）
#[inline]
pub fn get_category_bitmap(cat: &str) -> Option<[u64; 16]> {
  lookup_category(cat).map(|(_, bits)| bits)
}

/// 获取属于指定分类的所有命令列表（按索引顺序，返回静态字符串切片）
pub fn category_commands(cat: &str) -> Option<Vec<&'static str>> {
  let (_, bits) = lookup_category(cat)?;
  // 跳过索引 0 的 "none" 占位；CMD_NAMES 长度 368 < 1024，位必然落在 bits 界内
  Some(
    CMD_NAMES
      .iter()
      .enumerate()
      .skip(1)
      .filter(|(i, _)| bits[i / 64] & (1u64 << (i % 64)) != 0)
      .map(|(_, name)| *name)
      .collect(),
  )
}

/// 命令名称查找表（0..=367）
static CMD_NAMES: [&str; 368] = [
  "none",                           // 0
  "append",                         // 1
  "bitfield",                       // 2
  "bzmpop",                         // 3
  "bzpopmax",                       // 4
  "bzpopmin",                       // 5
  "decr",                           // 6
  "decrby",                         // 7
  "del",                            // 8
  "delifexpim",                     // 9
  "delifgreater",                   // 10
  "expire",                         // 11
  "expireat",                       // 12
  "flushall",                       // 13
  "flushdb",                        // 14
  "geoadd",                         // 15
  "georadius",                      // 16
  "georadiusbymember",              // 17
  "geosearchstore",                 // 18
  "getdel",                         // 19
  "getex",                          // 20
  "getset",                         // 21
  "hcollect",                       // 22
  "hdel",                           // 23
  "hexpire",                        // 24
  "hpexpire",                       // 25
  "hexpireat",                      // 26
  "hpexpireat",                     // 27
  "hpersist",                       // 28
  "hincrby",                        // 29
  "hincrbyfloat",                   // 30
  "hmset",                          // 31
  "hset",                           // 32
  "hsetnx",                         // 33
  "incr",                           // 34
  "incrby",                         // 35
  "incrbyfloat",                    // 36
  "linsert",                        // 37
  "lmove",                          // 38
  "lmpop",                          // 39
  "lpop",                           // 40
  "lpush",                          // 41
  "lpushx",                         // 42
  "lrem",                           // 43
  "lset",                           // 44
  "ltrim",                          // 45
  "blpop",                          // 46
  "brpop",                          // 47
  "blmove",                         // 48
  "brpoplpush",                     // 49
  "blmpop",                         // 50
  "migrate",                        // 51
  "mset",                           // 52
  "msetnx",                         // 53
  "persist",                        // 54
  "pexpire",                        // 55
  "pexpireat",                      // 56
  "pfadd",                          // 57
  "pfmerge",                        // 58
  "psetex",                         // 59
  "rename",                         // 60
  "ri.create",                      // 61
  "ri.del",                         // 62
  "ripromote",                      // 63
  "rirestore",                      // 64
  "ri.set",                         // 65
  "restore",                        // 66
  "renamenx",                       // 67
  "rpop",                           // 68
  "rpoplpush",                      // 69
  "rpush",                          // 70
  "rpushx",                         // 71
  "sadd",                           // 72
  "sdiffstore",                     // 73
  "set",                            // 74
  "setbit",                         // 75
  "setex",                          // 76
  "setexnx",                        // 77
  "setexxx",                        // 78
  "setnx",                          // 79
  "setifmatch",                     // 80
  "setifgreater",                   // 81
  "setwithetag",                    // 82
  "setkeepttl",                     // 83
  "setkeepttlxx",                   // 84
  "setrange",                       // 85
  "sinterstore",                    // 86
  "smove",                          // 87
  "spop",                           // 88
  "srem",                           // 89
  "sunionstore",                    // 90
  "swapdb",                         // 91
  "unlink",                         // 92
  "vadd",                           // 93
  "vrem",                           // 94
  "vsetattr",                       // 95
  "zadd",                           // 96
  "zcollect",                       // 97
  "zdiffstore",                     // 98
  "zexpire",                        // 99
  "zpexpire",                       // 100
  "zexpireat",                      // 101
  "zpexpireat",                     // 102
  "zpersist",                       // 103
  "zincrby",                        // 104
  "zmpop",                          // 105
  "zinterstore",                    // 106
  "zpopmax",                        // 107
  "zpopmin",                        // 108
  "zrangestore",                    // 109
  "zrem",                           // 110
  "zremrangebylex",                 // 111
  "zremrangebyrank",                // 112
  "zremrangebyscore",               // 113
  "zunionstore",                    // 114
  "bitop",                          // 115
  "bitop_and",                      // 116
  "bitop_or",                       // 117
  "bitop_xor",                      // 118
  "bitop_not",                      // 119
  "bitop_diff",                     // 120
  "bitcount",                       // 121
  "bitfield_ro",                    // 122
  "bitpos",                         // 123
  "coscan",                         // 124
  "dbsize",                         // 125
  "dump",                           // 126
  "exists",                         // 127
  "expiretime",                     // 128
  "geodist",                        // 129
  "geohash",                        // 130
  "geopos",                         // 131
  "georadius_ro",                   // 132
  "georadiusbymember_ro",           // 133
  "geosearch",                      // 134
  "get",                            // 135
  "getbit",                         // 136
  "getifnotmatch",                  // 137
  "getrange",                       // 138
  "getwithetag",                    // 139
  "hexists",                        // 140
  "hget",                           // 141
  "hgetall",                        // 142
  "hkeys",                          // 143
  "hlen",                           // 144
  "hmget",                          // 145
  "hrandfield",                     // 146
  "hscan",                          // 147
  "hstrlen",                        // 148
  "hvals",                          // 149
  "keys",                           // 150
  "lcs",                            // 151
  "httl",                           // 152
  "hpttl",                          // 153
  "hexpiretime",                    // 154
  "hpexpiretime",                   // 155
  "lindex",                         // 156
  "llen",                           // 157
  "lpos",                           // 158
  "lrange",                         // 159
  "memory|usage",                   // 160
  "mget",                           // 161
  "object|encoding",                // 162
  "object|freq",                    // 163
  "object|idletime",                // 164
  "object|refcount",                // 165
  "pexpiretime",                    // 166
  "pfcount",                        // 167
  "pttl",                           // 168
  "scan",                           // 169
  "scard",                          // 170
  "sdiff",                          // 171
  "sinter",                         // 172
  "sintercard",                     // 173
  "sismember",                      // 174
  "smembers",                       // 175
  "smismember",                     // 176
  "spublish",                       // 177
  "srandmember",                    // 178
  "sscan",                          // 179
  "ssubscribe",                     // 180
  "strlen",                         // 181
  "substr",                         // 182
  "sunion",                         // 183
  "ttl",                            // 184
  "type",                           // 185
  "vcard",                          // 186
  "vdim",                           // 187
  "vemb",                           // 188
  "vgetattr",                       // 189
  "vinfo",                          // 190
  "vismember",                      // 191
  "vlinks",                         // 192
  "vrandmember",                    // 193
  "vsim",                           // 194
  "watch",                          // 195
  "watchms",                        // 196
  "watchos",                        // 197
  "zcard",                          // 198
  "zcount",                         // 199
  "zdiff",                          // 200
  "zinter",                         // 201
  "zintercard",                     // 202
  "zlexcount",                      // 203
  "zmscore",                        // 204
  "zrandmember",                    // 205
  "zrange",                         // 206
  "zrangebylex",                    // 207
  "zrangebyscore",                  // 208
  "zrank",                          // 209
  "zrevrange",                      // 210
  "zrevrangebylex",                 // 211
  "zrevrangebyscore",               // 212
  "zrevrank",                       // 213
  "zttl",                           // 214
  "zpttl",                          // 215
  "zexpiretime",                    // 216
  "zpexpiretime",                   // 217
  "zscan",                          // 218
  "zscore",                         // 219
  "zunion",                         // 220
  "ri.config",                      // 221
  "ri.exists",                      // 222
  "ri.get",                         // 223
  "ri.metrics",                     // 224
  "ri.range",                       // 225
  "ri.scan",                        // 226
  "eval",                           // 227
  "evalsha",                        // 228
  "async",                          // 229
  "ping",                           // 230
  "pubsub",                         // 231
  "pubsub|channels",                // 232
  "pubsub|numpat",                  // 233
  "pubsub|numsub",                  // 234
  "publish",                        // 235
  "subscribe",                      // 236
  "psubscribe",                     // 237
  "unsubscribe",                    // 238
  "punsubscribe",                   // 239
  "asking",                         // 240
  "select",                         // 241
  "echo",                           // 242
  "client",                         // 243
  "client|id",                      // 244
  "client|info",                    // 245
  "client|list",                    // 246
  "client|kill",                    // 247
  "client|getname",                 // 248
  "client|setname",                 // 249
  "client|setinfo",                 // 250
  "client|unblock",                 // 251
  "monitor",                        // 252
  "module",                         // 253
  "module|loadcs",                  // 254
  "registercs",                     // 255
  "multi",                          // 256
  "exec",                           // 257
  "discard",                        // 258
  "unwatch",                        // 259
  "runtxp",                         // 260
  "readonly",                       // 261
  "readwrite",                      // 262
  "replicaof",                      // 263
  "slaveof",                        // 264
  "info",                           // 265
  "time",                           // 266
  "role",                           // 267
  "save",                           // 268
  "expdelscan",                     // 269
  "lastsave",                       // 270
  "bgsave",                         // 271
  "commitaof",                      // 272
  "failover",                       // 273
  "customtxn",                      // 274
  "customrawstringcmd",             // 275
  "customobjcmd",                   // 276
  "customprocedure",                // 277
  "script",                         // 278
  "script|exists",                  // 279
  "script|flush",                   // 280
  "script|load",                    // 281
  "acl",                            // 282
  "acl|cat",                        // 283
  "acl|deluser",                    // 284
  "acl|genpass",                    // 285
  "acl|getuser",                    // 286
  "acl|list",                       // 287
  "acl|load",                       // 288
  "acl|save",                       // 289
  "acl|setuser",                    // 290
  "acl|users",                      // 291
  "acl|whoami",                     // 292
  "command",                        // 293
  "command|count",                  // 294
  "command|docs",                   // 295
  "command|info",                   // 296
  "command|getkeys",                // 297
  "command|getkeysandflags",        // 298
  "memory",                         // 299
  "object",                         // 300
  "object|help",                    // 301
  "config",                         // 302
  "config|get",                     // 303
  "config|rewrite",                 // 304
  "config|set",                     // 305
  "debug",                          // 306
  "latency",                        // 307
  "latency|help",                   // 308
  "latency|histogram",              // 309
  "latency|reset",                  // 310
  "slowlog",                        // 311
  "slowlog|help",                   // 312
  "slowlog|len",                    // 313
  "slowlog|get",                    // 314
  "slowlog|reset",                  // 315
  "cluster",                        // 316
  "cluster|addslots",               // 317
  "cluster|addslotsrange",          // 318
  "cluster|advance_time",           // 319
  "cluster|appendlog",              // 320
  "cluster|attach_sync",            // 321
  "cluster|banlist",                // 322
  "cluster|begin_replica_recover",  // 323
  "cluster|bumpepoch",              // 324
  "cluster|countkeysinslot",        // 325
  "cluster|delkeysinslot",          // 326
  "cluster|delkeysinslotrange",     // 327
  "cluster|delslots",               // 328
  "cluster|delslotsrange",          // 329
  "cluster|endpoint",               // 330
  "cluster|failover",               // 331
  "cluster|failreplicationoffset",  // 332
  "cluster|failstopwrites",         // 333
  "cluster|flushall",               // 334
  "cluster|forget",                 // 335
  "cluster|getkeysinslot",          // 336
  "cluster|gossip",                 // 337
  "cluster|help",                   // 338
  "cluster|info",                   // 339
  "cluster|initiate_replica_sync",  // 340
  "cluster|keyslot",                // 341
  "cluster|meet",                   // 342
  "cluster|migrate",                // 343
  "cluster|mlog_key_time",          // 344
  "cluster|mtasks",                 // 345
  "cluster|myid",                   // 346
  "cluster|myparentid",             // 347
  "cluster|nodes",                  // 348
  "cluster|publish",                // 349
  "cluster|spublish",               // 350
  "cluster|replicas",               // 351
  "cluster|replicate",              // 352
  "cluster|reserve",                // 353
  "cluster|reset",                  // 354
  "cluster|send_ckpt_file_segment", // 355
  "cluster|send_ckpt_metadata",     // 356
  "cluster|set-config-epoch",       // 357
  "cluster|setslot",                // 358
  "cluster|setslotsrange",          // 359
  "cluster|shards",                 // 360
  "cluster|slots",                  // 361
  "cluster|slotstate",              // 362
  "cluster|snapshot_data",          // 363
  "cluster|sync",                   // 364
  "auth",                           // 365
  "hello",                          // 366
  "quit",                           // 367
];

/// 根据 RespCommand 获取其规范化小写命令名称（子命令格式如 client|id）
#[inline]
pub fn command_name(cmd: RespCommand) -> &'static str {
  let idx = cmd as usize;
  if idx < CMD_NAMES.len() {
    // SAFETY: idx 已被显式检查小于 CMD_NAMES.len()
    unsafe { CMD_NAMES.get_unchecked(idx) }
  } else {
    ""
  }
}

/// 命令权限集合，采用 1024 位（128 字节）固定大小位图与自定义命令集合
#[derive(Clone, Debug, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub struct CommandPermissionSet {
  /// 1024 位的命令权限位图（16 个 u64，仅 128 字节，O(1) 零堆分配）
  pub bits: [u64; 16],
  /// 权限规则描述文本（如 "+@all", "+set +get" 等）
  pub description: String,
  /// 显式允许的自定义命令名称集合（大写存储）
  pub custom_allowed: HashSet<String>,
  /// 显式拒绝的自定义命令名称集合（大写存储）
  pub custom_denied: HashSet<String>,
}

impl Default for CommandPermissionSet {
  fn default() -> Self {
    Self::new()
  }
}

impl CommandPermissionSet {
  /// 创建空权限集（禁用全部命令）
  pub fn new() -> Self {
    Self {
      bits: CAT_NONE,
      description: String::new(),
      custom_allowed: HashSet::default(),
      custom_denied: HashSet::default(),
    }
  }

  /// 附带规则描述创建权限集（对标 C# CommandPermissionSet(string)）
  #[inline]
  pub fn with_description(desc: impl Into<String>) -> Self {
    let mut s = Self::new();
    s.description = desc.into();
    s
  }

  /// 从位图与规则描述创建权限集（对标 C# CommandPermissionSet(ulong[], string)）
  #[inline]
  pub fn from_bitmap(bits: [u64; 16], desc: impl Into<String>) -> Self {
    Self {
      bits,
      description: desc.into(),
      custom_allowed: HashSet::default(),
      custom_denied: HashSet::default(),
    }
  }

  /// 创建全量权限集（允许全部命令：+@all）
  pub fn all() -> Self {
    Self {
      bits: CAT_ALL,
      description: "+@all".to_string(),
      custom_allowed: HashSet::default(),
      custom_denied: HashSet::default(),
    }
  }

  /// 检查是否为全量权限
  #[inline]
  pub fn is_all(&self) -> bool {
    self.bits == CAT_ALL && self.custom_denied.is_empty()
  }

  /// 检查是否为空权限
  #[inline]
  pub fn is_empty(&self) -> bool {
    self.bits == CAT_NONE && self.custom_allowed.is_empty()
  }

  /// 检查命令是否被允许执行（O(1) 零堆分配单指令位运算）
  ///
  /// 入参先经 ACL 规范化：衍生别名（如 Setexnx）与其主命令（Set）共享同一位，
  /// 与 `set`/`clear` 的写入口径严格对称
  #[inline(always)]
  pub fn allow(&self, cmd: RespCommand) -> bool {
    let idx = cmd.normalize_for_acls() as usize;
    if idx == 0 || idx >= 1024 {
      return false;
    }
    // SAFETY: idx < 1024 保证 idx >> 6 < 16，self.bits 长度恒为 16
    unsafe { (*self.bits.get_unchecked(idx >> 6) & (1u64 << (idx & 63))) != 0 }
  }

  /// 检查自定义（扩展）命令是否被允许执行（栈缓冲无堆分配查找）
  #[inline]
  pub fn allow_custom(&self, generic_cmd: RespCommand, custom_name: &str) -> bool {
    let len = custom_name.len();
    if len <= 64 {
      let mut buf = [0u8; 64];
      buf[..len].copy_from_slice(custom_name.as_bytes());
      buf[..len].make_ascii_uppercase();
      // SAFETY: custom_name 是合法 UTF-8，ASCII 转大写仍为合法 UTF-8
      let norm = unsafe { from_utf8_unchecked(&buf[..len]) };
      if self.custom_denied.contains(norm) {
        return false;
      }
      if self.custom_allowed.contains(norm) {
        return true;
      }
    } else {
      let norm = custom_name.to_ascii_uppercase();
      if self.custom_denied.contains(&norm) {
        return false;
      }
      if self.custom_allowed.contains(&norm) {
        return true;
      }
    }
    if self.is_all() {
      return true;
    }
    self.allow(generic_cmd)
  }

  /// 单位写位（on=置位 / off=清位，含 1024 位边界防护）
  #[inline(always)]
  fn set_bit_idx(&mut self, idx: usize, on: bool) {
    if idx > 0 && idx < 1024 {
      // SAFETY: idx < 1024 保证 idx >> 6 < 16
      unsafe {
        let word = self.bits.get_unchecked_mut(idx >> 6);
        let bit = 1u64 << (idx & 63);
        if on {
          *word |= bit;
        } else {
          *word &= !bit;
        }
      }
    }
  }

  /// 授权指定命令及其全部 ACL 展开命令（衍生命令统一归一到主命令位）
  #[inline(always)]
  fn apply_command_bits(&mut self, cmd: RespCommand, on: bool) {
    let norm = cmd.normalize_for_acls();
    self.set_bit_idx(norm as usize, on);
    for extra in norm.expand_for_acls() {
      self.set_bit_idx(*extra as usize, on);
    }
  }

  /// 授权指定命令（及其 ACL 展开命令）
  #[inline(always)]
  pub fn set(&mut self, cmd: RespCommand) {
    self.apply_command_bits(cmd, true);
  }

  /// 取消指定命令的授权（AUTH, HELLO, QUIT 保护不可取消）
  #[inline(always)]
  pub fn clear(&mut self, cmd: RespCommand) {
    if cmd.is_no_auth() {
      return;
    }
    self.apply_command_bits(cmd, false);
  }

  /// 授权全部命令
  #[inline]
  pub fn set_all(&mut self) {
    self.bits = CAT_ALL;
    self.custom_denied.clear();
  }

  /// 清空全部命令授权
  #[inline]
  pub fn clear_all(&mut self) {
    self.bits = CAT_NONE;
    self.custom_allowed.clear();
  }

  /// 检查分类下的全部命令是否已全部处于授权状态
  ///
  /// 空位图分类（如 @stream）恒为 true：与 C# 一致，+@空分类 为完全 no-op（含描述）
  #[inline]
  pub fn is_category_all_allowed(&self, cat_bits: &[u64; 16]) -> bool {
    cat_bits
      .iter()
      .zip(&self.bits)
      .all(|(&cat, &b)| (b & cat) == cat)
  }

  /// 检查分类下是否有任意命令当前已被授权
  #[inline]
  pub fn is_category_any_allowed(&self, cat_bits: &[u64; 16]) -> bool {
    cat_bits
      .iter()
      .zip(&self.bits)
      .any(|(&cat, &b)| (b & cat) != 0)
  }

  /// 按位并集合并分类位图（零堆分配）
  #[inline]
  pub fn or_bits(&mut self, bits: &[u64; 16]) {
    for (b, &cat) in self.bits.iter_mut().zip(bits) {
      *b |= cat;
    }
  }

  /// 按位清除分类位图（保留 NO_AUTH 免认证命令位，零堆分配）
  #[inline]
  pub fn andnot_bits_protect_noauth(&mut self, bits: &[u64; 16]) {
    for (b, (&cat, &no_auth)) in self.bits.iter_mut().zip(bits.iter().zip(&NO_AUTH_MASK)) {
      *b &= !(cat & !no_auth);
    }
  }

  /// 设置分类权限（allow = true 为授权，false 为收回，零堆分配）
  pub fn set_category(&mut self, cat: &str, allow: bool) -> Result<()> {
    if cat.eq_ignore_ascii_case("all") {
      if allow {
        self.set_all();
      } else {
        self.clear_all();
      }
      return Ok(());
    }
    let cat_bits =
      get_category_bitmap(cat).ok_or_else(|| Error::CategoryDoesNotExist(cat.to_string()))?;
    if allow {
      self.or_bits(&cat_bits);
    } else {
      self.andnot_bits_protect_noauth(&cat_bits);
    }
    Ok(())
  }

  /// 添加自定义命令授权
  pub fn add_custom_command(&mut self, name: &str) {
    let norm = name.to_ascii_uppercase();
    self.custom_denied.remove(&norm);
    self.custom_allowed.insert(norm);
  }

  /// 移除自定义命令授权
  pub fn remove_custom_command(&mut self, name: &str) {
    let norm = name.to_ascii_uppercase();
    self.custom_allowed.remove(&norm);
    self.custom_denied.insert(norm);
  }

  /// 获取显式允许的自定义命令集合引用
  #[inline]
  pub fn custom_allowed(&self) -> &HashSet<String> {
    &self.custom_allowed
  }

  /// 获取显式拒绝的自定义命令集合引用
  #[inline]
  pub fn custom_denied(&self) -> &HashSet<String> {
    &self.custom_denied
  }

  /// 获取权限规则描述文本
  #[inline]
  pub fn description(&self) -> &str {
    &self.description
  }

  /// 设置权限描述文本
  #[inline]
  pub fn set_description(&mut self, desc: impl Into<String>) {
    self.description = desc.into();
  }

  /// 追加单项操作规则描述（如 `+set`、`-del`、`+@keyspace`，零额外堆内存分配）
  ///
  /// `category=true` 时在 token 前插入 `@`（分类规则），与单项规则共用脚手架
  fn append_token(&mut self, prefix: char, at: bool, token: &str) {
    let extra = if at { 2 } else { 1 };
    if self.description.is_empty() {
      self.description.reserve(token.len() + extra);
    } else {
      self.description.reserve(token.len() + extra + 1);
      self.description.push(' ');
    }
    self.description.push(prefix);
    if at {
      self.description.push('@');
    }
    self.description.push_str(token);
  }

  /// 追加单项操作规则描述（如 `+set`、`-del`，零额外堆内存分配）
  pub fn append_op(&mut self, prefix: char, token: &str) {
    self.append_token(prefix, false, token);
  }

  /// 追加分类操作规则描述（如 `+@keyspace`、`-@all`，零额外堆内存分配）
  pub fn append_category_op(&mut self, prefix: char, cat: &str) {
    self.append_token(prefix, true, cat);
  }
}
