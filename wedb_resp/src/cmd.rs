use core::fmt;

use crate::cmd_hash::{lookup_primary, lookup_subcommand};

/// RESP 命令枚举，1:1 精确对齐 Microsoft Garnet 操作码与属性分类
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
#[repr(u16)]
pub enum RespCommand {
  /// 无命令
  #[default]
  None = 0x00,

  // ====== 写入（变异）命令（持久化到 AOF / 复制流） ======
  Append = 1,
  Bitfield = 2,
  Bzmpop = 3,
  Bzpopmax = 4,
  Bzpopmin = 5,
  Decr = 6,
  Decrby = 7,
  Del = 8,
  Delifexpim = 9,
  Delifgreater = 10,
  Expire = 11,
  Expireat = 12,
  Flushall = 13,
  Flushdb = 14,
  Geoadd = 15,
  Georadius = 16,
  Georadiusbymember = 17,
  Geosearchstore = 18,
  Getdel = 19,
  Getex = 20,
  Getset = 21,
  Hcollect = 22,
  Hdel = 23,
  Hexpire = 24,
  Hpexpire = 25,
  Hexpireat = 26,
  Hpexpireat = 27,
  Hpersist = 28,
  Hincrby = 29,
  Hincrbyfloat = 30,
  Hmset = 31,
  Hset = 32,
  Hsetnx = 33,
  Incr = 34,
  Incrby = 35,
  Incrbyfloat = 36,
  Linsert = 37,
  Lmove = 38,
  Lmpop = 39,
  Lpop = 40,
  Lpush = 41,
  Lpushx = 42,
  Lrem = 43,
  Lset = 44,
  Ltrim = 45,
  Blpop = 46,
  Brpop = 47,
  Blmove = 48,
  Brpoplpush = 49,
  Blmpop = 50,
  Migrate = 51,
  Mset = 52,
  Msetnx = 53,
  Persist = 54,
  Pexpire = 55,
  Pexpireat = 56,
  Pfadd = 57,
  Pfmerge = 58,
  Psetex = 59,
  Rename = 60,
  Ricreate = 61,
  Ridel = 62,
  Ripromote = 63,
  Rirestore = 64,
  Riset = 65,
  Restore = 66,
  Renamenx = 67,
  Rpop = 68,
  Rpoplpush = 69,
  Rpush = 70,
  Rpushx = 71,
  Sadd = 72,
  Sdiffstore = 73,
  Set = 74,
  Setbit = 75,
  Setex = 76,
  Setexnx = 77,
  Setexxx = 78,
  Setnx = 79,
  Setifmatch = 80,
  Setifgreater = 81,
  Setwithetag = 82,
  Setkeepttl = 83,
  Setkeepttlxx = 84,
  Setrange = 85,
  Sinterstore = 86,
  Smove = 87,
  Spop = 88,
  Srem = 89,
  Sunionstore = 90,
  Swapdb = 91,
  Unlink = 92,
  Vadd = 93,
  Vrem = 94,
  Vsetattr = 95,
  Zadd = 96,
  Zcollect = 97,
  Zdiffstore = 98,
  Zexpire = 99,
  Zpexpire = 100,
  Zexpireat = 101,
  Zpexpireat = 102,
  Zpersist = 103,
  Zincrby = 104,
  Zmpop = 105,
  Zinterstore = 106,
  Zpopmax = 107,
  Zpopmin = 108,
  Zrangestore = 109,
  Zrem = 110,
  Zremrangebylex = 111,
  Zremrangebyrank = 112,
  Zremrangebyscore = 113,
  Zunionstore = 114,

  // BITOP 及其伪子命令
  Bitop = 115,
  BitopAnd = 116,
  BitopOr = 117,
  BitopXor = 118,
  BitopNot = 119,
  BitopDiff = 120, // 最后一个写入命令

  // ====== 只读命令（永不写入 AOF） ======
  Bitcount = 121,
  BitfieldRo = 122,
  Bitpos = 123,
  Coscan = 124,
  Dbsize = 125,
  Dump = 126,
  Exists = 127,
  Expiretime = 128,
  Geodist = 129,
  Geohash = 130,
  Geopos = 131,
  GeoradiusRo = 132,
  GeoradiusbymemberRo = 133,
  Geosearch = 134,
  Get = 135,
  Getbit = 136,
  Getifnotmatch = 137,
  Getrange = 138,
  Getwithetag = 139,
  Hexists = 140,
  Hget = 141,
  Hgetall = 142,
  Hkeys = 143,
  Hlen = 144,
  Hmget = 145,
  Hrandfield = 146,
  Hscan = 147,
  Hstrlen = 148,
  Hvals = 149,
  Keys = 150,
  Lcs = 151,
  Httl = 152,
  Hpttl = 153,
  Hexpiretime = 154,
  Hpexpiretime = 155,
  Lindex = 156,
  Llen = 157,
  Lpos = 158,
  Lrange = 159,
  MemoryUsage = 160,
  Mget = 161,
  ObjectEncoding = 162,
  ObjectFreq = 163,
  ObjectIdletime = 164,
  ObjectRefcount = 165,
  Pexpiretime = 166,
  Pfcount = 167,
  Pttl = 168,
  Scan = 169,
  Scard = 170,
  Sdiff = 171,
  Sinter = 172,
  Sintercard = 173,
  Sismember = 174,
  Smembers = 175,
  Smismember = 176,
  Spublish = 177,
  Srandmember = 178,
  Sscan = 179,
  Ssubscribe = 180,
  Strlen = 181,
  Substr = 182,
  Sunion = 183,
  Ttl = 184,
  Type = 185,
  Vcard = 186,
  Vdim = 187,
  Vemb = 188,
  Vgetattr = 189,
  Vinfo = 190,
  Vismember = 191,
  Vlinks = 192,
  Vrandmember = 193,
  Vsim = 194,
  Watch = 195,
  Watchms = 196,
  Watchos = 197,
  Zcard = 198,
  Zcount = 199,
  Zdiff = 200,
  Zinter = 201,
  Zintercard = 202,
  Zlexcount = 203,
  Zmscore = 204,
  Zrandmember = 205,
  Zrange = 206,
  Zrangebylex = 207,
  Zrangebyscore = 208,
  Zrank = 209,
  Zrevrange = 210,
  Zrevrangebylex = 211,
  Zrevrangebyscore = 212,
  Zrevrank = 213,
  Zttl = 214,
  Zpttl = 215,
  Zexpiretime = 216,
  Zpexpiretime = 217,
  Zscan = 218,
  Zscore = 219,
  Zunion = 220,

  // 只读 RangeIndex 命令
  Riconfig = 221,
  Riexists = 222,
  Riget = 223,
  Rimetrics = 224,
  Rirange = 225,
  Riscan = 226, // 最后一个只读命令

  // 脚本执行命令
  Eval = 227,
  Evalsha = 228, // 最后一个数据命令

  // ====== 非读非写 / 控制管理命令 ======
  Async = 229,
  Ping = 230,

  // 发布订阅命令
  Pubsub = 231,
  PubsubChannels = 232,
  PubsubNumpat = 233,
  PubsubNumsub = 234,
  Publish = 235,
  Subscribe = 236,
  Psubscribe = 237,
  Unsubscribe = 238,
  Punsubscribe = 239,

  Asking = 240,
  Select = 241,
  Echo = 242,

  // 客户端连接命令
  Client = 243,
  ClientId = 244,
  ClientInfo = 245,
  ClientList = 246,
  ClientKill = 247,
  ClientGetname = 248,
  ClientSetname = 249,
  ClientSetinfo = 250,
  ClientUnblock = 251,

  Monitor = 252,
  Module = 253,
  ModuleLoadcs = 254,
  Registercs = 255,

  // 事务命令
  Multi = 256,
  Exec = 257,
  Discard = 258,
  Unwatch = 259,
  Runtxp = 260,

  Readonly = 261,
  Readwrite = 262,
  Replicaof = 263,
  Secondaryof = 264,

  Info = 265,
  Time = 266,
  Role = 267,
  Save = 268,
  Expdelscan = 269,
  Lastsave = 270,
  Bgsave = 271,
  Commitaof = 272,
  Failover = 273,

  // 自定义命令
  CustomTxn = 274,
  CustomRawStringCmd = 275,
  CustomObjCmd = 276,
  CustomProcedure = 277,

  // 脚本子命令
  Script = 278,
  ScriptExists = 279,
  ScriptFlush = 280,
  ScriptLoad = 281,

  // ACL 命令
  Acl = 282,
  AclCat = 283,
  AclDeluser = 284,
  AclGenpass = 285,
  AclGetuser = 286,
  AclList = 287,
  AclLoad = 288,
  AclSave = 289,
  AclSetuser = 290,
  AclUsers = 291,
  AclWhoami = 292,

  // COMMAND 命令
  Command = 293,
  CommandCount = 294,
  CommandDocs = 295,
  CommandInfo = 296,
  CommandGetkeys = 297,
  CommandGetkeysandflags = 298,

  Memory = 299,
  Object = 300,
  ObjectHelp = 301,

