# Microsoft Garnet 官方指令与 wedb 实现全量审计报告

> 审计环境：Garnet 官方规范元数据 (`RespCommandsInfo.json`, `RespCommandsDocs.json`, `RespCommand.cs`) vs 本项目 Rust 实现 (`wedb_resp`, `wedb_server`, `wedb_acl`, `wedb_blocking`)
> 审计标准遵循：`.agents/skills/code_review/SKILL.md`、`.agents/skills/rust_review/SKILL.md` 与 `.agents/skills/js_review/SKILL.md`

## 一、总体审计摘要

| 指标项 | 统计数量 | 说明 |
| :--- | :--- | :--- |
| **Garnet 官方规范命令总数** | **356** | 涵盖 262 个顶层命令 + 94 个嵌套子命令 |
| **Garnet C# 内部操作码总数** | **368** | 包含 internal 优化操作码与复合指令 |
| **wedb_resp 协议收录枚举总数** | **368** | **100% 1:1 精确对齐 Garnet C# 内部数值与物理布局（断言零偏差）** |
| **分类 A：完整迁移且参数严谨** | **114** | 参数校验严格匹配 Redis/Garnet Arity 规范 |
| **分类 B：已分发但参数校验有出入** | **47** | 存在多参数放行、未校验参数、反向报错或子命令路由状态机断裂 |
| **分类 C：已定义未在 Server 分发** | **195** | 按官方规范领域划分：阻塞、流、位图、HNSW 向量、集群等 |
| **分类 D：Garnet 存在但协议层未收录** | **0** | 缺失的官方命令（全量对比结果为 0） |

## 二、核心架构与代码缺陷深度剖析 (Critical Findings)

### 1. 子命令二次路由状态机断裂 (Subcommand Double Routing Bug)
在协议解析层 `wedb_resp/src/parse_state.rs` 中：
```rust
// parse_array_command 遇到具备子命令的主命令 (如 CLIENT, CONFIG, COMMAND) 时：
if let Some(sub_cmd) = RespCommand::lookup_subcommand(primary_cmd, next_arg) {
    final_cmd = sub_cmd; // 例如 ClientId, ConfigGet, CommandDocs
    remaining_count -= 1;
}
```
解析器在匹配到子命令时，**已经消费出队了子命令 token**，返回给 `wedb_server` 的命令是具体的子命令枚举变体（如 `RespCommand::ClientId`、`RespCommand::ConfigGet`），此时 `args` 中仅包含子命令之后的实际参数。

然而在 `wedb_server/src/dispatcher.rs` 的 `execute_single_command` 中：
```rust
RespCommand::CLIENT => {
    let sub = args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("");
    if sub.eq_ignore_ascii_case("ID") { ... }
}
RespCommand::CONFIG => {
    let sub = args.first().and_then(|s| from_utf8(s).ok()).unwrap_or("");
    if sub.eq_ignore_ascii_case("GET") { ... }
}
RespCommand::COMMAND => {
    buf.write_array_header(0);
}
```
**严重后果**：
1. 客户端发送 `CLIENT ID` 时，`parse_session_command` 返回的是 `RespCommand::ClientId`。
2. `execute_single_command` 的 `match cmd` 中没有 `RespCommand::ClientId` 分支，只有 `RespCommand::CLIENT`。
3. 指令直接穿透 match 所有显式分支，进入兜底分支：`_ => buf.write_error_fmt(format_args!("ERR unknown command '{cmd:?}'"))`，客户端收到 `ERR unknown command 'ClientId'` 错误！
4. `CONFIG GET` 与 `COMMAND DOCS` 同理全部崩溃！

> [!WARNING]
> **修复陷阱警示**：修复时**绝不能**仅仅将 `RespCommand::ClientId` 并列到 `CLIENT` 分支然后继续读 `args.first()`，因为当 `cmd == RespCommand::ClientId` 时，子命令名称 `"ID"` 已经在解析层被消费，`args.first()` 已经是后续参数甚至为空！
> 正确的做法应当参照成熟的 `handle_cluster` 模式：
> ```rust
> let is_id = cmd == RespCommand::CLIENT_ID || (cmd == RespCommand::CLIENT && args.first().is_some_and(|s| s.eq_ignore_ascii_case(b"ID")));
> ```

### 2. 定长参数命令的多余参数放行漏洞 (Over-tolerant Argument Validation)
Redis 官方 Arity 规范规定：正数 N 代表总 token 数量为 N（即用户参数 `args.len() == N - 1`）。若传入多于 N - 1 个参数，必须报错 `ERR wrong number of arguments for 'xxx' command`。
但在 `dispatcher.rs` 中：
- `GET`、`TTL`、`HLEN`、`HGETALL`、`HKEYS`、`HVALS`、`LLEN`、`SCARD`、`SMEMBERS`、`ZCARD`：官方 Arity 为 2（恰好 1 个 key），但代码仅检查了 `if args.is_empty()`。导致客户端若发送 `GET k1 k2 k3`，服务端非但不报错，反而静默读取 `k1` 并丢弃后续参数；
- `HGET`、`HEXISTS`、`ZSCORE`：官方 Arity 为 3（恰好 key field 2 个参数），但代码仅检查 `if args.len() < 2`，传入 3 个以上参数直接放行；
- `ECHO`、`SELECT`、`PUBLISH`：官方规定固定参数，代码均未限制参数个数上限。

### 3. UNSUBSCRIBE 命令的逆向反向报错 Bug
Redis / Garnet 官方规范中：
- `UNSUBSCRIBE [channel [channel ...]]` 的 Arity 为 `-1`。
- 当客户端执行无参数的 `UNSUBSCRIBE` 时，语义是**退订当前连接的所有已订阅频道**，属于完全合法的核心行为。
但在 `dispatcher.rs` 第 1580 行：
```rust
RespCommand::UNSUBSCRIBE => {
    if args.is_empty() {
        buf.write_error(b"ERR wrong number of arguments for 'unsubscribe' command");
        return Ok(());
    }
```
在参数为空时直接粗暴返回参数错误，导致所有标准 Redis 客户端（包括 redis-cli、Jedis、redis-py 等）在无参数退订时报错崩溃！

### 4. 事务与非事务状态下的校验逻辑脱节 (validate_command_syntax vs execute_single_command)
- `validate_command_syntax` 仅在 `session.in_txn` 为 true 时被触发，常规执行走 `execute_single_command`。
- 例如 `DBSIZE` 在 `validate_command_syntax` 中严谨校验了 `if !args.is_empty()`，但在 `execute_single_command` 中零校验。客户端直接执行 `DBSIZE foo bar` 成功，而在事务中排队 `DBSIZE foo bar` 则报错，产生严重的行为不一致。
- `WATCH`、`UNWATCH`、`MULTI`、`EXEC`、`DISCARD`、`REPLICAOF`、`ASKING`、`READONLY`、`READWRITE` 等辅助控制命令在常规执行时完全零参数校验。

