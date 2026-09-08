//! RESP 命令字与修饰参数常量定义（1:1 对标 Microsoft Garnet CmdStrings）

// ====== 键值与字符串命令 ======
pub const GET: &[u8] = b"GET";
pub const SET: &[u8] = b"SET";
pub const DEL: &[u8] = b"DEL";
pub const MGET: &[u8] = b"MGET";
pub const MSET: &[u8] = b"MSET";
pub const MSETNX: &[u8] = b"MSETNX";
pub const INCR: &[u8] = b"INCR";
pub const DECR: &[u8] = b"DECR";
pub const INCRBY: &[u8] = b"INCRBY";
pub const DECRBY: &[u8] = b"DECRBY";
pub const INCRBYFLOAT: &[u8] = b"INCRBYFLOAT";
pub const APPEND: &[u8] = b"APPEND";
pub const STRLEN: &[u8] = b"STRLEN";
pub const GETRANGE: &[u8] = b"GETRANGE";
pub const SETRANGE: &[u8] = b"SETRANGE";
pub const GETSET: &[u8] = b"GETSET";
pub const GETDEL: &[u8] = b"GETDEL";
pub const GETEX: &[u8] = b"GETEX";
pub const EXISTS: &[u8] = b"EXISTS";
pub const TYPE: &[u8] = b"TYPE";
pub const DBSIZE: &[u8] = b"DBSIZE";
pub const RENAME: &[u8] = b"RENAME";
pub const RENAMENX: &[u8] = b"RENAMENX";
pub const PERSIST: &[u8] = b"PERSIST";
pub const EXPIRE: &[u8] = b"EXPIRE";
pub const PEXPIRE: &[u8] = b"PEXPIRE";
pub const EXPIREAT: &[u8] = b"EXPIREAT";
pub const PEXPIREAT: &[u8] = b"PEXPIREAT";
pub const TTL: &[u8] = b"TTL";
pub const PTTL: &[u8] = b"PTTL";
pub const KEYS: &[u8] = b"KEYS";
pub const SCAN: &[u8] = b"SCAN";
pub const UNLINK: &[u8] = b"UNLINK";
pub const COPY: &[u8] = b"COPY";
pub const TOUCH: &[u8] = b"TOUCH";

// ====== 列表命令 ======
pub const LPUSH: &[u8] = b"LPUSH";
pub const RPUSH: &[u8] = b"RPUSH";
pub const LPUSHX: &[u8] = b"LPUSHX";
pub const RPUSHX: &[u8] = b"RPUSHX";
pub const LPOP: &[u8] = b"LPOP";
pub const RPOP: &[u8] = b"RPOP";
pub const LLEN: &[u8] = b"LLEN";
pub const LRANGE: &[u8] = b"LRANGE";
pub const LINDEX: &[u8] = b"LINDEX";
pub const LSET: &[u8] = b"LSET";
pub const LTRIM: &[u8] = b"LTRIM";
pub const LREM: &[u8] = b"LREM";
pub const LINSERT: &[u8] = b"LINSERT";
pub const LPOS: &[u8] = b"LPOS";
pub const LMOVE: &[u8] = b"LMOVE";
pub const LMPOP: &[u8] = b"LMPOP";
pub const RPOPLPUSH: &[u8] = b"RPOPLPUSH";

// ====== 阻塞集合命令 ======
pub const BLPOP: &[u8] = b"BLPOP";
pub const BRPOP: &[u8] = b"BRPOP";
pub const BLMOVE: &[u8] = b"BLMOVE";
pub const BLMPOP: &[u8] = b"BLMPOP";
pub const BRPOPLPUSH: &[u8] = b"BRPOPLPUSH";
pub const BZMPOP: &[u8] = b"BZMPOP";
pub const BZPOPMAX: &[u8] = b"BZPOPMAX";
pub const BZPOPMIN: &[u8] = b"BZPOPMIN";