  // CONFIG 命令
  Config = 302,
  ConfigGet = 303,
  ConfigRewrite = 304,
  ConfigSet = 305,

  Debug = 306,

  // LATENCY 命令
  Latency = 307,
  LatencyHelp = 308,
  LatencyHistogram = 309,
  LatencyReset = 310,

  // SLOWLOG 命令
  Slowlog = 311,
  SlowlogHelp = 312,
  SlowlogLen = 313,
  SlowlogGet = 314,
  SlowlogReset = 315,

  // 集群命令
  Cluster = 316,
  ClusterAddslots = 317,
  ClusterAddslotsrange = 318,
  ClusterAdvanceTime = 319,
  ClusterAppendlog = 320,
  ClusterAttachSync = 321,
  ClusterBanlist = 322,
  ClusterBeginReplicaRecover = 323,
  ClusterBumpepoch = 324,
  ClusterCountkeysinslot = 325,
  ClusterDelkeysinslot = 326,
  ClusterDelkeysinslotrange = 327,
  ClusterDelslots = 328,
  ClusterDelslotsrange = 329,
  ClusterEndpoint = 330,
  ClusterFailover = 331,
  ClusterFailreplicationoffset = 332,
  ClusterFailstopwrites = 333,
  ClusterFlushall = 334,
  ClusterForget = 335,
  ClusterGetkeysinslot = 336,
  ClusterGossip = 337,
  ClusterHelp = 338,
  ClusterInfo = 339,
  ClusterInitiateReplicaSync = 340,
  ClusterKeyslot = 341,
  ClusterMeet = 342,
  ClusterMigrate = 343,
  ClusterMlogKeyTime = 344,
  ClusterMtasks = 345,
  ClusterMyid = 346,
  ClusterMyparentid = 347,
  ClusterNodes = 348,
  ClusterPublish = 349,
  ClusterSpublish = 350,
  ClusterReplicas = 351,
  ClusterReplicate = 352,
  ClusterReserve = 353,
  ClusterReset = 354,
  ClusterSendCkptFileSegment = 355,
  ClusterSendCkptMetadata = 356,
  ClusterSetconfigepoch = 357,
  ClusterSetslot = 358,
  ClusterSetslotsrange = 359,
  ClusterShards = 360,
  ClusterSlots = 361,
  ClusterSlotstate = 362,
  ClusterSnapshotData = 363,
  ClusterSync = 364,

  // 免认证命令
  Auth = 365,
  Hello = 366,
  Quit = 367,

  /// 无效命令
  Invalid = 0xFFFF,
}