## 三、分类 A：完整迁移且参数校验严谨的命令列表
共 **114** 个命令：

| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 期望参数长度 | 分发位置 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `ACL` | `Acl` | `-2` | `>= 1` | `execute_single_command` |
| 2 | `ACL|CAT` | `AclCat` | `-2` | `[0, 1]` | `handle_acl` |
| 3 | `ACL|DELUSER` | `AclDeluser` | `-3` | `>= 1` | `handle_acl` |
| 4 | `ACL|LIST` | `AclList` | `2` | `== 0` | `handle_acl` |
| 5 | `ACL|SETUSER` | `AclSetuser` | `-3` | `>= 1` | `handle_acl` |
| 6 | `ACL|USERS` | `AclUsers` | `2` | `== 0` | `handle_acl` |
| 7 | `ACL|WHOAMI` | `AclWhoami` | `2` | `== 0` | `handle_acl` |
| 8 | `APPEND` | `Append` | `3` | `== 2` | `execute_single_command` |
| 9 | `AUTH` | `Auth` | `-2` | `[1, 2]` | `execute_single_command` |
| 10 | `BITCOUNT` | `Bitcount` | `-2` | `[1, 2]` | `execute_single_command` |
| 11 | `BLMOVE` | `Blmove` | `6` | `== 5` | `execute_single_command` |
| 12 | `BLPOP` | `Blpop` | `-3` | `>= 2` | `execute_single_command` |
| 13 | `BRPOP` | `Brpop` | `-3` | `>= 2` | `execute_single_command` |
| 14 | `BRPOPLPUSH` | `Brpoplpush` | `4` | `== 3` | `execute_single_command` |
| 15 | `CLIENT` | `Client` | `-2` | `>= 1` | `execute_single_command` |
| 16 | `CLUSTER` | `Cluster` | `-2` | `>= 1` | `execute_single_command` |
| 17 | `CLUSTER|INFO` | `ClusterInfo` | `2` | `== 0` | `handle_cluster` |
| 18 | `CLUSTER|MIGRATE` | `ClusterMigrate` | `4` | `== 2` | `handle_cluster` |
| 19 | `CLUSTER|NODES` | `ClusterNodes` | `2` | `== 0` | `handle_cluster` |
| 20 | `CLUSTER|SLOTS` | `ClusterSlots` | `2` | `== 0` | `handle_cluster` |
| 21 | `COMMAND` | `Command` | `-1` | `>= 0` | `execute_single_command` |
| 22 | `CONFIG` | `Config` | `-2` | `>= 1` | `execute_single_command` |
| 23 | `DECR` | `Decr` | `2` | `== 1` | `execute_single_command` |
| 24 | `DECRBY` | `Decrby` | `3` | `== 2` | `execute_single_command` |
| 25 | `DEL` | `Del` | `-2` | `>= 1` | `execute_single_command` |
| 26 | `EXISTS` | `Exists` | `-2` | `>= 1` | `execute_single_command` |
| 27 | `EXPIRE` | `Expire` | `-3` | `[2, 3]` | `execute_single_command` |
| 28 | `EXPIREAT` | `Expireat` | `-3` | `[2, 3]` | `execute_single_command` |
| 29 | `EXPIRETIME` | `Expiretime` | `2` | `== 1` | `execute_single_command` |
| 30 | `FLUSHALL` | `Flushall` | `-1` | `[0, 1]` | `execute_single_command` |
| 31 | `FLUSHDB` | `Flushdb` | `-1` | `[0, 1]` | `execute_single_command` |
| 32 | `GETBIT` | `Getbit` | `3` | `== 2` | `execute_single_command` |
| 33 | `GETDEL` | `Getdel` | `2` | `== 1` | `execute_single_command` |
| 34 | `GETRANGE` | `Getrange` | `4` | `== 3` | `execute_single_command` |
| 35 | `GETSET` | `Getset` | `3` | `== 2` | `execute_single_command` |
| 36 | `HDEL` | `Hdel` | `-3` | `>= 2` | `execute_single_command` |
| 37 | `HEXPIRE` | `Hexpire` | `-6` | `[5, 4]` | `execute_single_command` |
| 38 | `HEXPIREAT` | `Hexpireat` | `-6` | `[5, 4]` | `execute_single_command` |
| 39 | `HINCRBY` | `Hincrby` | `4` | `== 3` | `execute_single_command` |
| 40 | `HINCRBYFLOAT` | `Hincrbyfloat` | `4` | `== 3` | `execute_single_command` |
| 41 | `HMGET` | `Hmget` | `-3` | `>= 2` | `execute_single_command` |
| 42 | `HMSET` | `Hmset` | `-4` | `>= 3` | `execute_single_command` |
| 43 | `HPERSIST` | `Hpersist` | `-5` | `[4, 2]` | `execute_single_command` |
| 44 | `HSCAN` | `Hscan` | `-3` | `[2, 4]` | `execute_single_command` |
| 45 | `HSET` | `Hset` | `-4` | `>= 3` | `execute_single_command` |
| 46 | `HSETNX` | `Hsetnx` | `4` | `== 3` | `execute_single_command` |
| 47 | `HTTL` | `Httl` | `-5` | `[4, 2]` | `execute_single_command` |
| 48 | `INCR` | `Incr` | `2` | `== 1` | `execute_single_command` |
| 49 | `INCRBY` | `Incrby` | `3` | `== 2` | `execute_single_command` |
| 50 | `INCRBYFLOAT` | `Incrbyfloat` | `3` | `== 2` | `execute_single_command` |
| 51 | `INFO` | `Info` | `-1` | `>= 0` | `execute_single_command` |
| 52 | `KEYS` | `Keys` | `2` | `== 1` | `execute_single_command` |
| 53 | `LINDEX` | `Lindex` | `3` | `== 2` | `execute_single_command` |
| 54 | `LINSERT` | `Linsert` | `5` | `== 4` | `execute_single_command` |
| 55 | `LMOVE` | `Lmove` | `5` | `== 4` | `execute_single_command` |
| 56 | `LMPOP` | `Lmpop` | `-4` | `>= 3` | `execute_single_command` |
| 57 | `LPOS` | `Lpos` | `-3` | `[2, 5]` | `execute_single_command` |
| 58 | `LPUSH` | `Lpush` | `-3` | `>= 2` | `execute_single_command` |
| 59 | `LPUSHX` | `Lpushx` | `-3` | `>= 2` | `execute_single_command` |
| 60 | `LRANGE` | `Lrange` | `4` | `== 3` | `execute_single_command` |
| 61 | `LREM` | `Lrem` | `4` | `== 3` | `execute_single_command` |
| 62 | `LSET` | `Lset` | `4` | `== 3` | `execute_single_command` |
| 63 | `LTRIM` | `Ltrim` | `4` | `== 3` | `execute_single_command` |
| 64 | `MGET` | `Mget` | `-2` | `>= 1` | `execute_single_command` |
| 65 | `MIGRATE` | `Migrate` | `-6` | `>= 5` | `execute_single_command` |
| 66 | `MSET` | `Mset` | `-3` | `>= 2` | `execute_single_command` |
| 67 | `MSETNX` | `Msetnx` | `-3` | `>= 2` | `execute_single_command` |
| 68 | `PERSIST` | `Persist` | `2` | `== 1` | `execute_single_command` |
| 69 | `PEXPIRE` | `Pexpire` | `-3` | `[2, 3]` | `execute_single_command` |
| 70 | `PEXPIREAT` | `Pexpireat` | `-3` | `[2, 3]` | `execute_single_command` |
| 71 | `PEXPIRETIME` | `Pexpiretime` | `2` | `== 1` | `execute_single_command` |
| 72 | `PSETEX` | `Psetex` | `4` | `== 3` | `execute_single_command` |
| 73 | `PTTL` | `Pttl` | `2` | `== 1` | `execute_single_command` |
| 74 | `QUIT` | `Quit` | `-1` | `>= 0` | `execute_single_command` |
| 75 | `RENAME` | `Rename` | `-3` | `[2, 3]` | `execute_single_command` |
| 76 | `RENAMENX` | `Renamenx` | `-3` | `[2, 3]` | `execute_single_command` |
| 77 | `RI.CONFIG` | `Riconfig` | `2` | `== 1` | `execute_single_command` |
| 78 | `RI.CREATE` | `Ricreate` | `-2` | `[1, 7]` | `execute_single_command` |
| 79 | `RI.DEL` | `Ridel` | `3` | `== 2` | `execute_single_command` |
| 80 | `RI.EXISTS` | `Riexists` | `2` | `== 1` | `execute_single_command` |
| 81 | `RI.GET` | `Riget` | `3` | `== 2` | `execute_single_command` |
| 82 | `RI.METRICS` | `Rimetrics` | `2` | `== 1` | `execute_single_command` |
| 83 | `RI.RANGE` | `Rirange` | `-4` | `[3, 4]` | `execute_single_command` |
| 84 | `RI.SCAN` | `Riscan` | `-5` | `== 4` | `execute_single_command` |
| 85 | `RI.SET` | `Riset` | `4` | `== 3` | `execute_single_command` |
| 86 | `RPOPLPUSH` | `Rpoplpush` | `3` | `== 2` | `execute_single_command` |
| 87 | `RPUSH` | `Rpush` | `-3` | `>= 2` | `execute_single_command` |
| 88 | `RPUSHX` | `Rpushx` | `-3` | `>= 2` | `execute_single_command` |
| 89 | `SADD` | `Sadd` | `-3` | `>= 2` | `execute_single_command` |
| 90 | `SET` | `Set` | `-3` | `[2, 6]` | `execute_single_command` |
| 91 | `SETBIT` | `Setbit` | `4` | `== 3` | `execute_single_command` |
| 92 | `SETEX` | `Setex` | `4` | `== 3` | `execute_single_command` |
| 93 | `SETNX` | `Setnx` | `3` | `== 2` | `execute_single_command` |
| 94 | `SETRANGE` | `Setrange` | `4` | `== 3` | `execute_single_command` |
| 95 | `SISMEMBER` | `Sismember` | `3` | `== 2` | `execute_single_command` |
| 96 | `SMISMEMBER` | `Smismember` | `-3` | `>= 2` | `execute_single_command` |
| 97 | `SMOVE` | `Smove` | `4` | `== 3` | `execute_single_command` |
| 98 | `SPOP` | `Spop` | `-2` | `[1, 2]` | `execute_single_command` |
| 99 | `SREM` | `Srem` | `-3` | `>= 2` | `execute_single_command` |
| 100 | `SSCAN` | `Sscan` | `-3` | `[2, 4]` | `execute_single_command` |
| 101 | `STRLEN` | `Strlen` | `2` | `== 1` | `execute_single_command` |
| 102 | `SUBSCRIBE` | `Subscribe` | `-2` | `>= 1` | `execute_single_command` |
| 103 | `SUBSTR` | `Substr` | `4` | `== 3` | `execute_single_command` |
| 104 | `TIME` | `Time` | `1` | `== 0` | `execute_single_command` |
| 105 | `TYPE` | `Type` | `2` | `== 1` | `execute_single_command` |
| 106 | `UNLINK` | `Unlink` | `-2` | `>= 1` | `execute_single_command` |
| 107 | `ZADD` | `Zadd` | `-4` | `>= 3` | `execute_single_command` |
| 108 | `ZCOUNT` | `Zcount` | `4` | `== 3` | `execute_single_command` |
| 109 | `ZINCRBY` | `Zincrby` | `4` | `== 3` | `execute_single_command` |
| 110 | `ZRANGE` | `Zrange` | `-4` | `[3, 7]` | `execute_single_command` |
| 111 | `ZRANK` | `Zrank` | `-3` | `[2, 3]` | `execute_single_command` |
| 112 | `ZREM` | `Zrem` | `-3` | `>= 2` | `execute_single_command` |
| 113 | `ZREVRANK` | `Zrevrank` | `-3` | `[2, 3]` | `execute_single_command` |
| 114 | `ZSCAN` | `Zscan` | `-3` | `[2, 4]` | `execute_single_command` |