// ====== 哈希字典命令 ======
pub const HSET: &[u8] = b"HSET";
pub const HGET: &[u8] = b"HGET";
pub const HMSET: &[u8] = b"HMSET";
pub const HMGET: &[u8] = b"HMGET";
pub const HDEL: &[u8] = b"HDEL";
pub const HLEN: &[u8] = b"HLEN";
pub const HEXISTS: &[u8] = b"HEXISTS";
pub const HKEYS: &[u8] = b"HKEYS";
pub const HVALS: &[u8] = b"HVALS";
pub const HGETALL: &[u8] = b"HGETALL";
pub const HINCRBY: &[u8] = b"HINCRBY";
pub const HINCRBYFLOAT: &[u8] = b"HINCRBYFLOAT";
pub const HSTRLEN: &[u8] = b"HSTRLEN";
pub const HSETNX: &[u8] = b"HSETNX";
pub const HRANDFIELD: &[u8] = b"HRANDFIELD";
pub const HEXPIRE: &[u8] = b"HEXPIRE";
pub const HPEXPIRE: &[u8] = b"HPEXPIRE";
pub const HEXPIREAT: &[u8] = b"HEXPIREAT";
pub const HPEXPIREAT: &[u8] = b"HPEXPIREAT";
pub const HTTL: &[u8] = b"HTTL";
pub const HPTTL: &[u8] = b"HPTTL";
pub const HPERSIST: &[u8] = b"HPERSIST";
pub const HSCAN: &[u8] = b"HSCAN";

// ====== 无序集合命令 ======
pub const SADD: &[u8] = b"SADD";
pub const SREM: &[u8] = b"SREM";
pub const SMEMBERS: &[u8] = b"SMEMBERS";
pub const SISMEMBER: &[u8] = b"SISMEMBER";
pub const SCARD: &[u8] = b"SCARD";
pub const SPOP: &[u8] = b"SPOP";
pub const SRANDMEMBER: &[u8] = b"SRANDMEMBER";
pub const SMOVE: &[u8] = b"SMOVE";
pub const SINTER: &[u8] = b"SINTER";
pub const SUNION: &[u8] = b"SUNION";
pub const SDIFF: &[u8] = b"SDIFF";
pub const SINTERSTORE: &[u8] = b"SINTERSTORE";
pub const SUNIONSTORE: &[u8] = b"SUNIONSTORE";
pub const SDIFFSTORE: &[u8] = b"SDIFFSTORE";
pub const SINTERCARD: &[u8] = b"SINTERCARD";
pub const SSCAN: &[u8] = b"SSCAN";

// ====== 有序集合命令 ======
pub const ZADD: &[u8] = b"ZADD";
pub const ZREM: &[u8] = b"ZREM";
pub const ZSCORE: &[u8] = b"ZSCORE";
pub const ZMSCORE: &[u8] = b"ZMSCORE";
pub const ZRANGE: &[u8] = b"ZRANGE";
pub const ZREVRANGE: &[u8] = b"ZREVRANGE";
pub const ZCARD: &[u8] = b"ZCARD";
pub const ZCOUNT: &[u8] = b"ZCOUNT";
pub const ZINCRBY: &[u8] = b"ZINCRBY";
pub const ZRANK: &[u8] = b"ZRANK";
pub const ZREVRANK: &[u8] = b"ZREVRANK";
pub const ZREMRANGEBYRANK: &[u8] = b"ZREMRANGEBYRANK";
pub const ZREMRANGEBYSCORE: &[u8] = b"ZREMRANGEBYSCORE";
pub const ZREMRANGEBYLEX: &[u8] = b"ZREMRANGEBYLEX";
pub const ZRANGEBYSCORE: &[u8] = b"ZRANGEBYSCORE";
pub const ZRANGEBYLEX: &[u8] = b"ZRANGEBYLEX";
pub const ZPOPMAX: &[u8] = b"ZPOPMAX";
pub const ZPOPMIN: &[u8] = b"ZPOPMIN";
pub const ZMPOP: &[u8] = b"ZMPOP";
pub const ZRANDMEMBER: &[u8] = b"ZRANDMEMBER";
pub const ZSCAN: &[u8] = b"ZSCAN";
pub const ZINTERSTORE: &[u8] = b"ZINTERSTORE";
pub const ZUNIONSTORE: &[u8] = b"ZUNIONSTORE";
pub const ZDIFFSTORE: &[u8] = b"ZDIFFSTORE";
pub const ZINTER: &[u8] = b"ZINTER";
pub const ZUNION: &[u8] = b"ZUNION";
pub const ZDIFF: &[u8] = b"ZDIFF";