impl RespCommand {
  /// 获取命令的规范大写名称字符串
  #[inline]
  pub const fn as_str(&self) -> &'static str {
    match self {
      Self::None => "NONE",
      Self::Append => "APPEND",
      Self::Bitfield => "BITFIELD",
      Self::Bzmpop => "BZMPOP",
      Self::Bzpopmax => "BZPOPMAX",
      Self::Bzpopmin => "BZPOPMIN",
      Self::Decr => "DECR",
      Self::Decrby => "DECRBY",
      Self::Del => "DEL",
      Self::Delifexpim => "DELIFEXPIM",
      Self::Delifgreater => "DELIFGREATER",
      Self::Expire => "EXPIRE",
      Self::Expireat => "EXPIREAT",
      Self::Flushall => "FLUSHALL",
      Self::Flushdb => "FLUSHDB",
      Self::Geoadd => "GEOADD",
      Self::Georadius => "GEORADIUS",
      Self::Georadiusbymember => "GEORADIUSBYMEMBER",
      Self::Geosearchstore => "GEOSEARCHSTORE",
      Self::Getdel => "GETDEL",
      Self::Getex => "GETEX",
      Self::Getset => "GETSET",
      Self::Hcollect => "HCOLLECT",
      Self::Hdel => "HDEL",
      Self::Hexpire => "HEXPIRE",
      Self::Hpexpire => "HPEXPIRE",
      Self::Hexpireat => "HEXPIREAT",
      Self::Hpexpireat => "HPEXPIREAT",
      Self::Hpersist => "HPERSIST",
      Self::Hincrby => "HINCRBY",
      Self::Hincrbyfloat => "HINCRBYFLOAT",
      Self::Hmset => "HMSET",
      Self::Hset => "HSET",
      Self::Hsetnx => "HSETNX",
      Self::Incr => "INCR",
      Self::Incrby => "INCRBY",
      Self::Incrbyfloat => "INCRBYFLOAT",
      Self::Linsert => "LINSERT",
      Self::Lmove => "LMOVE",
      Self::Lmpop => "LMPOP",
      Self::Lpop => "LPOP",
      Self::Lpush => "LPUSH",
      Self::Lpushx => "LPUSHX",
      Self::Lrem => "LREM",
      Self::Lset => "LSET",
      Self::Ltrim => "LTRIM",
      Self::Blpop => "BLPOP",
      Self::Brpop => "BRPOP",
      Self::Blmove => "BLMOVE",
      Self::Brpoplpush => "BRPOPLPUSH",
      Self::Blmpop => "BLMPOP",
      Self::Migrate => "MIGRATE",
      Self::Mset => "MSET",
      Self::Msetnx => "MSETNX",
      Self::Persist => "PERSIST",
      Self::Pexpire => "PEXPIRE",
      Self::Pexpireat => "PEXPIREAT",
      Self::Pfadd => "PFADD",
      Self::Pfmerge => "PFMERGE",
      Self::Psetex => "PSETEX",
      Self::Rename => "RENAME",
      Self::Ricreate => "RICREATE",
      Self::Ridel => "RIDEL",
      Self::Ripromote => "RIPROMOTE",
      Self::Rirestore => "RIRESTORE",
      Self::Riset => "RISET",
      Self::Restore => "RESTORE",
      Self::Renamenx => "RENAMENX",
      Self::Rpop => "RPOP",
      Self::Rpoplpush => "RPOPLPUSH",
      Self::Rpush => "RPUSH",
      Self::Rpushx => "RPUSHX",
      Self::Sadd => "SADD",
      Self::Sdiffstore => "SDIFFSTORE",
      Self::Set => "SET",
      Self::Setbit => "SETBIT",
      Self::Setex => "SETEX",
      Self::Setexnx => "SETEXNX",
      Self::Setexxx => "SETEXXX",
      Self::Setnx => "SETNX",
      Self::Setifmatch => "SETIFMATCH",
      Self::Setifgreater => "SETIFGREATER",
      Self::Setwithetag => "SETWITHETAG",
      Self::Setkeepttl => "SETKEEPTTL",
      Self::Setkeepttlxx => "SETKEEPTTLXX",
      Self::Setrange => "SETRANGE",
      Self::Sinterstore => "SINTERSTORE",
      Self::Smove => "SMOVE",
      Self::Spop => "SPOP",
      Self::Srem => "SREM",
      Self::Sunionstore => "SUNIONSTORE",
      Self::Swapdb => "SWAPDB",
      Self::Unlink => "UNLINK",
      Self::Vadd => "VADD",
      Self::Vrem => "VREM",
      Self::Vsetattr => "VSETATTR",
      Self::Zadd => "ZADD",
      Self::Zcollect => "ZCOLLECT",
      Self::Zdiffstore => "ZDIFFSTORE",
      Self::Zexpire => "ZEXPIRE",
      Self::Zpexpire => "ZPEXPIRE",
      Self::Zexpireat => "ZEXPIREAT",
      Self::Zpexpireat => "ZPEXPIREAT",
      Self::Zpersist => "ZPERSIST",
      Self::Zincrby => "ZINCRBY",
      Self::Zmpop => "ZMPOP",
      Self::Zinterstore => "ZINTERSTORE",
      Self::Zpopmax => "ZPOPMAX",
      Self::Zpopmin => "ZPOPMIN",
      Self::Zrangestore => "ZRANGESTORE",
      Self::Zrem => "ZREM",
      Self::Zremrangebylex => "ZREMRANGEBYLEX",
      Self::Zremrangebyrank => "ZREMRANGEBYRANK",
      Self::Zremrangebyscore => "ZREMRANGEBYSCORE",
      Self::Zunionstore => "ZUNIONSTORE",
      Self::Bitop => "BITOP",
      Self::BitopAnd => "BITOP_AND",
      Self::BitopOr => "BITOP_OR",
      Self::BitopXor => "BITOP_XOR",
      Self::BitopNot => "BITOP_NOT",
      Self::BitopDiff => "BITOP_DIFF",
      Self::Bitcount => "BITCOUNT",
      Self::BitfieldRo => "BITFIELD_RO",
      Self::Bitpos => "BITPOS",
      Self::Coscan => "COSCAN",
      Self::Dbsize => "DBSIZE",
      Self::Dump => "DUMP",
      Self::Exists => "EXISTS",
      Self::Expiretime => "EXPIRETIME",
      Self::Geodist => "GEODIST",
      Self::Geohash => "GEOHASH",
      Self::Geopos => "GEOPOS",
      Self::GeoradiusRo => "GEORADIUS_RO",
      Self::GeoradiusbymemberRo => "GEORADIUSBYMEMBER_RO",
      Self::Geosearch => "GEOSEARCH",
      Self::Get => "GET",
      Self::Getbit => "GETBIT",
      Self::Getifnotmatch => "GETIFNOTMATCH",
      Self::Getrange => "GETRANGE",
      Self::Getwithetag => "GETWITHETAG",
      Self::Hexists => "HEXISTS",
      Self::Hget => "HGET",
      Self::Hgetall => "HGETALL",
      Self::Hkeys => "HKEYS",
      Self::Hlen => "HLEN",
      Self::Hmget => "HMGET",
      Self::Hrandfield => "HRANDFIELD",
      Self::Hscan => "HSCAN",
      Self::Hstrlen => "HSTRLEN",
      Self::Hvals => "HVALS",
      Self::Keys => "KEYS",
      Self::Lcs => "LCS",
      Self::Httl => "HTTL",
      Self::Hpttl => "HPTTL",
      Self::Hexpiretime => "HEXPIRETIME",
      Self::Hpexpiretime => "HPEXPIRETIME",
      Self::Lindex => "LINDEX",
      Self::Llen => "LLEN",
      Self::Lpos => "LPOS",
      Self::Lrange => "LRANGE",
      Self::MemoryUsage => "MEMORY_USAGE",
      Self::Mget => "MGET",
      Self::ObjectEncoding => "OBJECT_ENCODING",
      Self::ObjectFreq => "OBJECT_FREQ",
      Self::ObjectIdletime => "OBJECT_IDLETIME",
      Self::ObjectRefcount => "OBJECT_REFCOUNT",
      Self::Pexpiretime => "PEXPIRETIME",
      Self::Pfcount => "PFCOUNT",
      Self::Pttl => "PTTL",
      Self::Scan => "SCAN",
      Self::Scard => "SCARD",
      Self::Sdiff => "SDIFF",
      Self::Sinter => "SINTER",
      Self::Sintercard => "SINTERCARD",
      Self::Sismember => "SISMEMBER",
      Self::Smembers => "SMEMBERS",
      Self::Smismember => "SMISMEMBER",
      Self::Spublish => "SPUBLISH",
      Self::Srandmember => "SRANDMEMBER",
      Self::Sscan => "SSCAN",
      Self::Ssubscribe => "SSUBSCRIBE",
      Self::Strlen => "STRLEN",
      Self::Substr => "SUBSTR",
      Self::Sunion => "SUNION",
      Self::Ttl => "TTL",
      Self::Type => "TYPE",
      Self::Vcard => "VCARD",
      Self::Vdim => "VDIM",
      Self::Vemb => "VEMB",
      Self::Vgetattr => "VGETATTR",
      Self::Vinfo => "VINFO",
      Self::Vismember => "VISMEMBER",
      Self::Vlinks => "VLINKS",
      Self::Vrandmember => "VRANDMEMBER",
      Self::Vsim => "VSIM",
      Self::Watch => "WATCH",
      Self::Watchms => "WATCHMS",
      Self::Watchos => "WATCHOS",
      Self::Zcard => "ZCARD",
      Self::Zcount => "ZCOUNT",
      Self::Zdiff => "ZDIFF",
      Self::Zinter => "ZINTER",
      Self::Zintercard => "ZINTERCARD",
      Self::Zlexcount => "ZLEXCOUNT",
      Self::Zmscore => "ZMSCORE",
      Self::Zrandmember => "ZRANDMEMBER",
      Self::Zrange => "ZRANGE",
      Self::Zrangebylex => "ZRANGEBYLEX",
      Self::Zrangebyscore => "ZRANGEBYSCORE",
      Self::Zrank => "ZRANK",
      Self::Zrevrange => "ZREVRANGE",
      Self::Zrevrangebylex => "ZREVRANGEBYLEX",
      Self::Zrevrangebyscore => "ZREVRANGEBYSCORE",
      Self::Zrevrank => "ZREVRANK",
      Self::Zttl => "ZTTL",
      Self::Zpttl => "ZPTTL",
      Self::Zexpiretime => "ZEXPIRETIME",
      Self::Zpexpiretime => "ZPEXPIRETIME",
      Self::Zscan => "ZSCAN",
      Self::Zscore => "ZSCORE",
      Self::Zunion => "ZUNION",
      Self::Riconfig => "RICONFIG",
      Self::Riexists => "RIEXISTS",
      Self::Riget => "RIGET",
      Self::Rimetrics => "RIMETRICS",
      Self::Rirange => "RIRANGE",
      Self::Riscan => "RISCAN",
      Self::Eval => "EVAL",
      Self::Evalsha => "EVALSHA",
      Self::Async => "ASYNC",
      Self::Ping => "PING",
      Self::Pubsub => "PUBSUB",
      Self::PubsubChannels => "PUBSUB_CHANNELS",
      Self::PubsubNumpat => "PUBSUB_NUMPAT",
      Self::PubsubNumsub => "PUBSUB_NUMSUB",
      Self::Publish => "PUBLISH",
      Self::Subscribe => "SUBSCRIBE",
      Self::Psubscribe => "PSUBSCRIBE",
      Self::Unsubscribe => "UNSUBSCRIBE",
      Self::Punsubscribe => "PUNSUBSCRIBE",
      Self::Asking => "ASKING",
      Self::Select => "SELECT",
      Self::Echo => "ECHO",
      Self::Client => "CLIENT",
      Self::ClientId => "CLIENT_ID",
      Self::ClientInfo => "CLIENT_INFO",
      Self::ClientList => "CLIENT_LIST",
      Self::ClientKill => "CLIENT_KILL",
      Self::ClientGetname => "CLIENT_GETNAME",
      Self::ClientSetname => "CLIENT_SETNAME",
      Self::ClientSetinfo => "CLIENT_SETINFO",
      Self::ClientUnblock => "CLIENT_UNBLOCK",
      Self::Monitor => "MONITOR",
      Self::Module => "MODULE",
      Self::ModuleLoadcs => "MODULE_LOADCS",
      Self::Registercs => "REGISTERCS",
      Self::Multi => "MULTI",
      Self::Exec => "EXEC",
      Self::Discard => "DISCARD",
      Self::Unwatch => "UNWATCH",
      Self::Runtxp => "RUNTXP",
      Self::Readonly => "READONLY",
      Self::Readwrite => "READWRITE",
      Self::Replicaof => "REPLICAOF",
      Self::Secondaryof => "SECONDARYOF",
      Self::Info => "INFO",
      Self::Time => "TIME",
      Self::Role => "ROLE",
      Self::Save => "SAVE",
      Self::Expdelscan => "EXPDELSCAN",
      Self::Lastsave => "LASTSAVE",
      Self::Bgsave => "BGSAVE",
      Self::Commitaof => "COMMITAOF",
      Self::Failover => "FAILOVER",
      Self::CustomTxn => "CustomTxn",
      Self::CustomRawStringCmd => "CustomRawStringCmd",
      Self::CustomObjCmd => "CustomObjCmd",
      Self::CustomProcedure => "CustomProcedure",
      Self::Script => "SCRIPT",
      Self::ScriptExists => "SCRIPT_EXISTS",
      Self::ScriptFlush => "SCRIPT_FLUSH",
      Self::ScriptLoad => "SCRIPT_LOAD",
      Self::Acl => "ACL",
      Self::AclCat => "ACL_CAT",
      Self::AclDeluser => "ACL_DELUSER",
      Self::AclGenpass => "ACL_GENPASS",
      Self::AclGetuser => "ACL_GETUSER",
      Self::AclList => "ACL_LIST",
      Self::AclLoad => "ACL_LOAD",
      Self::AclSave => "ACL_SAVE",
      Self::AclSetuser => "ACL_SETUSER",
      Self::AclUsers => "ACL_USERS",
      Self::AclWhoami => "ACL_WHOAMI",
      Self::Command => "COMMAND",
      Self::CommandCount => "COMMAND_COUNT",
      Self::CommandDocs => "COMMAND_DOCS",
      Self::CommandInfo => "COMMAND_INFO",
      Self::CommandGetkeys => "COMMAND_GETKEYS",
      Self::CommandGetkeysandflags => "COMMAND_GETKEYSANDFLAGS",
      Self::Memory => "MEMORY",
      Self::Object => "OBJECT",
      Self::ObjectHelp => "OBJECT_HELP",
      Self::Config => "CONFIG",
      Self::ConfigGet => "CONFIG_GET",
      Self::ConfigRewrite => "CONFIG_REWRITE",
      Self::ConfigSet => "CONFIG_SET",
      Self::Debug => "DEBUG",
      Self::Latency => "LATENCY",
      Self::LatencyHelp => "LATENCY_HELP",
      Self::LatencyHistogram => "LATENCY_HISTOGRAM",
      Self::LatencyReset => "LATENCY_RESET",
      Self::Slowlog => "SLOWLOG",
      Self::SlowlogHelp => "SLOWLOG_HELP",
      Self::SlowlogLen => "SLOWLOG_LEN",
      Self::SlowlogGet => "SLOWLOG_GET",
      Self::SlowlogReset => "SLOWLOG_RESET",
      Self::Cluster => "CLUSTER",
      Self::ClusterAddslots => "CLUSTER_ADDSLOTS",
      Self::ClusterAddslotsrange => "CLUSTER_ADDSLOTSRANGE",
      Self::ClusterAdvanceTime => "CLUSTER_ADVANCE_TIME",
      Self::ClusterAppendlog => "CLUSTER_APPENDLOG",
      Self::ClusterAttachSync => "CLUSTER_ATTACH_SYNC",
      Self::ClusterBanlist => "CLUSTER_BANLIST",
      Self::ClusterBeginReplicaRecover => "CLUSTER_BEGIN_REPLICA_RECOVER",
      Self::ClusterBumpepoch => "CLUSTER_BUMPEPOCH",
      Self::ClusterCountkeysinslot => "CLUSTER_COUNTKEYSINSLOT",
      Self::ClusterDelkeysinslot => "CLUSTER_DELKEYSINSLOT",
      Self::ClusterDelkeysinslotrange => "CLUSTER_DELKEYSINSLOTRANGE",
      Self::ClusterDelslots => "CLUSTER_DELSLOTS",
      Self::ClusterDelslotsrange => "CLUSTER_DELSLOTSRANGE",
      Self::ClusterEndpoint => "CLUSTER_ENDPOINT",
      Self::ClusterFailover => "CLUSTER_FAILOVER",
      Self::ClusterFailreplicationoffset => "CLUSTER_FAILREPLICATIONOFFSET",
      Self::ClusterFailstopwrites => "CLUSTER_FAILSTOPWRITES",
      Self::ClusterFlushall => "CLUSTER_FLUSHALL",
      Self::ClusterForget => "CLUSTER_FORGET",
      Self::ClusterGetkeysinslot => "CLUSTER_GETKEYSINSLOT",
      Self::ClusterGossip => "CLUSTER_GOSSIP",
      Self::ClusterHelp => "CLUSTER_HELP",
      Self::ClusterInfo => "CLUSTER_INFO",
      Self::ClusterInitiateReplicaSync => "CLUSTER_INITIATE_REPLICA_SYNC",
      Self::ClusterKeyslot => "CLUSTER_KEYSLOT",
      Self::ClusterMeet => "CLUSTER_MEET",
      Self::ClusterMigrate => "CLUSTER_MIGRATE",
      Self::ClusterMlogKeyTime => "CLUSTER_MLOG_KEY_TIME",
      Self::ClusterMtasks => "CLUSTER_MTASKS",
      Self::ClusterMyid => "CLUSTER_MYID",
      Self::ClusterMyparentid => "CLUSTER_MYPARENTID",
      Self::ClusterNodes => "CLUSTER_NODES",
      Self::ClusterPublish => "CLUSTER_PUBLISH",
      Self::ClusterSpublish => "CLUSTER_SPUBLISH",
      Self::ClusterReplicas => "CLUSTER_REPLICAS",
      Self::ClusterReplicate => "CLUSTER_REPLICATE",
      Self::ClusterReserve => "CLUSTER_RESERVE",
      Self::ClusterReset => "CLUSTER_RESET",
      Self::ClusterSendCkptFileSegment => "CLUSTER_SEND_CKPT_FILE_SEGMENT",
      Self::ClusterSendCkptMetadata => "CLUSTER_SEND_CKPT_METADATA",
      Self::ClusterSetconfigepoch => "CLUSTER_SETCONFIGEPOCH",
      Self::ClusterSetslot => "CLUSTER_SETSLOT",
      Self::ClusterSetslotsrange => "CLUSTER_SETSLOTSRANGE",
      Self::ClusterShards => "CLUSTER_SHARDS",
      Self::ClusterSlots => "CLUSTER_SLOTS",
      Self::ClusterSlotstate => "CLUSTER_SLOTSTATE",
      Self::ClusterSnapshotData => "CLUSTER_SNAPSHOT_DATA",
      Self::ClusterSync => "CLUSTER_SYNC",
      Self::Auth => "AUTH",
      Self::Hello => "HELLO",
      Self::Quit => "QUIT",
      Self::Invalid => "INVALID",
    }
  }

  pub const NONE: Self = Self::None;
  pub const APPEND: Self = Self::Append;
  pub const BITFIELD: Self = Self::Bitfield;
  pub const BZMPOP: Self = Self::Bzmpop;
  pub const BZPOPMAX: Self = Self::Bzpopmax;
  pub const BZPOPMIN: Self = Self::Bzpopmin;
  pub const DECR: Self = Self::Decr;
  pub const DECRBY: Self = Self::Decrby;
  pub const DEL: Self = Self::Del;
  pub const DELIFEXPIM: Self = Self::Delifexpim;
  pub const DELIFGREATER: Self = Self::Delifgreater;
  pub const EXPIRE: Self = Self::Expire;
  pub const EXPIREAT: Self = Self::Expireat;
  pub const FLUSHALL: Self = Self::Flushall;
  pub const FLUSHDB: Self = Self::Flushdb;
  pub const GEOADD: Self = Self::Geoadd;
  pub const GEORADIUS: Self = Self::Georadius;
  pub const GEORADIUSBYMEMBER: Self = Self::Georadiusbymember;
  pub const GEOSEARCHSTORE: Self = Self::Geosearchstore;
  pub const GETDEL: Self = Self::Getdel;
  pub const GETEX: Self = Self::Getex;
  pub const GETSET: Self = Self::Getset;
  pub const HCOLLECT: Self = Self::Hcollect;
  pub const HDEL: Self = Self::Hdel;
  pub const HEXPIRE: Self = Self::Hexpire;
  pub const HPEXPIRE: Self = Self::Hpexpire;
  pub const HEXPIREAT: Self = Self::Hexpireat;
  pub const HPEXPIREAT: Self = Self::Hpexpireat;
  pub const HPERSIST: Self = Self::Hpersist;
  pub const HINCRBY: Self = Self::Hincrby;
  pub const HINCRBYFLOAT: Self = Self::Hincrbyfloat;
  pub const HMSET: Self = Self::Hmset;
  pub const HSET: Self = Self::Hset;
  pub const HSETNX: Self = Self::Hsetnx;
  pub const INCR: Self = Self::Incr;
  pub const INCRBY: Self = Self::Incrby;
  pub const INCRBYFLOAT: Self = Self::Incrbyfloat;
  pub const LINSERT: Self = Self::Linsert;
  pub const LMOVE: Self = Self::Lmove;
  pub const LMPOP: Self = Self::Lmpop;
  pub const LPOP: Self = Self::Lpop;
  pub const LPUSH: Self = Self::Lpush;
  pub const LPUSHX: Self = Self::Lpushx;
  pub const LREM: Self = Self::Lrem;
  pub const LSET: Self = Self::Lset;
  pub const LTRIM: Self = Self::Ltrim;
  pub const BLPOP: Self = Self::Blpop;
  pub const BRPOP: Self = Self::Brpop;
  pub const BLMOVE: Self = Self::Blmove;
  pub const BRPOPLPUSH: Self = Self::Brpoplpush;
  pub const BLMPOP: Self = Self::Blmpop;
  pub const MIGRATE: Self = Self::Migrate;
  pub const MSET: Self = Self::Mset;
  pub const MSETNX: Self = Self::Msetnx;
  pub const PERSIST: Self = Self::Persist;
  pub const PEXPIRE: Self = Self::Pexpire;
  pub const PEXPIREAT: Self = Self::Pexpireat;
  pub const PFADD: Self = Self::Pfadd;
  pub const PFMERGE: Self = Self::Pfmerge;
  pub const PSETEX: Self = Self::Psetex;
  pub const RENAME: Self = Self::Rename;
  pub const RICREATE: Self = Self::Ricreate;
  pub const RIDEL: Self = Self::Ridel;
  pub const RIPROMOTE: Self = Self::Ripromote;
  pub const RIRESTORE: Self = Self::Rirestore;
  pub const RISET: Self = Self::Riset;
  pub const RESTORE: Self = Self::Restore;
  pub const RENAMENX: Self = Self::Renamenx;
  pub const RPOP: Self = Self::Rpop;
  pub const RPOPLPUSH: Self = Self::Rpoplpush;
  pub const RPUSH: Self = Self::Rpush;
  pub const RPUSHX: Self = Self::Rpushx;
  pub const SADD: Self = Self::Sadd;
  pub const SDIFFSTORE: Self = Self::Sdiffstore;
  pub const SET: Self = Self::Set;
  pub const SETBIT: Self = Self::Setbit;
  pub const SETEX: Self = Self::Setex;
  pub const SETEXNX: Self = Self::Setexnx;
  pub const SETEXXX: Self = Self::Setexxx;
  pub const SETNX: Self = Self::Setnx;
  pub const SETIFMATCH: Self = Self::Setifmatch;
  pub const SETIFGREATER: Self = Self::Setifgreater;
  pub const SETWITHETAG: Self = Self::Setwithetag;
  pub const SETKEEPTTL: Self = Self::Setkeepttl;
  pub const SETKEEPTTLXX: Self = Self::Setkeepttlxx;
  pub const SETRANGE: Self = Self::Setrange;
  pub const SINTERSTORE: Self = Self::Sinterstore;
  pub const SMOVE: Self = Self::Smove;
  pub const SPOP: Self = Self::Spop;
  pub const SREM: Self = Self::Srem;
  pub const SUNIONSTORE: Self = Self::Sunionstore;
  pub const SWAPDB: Self = Self::Swapdb;
  pub const UNLINK: Self = Self::Unlink;
  pub const VADD: Self = Self::Vadd;
  pub const VREM: Self = Self::Vrem;
  pub const VSETATTR: Self = Self::Vsetattr;
  pub const ZADD: Self = Self::Zadd;
  pub const ZCOLLECT: Self = Self::Zcollect;
  pub const ZDIFFSTORE: Self = Self::Zdiffstore;
  pub const ZEXPIRE: Self = Self::Zexpire;
  pub const ZPEXPIRE: Self = Self::Zpexpire;
  pub const ZEXPIREAT: Self = Self::Zexpireat;
  pub const ZPEXPIREAT: Self = Self::Zpexpireat;
  pub const ZPERSIST: Self = Self::Zpersist;
  pub const ZINCRBY: Self = Self::Zincrby;
  pub const ZMPOP: Self = Self::Zmpop;
  pub const ZINTERSTORE: Self = Self::Zinterstore;
  pub const ZPOPMAX: Self = Self::Zpopmax;
  pub const ZPOPMIN: Self = Self::Zpopmin;
  pub const ZRANGESTORE: Self = Self::Zrangestore;
  pub const ZREM: Self = Self::Zrem;
  pub const ZREMRANGEBYLEX: Self = Self::Zremrangebylex;
  pub const ZREMRANGEBYRANK: Self = Self::Zremrangebyrank;
  pub const ZREMRANGEBYSCORE: Self = Self::Zremrangebyscore;
  pub const ZUNIONSTORE: Self = Self::Zunionstore;
  pub const BITOP: Self = Self::Bitop;
  pub const BITOP_AND: Self = Self::BitopAnd;
  pub const BITOP_OR: Self = Self::BitopOr;
  pub const BITOP_XOR: Self = Self::BitopXor;
  pub const BITOP_NOT: Self = Self::BitopNot;
  pub const BITOP_DIFF: Self = Self::BitopDiff;
  pub const BITCOUNT: Self = Self::Bitcount;
  pub const BITFIELD_RO: Self = Self::BitfieldRo;
  pub const BITPOS: Self = Self::Bitpos;
  pub const COSCAN: Self = Self::Coscan;
  pub const DBSIZE: Self = Self::Dbsize;
  pub const DUMP: Self = Self::Dump;
  pub const EXISTS: Self = Self::Exists;
  pub const EXPIRETIME: Self = Self::Expiretime;
  pub const GEODIST: Self = Self::Geodist;
  pub const GEOHASH: Self = Self::Geohash;
  pub const GEOPOS: Self = Self::Geopos;
  pub const GEORADIUS_RO: Self = Self::GeoradiusRo;
  pub const GEORADIUSBYMEMBER_RO: Self = Self::GeoradiusbymemberRo;
  pub const GEOSEARCH: Self = Self::Geosearch;
  pub const GET: Self = Self::Get;
  pub const GETBIT: Self = Self::Getbit;
  pub const GETIFNOTMATCH: Self = Self::Getifnotmatch;
  pub const GETRANGE: Self = Self::Getrange;
  pub const GETWITHETAG: Self = Self::Getwithetag;
  pub const HEXISTS: Self = Self::Hexists;
  pub const HGET: Self = Self::Hget;
  pub const HGETALL: Self = Self::Hgetall;
  pub const HKEYS: Self = Self::Hkeys;
  pub const HLEN: Self = Self::Hlen;
  pub const HMGET: Self = Self::Hmget;
  pub const HRANDFIELD: Self = Self::Hrandfield;
  pub const HSCAN: Self = Self::Hscan;
  pub const HSTRLEN: Self = Self::Hstrlen;
  pub const HVALS: Self = Self::Hvals;
  pub const KEYS: Self = Self::Keys;
  pub const LCS: Self = Self::Lcs;
  pub const HTTL: Self = Self::Httl;
  pub const HPTTL: Self = Self::Hpttl;
  pub const HEXPIRETIME: Self = Self::Hexpiretime;
  pub const HPEXPIRETIME: Self = Self::Hpexpiretime;
  pub const LINDEX: Self = Self::Lindex;
  pub const LLEN: Self = Self::Llen;
  pub const LPOS: Self = Self::Lpos;
  pub const LRANGE: Self = Self::Lrange;
  pub const MEMORY_USAGE: Self = Self::MemoryUsage;
  pub const MGET: Self = Self::Mget;
  pub const OBJECT_ENCODING: Self = Self::ObjectEncoding;
  pub const OBJECT_FREQ: Self = Self::ObjectFreq;
  pub const OBJECT_IDLETIME: Self = Self::ObjectIdletime;
  pub const OBJECT_REFCOUNT: Self = Self::ObjectRefcount;
  pub const PEXPIRETIME: Self = Self::Pexpiretime;
  pub const PFCOUNT: Self = Self::Pfcount;
  pub const PTTL: Self = Self::Pttl;
  pub const SCAN: Self = Self::Scan;
  pub const SCARD: Self = Self::Scard;
  pub const SDIFF: Self = Self::Sdiff;
  pub const SINTER: Self = Self::Sinter;
  pub const SINTERCARD: Self = Self::Sintercard;
  pub const SISMEMBER: Self = Self::Sismember;
  pub const SMEMBERS: Self = Self::Smembers;
  pub const SMISMEMBER: Self = Self::Smismember;
  pub const SPUBLISH: Self = Self::Spublish;
  pub const SRANDMEMBER: Self = Self::Srandmember;
  pub const SSCAN: Self = Self::Sscan;
  pub const SSUBSCRIBE: Self = Self::Ssubscribe;
  pub const STRLEN: Self = Self::Strlen;
  pub const SUBSTR: Self = Self::Substr;
  pub const SUNION: Self = Self::Sunion;
  pub const TTL: Self = Self::Ttl;
  pub const TYPE: Self = Self::Type;
  pub const VCARD: Self = Self::Vcard;
  pub const VDIM: Self = Self::Vdim;
  pub const VEMB: Self = Self::Vemb;
  pub const VGETATTR: Self = Self::Vgetattr;
  pub const VINFO: Self = Self::Vinfo;
  pub const VISMEMBER: Self = Self::Vismember;
  pub const VLINKS: Self = Self::Vlinks;
  pub const VRANDMEMBER: Self = Self::Vrandmember;
  pub const VSIM: Self = Self::Vsim;
  pub const WATCH: Self = Self::Watch;
  pub const WATCHMS: Self = Self::Watchms;
  pub const WATCHOS: Self = Self::Watchos;
  pub const ZCARD: Self = Self::Zcard;
  pub const ZCOUNT: Self = Self::Zcount;
  pub const ZDIFF: Self = Self::Zdiff;
  pub const ZINTER: Self = Self::Zinter;
  pub const ZINTERCARD: Self = Self::Zintercard;
  pub const ZLEXCOUNT: Self = Self::Zlexcount;
  pub const ZMSCORE: Self = Self::Zmscore;
  pub const ZRANDMEMBER: Self = Self::Zrandmember;
  pub const ZRANGE: Self = Self::Zrange;
  pub const ZRANGEBYLEX: Self = Self::Zrangebylex;
  pub const ZRANGEBYSCORE: Self = Self::Zrangebyscore;
  pub const ZRANK: Self = Self::Zrank;
  pub const ZREVRANGE: Self = Self::Zrevrange;
  pub const ZREVRANGEBYLEX: Self = Self::Zrevrangebylex;
  pub const ZREVRANGEBYSCORE: Self = Self::Zrevrangebyscore;
  pub const ZREVRANK: Self = Self::Zrevrank;
  pub const ZTTL: Self = Self::Zttl;
  pub const ZPTTL: Self = Self::Zpttl;
  pub const ZEXPIRETIME: Self = Self::Zexpiretime;
  pub const ZPEXPIRETIME: Self = Self::Zpexpiretime;
  pub const ZSCAN: Self = Self::Zscan;
  pub const ZSCORE: Self = Self::Zscore;
  pub const ZUNION: Self = Self::Zunion;
  pub const RICONFIG: Self = Self::Riconfig;
  pub const RIEXISTS: Self = Self::Riexists;
  pub const RIGET: Self = Self::Riget;
  pub const RIMETRICS: Self = Self::Rimetrics;
  pub const RIRANGE: Self = Self::Rirange;
  pub const RISCAN: Self = Self::Riscan;
  pub const EVAL: Self = Self::Eval;
  pub const EVALSHA: Self = Self::Evalsha;
  pub const ASYNC: Self = Self::Async;
  pub const PING: Self = Self::Ping;
  pub const PUBSUB: Self = Self::Pubsub;
  pub const PUBSUB_CHANNELS: Self = Self::PubsubChannels;
  pub const PUBSUB_NUMPAT: Self = Self::PubsubNumpat;
  pub const PUBSUB_NUMSUB: Self = Self::PubsubNumsub;
  pub const PUBLISH: Self = Self::Publish;
  pub const SUBSCRIBE: Self = Self::Subscribe;
  pub const PSUBSCRIBE: Self = Self::Psubscribe;
  pub const UNSUBSCRIBE: Self = Self::Unsubscribe;
  pub const PUNSUBSCRIBE: Self = Self::Punsubscribe;
  pub const ASKING: Self = Self::Asking;
  pub const SELECT: Self = Self::Select;
  pub const ECHO: Self = Self::Echo;
  pub const CLIENT: Self = Self::Client;
  pub const CLIENT_ID: Self = Self::ClientId;
  pub const CLIENT_INFO: Self = Self::ClientInfo;
  pub const CLIENT_LIST: Self = Self::ClientList;
  pub const CLIENT_KILL: Self = Self::ClientKill;
  pub const CLIENT_GETNAME: Self = Self::ClientGetname;
  pub const CLIENT_SETNAME: Self = Self::ClientSetname;
  pub const CLIENT_SETINFO: Self = Self::ClientSetinfo;
  pub const CLIENT_UNBLOCK: Self = Self::ClientUnblock;
  pub const MONITOR: Self = Self::Monitor;
  pub const MODULE: Self = Self::Module;
  pub const MODULE_LOADCS: Self = Self::ModuleLoadcs;
  pub const REGISTERCS: Self = Self::Registercs;
  pub const MULTI: Self = Self::Multi;
  pub const EXEC: Self = Self::Exec;
  pub const DISCARD: Self = Self::Discard;
  pub const UNWATCH: Self = Self::Unwatch;
  pub const RUNTXP: Self = Self::Runtxp;
  pub const READONLY: Self = Self::Readonly;
  pub const READWRITE: Self = Self::Readwrite;
  pub const REPLICAOF: Self = Self::Replicaof;
  pub const SECONDARYOF: Self = Self::Secondaryof;
  pub const INFO: Self = Self::Info;
  pub const TIME: Self = Self::Time;
  pub const ROLE: Self = Self::Role;
  pub const SAVE: Self = Self::Save;
  pub const EXPDELSCAN: Self = Self::Expdelscan;
  pub const LASTSAVE: Self = Self::Lastsave;
  pub const BGSAVE: Self = Self::Bgsave;
  pub const COMMITAOF: Self = Self::Commitaof;
  pub const FAILOVER: Self = Self::Failover;
  pub const CUSTOM_TXN: Self = Self::CustomTxn;
  pub const CUSTOM_RAW_STRING_CMD: Self = Self::CustomRawStringCmd;
  pub const CUSTOM_OBJ_CMD: Self = Self::CustomObjCmd;
  pub const CUSTOM_PROCEDURE: Self = Self::CustomProcedure;
  pub const SCRIPT: Self = Self::Script;
  pub const SCRIPT_EXISTS: Self = Self::ScriptExists;
  pub const SCRIPT_FLUSH: Self = Self::ScriptFlush;
  pub const SCRIPT_LOAD: Self = Self::ScriptLoad;
  pub const ACL: Self = Self::Acl;
  pub const ACL_CAT: Self = Self::AclCat;
  pub const ACL_DELUSER: Self = Self::AclDeluser;
  pub const ACL_GENPASS: Self = Self::AclGenpass;
  pub const ACL_GETUSER: Self = Self::AclGetuser;
  pub const ACL_LIST: Self = Self::AclList;
  pub const ACL_LOAD: Self = Self::AclLoad;
  pub const ACL_SAVE: Self = Self::AclSave;
  pub const ACL_SETUSER: Self = Self::AclSetuser;
  pub const ACL_USERS: Self = Self::AclUsers;
  pub const ACL_WHOAMI: Self = Self::AclWhoami;
  pub const COMMAND: Self = Self::Command;
  pub const COMMAND_COUNT: Self = Self::CommandCount;
  pub const COMMAND_DOCS: Self = Self::CommandDocs;
  pub const COMMAND_INFO: Self = Self::CommandInfo;
  pub const COMMAND_GETKEYS: Self = Self::CommandGetkeys;
  pub const COMMAND_GETKEYSANDFLAGS: Self = Self::CommandGetkeysandflags;
  pub const MEMORY: Self = Self::Memory;
  pub const OBJECT: Self = Self::Object;
  pub const OBJECT_HELP: Self = Self::ObjectHelp;
  pub const CONFIG: Self = Self::Config;
  pub const CONFIG_GET: Self = Self::ConfigGet;
  pub const CONFIG_REWRITE: Self = Self::ConfigRewrite;
  pub const CONFIG_SET: Self = Self::ConfigSet;
  pub const DEBUG: Self = Self::Debug;
  pub const LATENCY: Self = Self::Latency;
  pub const LATENCY_HELP: Self = Self::LatencyHelp;
  pub const LATENCY_HISTOGRAM: Self = Self::LatencyHistogram;
  pub const LATENCY_RESET: Self = Self::LatencyReset;
  pub const SLOWLOG: Self = Self::Slowlog;
  pub const SLOWLOG_HELP: Self = Self::SlowlogHelp;
  pub const SLOWLOG_LEN: Self = Self::SlowlogLen;
  pub const SLOWLOG_GET: Self = Self::SlowlogGet;
  pub const SLOWLOG_RESET: Self = Self::SlowlogReset;
  pub const CLUSTER: Self = Self::Cluster;
  pub const CLUSTER_ADDSLOTS: Self = Self::ClusterAddslots;
  pub const CLUSTER_ADDSLOTSRANGE: Self = Self::ClusterAddslotsrange;
  pub const CLUSTER_ADVANCE_TIME: Self = Self::ClusterAdvanceTime;
  pub const CLUSTER_APPENDLOG: Self = Self::ClusterAppendlog;
  pub const CLUSTER_ATTACH_SYNC: Self = Self::ClusterAttachSync;
  pub const CLUSTER_BANLIST: Self = Self::ClusterBanlist;
  pub const CLUSTER_BEGIN_REPLICA_RECOVER: Self = Self::ClusterBeginReplicaRecover;
  pub const CLUSTER_BUMPEPOCH: Self = Self::ClusterBumpepoch;
  pub const CLUSTER_COUNTKEYSINSLOT: Self = Self::ClusterCountkeysinslot;
  pub const CLUSTER_DELKEYSINSLOT: Self = Self::ClusterDelkeysinslot;
  pub const CLUSTER_DELKEYSINSLOTRANGE: Self = Self::ClusterDelkeysinslotrange;
  pub const CLUSTER_DELSLOTS: Self = Self::ClusterDelslots;
  pub const CLUSTER_DELSLOTSRANGE: Self = Self::ClusterDelslotsrange;
  pub const CLUSTER_ENDPOINT: Self = Self::ClusterEndpoint;
  pub const CLUSTER_FAILOVER: Self = Self::ClusterFailover;
  pub const CLUSTER_FAILREPLICATIONOFFSET: Self = Self::ClusterFailreplicationoffset;
  pub const CLUSTER_FAILSTOPWRITES: Self = Self::ClusterFailstopwrites;
  pub const CLUSTER_FLUSHALL: Self = Self::ClusterFlushall;
  pub const CLUSTER_FORGET: Self = Self::ClusterForget;
  pub const CLUSTER_GETKEYSINSLOT: Self = Self::ClusterGetkeysinslot;
  pub const CLUSTER_GOSSIP: Self = Self::ClusterGossip;
  pub const CLUSTER_HELP: Self = Self::ClusterHelp;
  pub const CLUSTER_INFO: Self = Self::ClusterInfo;
  pub const CLUSTER_INITIATE_REPLICA_SYNC: Self = Self::ClusterInitiateReplicaSync;
  pub const CLUSTER_KEYSLOT: Self = Self::ClusterKeyslot;
  pub const CLUSTER_MEET: Self = Self::ClusterMeet;
  pub const CLUSTER_MIGRATE: Self = Self::ClusterMigrate;
  pub const CLUSTER_MLOG_KEY_TIME: Self = Self::ClusterMlogKeyTime;
  pub const CLUSTER_MTASKS: Self = Self::ClusterMtasks;
  pub const CLUSTER_MYID: Self = Self::ClusterMyid;
  pub const CLUSTER_MYPARENTID: Self = Self::ClusterMyparentid;
  pub const CLUSTER_NODES: Self = Self::ClusterNodes;
  pub const CLUSTER_PUBLISH: Self = Self::ClusterPublish;
  pub const CLUSTER_SPUBLISH: Self = Self::ClusterSpublish;
  pub const CLUSTER_REPLICAS: Self = Self::ClusterReplicas;
  pub const CLUSTER_REPLICATE: Self = Self::ClusterReplicate;
  pub const CLUSTER_RESERVE: Self = Self::ClusterReserve;
  pub const CLUSTER_RESET: Self = Self::ClusterReset;
  pub const CLUSTER_SEND_CKPT_FILE_SEGMENT: Self = Self::ClusterSendCkptFileSegment;
  pub const CLUSTER_SEND_CKPT_METADATA: Self = Self::ClusterSendCkptMetadata;
  pub const CLUSTER_SETCONFIGEPOCH: Self = Self::ClusterSetconfigepoch;
  pub const CLUSTER_SETSLOT: Self = Self::ClusterSetslot;
  pub const CLUSTER_SETSLOTSRANGE: Self = Self::ClusterSetslotsrange;
  pub const CLUSTER_SHARDS: Self = Self::ClusterShards;
  pub const CLUSTER_SLOTS: Self = Self::ClusterSlots;
  pub const CLUSTER_SLOTSTATE: Self = Self::ClusterSlotstate;
  pub const CLUSTER_SNAPSHOT_DATA: Self = Self::ClusterSnapshotData;
  pub const CLUSTER_SYNC: Self = Self::ClusterSync;
  pub const AUTH: Self = Self::Auth;
  pub const HELLO: Self = Self::Hello;
  pub const QUIT: Self = Self::Quit;
  pub const LAST_VALID_COMMAND: Self = Self::Quit;
  pub const INVALID: Self = Self::Invalid;
}