## 四、分类 B：已迁移但参数校验存在出入的命令列表
共 **47** 个命令：

| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 期望参数长度 | 存在缺陷与不一致说明 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `ASKING` | `Asking` | `1` | `== 0` | Garnet Arity 为 1 (定长 0 参数)，但代码零参数校验直接放行并执行 |
| 2 | `CLIENT|GETNAME` | `ClientGetname` | `2` | `== 0` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 3 | `CLIENT|ID` | `ClientId` | `2` | `== 0` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 4 | `CLIENT|INFO` | `ClientInfo` | `2` | `== 0` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 5 | `CLIENT|KILL` | `ClientKill` | `-3` | `== 1` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 6 | `CLIENT|LIST` | `ClientList` | `-2` | `>= 0` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 7 | `CLIENT|SETINFO` | `ClientSetinfo` | `4` | `== 2` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 8 | `CLIENT|SETNAME` | `ClientSetname` | `3` | `== 1` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 9 | `CLIENT|UNBLOCK` | `ClientUnblock` | `-3` | `[1, 2]` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 10 | `CLUSTER|KEYSLOT` | `ClusterKeyslot` | `3` | `== 1` | Garnet Arity 为 3 (恰好 1 个参数)，未严格校验参数上限 args.len() == 1 |
| 11 | `COMMAND|COUNT` | `CommandCount` | `2` | `== 0` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 12 | `COMMAND|DOCS` | `CommandDocs` | `-2` | `>= 0` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 13 | `COMMAND|GETKEYS` | `CommandGetkeys` | `-3` | `>= 1` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 14 | `COMMAND|GETKEYSANDFLAGS` | `CommandGetkeysandflags` | `-3` | `>= 1` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 15 | `COMMAND|INFO` | `CommandInfo` | `-2` | `>= 0` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 16 | `CONFIG|GET` | `ConfigGet` | `-3` | `>= 1` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 17 | `CONFIG|REWRITE` | `ConfigRewrite` | `2` | `== 0` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 18 | `CONFIG|SET` | `ConfigSet` | `-4` | `>= 2` | 【状态机路由断裂】parse_session_command 已将子命令解析为独立枚举，但 dispatcher 仅匹配父命令，客户端调用时直接报错未知命令 |
| 19 | `DBSIZE` | `Dbsize` | `1` | `== 0` | 【一致性缺陷】validate_command_syntax 严谨校验 !args.is_empty()，但在 execute_single_command 中零校验，常规执行 DBSIZE foo 不会报错 |
| 20 | `DISCARD` | `Discard` | `1` | `== 0` | Garnet Arity 为 1 (定长 0 参数)，但代码零参数校验直接放行并执行 |
| 21 | `ECHO` | `Echo` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个参数)，但代码仅检查 if args.is_empty()，多余参数被静默忽略未报错 |
| 22 | `EXEC` | `Exec` | `1` | `== 0` | Garnet Arity 为 1 (定长 0 参数)，但代码零参数校验直接放行并执行 |
| 23 | `GET` | `Get` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 24 | `HEXISTS` | `Hexists` | `3` | `== 2` | Garnet Arity 为 3 (恰好 2 个参数)，但代码仅检查 if args.len() < 2，传入 3 个以上参数未报错 |
| 25 | `HGET` | `Hget` | `3` | `== 2` | Garnet Arity 为 3 (恰好 2 个参数)，但代码仅检查 if args.len() < 2，传入 3 个以上参数未报错 |
| 26 | `HGETALL` | `Hgetall` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 27 | `HKEYS` | `Hkeys` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 28 | `HLEN` | `Hlen` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 29 | `HVALS` | `Hvals` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 30 | `LLEN` | `Llen` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 31 | `LPOP` | `Lpop` | `-2` | `[1, 2]` | Garnet Arity 为 -2 (支持 key 或 key count，最多 2 参数)，代码仅检查 if args.is_empty()，传入 3 个以上参数未报错 |
| 32 | `MULTI` | `Multi` | `1` | `== 0` | Garnet Arity 为 1 (定长 0 参数)，但代码零参数校验直接放行并执行 |
| 33 | `PING` | `Ping` | `-1` | `[0, 1]` | Garnet Arity 为 -1 (支持 0 或 1 参数)，传入 2 个以上参数未报 WRONG_NUM_ARGS 错误，而是静默忽略后续参数 |
| 34 | `PUBLISH` | `Publish` | `3` | `== 2` | Garnet Arity 为 3 (恰好 2 个参数)，但代码仅检查 if args.len() < 2，传入 3 个以上参数未报错 |
| 35 | `READONLY` | `Readonly` | `1` | `== 0` | Garnet Arity 为 1 (定长 0 参数)，但代码零参数校验直接放行并执行 |
| 36 | `READWRITE` | `Readwrite` | `1` | `== 0` | Garnet Arity 为 1 (定长 0 参数)，但代码零参数校验直接放行并执行 |
| 37 | `REPLICAOF` | `Replicaof` | `3` | `== 2` | Garnet Arity 为 3 (恰好 host port 2 参数)，代码零参数校验直接 buf.write_ok() |
| 38 | `RPOP` | `Rpop` | `-2` | `[1, 2]` | Garnet Arity 为 -2 (支持 key 或 key count，最多 2 参数)，代码仅检查 if args.is_empty()，传入 3 个以上参数未报错 |
| 39 | `SCARD` | `Scard` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 40 | `SELECT` | `Select` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个参数)，但代码仅检查 if args.is_empty()，多余参数被静默忽略未报错 |
| 41 | `SMEMBERS` | `Smembers` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 42 | `TTL` | `Ttl` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 43 | `UNSUBSCRIBE` | `Unsubscribe` | `-1` | `>= 0` | 【严重反向报错】Redis 规范 Arity 为 -1 (无参时退订全部频道)，代码强制校验 if args.is_empty() 并报错参数错误，违背标准协议 |
| 44 | `UNWATCH` | `Unwatch` | `1` | `== 0` | Garnet Arity 为 1 (定长 0 参数)，但代码零参数校验直接放行并执行 |
| 45 | `WATCH` | `Watch` | `-2` | `>= 1` | 【缺失校验】Garnet Arity 为 -2 (至少 1 个 key)，代码零参数校验直接 buf.write_ok()，空参数执行不报错 |
| 46 | `ZCARD` | `Zcard` | `2` | `== 1` | Garnet Arity 为 2 (恰好 1 个 key)，但 dispatcher 仅检查 if args.is_empty()，传入 GET k1 k2 时静默放行并丢弃后续参数 |
| 47 | `ZSCORE` | `Zscore` | `3` | `== 2` | Garnet Arity 为 3 (恰好 2 个参数)，但代码仅检查 if args.len() < 2，传入 3 个以上参数未报错 |