// ====== 发布订阅命令 ======
pub const SUBSCRIBE: &[u8] = b"SUBSCRIBE";
pub const UNSUBSCRIBE: &[u8] = b"UNSUBSCRIBE";
pub const PUBLISH: &[u8] = b"PUBLISH";
pub const PSUBSCRIBE: &[u8] = b"PSUBSCRIBE";
pub const PUNSUBSCRIBE: &[u8] = b"PUNSUBSCRIBE";
pub const PUBSUB: &[u8] = b"PUBSUB";
pub const CHANNELS: &[u8] = b"CHANNELS";
pub const NUMSUB: &[u8] = b"NUMSUB";
pub const NUMPAT: &[u8] = b"NUMPAT";

// ====== 服务端 / 事务 / 客户端管理命令 ======
pub const PING: &[u8] = b"PING";
pub const ECHO: &[u8] = b"ECHO";
pub const AUTH: &[u8] = b"AUTH";
pub const QUIT: &[u8] = b"QUIT";
pub const SELECT: &[u8] = b"SELECT";
pub const INFO: &[u8] = b"INFO";
pub const CONFIG: &[u8] = b"CONFIG";
pub const CLIENT: &[u8] = b"CLIENT";
pub const MONITOR: &[u8] = b"MONITOR";
pub const COMMAND: &[u8] = b"COMMAND";
pub const MULTI: &[u8] = b"MULTI";
pub const EXEC: &[u8] = b"EXEC";
pub const DISCARD: &[u8] = b"DISCARD";
pub const WATCH: &[u8] = b"WATCH";
pub const UNWATCH: &[u8] = b"UNWATCH";
pub const RESET: &[u8] = b"RESET";
pub const FLUSHDB: &[u8] = b"FLUSHDB";
pub const FLUSHALL: &[u8] = b"FLUSHALL";
pub const TIME: &[u8] = b"TIME";
pub const ROLE: &[u8] = b"ROLE";
pub const ACL: &[u8] = b"ACL";
pub const CLUSTER: &[u8] = b"CLUSTER";
pub const FAILOVER: &[u8] = b"FAILOVER";
pub const REPLICAOF: &[u8] = b"REPLICAOF";
pub const SLAVEOF: &[u8] = b"SLAVEOF";

// ====== 参数选项与修饰关键字 ======
pub const NX: &[u8] = b"NX";
pub const XX: &[u8] = b"XX";
pub const EX: &[u8] = b"EX";
pub const PX: &[u8] = b"PX";
pub const EXAT: &[u8] = b"EXAT";
pub const PXAT: &[u8] = b"PXAT";
pub const KEEPTTL: &[u8] = b"KEEPTTL";
pub const CH: &[u8] = b"CH";
pub const INCR_OPT: &[u8] = b"INCR";

pub const LEFT: &[u8] = b"LEFT";
pub const RIGHT: &[u8] = b"RIGHT";
pub const BEFORE: &[u8] = b"BEFORE";
pub const AFTER: &[u8] = b"AFTER";

pub const WITHSCORES: &[u8] = b"WITHSCORES";
pub const LIMIT: &[u8] = b"LIMIT";
pub const WEIGHTS: &[u8] = b"WEIGHTS";
pub const AGGREGATE: &[u8] = b"AGGREGATE";
pub const SUM: &[u8] = b"SUM";
pub const MIN: &[u8] = b"MIN";
pub const MAX: &[u8] = b"MAX";

pub const MATCH: &[u8] = b"MATCH";
pub const COUNT: &[u8] = b"COUNT";
pub const TYPE_OPT: &[u8] = b"TYPE";

pub const ID: &[u8] = b"ID";
pub const SETNAME: &[u8] = b"SETNAME";
pub const GETNAME: &[u8] = b"GETNAME";
pub const LIST: &[u8] = b"LIST";
pub const KILL: &[u8] = b"KILL";
pub const UNBLOCK: &[u8] = b"UNBLOCK";
pub const PAUSE: &[u8] = b"PAUSE";
pub const REWRITE: &[u8] = b"REWRITE";
pub const NOVALUES: &[u8] = b"NOVALUES";