// 命令区间界限常量
pub const FIRST_WRITE_COMMAND: u16 = RespCommand::Append as u16;
pub const LAST_WRITE_COMMAND: u16 = RespCommand::BitopDiff as u16;
pub const FIRST_READ_COMMAND: u16 = LAST_WRITE_COMMAND + 1;
pub const LAST_READ_COMMAND: u16 = RespCommand::Riscan as u16;
pub const FIRST_DATA_COMMAND: u16 = FIRST_WRITE_COMMAND;
pub const LAST_DATA_COMMAND: u16 = RespCommand::Evalsha as u16;
pub const FIRST_NO_AUTH: u16 = RespCommand::Auth as u16;
pub const LAST_NO_AUTH: u16 = RespCommand::Quit as u16;
pub const FIRST_CLUSTER_SUB: u16 = RespCommand::ClusterAddslots as u16;
pub const LAST_CLUSTER_SUB: u16 = RespCommand::ClusterSync as u16;
pub const LAST_VALID_COMMAND: RespCommand = RespCommand::Quit;

const EXPANDED_SET: [RespCommand; 4] = [
  RespCommand::Setexnx,
  RespCommand::Setexxx,
  RespCommand::Setkeepttl,
  RespCommand::Setkeepttlxx,
];

const EXPANDED_BITOP: [RespCommand; 5] = [
  RespCommand::BitopAnd,
  RespCommand::BitopNot,
  RespCommand::BitopOr,
  RespCommand::BitopXor,
  RespCommand::BitopDiff,
];