## 五、分类 C：协议层 (wedb_resp) 已定义但尚未在 Server 分发的命令列表
共 **195** 个命令，按官方规范领域分类详细统计：

### 5.ACL (访问控制) (共 4 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `ACL|GENPASS` | `AclGenpass` | `-2` | `Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 2 | `ACL|GETUSER` | `AclGetuser` | `3` | `Admin, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 3 | `ACL|LOAD` | `AclLoad` | `2` | `Admin, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 4 | `ACL|SAVE` | `AclSave` | `2` | `Admin, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |

### 5.Generic (通用键空间) (共 13 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `ASYNC` | `Async` | `1` | `NoMulti, NoScript, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 2 | `COSCAN` | `Coscan` | `-3` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 3 | `DELIFGREATER` | `Delifgreater` | `2` | `None` | 待在 dispatcher.rs 中接入 |
| 4 | `DUMP` | `Dump` | `2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 5 | `EXPDELSCAN` | `Expdelscan` | `-1` | `Admin, NoMulti, NoScript, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 6 | `OBJECT` | `Object` | `-2` | `None` | 待在 dispatcher.rs 中接入 |
| 7 | `OBJECT|ENCODING` | `ObjectEncoding` | `3` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 8 | `OBJECT|FREQ` | `ObjectFreq` | `3` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 9 | `OBJECT|HELP` | `ObjectHelp` | `2` | `Loading, Stale` | 待在 dispatcher.rs 中接入 |
| 10 | `OBJECT|IDLETIME` | `ObjectIdletime` | `3` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 11 | `OBJECT|REFCOUNT` | `ObjectRefcount` | `3` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 12 | `RESTORE` | `Restore` | `-4` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |
| 13 | `SCAN` | `Scan` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |

### 5.Server & Admin (服务与运维监控) (共 25 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `BGSAVE` | `Bgsave` | `-1` | `Admin, NoAsyncLoading, NoScript` | 待在 dispatcher.rs 中接入 |
| 2 | `COMMITAOF` | `Commitaof` | `-1` | `Admin, NoMulti, NoScript, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 3 | `DEBUG` | `Debug` | `-2` | `Admin, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 4 | `FAILOVER` | `Failover` | `-1` | `Admin, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 5 | `LASTSAVE` | `Lastsave` | `-1` | `Fast, Loading, Stale` | 待在 dispatcher.rs 中接入 |
| 6 | `LATENCY` | `Latency` | `-2` | `None` | 待在 dispatcher.rs 中接入 |
| 7 | `LATENCY|HELP` | `LatencyHelp` | `2` | `Admin, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 8 | `LATENCY|HISTOGRAM` | `LatencyHistogram` | `-2` | `Admin, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 9 | `LATENCY|RESET` | `LatencyReset` | `-2` | `Admin, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 10 | `MEMORY` | `Memory` | `-2` | `None` | 待在 dispatcher.rs 中接入 |
| 11 | `MEMORY|USAGE` | `MemoryUsage` | `-3` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 12 | `MODULE` | `Module` | `-2` | `Admin` | 待在 dispatcher.rs 中接入 |
| 13 | `MODULE|LOADCS` | `ModuleLoadcs` | `-3` | `Admin, NoAsyncLoading, NoScript` | 待在 dispatcher.rs 中接入 |
| 14 | `MONITOR` | `Monitor` | `1` | `Admin, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 15 | `REGISTERCS` | `Registercs` | `-5` | `Admin, NoMulti, NoScript, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 16 | `ROLE` | `Role` | `1` | `Fast, Loading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 17 | `SAVE` | `Save` | `-1` | `Admin, NoAsyncLoading, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 18 | `SECONDARYOF` | `Secondaryof` | `3` | `Admin, NoAsyncLoading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 19 | `SLAVEOF` | `Secondaryof` | `3` | `Admin, NoAsyncLoading, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 20 | `SLOWLOG` | `Slowlog` | `-2` | `None` | 待在 dispatcher.rs 中接入 |
| 21 | `SLOWLOG|GET` | `SlowlogGet` | `-2` | `Admin, Loading, Stale` | 待在 dispatcher.rs 中接入 |
| 22 | `SLOWLOG|HELP` | `SlowlogHelp` | `2` | `Loading, Stale` | 待在 dispatcher.rs 中接入 |
| 23 | `SLOWLOG|LEN` | `SlowlogLen` | `2` | `Admin, Loading, Stale` | 待在 dispatcher.rs 中接入 |
| 24 | `SLOWLOG|RESET` | `SlowlogReset` | `2` | `Admin, Loading, Stale` | 待在 dispatcher.rs 中接入 |
| 25 | `SWAPDB` | `Swapdb` | `3` | `Fast, Write` | 待在 dispatcher.rs 中接入 |

### 5.Bitmap (位图) (共 4 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `BITFIELD` | `Bitfield` | `-2` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |
| 2 | `BITFIELD_RO` | `BitfieldRo` | `-2` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 3 | `BITOP` | `Bitop` | `-4` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |
| 4 | `BITPOS` | `Bitpos` | `-3` | `ReadOnly` | 待在 dispatcher.rs 中接入 |

### 5.Blocking (阻塞列表/ZSet) (共 4 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `BLMPOP` | `Blmpop` | `-5` | `Blocking, MovableKeys, Write` | 🔥 **wedb_blocking 中已有完整底层实现与单元测试，亟待接入 dispatcher** |
| 2 | `BZMPOP` | `Bzmpop` | `-5` | `Blocking, MovableKeys, Write` | 🔥 **wedb_blocking 中已有完整底层实现与单元测试，亟待接入 dispatcher** |
| 3 | `BZPOPMAX` | `Bzpopmax` | `-3` | `Blocking, Fast, Write` | 🔥 **wedb_blocking 中已有完整底层实现与单元测试，亟待接入 dispatcher** |
| 4 | `BZPOPMIN` | `Bzpopmin` | `-3` | `Blocking, Fast, Write` | 🔥 **wedb_blocking 中已有完整底层实现与单元测试，亟待接入 dispatcher** |