impl RespCommand {
  /// 命令的 Redis 语义 arity（token 计数含命令名，对标 Garnet RespCommandsInfo.Arity）
  ///
  /// - 正值：token 总数必须恰好等于 arity；
  /// - 负值：token 总数至少为 `-arity`；
  /// - 0：本实现未定义 arity（可变参数或非数据管理命令），入队期跳过校验。
  ///
  /// 仅覆盖协议规范明确定义参数个数的命令；对实现子集尚未支持的变体一律保守
  /// 返回 0，杜绝误拒合法报文。
  #[inline]
  pub const fn arity(&self) -> i16 {
    match self {
      // 连接与控制
      Self::Auth => -2,
      Self::Echo => 2,
      Self::Ping => -1,
      Self::Select => 2,
      Self::Quit => 1,
      Self::Info => -1,
      Self::Time => 1,
      // 事务
      Self::Multi | Self::Exec | Self::Discard | Self::Unwatch => 1,
      Self::Watch => -2,
      // 键与字符串
      Self::Append | Self::Getset | Self::Incrby | Self::Decrby | Self::Incrbyfloat => 3,
      Self::Get
      | Self::Getdel
      | Self::Strlen
      | Self::Incr
      | Self::Decr
      | Self::Persist
      | Self::Ttl
      | Self::Pttl
      | Self::Expiretime
      | Self::Pexpiretime
      | Self::Type
      | Self::Dump => 2,
      Self::Set => -3,
      Self::Setex | Self::Psetex => 4,
      Self::Setnx | Self::Getifnotmatch => 3,
      Self::Getex => -2,
      Self::Getbit => 3,
      Self::Getrange => 4,
      Self::Setrange => 4,
      Self::Setbit => 4,
      Self::Del | Self::Unlink | Self::Exists | Self::Mget | Self::Bitcount => -2,
      Self::Bitpos => -3,
      Self::Mset | Self::Msetnx => -3,
      Self::Expire | Self::Pexpire | Self::Expireat | Self::Pexpireat => -3,
      // 哈希
      Self::Hset | Self::Hsetnx | Self::Hmset | Self::Hincrby | Self::Hincrbyfloat => -4,
      Self::Hget | Self::Hexists | Self::Hstrlen => 3,
      Self::Hmget | Self::Hdel | Self::Hscan => -2,
      Self::Hgetall | Self::Hkeys | Self::Hvals | Self::Hlen => 2,
      // 列表
      Self::Lpush | Self::Rpush | Self::Lpushx | Self::Rpushx | Self::Lrem => -3,
      Self::Lpop | Self::Rpop => -2,
      Self::Llen => 2,
      Self::Lrange => 4,
      Self::Lindex => 3,
      Self::Lset | Self::Ltrim => 4,
      Self::Rpoplpush | Self::Brpoplpush => 3,
      Self::Lmove | Self::Blmove => 5,
      Self::Lpos => -3,
      // 集合
      Self::Sadd | Self::Srem | Self::Smismember => -3,
      Self::Smembers | Self::Scard => 2,
      Self::Sismember => 3,
      Self::Spop | Self::Srandmember | Self::Sscan => -2,
      Self::Sdiff | Self::Sinter | Self::Sunion => -2,
      Self::Sdiffstore | Self::Sinterstore | Self::Sunionstore => -3,
      Self::Smove => 4,
      Self::Sintercard => -3,
      // 有序集合
      Self::Zadd | Self::Zrangebyscore | Self::Zrangebylex | Self::Zrangestore => -4,
      Self::Zrem => -3,
      Self::Zscore => 3,
      Self::Zcard => 2,
      Self::Zcount | Self::Zlexcount | Self::Zrevrange => 4,
      Self::Zrange => -4,
      Self::Zrank | Self::Zrevrank => -3,
      Self::Zincrby => 4,
      Self::Zpopmin | Self::Zpopmax => -2,
      Self::Zdiff | Self::Zinter | Self::Zunion => -3,
      Self::Zdiffstore | Self::Zinterstore | Self::Zunionstore => -4,
      Self::Zintercard => -3,
      Self::Zscan | Self::Zmscore => -2,
      // HyperLogLog
      Self::Pfadd | Self::Pfmerge => -2,
      Self::Pfcount => -2,
      // 发布订阅
      Self::Subscribe | Self::Psubscribe => -2,
      Self::Unsubscribe | Self::Punsubscribe => -1,
      Self::Publish => 3,
      Self::Pubsub => -2,
      // 脚本与多键操作
      Self::Eval | Self::Evalsha => -3,
      Self::Blpop | Self::Brpop | Self::Bzpopmin | Self::Bzpopmax => -3,
      Self::Blmpop | Self::Bzmpop => -5,
      Self::Lmpop | Self::Zmpop => -4,
      Self::Bitop => -4,
      Self::Migrate => -6,
      Self::Rename | Self::Renamenx => 3,
      Self::Keys => 2,
      Self::Scan => -2,
      // 管理
      Self::Flushdb | Self::Flushall => -1,
      Self::Swapdb => 3,
      Self::Dbsize => 1,
      // 其余可变参数 / 子命令 / 未实现变体：不做 arity 校验
      _ => 0,
    }
  }

  /// 入队期 arity 校验（对齐 Garnet MultiProcessCommand 的入队校验时机）
  ///
  /// `args_len` 为除去命令名后的参数个数；内部补 1 对齐含命令名的 token 计数口径。
  /// arity 未定义（0）时恒放行。
  #[inline]
  pub const fn check_arity(&self, args_len: usize) -> bool {
    let arity = self.arity();
    if arity == 0 {
      return true;
    }
    // 参数上限受协议解析器 multibulk 元素数约束，远小于 i32 溢出边界
    let tokens = args_len as i32 + 1;
    if arity > 0 {
      tokens == arity as i32
    } else {
      tokens >= -(arity as i32)
    }
  }

  /// 是否为写入修改命令
  #[inline(always)]
  pub const fn is_write(&self) -> bool {
    (*self as u16).wrapping_sub(FIRST_WRITE_COMMAND) <= (LAST_WRITE_COMMAND - FIRST_WRITE_COMMAND)
  }

  /// 是否为只读查询命令
  #[inline(always)]
  pub const fn is_readonly(&self) -> bool {
    (*self as u16).wrapping_sub(FIRST_READ_COMMAND) <= (LAST_READ_COMMAND - FIRST_READ_COMMAND)
  }

  /// 若为写入修改命令则返回 1，否则返回 0（无分支位运算，对标 Garnet OneIfWrite）
  #[inline(always)]
  pub const fn one_if_write(&self) -> u64 {
    if self.is_write() { 1 } else { 0 }
  }