### 5.Cluster (集群路由与管理) (共 43 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `CLUSTER|ADDSLOTS` | `ClusterAddslots` | `-3` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 2 | `CLUSTER|ADDSLOTSRANGE` | `ClusterAddslotsrange` | `-4` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 3 | `CLUSTER|ADVANCE_TIME` | `ClusterAdvanceTime` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 4 | `CLUSTER|APPENDLOG` | `ClusterAppendlog` | `6` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 5 | `CLUSTER|ATTACH_SYNC` | `ClusterAttachSync` | `3` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 6 | `CLUSTER|BANLIST` | `ClusterBanlist` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 7 | `CLUSTER|BEGIN_REPLICA_RECOVER` | `ClusterBeginReplicaRecover` | `8` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 8 | `CLUSTER|BUMPEPOCH` | `ClusterBumpepoch` | `2` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 9 | `CLUSTER|COUNTKEYSINSLOT` | `ClusterCountkeysinslot` | `3` | `Stale` | 待在 dispatcher.rs 中接入 |
| 10 | `CLUSTER|DELKEYSINSLOT` | `ClusterDelkeysinslot` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 11 | `CLUSTER|DELKEYSINSLOTRANGE` | `ClusterDelkeysinslotrange` | `-3` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 12 | `CLUSTER|DELSLOTS` | `ClusterDelslots` | `-3` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 13 | `CLUSTER|DELSLOTSRANGE` | `ClusterDelslotsrange` | `-4` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 14 | `CLUSTER|ENDPOINT` | `ClusterEndpoint` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 15 | `CLUSTER|FAILOVER` | `ClusterFailover` | `-2` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 16 | `CLUSTER|FAILREPLICATIONOFFSET` | `ClusterFailreplicationoffset` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 17 | `CLUSTER|FAILSTOPWRITES` | `ClusterFailstopwrites` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 18 | `CLUSTER|FLUSHALL` | `ClusterFlushall` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 19 | `CLUSTER|FORGET` | `ClusterForget` | `3` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 20 | `CLUSTER|GETKEYSINSLOT` | `ClusterGetkeysinslot` | `4` | `Stale` | 待在 dispatcher.rs 中接入 |
| 21 | `CLUSTER|GOSSIP` | `ClusterGossip` | `-2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 22 | `CLUSTER|HELP` | `ClusterHelp` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 23 | `CLUSTER|INITIATE_REPLICA_SYNC` | `ClusterInitiateReplicaSync` | `6` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 24 | `CLUSTER|MEET` | `ClusterMeet` | `-4` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 25 | `CLUSTER|MLOG_KEY_TIME` | `ClusterMlogKeyTime` | `-2` | `Admin, NoMulti, NoScript, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 26 | `CLUSTER|MTASKS` | `ClusterMtasks` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 27 | `CLUSTER|MYID` | `ClusterMyid` | `2` | `Stale` | 待在 dispatcher.rs 中接入 |
| 28 | `CLUSTER|MYPARENTID` | `ClusterMyparentid` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 29 | `CLUSTER|PUBLISH` | `ClusterPublish` | `4` | `Loading, NoScript, PubSub, Stale` | 待在 dispatcher.rs 中接入 |
| 30 | `CLUSTER|REPLICAS` | `ClusterReplicas` | `3` | `Admin, Stale` | 待在 dispatcher.rs 中接入 |
| 31 | `CLUSTER|REPLICATE` | `ClusterReplicate` | `3` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 32 | `CLUSTER|RESERVE` | `ClusterReserve` | `4` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 33 | `CLUSTER|RESET` | `ClusterReset` | `-2` | `Admin, NoScript, Stale` | 待在 dispatcher.rs 中接入 |
| 34 | `CLUSTER|SEND_CKPT_FILE_SEGMENT` | `ClusterSendCkptFileSegment` | `6` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 35 | `CLUSTER|SEND_CKPT_METADATA` | `ClusterSendCkptMetadata` | `4` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 36 | `CLUSTER|SET-CONFIG-EPOCH` | `ClusterSetconfigepoch` | `3` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 37 | `CLUSTER|SETSLOT` | `ClusterSetslot` | `-4` | `Admin, NoAsyncLoading, Stale` | 待在 dispatcher.rs 中接入 |
| 38 | `CLUSTER|SETSLOTSRANGE` | `ClusterSetslotsrange` | `-4` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 39 | `CLUSTER|SHARDS` | `ClusterShards` | `2` | `Loading, Stale` | 待在 dispatcher.rs 中接入 |
| 40 | `CLUSTER|SLOTSTATE` | `ClusterSlotstate` | `2` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 41 | `CLUSTER|SNAPSHOT_DATA` | `ClusterSnapshotData` | `6` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 42 | `CLUSTER|SPUBLISH` | `ClusterSpublish` | `4` | `Loading, NoScript, PubSub, Stale` | 待在 dispatcher.rs 中接入 |
| 43 | `CLUSTER|SYNC` | `ClusterSync` | `4` | `Admin, NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |

### 5.Extension (Garnet 插件与自定义扩展) (共 4 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `CustomObjCmd` | `CustomObjCmd` | `1` | `None` | 待在 dispatcher.rs 中接入 |
| 2 | `CustomProcedure` | `CustomProcedure` | `1` | `None` | 待在 dispatcher.rs 中接入 |
| 3 | `CustomRawStringCmd` | `CustomRawStringCmd` | `1` | `None` | 待在 dispatcher.rs 中接入 |
| 4 | `CustomTxn` | `CustomTxn` | `1` | `None` | 待在 dispatcher.rs 中接入 |

### 5.Scripting (Lua/脚本) (共 6 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `EVAL` | `Eval` | `-3` | `MovableKeys, NoMandatoryKeys, NoScript, SkipMonitor, Stale` | 待在 dispatcher.rs 中接入 |
| 2 | `EVALSHA` | `Evalsha` | `-3` | `MovableKeys, NoMandatoryKeys, NoScript, SkipMonitor, Stale` | 待在 dispatcher.rs 中接入 |
| 3 | `SCRIPT` | `Script` | `-2` | `None` | 待在 dispatcher.rs 中接入 |
| 4 | `SCRIPT|EXISTS` | `ScriptExists` | `-3` | `NoScript` | 待在 dispatcher.rs 中接入 |
| 5 | `SCRIPT|FLUSH` | `ScriptFlush` | `-2` | `NoScript` | 待在 dispatcher.rs 中接入 |
| 6 | `SCRIPT|LOAD` | `ScriptLoad` | `3` | `NoScript, Stale` | 待在 dispatcher.rs 中接入 |

### 5.Geo (地理空间索引) (共 10 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `GEOADD` | `Geoadd` | `-5` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |
| 2 | `GEODIST` | `Geodist` | `-4` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 3 | `GEOHASH` | `Geohash` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 4 | `GEOPOS` | `Geopos` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 5 | `GEORADIUS` | `Georadius` | `-6` | `DenyOom, MovableKeys, Write` | 待在 dispatcher.rs 中接入 |
| 6 | `GEORADIUS_RO` | `GeoradiusRo` | `-6` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 7 | `GEORADIUSBYMEMBER` | `Georadiusbymember` | `-5` | `DenyOom, MovableKeys, Write` | 待在 dispatcher.rs 中接入 |
| 8 | `GEORADIUSBYMEMBER_RO` | `GeoradiusbymemberRo` | `-5` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 9 | `GEOSEARCH` | `Geosearch` | `-7` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 10 | `GEOSEARCHSTORE` | `Geosearchstore` | `-8` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |

### 5.KV / String (字符串与键值) (共 7 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `GETEX` | `Getex` | `-2` | `Fast, Write` | 待在 dispatcher.rs 中接入 |
| 2 | `GETIFNOTMATCH` | `Getifnotmatch` | `3` | `None` | 待在 dispatcher.rs 中接入 |
| 3 | `GETWITHETAG` | `Getwithetag` | `2` | `None` | 待在 dispatcher.rs 中接入 |
| 4 | `LCS` | `Lcs` | `-3` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 5 | `SETIFGREATER` | `Setifgreater` | `-4` | `None` | 待在 dispatcher.rs 中接入 |
| 6 | `SETIFMATCH` | `Setifmatch` | `-4` | `None` | 待在 dispatcher.rs 中接入 |
| 7 | `SETWITHETAG` | `Setwithetag` | `-3` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |

### 5.Hash (哈希散列) (共 9 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `HCOLLECT` | `Hcollect` | `2` | `Admin, Write` | 待在 dispatcher.rs 中接入 |
| 2 | `HEXPIRETIME` | `Hexpiretime` | `-5` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 3 | `HPEXPIRE` | `Hpexpire` | `-6` | `DenyOom, Fast, Write` | 待在 dispatcher.rs 中接入 |
| 4 | `HPEXPIREAT` | `Hpexpireat` | `-6` | `DenyOom, Fast, Write` | 待在 dispatcher.rs 中接入 |
| 5 | `HPEXPIRETIME` | `Hpexpiretime` | `-5` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 6 | `HPTTL` | `Hpttl` | `-5` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 7 | `HRANDFIELD` | `Hrandfield` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 8 | `HSTRLEN` | `Hstrlen` | `3` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 9 | `ZCOLLECT` | `Zcollect` | `2` | `Admin, Write` | 待在 dispatcher.rs 中接入 |

### 5.Connection (网络连接管理) (共 1 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `HELLO` | `Hello` | `-1` | `Fast, Loading, NoAuth, NoScript, Stale, AllowBusy` | 待在 dispatcher.rs 中接入 |

### 5.HyperLogLog (基数统计) (共 3 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `PFADD` | `Pfadd` | `-2` | `DenyOom, Fast, Write` | 待在 dispatcher.rs 中接入 |
| 2 | `PFCOUNT` | `Pfcount` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 3 | `PFMERGE` | `Pfmerge` | `-2` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |

### 5.PubSub (发布订阅) (共 8 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `PSUBSCRIBE` | `Psubscribe` | `-2` | `Loading, NoScript, PubSub, Stale` | 待在 dispatcher.rs 中接入 |
| 2 | `PUBSUB` | `Pubsub` | `-2` | `None` | 待在 dispatcher.rs 中接入 |
| 3 | `PUBSUB|CHANNELS` | `PubsubChannels` | `-2` | `Loading, PubSub, Stale` | 待在 dispatcher.rs 中接入 |
| 4 | `PUBSUB|NUMPAT` | `PubsubNumpat` | `2` | `Loading, PubSub, Stale` | 待在 dispatcher.rs 中接入 |
| 5 | `PUBSUB|NUMSUB` | `PubsubNumsub` | `-2` | `Loading, PubSub, Stale` | 待在 dispatcher.rs 中接入 |
| 6 | `PUNSUBSCRIBE` | `Punsubscribe` | `-1` | `Loading, NoScript, PubSub, Stale` | 待在 dispatcher.rs 中接入 |
| 7 | `SPUBLISH` | `Spublish` | `3` | `Loading, NoScript, PubSub, Stale` | 待在 dispatcher.rs 中接入 |
| 8 | `SSUBSCRIBE` | `Ssubscribe` | `-2` | `Loading, NoScript, PubSub, Stale` | 待在 dispatcher.rs 中接入 |

### 5.Transaction (事务处理) (共 3 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `RUNTXP` | `Runtxp` | `-2` | `NoMulti, NoScript` | 待在 dispatcher.rs 中接入 |
| 2 | `WATCHMS` | `Watchms` | `-2` | `Fast, Loading, NoScript, Stale, AllowBusy` | 待在 dispatcher.rs 中接入 |
| 3 | `WATCHOS` | `Watchos` | `-2` | `Fast, Loading, NoScript, Stale, AllowBusy` | 待在 dispatcher.rs 中接入 |

### 5.Set (无序集合) (共 8 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `SDIFF` | `Sdiff` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 2 | `SDIFFSTORE` | `Sdiffstore` | `-3` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |
| 3 | `SINTER` | `Sinter` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 4 | `SINTERCARD` | `Sintercard` | `-3` | `MovableKeys, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 5 | `SINTERSTORE` | `Sinterstore` | `-3` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |
| 6 | `SRANDMEMBER` | `Srandmember` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 7 | `SUNION` | `Sunion` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 8 | `SUNIONSTORE` | `Sunionstore` | `-3` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |

### 5.Vector (向量检索与 HNSW) (共 12 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `VADD` | `Vadd` | `-1` | `DenyOom, Write, Module` | 待在 dispatcher.rs 中接入 |
| 2 | `VCARD` | `Vcard` | `-1` | `Fast, ReadOnly, Module` | 待在 dispatcher.rs 中接入 |
| 3 | `VDIM` | `Vdim` | `-1` | `Fast, ReadOnly, Module` | 待在 dispatcher.rs 中接入 |
| 4 | `VEMB` | `Vemb` | `-1` | `Fast, ReadOnly, Module` | 待在 dispatcher.rs 中接入 |
| 5 | `VGETATTR` | `Vgetattr` | `-1` | `Fast, ReadOnly, Module` | 待在 dispatcher.rs 中接入 |
| 6 | `VINFO` | `Vinfo` | `-1` | `Fast, ReadOnly, Module` | 待在 dispatcher.rs 中接入 |
| 7 | `VISMEMBER` | `Vismember` | `3` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 8 | `VLINKS` | `Vlinks` | `-1` | `Fast, ReadOnly, Module` | 待在 dispatcher.rs 中接入 |
| 9 | `VRANDMEMBER` | `Vrandmember` | `-1` | `ReadOnly, Module` | 待在 dispatcher.rs 中接入 |
| 10 | `VREM` | `Vrem` | `-1` | `Write, Module` | 待在 dispatcher.rs 中接入 |
| 11 | `VSETATTR` | `Vsetattr` | `-1` | `Fast, Write, Module` | 待在 dispatcher.rs 中接入 |
| 12 | `VSIM` | `Vsim` | `-1` | `ReadOnly, Module` | 待在 dispatcher.rs 中接入 |