  /// 若为只读查询命令则返回 1，否则返回 0（无分支位运算，对标 Garnet OneIfRead）
  #[inline(always)]
  pub const fn one_if_read(&self) -> u64 {
    if self.is_readonly() { 1 } else { 0 }
  }

  /// 是否为数据操作命令（包括读、写、脚本）
  #[inline]
  pub const fn is_data(&self) -> bool {
    match self {
      Self::Migrate
      | Self::Dbsize
      | Self::MemoryUsage
      | Self::Flushdb
      | Self::Flushall
      | Self::Keys
      | Self::Scan
      | Self::Swapdb => false,
      _ => {
        let val = *self as u16;
        val >= FIRST_DATA_COMMAND && val <= LAST_DATA_COMMAND
      }
    }
  }

  /// 未认证状态下是否允许执行
  #[inline(always)]
  pub const fn is_no_auth(&self) -> bool {
    (*self as u16).wrapping_sub(FIRST_NO_AUTH) <= (LAST_NO_AUTH - FIRST_NO_AUTH)
  }

  /// 是否为集群子命令
  #[inline(always)]
  pub const fn is_cluster_subcommand(&self) -> bool {
    (*self as u16).wrapping_sub(FIRST_CLUSTER_SUB) <= (LAST_CLUSTER_SUB - FIRST_CLUSTER_SUB)
  }