### 5.ZSet (有序集合) (共 31 个)
| 序号 | 命令名称 | RESP 枚举变体 | 官方 Arity | 标志位 (Flags) | 现状与接入建议 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 1 | `ZDIFF` | `Zdiff` | `-3` | `MovableKeys, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 2 | `ZDIFFSTORE` | `Zdiffstore` | `-4` | `DenyOom, MovableKeys, Write` | 待在 dispatcher.rs 中接入 |
| 3 | `ZEXPIRE` | `Zexpire` | `-6` | `DenyOom, Fast, Write` | 待在 dispatcher.rs 中接入 |
| 4 | `ZEXPIREAT` | `Zexpireat` | `-6` | `DenyOom, Fast, Write` | 待在 dispatcher.rs 中接入 |
| 5 | `ZEXPIRETIME` | `Zexpiretime` | `-5` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 6 | `ZINTER` | `Zinter` | `-3` | `MovableKeys, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 7 | `ZINTERCARD` | `Zintercard` | `-3` | `MovableKeys, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 8 | `ZINTERSTORE` | `Zinterstore` | `-4` | `DenyOom, MovableKeys, Write` | 待在 dispatcher.rs 中接入 |
| 9 | `ZLEXCOUNT` | `Zlexcount` | `4` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 10 | `ZMPOP` | `Zmpop` | `-4` | `MovableKeys, Write` | 待在 dispatcher.rs 中接入 |
| 11 | `ZMSCORE` | `Zmscore` | `-3` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 12 | `ZPERSIST` | `Zpersist` | `-5` | `Fast, Write` | 待在 dispatcher.rs 中接入 |
| 13 | `ZPEXPIRE` | `Zpexpire` | `-6` | `DenyOom, Fast, Write` | 待在 dispatcher.rs 中接入 |
| 14 | `ZPEXPIREAT` | `Zpexpireat` | `-6` | `DenyOom, Fast, Write` | 待在 dispatcher.rs 中接入 |
| 15 | `ZPEXPIRETIME` | `Zpexpiretime` | `-5` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 16 | `ZPOPMAX` | `Zpopmax` | `-2` | `Fast, Write` | 待在 dispatcher.rs 中接入 |
| 17 | `ZPOPMIN` | `Zpopmin` | `-2` | `Fast, Write` | 待在 dispatcher.rs 中接入 |
| 18 | `ZPTTL` | `Zpttl` | `-5` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 19 | `ZRANDMEMBER` | `Zrandmember` | `-2` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 20 | `ZRANGEBYLEX` | `Zrangebylex` | `-4` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 21 | `ZRANGEBYSCORE` | `Zrangebyscore` | `-4` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 22 | `ZRANGESTORE` | `Zrangestore` | `-5` | `DenyOom, Write` | 待在 dispatcher.rs 中接入 |
| 23 | `ZREMRANGEBYLEX` | `Zremrangebylex` | `4` | `Write` | 待在 dispatcher.rs 中接入 |
| 24 | `ZREMRANGEBYRANK` | `Zremrangebyrank` | `4` | `Write` | 待在 dispatcher.rs 中接入 |
| 25 | `ZREMRANGEBYSCORE` | `Zremrangebyscore` | `4` | `Write` | 待在 dispatcher.rs 中接入 |
| 26 | `ZREVRANGE` | `Zrevrange` | `-4` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 27 | `ZREVRANGEBYLEX` | `Zrevrangebylex` | `-4` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 28 | `ZREVRANGEBYSCORE` | `Zrevrangebyscore` | `-4` | `ReadOnly` | 待在 dispatcher.rs 中接入 |
| 29 | `ZTTL` | `Zttl` | `-5` | `Fast, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 30 | `ZUNION` | `Zunion` | `-3` | `MovableKeys, ReadOnly` | 待在 dispatcher.rs 中接入 |
| 31 | `ZUNIONSTORE` | `Zunionstore` | `-4` | `DenyOom, MovableKeys, Write` | 待在 dispatcher.rs 中接入 |

## 六、分类 D：Garnet 中存在但在 wedb_resp 中尚未收录的命令
**无缺失（0 个）！** wedb_resp 对 Microsoft Garnet 官方规范及底层 RespCommand.cs 实现了 **100% 完整收录 (368/368，数值完全对齐)**。

## 七、架构重构与整改落地行动项 (Action Items)

依据 `.agents/skills/code_review/SKILL.md` 与 `.agents/skills/rust_review/SKILL.md` 代码审查规范，建议立即采取以下优化措施：

### 1. 立即修复子命令分发 match 目标与参数偏移
在 `wedb_server/src/dispatcher.rs` 中，对所有带有子命令的主命令补齐枚举变体匹配，并正确处理参数偏移：
```rust
RespCommand::CLIENT
| RespCommand::CLIENT_ID
| RespCommand::CLIENT_GETNAME
| RespCommand::CLIENT_SETNAME
| RespCommand::CLIENT_INFO
| RespCommand::CLIENT_LIST
| RespCommand::CLIENT_KILL
| RespCommand::CLIENT_SETINFO
| RespCommand::CLIENT_UNBLOCK => {
    Self::handle_client(session, cmd, args, buf);
}
```
在 `handle_client` 内部：
```rust
let is_id = cmd == RespCommand::CLIENT_ID
    || (cmd == RespCommand::CLIENT && args.first().is_some_and(|s| s.eq_ignore_ascii_case(b"ID")));
let is_getname = cmd == RespCommand::CLIENT_GETNAME
    || (cmd == RespCommand::CLIENT && args.first().is_some_and(|s| s.eq_ignore_ascii_case(b"GETNAME")));
```
同理对 `CONFIG`、`COMMAND` 进行补齐与防偏移重构。

### 2. 修正 UNSUBSCRIBE 语义与退订行为
移除 `if args.is_empty()` 的报错逻辑。当 `args.is_empty()` 时，退订当前会话的所有订阅频道并向客户端返回成功回复，符合 Redis 官方协议规范。

### 3. 统一参数长度与语法校验函数
将 `validate_command_syntax` 提取为公共的零成本内联校验函数（或静态函数表），无论是否在 MULTI 事务中，均在 `dispatch` 最前置阶段完成 Arity 与格式校验，确保行为一致。对于定长参数命令严格使用 `args.len() == N` 校验。

### 4. 接入 wedb_blocking 阻塞队列支持
`wedb_blocking` 已经实现了 `BLPOP`、`BRPOP`、`BLMOVE`、`BLMPOP`、`BZMPOP`、`BZPOPMAX`、`BZPOPMIN` 的异步观察者（Observer）和事件分发器（Broker），应在 `dispatcher.rs` 中正式分发这些命令并对接异步等待机制。