  /// 是否可在 RangeIndex 键上合法操作
  #[inline]
  pub const fn is_legal_on_range_index(&self) -> bool {
    matches!(
      self,
      Self::Del
        | Self::Unlink
        | Self::Type
        | Self::Debug
        | Self::Rename
        | Self::Renamenx
        | Self::Ricreate
        | Self::Ripromote
        | Self::Rirestore
        | Self::Riset
        | Self::Riget
        | Self::Ridel
        | Self::Riscan
        | Self::Rirange
        | Self::Riexists
        | Self::Riconfig
        | Self::Rimetrics
    )
  }

  /// 是否为专属 RangeIndex 命令
  #[inline]
  pub const fn is_range_index_command(&self) -> bool {
    matches!(
      self,
      Self::Ricreate
        | Self::Riset
        | Self::Riget
        | Self::Ridel
        | Self::Riscan
        | Self::Rirange
        | Self::Ripromote
        | Self::Rirestore
        | Self::Riexists
        | Self::Riconfig
        | Self::Rimetrics
    )
  }

  /// 是否为专属 VectorSet 命令
  #[inline]
  pub const fn is_vector_set_command(&self) -> bool {
    matches!(
      self,
      Self::Vadd
        | Self::Vcard
        | Self::Vdim
        | Self::Vemb
        | Self::Vgetattr
        | Self::Vinfo
        | Self::Vismember
        | Self::Vlinks
        | Self::Vrandmember
        | Self::Vrem
        | Self::Vsetattr
        | Self::Vsim
    )
  }

  /// 是否允许在 VectorSet 上操作
  #[inline]
  pub const fn is_legal_on_vector_set(&self) -> bool {
    matches!(
      self,
      Self::Del
        | Self::Unlink
        | Self::Type
        | Self::Debug
        | Self::Rename
        | Self::Renamenx
        | Self::Vadd
        | Self::Vcard
        | Self::Vdim
        | Self::Vemb
        | Self::Vgetattr
        | Self::Vinfo
        | Self::Vismember
        | Self::Vlinks
        | Self::Vrandmember
        | Self::Vrem
        | Self::Vsetattr
        | Self::Vsim
    )
  }

  /// 处于发布订阅订阅模式下时是否允许执行
  #[inline]
  pub const fn is_allowed_in_subscription_mode(&self) -> bool {
    matches!(
      self,
      Self::Subscribe
        | Self::Unsubscribe
        | Self::Psubscribe
        | Self::Punsubscribe
        | Self::Ssubscribe
        | Self::Ping
        | Self::Quit
    )
  }

  /// ACL 规范化（将衍生子命令归一为主命令）
  #[inline]
  pub const fn normalize_for_acls(&self) -> Self {
    match self {
      Self::Setexnx | Self::Setexxx | Self::Setkeepttl | Self::Setkeepttlxx => Self::Set,
      Self::BitopAnd | Self::BitopNot | Self::BitopOr | Self::BitopXor | Self::BitopDiff => {
        Self::Bitop
      }
      _ => *self,
    }
  }

  /// ACL 展开（获取该主命令所涵盖的所有衍生命令）
  #[inline]
  pub const fn expand_for_acls(&self) -> &'static [Self] {
    match self {
      Self::Set => &EXPANDED_SET,
      Self::Bitop => &EXPANDED_BITOP,
      _ => &[],
    }
  }

  /// 是否为 AOF 独立命令（静态应答或跨会话无 AOF 并发冲突的命令，对齐 Garnet AofIndependentCommands）
  #[inline]
  pub const fn is_aof_independent(&self) -> bool {
    matches!(
      self,
      Self::Async
        | Self::Ping
        | Self::Select
        | Self::Swapdb
        | Self::Echo
        | Self::Monitor
        | Self::ModuleLoadcs
        | Self::Registercs
        | Self::Info
        | Self::Time
        | Self::Lastsave
        | Self::AclCat
        | Self::AclDeluser
        | Self::AclGenpass
        | Self::AclGetuser
        | Self::AclList
        | Self::AclLoad
        | Self::AclSave
        | Self::AclSetuser
        | Self::AclUsers
        | Self::AclWhoami
        | Self::ClientId
        | Self::ClientInfo
        | Self::ClientList
        | Self::ClientKill
        | Self::ClientGetname
        | Self::ClientSetname
        | Self::ClientSetinfo
        | Self::ClientUnblock
        | Self::Command
        | Self::CommandCount
        | Self::CommandDocs
        | Self::CommandInfo
        | Self::CommandGetkeys
        | Self::CommandGetkeysandflags
        | Self::MemoryUsage
        | Self::ConfigGet
        | Self::ConfigRewrite
        | Self::ConfigSet
        | Self::LatencyHelp
        | Self::LatencyHistogram
        | Self::LatencyReset
        | Self::SlowlogHelp
        | Self::SlowlogLen
        | Self::SlowlogGet
        | Self::SlowlogReset
        | Self::Multi
    )
  }
}

impl RespCommand {
  /// 从字节切片查找主命令（忽略大小写，零堆分配，硬件 CRC32 加速）
  /// 返回 `Some((command, has_subcommands))`
  #[inline]
  pub fn lookup(name: &[u8]) -> Option<(Self, bool)> {
    if name.is_empty() || name.len() > 24 {
      return None;
    }

    // 1. 硬件 CRC32 主命令表快速路径（零哈希冲突单指令直达）
    let (cmd, has_sub) = lookup_primary(name);
    if cmd != Self::None {
      return Some((cmd, has_sub));
    }

    // 2. 检查是否有小写字母，单趟扫描就地转大写并探测
    let mut has_lower = false;
    let mut buf = [0u8; 24];
    for (i, &b) in name.iter().enumerate() {
      if b.is_ascii_lowercase() {
        has_lower = true;
        buf[i] = b - 0x20;
      } else {
        buf[i] = b;
      }
    }

    if has_lower {
      let upper = &buf[..name.len()];
      let (cmd, has_sub) = lookup_primary(upper);
      if cmd != Self::None {
        return Some((cmd, has_sub));
      }
    }

    None
  }

  /// 查找具有父命令的子命令（忽略大小写，零堆分配，硬件 CRC32 加速）
  #[inline]
  pub fn lookup_subcommand(parent: Self, sub_name: &[u8]) -> Option<Self> {
    if sub_name.is_empty() || sub_name.len() > 24 {
      return None;
    }

    // 1. 硬件 CRC32 子命令表快速路径
    let cmd = lookup_subcommand(parent, sub_name);
    if cmd != Self::None {
      return Some(cmd);
    }

    // 2. 检查是否有小写字母
    let mut has_lower = false;
    let mut buf = [0u8; 24];
    for (i, &b) in sub_name.iter().enumerate() {
      if b.is_ascii_lowercase() {
        has_lower = true;
        buf[i] = b - 0x20;
      } else {
        buf[i] = b;
      }
    }

    if has_lower {
      let upper = &buf[..sub_name.len()];
      let cmd = lookup_subcommand(parent, upper);
      if cmd != Self::None {
        return Some(cmd);
      }
    }

    None
  }

  /// 从字节切片快速解析命令（忽略大小写，不含子命令分支）
  #[inline]
  pub fn from_slice(name: &[u8]) -> Option<Self> {
    Self::lookup(name).map(|(cmd, _)| cmd)
  }
}

impl fmt::Debug for RespCommand {
  #[inline]
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}

impl fmt::Display for RespCommand {
  #[inline]
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.write_str(self.as_str())
  }
}
