# WeDB 命名空间与多数据库 (Namespace & DB) 物理隔离方案与变长编码规范

## 一、 概述与多层级隔离架构

在多租户（Multi-Tenancy）与企业级云原生存储场景中，系统需要同时支持租户级硬隔离与租户内部多数据库（Multi-DB）细粒度逻辑划分，核心架构诉求如下：
1. 双层键空间隔离体系（Namespace → DB → Key）：
   - 第一层：命名空间（Namespace, ns: u64），对应租户/企业/业务沙箱，是安全授权与物理隔离的最高边界。
   - 第二层：数据库编号（Database, db: u64），对应租户内部的多逻辑数据库（完全支持 Redis 原生 SELECT <db> 语义，db 上限高达 u64 全范围）。
   - 第三层：用户逻辑键（User Key），允许任意二进制字节流。不同租户之间、或同一租户的不同 DB 之间，逻辑键完全重叠且互不干扰。
2. 100% 二进制安全与类型穿透阻断：
   用户键允许包含任意二进制字节（包括 \x00、\x01、\xFF、Protobuf 序列化串等）。在接入层与存储层之间采用严格正交的物理键前缀编码，从数学顶层杜绝任何类型混淆与键穿透。
3. 彻底消除魔法字符串与 ASCII 冒号（独立 `bftag.rs` 纯 1 字节前缀）：
   严格遵循项目规范，所有内部物理键与系统键彻底移除 "__wedb:..." 及冒号 ":" 等字符串前缀。BfTree 系统元数据与业务子键全面迁移至独立模块 `wedb_record::bftag::BfTag`，采用 1 字节紧凑枚举直存（0 堆分配、0 额外包装），系统与业务各预留 32 个槽位，物理距离充足。
4. 集合打平子键与全库统一物理前缀：
   全库 HLog 物理键均采用 `[NsVarint] + [DbVarint] + [KeyTag: 1B] + [Payload]` 统一规范。集合打平子键同样包含租户与 DB 前缀，彻底杜绝单字节标签二义性冲突；而集合元素生命周期与版本控制由全局唯一 `key_id` 与 `version` 保证。
5. 命名空间与数据库前缀先行与双 OPPV 严格保序（Namespace-First & Dual-OPPV）：
   物理键首部恒定为 `[NsVarint] + [DbVarint]`，紧随 1 字节 `KeyTag`。普通字符串（String）采用专属前缀 `KeyTag::String (0x00)`，集合元数据采用 `KeyTag::Meta (0x01)`。
   同会话的所有物理键共享完全一致的 `session_prefix = [NsVarint, DbVarint]`。热路径扫描仅需单次 SIMD `strip_prefix(session_prefix)` 即可直接获取 `KeyTag` 与用户键，零 Varint 解码开销，彻底杜绝长度探测 Hack。
6. 完备的生命周期与命令协同：
   - 普通租户账号：强制锁定在分配的 Namespace（Some(N)）。默认活跃数据库为 db 0，并允许执行 SELECT <db>（db 为任意 u64）在自身租户内自由切换数据库。严禁越权切换至其他租户。
   - 系统超管账号（None）：拥有全库视界。默认活跃命名空间为 1，可通过专属指令或 SELECT 自由切换命名空间与数据库，实现跨租户运维巡检与单点修复。

---

## 二、 紧凑保序无前缀重叠变长编码 (Prefix-Free Varint / OPPV)

### 1. 变长编码设计原理

为同时支撑 Namespace (u64) 与 DB (u64) 的极致空间压缩、自定界与大端字典序保序，采用基于首字节高位一元码（Unary Prefix）的自定界变长编码 OPPV：

| 数值范围 (ns 或 db) | 占用字节 | 首字节高位前缀 | 编码结构 (二进制) | 负载位数与偏移量 |
| :--- | :--- | :--- | :--- | :--- |
| `0 <= val < 128` | 1 字节 | `0...` (最高位为 0) | `[0xxxxxxx]` | 7 位负载（直接存储数值） |
| `128 <= val < 16,512` | 2 字节 | `10...` (前两位为 10) | `[10xxxxxx, xxxxxxxx]` | 14 位负载，偏移 `val - 128` |
| `16,512 <= val < 2,113,664` | 3 字节 | `110...` (前三位为 110) | `[110xxxxx, xxxxxxxx, xxxxxxxx]` | 21 位负载，偏移 `val - 16,512` |
| `2,113,664 <= val < 270,549,120` | 4 字节 | `1110...` (前四位为 1110) | `[1110xxxx, 24位负载...]` | 28 位负载，偏移 `val - 2,113,664` |
| `>= 270,549,120` | 9 字节 | `11111111` (0xFF) | `[0xFF, 8字节原始大端序 u64]` | 满 64 位负载，大端保序直存 |

### 2. 数学严密性与自定界判定

1. 长度单步位运算判定（Zero-Backtracking）：
   - 首字节 `< 0x80`：恒为 1 字节；
   - 首字节 `0x80 ..= 0xBF`：恒为 2 字节；
   - 首字节 `0xC0 ..= 0xDF`：恒为 3 字节；
   - 首字节 `0xE0 ..= 0xEF`：恒为 4 字节；
   - 首字节 `== 0xFF`：恒为 9 字节；
   - 解码器仅凭首字节即可立即确定变长数值占用长度，先查首字节获取准确预期长度，杜绝模式匹配中的切片截断与误报错。
2. 字典序严格保序性（Order-Preserving）：
   - 跨阶比较：阶数较低的区间，其首字节高位包含 0 的位置更靠前，在二进制字典序上严格小于高阶首字节（`0x7F < 0x80`, `0xBF < 0xC0`, `0xDF < 0xE0`, `0xEF < 0xFF`）。
   - 同阶比较：同阶内前缀相同，有效负载按大端序（Big-Endian）连续单调递增。因此恒有 a < b 等价于 bytes(a) < bytes(b)。
3. 规范性拦截与上限约束：
   - 9 字节编码解出值若小于 270,549,120，强制拒绝并返回 `Error::NonCanonicalEncoding`，严禁多重二进制假名别名注入。
   - 首字节处于 `0xF0 ..= 0xFE` 视为未定义前缀，直接返回 `Error::NonCanonicalEncoding`。
   - 定义 `MAX_TENANT_NAMESPACE = u64::MAX - 1024`，租户 ID 不得超过此上限。

---

## 三、 全局物理键排布规范 (双引擎正交分层架构)

### 1. 顶层键与打平子键的正交分级设计

全库所有物理键采用命名空间先行（Namespace-First）的全局统一规范，由底层的两个存储引擎分工承载：
- **HybridLog (HLog + HashIndex)**：
  - 物理键统一排布结构：`[NsVarint] + [DbVarint] + [KeyTag: 1B] + [Payload]`。
  - 双变长编码 OPPV 位于最前，形成 `ns → db → tag → payload` 的自然大端字典序保序空间。
  - 普通用户字符串键：`[NsVarint] + [DbVarint] + [0x00] + [user_key]`。
  - 集合元数据键（Meta）：`[NsVarint] + [DbVarint] + [0x01] + [user_key]`。
  - 集合打平子键（Hash Field、Set Member、List Chunk 等）：
    排布为 `[NsVarint] + [DbVarint] + [KeyTag (0x02..=0x08)] + [key_id: 8B] + [version: 8B] + [subkey_payload]`。
    每个会话的所有数据共享同一个 `session_prefix = [NsVarint, DbVarint]`，实现热路径单次 SIMD 比对剥离。
- **BfTree (磁盘块级按序 B 树)**：
  - 首字节固定为 `BfTag` 单字节枚举（定义于 `wedb_record::bftag`）。
  - 键空间划分为两大区间：
    1. **业务有序数据区（`0 ..= 31`，预留 32 个槽位）**：
       - `0`: `ZMember`，结构 `[0x00, key_id: 8B, version: 8B, member]`
       - `1`: `ZScore`，结构 `[0x01, key_id: 8B, version: 8B, score: 8B, member]`
       - `2 ..= 31`：预留给未来业务有序结构（如 Geo, Vector, Stream）
    2. **系统元数据区（`32 ..= 63`，预留 32 个槽位）**：
       - 从 `32` (0x20) 开始分配，每张系统表独占 1 字节前缀，物理键排布为 `[BfTag: 1B] + [payload]`，单指令高速判别（`tag >= 32` 即为系统元数据）。

### 2. 物理编码全景对照表

| 逻辑数据类型 | 类型标签 | 完整物理键组成 (方案 A) | 典型前缀长度 | 底层存储引擎 | 穿透与隔离机制 |
| :--- | :--- | :--- | :--- | :--- | :--- |
| 普通字符串 (String) | `KeyTag::String (0x00)` | `[NsVarint] + [DbVarint] + [0x00] + [user_key]` | 3 字节 (ns<128, db<128) | HybridLog | Tag 专属隔离，双 OPPV 隔离 |
| 集合元数据 (Meta) | `KeyTag::Meta (0x01)` | `[NsVarint] + [DbVarint] + [0x01] + [user_key]` | 3 字节 (ns<128, db<128) | HybridLog | Tag 专属隔离，双 OPPV 隔离 |
| 哈希字段 (Hash Field) | `KeyTag::Hash (0x02)` | `[NsVarint] + [DbVarint] + [0x02] + [key_id: 8B] + [version: 8B] + [field]` | 19 字节 (含 2B ns/db) | HybridLog | 全局 key_id + 会话前缀双重隔离 |
| 无序集成员 (Set Member) | `KeyTag::Set (0x03)` | `[NsVarint] + [DbVarint] + [0x03] + [key_id: 8B] + [version: 8B] + [member]` | 19 字节 (含 2B ns/db) | HybridLog | 全局 key_id + 会话前缀双重隔离 |
| 有序集成员 (ZSet Member) | `BfTag::ZMember (0)` | `[0x00] + [key_id: 8B] + [version: 8B] + [member]` | 17 字节定长头 | BfTree | 全局 key_id 物理隔离，0 膨胀 |
| 有序集分值 (ZSet Score) | `BfTag::ZScore (1)` | `[0x01] + [key_id: 8B] + [version: 8B] + [score: 8B] + [member]` | 25 字节定长头 | BfTree | 字典序分值保序排布 |
| 列表分块 (List Chunk) | `KeyTag::ListChunk (0x06)`| `[NsVarint] + [DbVarint] + [0x06] + [key_id: 8B] + [version: 8B] + [chunk_id: 4B]` | 23 字节 (含 2B ns/db) | HybridLog | 逻辑索引分块存储 |
| 哈希分块 (Hash Chunk) | `KeyTag::HashChunk (0x07)`| `[NsVarint] + [DbVarint] + [0x07] + [key_id: 8B] + [version: 8B] + [chunk_id: 4B]` | 23 字节 (含 2B ns/db) | HybridLog | 大哈希二级索引分块 |
| 集合分块 (Set Chunk) | `KeyTag::SetChunk (0x08)` | `[NsVarint] + [DbVarint] + [0x08] + [key_id: 8B] + [version: 8B] + [chunk_id: 4B]` | 23 字节 (含 2B ns/db) | HybridLog | 大集合二级索引分块 |
| 水位元数据 (NextNamespace) | `BfTag::NextNamespace (32)`| `[0x20]` | 1 字节 | BfTree | 物理键总长 1 字节，Val: 8B be |
| ACL 用户数据 (AclUser) | `BfTag::AclUser (33)` | `[0x21] + [username bytes]` | 1 字节头 | BfTree | 1 字节前缀，Val: bitcode |
| ACL 统计元数据 (AclMeta) | `BfTag::AclMeta (34)` | `[0x22]` | 1 字节 | BfTree | 物理键总长 1 字节，Val: 8B be |
| 集群拓扑元数据 (ClusterMeta)| `BfTag::ClusterMeta (35)`| `[0x23]` | 1 字节 | BfTree | 物理键总长 1 字节，Val: bitcode |
| 复制位点元数据 (ReplMeta) | `BfTag::ReplMeta (36)` | `[0x24]` | 1 字节 | BfTree | 物理键总长 1 字节，Val: 8B be |

### 3. BfTree 专属键标签定义 (`wedb_record::bftag`)

```rust
// wedb_record/src/bftag.rs

#[derive(
  Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord,
  strum::FromRepr, strum::Display, strum::AsRefStr, strum::IntoStaticStr,
  bitcode::Encode, bitcode::Decode,
)]
#[repr(u8)]
pub enum BfTag {
  // --- 业务有序数据区 (预留 32 个槽位: 0..=31) ---
  /// 有序集合成员索引 (0，物理键: [0x00, key_id: 8B, version: 8B, member])
  ZMember = 0,
  /// 有序集合分值索引 (1，物理键: [0x01, key_id: 8B, version: 8B, score: 8B, member])
  ZScore  = 1,

  // --- 系统内部元数据区 (预留 32 个槽位: 32..=63) ---
  /// 命名空间自增持久化水位 (32，物理键: [0x20], Val: 8B be u64)
  NextNamespace = 32,
  /// ACL 用户实体数据 (33，物理键: [0x21, username bytes], Val: bitcode)
  AclUser       = 33,
  /// ACL 用户计数元数据 (34，物理键: [0x22], Val: 8B be u64)
  AclMeta       = 34,
  /// 集群拓扑元数据 (35，物理键: [0x23], Val: bitcode)
  ClusterMeta   = 35,
  /// 复制位点元数据 (36，物理键: [0x24], Val: 8B be u64)
  ReplMeta      = 36,
}

impl BfTag {
  pub const TAG_LEN: usize = 1;
  pub const BUSINESS_TAG_MAX: u8 = 31;
  pub const SYSTEM_TAG_BASE: u8 = 32;
  pub const SYSTEM_TAG_MAX: u8 = 63;
  pub const STACK_KEY_CAP: usize = 64;

  #[inline(always)]
  pub const fn from_u8(val: u8) -> Option<Self> {
    Self::from_repr(val)
  }

  #[inline(always)]
  pub const fn as_u8(self) -> u8 {
    self as u8
  }

  /// 转换为静态字符串切片 (const fn)
  #[inline(always)]
  pub const fn as_str(self) -> &'static str {
    match self {
      Self::ZMember => "ZMember",
      Self::ZScore => "ZScore",
      Self::NextNamespace => "NextNamespace",
      Self::AclUser => "AclUser",
      Self::AclMeta => "AclMeta",
      Self::ClusterMeta => "ClusterMeta",
      Self::ReplMeta => "ReplMeta",
    }
  }

  #[inline(always)]
  pub const fn prefix(self) -> [u8; Self::TAG_LEN] {
    [self as u8]
  }

  /// 单指令判定是否为业务有序数据标签 (0..=31)
  #[inline(always)]
  pub const fn is_business(self) -> bool {
    (self as u8) <= Self::BUSINESS_TAG_MAX
  }

  /// 单指令判定是否为系统元数据 (32..=63)
  #[inline(always)]
  pub const fn is_system(self) -> bool {
    (self as u8) >= Self::SYSTEM_TAG_BASE
  }

  /// 判定是否为有序集合业务子键 (0, 1)
  #[inline(always)]
  pub const fn is_zset(self) -> bool {
    matches!(self, Self::ZMember | Self::ZScore)
  }

  /// 栈闭包零分配构造完整 BfTree 键 (<= 63B 子键全程零堆分配)
  #[inline]
  pub fn with_key<R>(self, sub_key: &[u8], f: impl FnOnce(&[u8]) -> R) -> R {
    let total_len = Self::TAG_LEN + sub_key.len();
    if total_len <= Self::STACK_KEY_CAP {
      let mut buf = [0u8; Self::STACK_KEY_CAP];
      buf[0] = self as u8;
      buf[1..total_len].copy_from_slice(sub_key);
      f(&buf[..total_len])
    } else {
      let mut vec = Vec::with_capacity(total_len);
      vec.push(self as u8);
      vec.extend_from_slice(sub_key);
      f(&vec)
    }
  }

  /// 构造拼接子标识的完整 BfTree 键 Vec
  #[inline]
  pub fn encode_key(self, sub_key: &[u8]) -> Vec<u8> {
    let total_len = Self::TAG_LEN + sub_key.len();
    let mut key = Vec::with_capacity(total_len);
    key.push(self as u8);
    key.extend_from_slice(sub_key);
    key
  }

  /// 从完整键中剥离单字节标签 (const fn)
  #[inline(always)]
  pub const fn strip_prefix<'a>(self, key: &'a [u8]) -> Option<&'a [u8]> {
    match key {
      [first, rest @ ..] if *first == self as u8 => Some(rest),
      _ => None,
    }
  }
}
```

### 4. 物理隔离与防穿透效果演示

- 场景 A：同租户在不同数据库使用相同的键 "config"
  - 租户 1 在 DB 0 写入 `SET "config" "V0"`：
    物理键为 `[0x00, 0x01, 0x00, b'c', b'o', b'n', b'f', b'i', b'g']`
  - 租户 1 在 DB 1 写入 `SET "config" "V1"`：
    物理键为 `[0x00, 0x01, 0x01, b'c', b'o', b'n', b'f', b'i', b'g']`
  - 判定结果：前两字节完全相同，第三字节分别为 0x00 与 0x01，物理键天然分离，互不影响。
- 场景 B：不同租户在相同数据库使用相同的键 "config"
  - 租户 2 在 DB 0 写入 `SET "config" "V2"`：
    物理键为 `[0x00, 0x02, 0x00, b'c', b'o', b'n', b'f', b'i', b'g']`
  - 判定结果：第二字节分别为 0x01 与 0x02，物理键天然分离。
- 场景 C：租户写入模拟内部 Hash 子键的极端二进制键
  - 租户 2 在 DB 0 写入 `SET "\x02\x00\x00\x00..." "evil"`：
    物理键为 `[0x00, 0x02, 0x00, 0x02, 0x00, 0x00, 0x00, ...]`
  - 系统真正的 Hash 子键：
    物理键为 `[0x02, 0x00, 0x00, 0x00, ...]`
  - 判定结果：租户键首字节锁定为 0x00，系统 Hash 子键首字节锁定为 0x02，首字节绝对正交截断，数学上 100% 杜绝类型伪造与越权穿透。

---

## 四、 多数据库命令与语义闭环 (SELECT, FLUSHDB, DBSIZE, SCAN)

### 1. 会话状态与 SELECT <db> 语义

每个客户端连接会话持有当前绑定的 Namespace 与当前活跃的 DB 编号：
- 普通租户会话（namespace = Some(N)）：
  - 锁定于租户命名空间 N，不可越权逃逸出命名空间。
  - 会话初始化时，默认进入 `db = 0`。
  - 支持执行 `SELECT <db>`：db 参数支持从 0 到 u64::MAX 的任意无符号 64 位整数。会话收到有效参数后将当前活跃数据库切换为该 db，并返回 `+OK`。
  - 标准 Redis 客户端连接建立时默认发送的 `SELECT 0` 得到 100% 原生支持与握手兼容。
- 系统超级管理员会话（namespace = None）：
  - 拥有全库视界。默认活跃上下文为 `ns = 1, db = 0`。
  - 可通过 `SELECT <db>` 切换数据库，亦可通过超管专用控制面指令切换目标 Namespace，兼顾全库跨租户全局运维与对任意租户任意 DB 的单点精确读写。

### 2. 多数据库核心命令行为规格表

| Redis 命令 | 普通租户 (ns = Some(N), active_db = D) 行为 | 系统超管 (ns = None, active_db = D) 行为 |
| :--- | :--- | :--- |
| `GET / SET / DEL` | 仅操作本沙箱 `[Tag, NsVarint(N), DbVarint(D), key]` | 操作超管当前活跃沙箱 `[Tag, NsVarint(active_ns), DbVarint(D), key]` |
| `SELECT <db>` | 自由切换当前租户沙箱内的活跃数据库 `active_db = db (u64)` | 切换超管当前活跃数据库上下文 |
| `KEYS / SCAN` | 默认仅扫描遍历当前 `(N, D)` 下的所有存活用户键 | 支持流式扫描全库所有租户，或按指定活跃沙箱过滤 |
| `DBSIZE` | 精确流式统计本租户当前数据库 `(N, D)` 的用户键总数，空间 O(1) | 统计全库总键数或当前超管活跃数据库键数 |
| `FLUSHDB` | 遍历当前 `(N, D)` 下的存活用户键并逐一追加墓碑记录（物理无序哈希表机制，不影响租户其他 DB） | 仅清空当前超管活跃沙箱数据库 `(active_ns, D)` |
| `FLUSHALL` | 清空当前租户命名空间下的所有数据库（逐键追加墓碑，清空 N 租户全库，不跨租户） | 允许清空系统全库所有数据（需最高权限确认） |

### 3. ZSet 生命周期与 BfTree 级联清理说明

- **存储权衡**：
  ZSet 子键（`BfTag::ZMember` 与 `BfTag::ZScore`）为了保持紧凑（0 空间膨胀），在 BfTree 中不携带 `NsVarint` 和 `DbVarint`，仅由全局唯一的 `key_id` 物理隔离。
- **清理路径**：
  在执行租户删除、`FLUSHDB` 或 `DEL <zset_key>` 时，底层无法依靠单一 BfTree 前缀扫描完成批量删除。系统必须先读取目标集合的 `Meta` 记录拿到其 `(key_id, version)`，再调用 `clear_bftree_zset(key_id, version)` 精准范围清除 BfTree 中的成员与分值索引。

---

## 五、 命名空间分配器架构与崩溃一致性 (NamespaceAllocator)

### 1. 分配器架构与单调持久化设计

命名空间 ID（ns）由底层控制面原子单调自增分配器统一管理，杜绝并发竞争覆盖与宕机 ID 回退碰撞：

```
┌────────────────────────────────────────────────────────┐
│             NamespaceAllocator (Control Plane)         │
│  - disk_watermark: Mutex<u64> (串行落盘临界区保护)     │
│  - current: AtomicU64 (内存无锁快照)                   │
└───────────────────────────┬────────────────────────────┘
                            │
              串行写落盘 (持久化 next_id = current_id + 1)
                            │
                            ▼
┌────────────────────────────────────────────────────────┐
│                BfTree 系统元数据持久化                 │
│  Key: [0x20] (BfTag::NextNamespace=32，定长仅 1 字节)  │
│  Val: next_id (8 字节大端序 u64，表示下一个可用分配值) │
└────────────────────────────────────────────────────────┘
```

1. 系统启动初始化：
   - 从 BfTree 查询 Key `[0x20]`。若不存在则初始化写入 2（预留 1 作为主业务库），并将数值 2 载入内存与互斥锁中。
   - 若存在则直接读取 8 字节大端序作为 `next_id`。
2. 自动分配模式（显式指定 `NS 0`）：
   - 在 `disk_watermark` 互斥保护下执行：
     分配当前 ID：`current_id = *disk_watermark`；
     计算下一水位：`next_watermark = current_id + 1`；
     将 `next_watermark` 同步写入 BfTree 落盘；
     落盘成功后更新内存原子变量 `current.store(next_watermark)` 并将 `current_id` 赋予新建租户。
   - 杜绝宕机重启碰撞：若分配 2 并成功持久化 3，即使随后发生断电，重启后系统加载的水位依然为 3，后续分配绝不重复。
3. 手动指定模式（`NS N, N >= 1`）：
   - 校验合规范围：`1 <= N <= MAX_TENANT_NAMESPACE`。
   - 若 `N >= current`，在锁保护下将 BfTree 与内存水位同步推进至 `N + 1`，杜绝后续自增发生碰撞。

---

## 六、 RESP 协议扩展语法与 ACL 用户流转

### 1. User 实体定义

```rust
// wedb_acl/src/user.rs

#[derive(Clone, Debug, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub struct User {
  pub name: String,
  pub enabled: bool,
  pub nopass: bool,
  pub passwords: Vec<AclPassword>,
  pub commands: CommandPermissionSet,
  pub allkeys: bool,
  pub key_patterns: Vec<KeyPattern>,
  pub allchannels: bool,
  pub channel_patterns: Vec<String>,
  /// 绑定的多租户命名空间 ID
  /// - None: 系统管理员全局视界 (default 用户、未指定 NS 或显式声明 NS none/all)
  /// - Some(N): 锁定绑定的租户沙箱 (N >= 1)
  pub namespace: Option<u64>,
}
```

### 2. ACL SETUSER 语法规格与防提权保护

在 `ACL SETUSER` 中支持 `NS <value>` 语法：

1. 普通租户创建子账号（防垂直提权）：
   - 若当前会话为普通租户（Some(N)），创建的新用户强制锁定继承 `namespace = Some(N)`。
   - 普通租户声明 `NS none` 或 `NS <other>` 立即被拦截并返回错误 `-ERR Permission denied: cannot grant namespace outside of the current tenant scope`。
2. 超管创建用户：
   - 未指定 NS 参数时，默认是超级用户（`namespace = None`，具备跨租户全局视界）。
   - 显式指定 `NS 0`：触发原子分配器分配专属独立新租户。
   - 显式指定 `NS <N>`：绑定指定租户沙箱空间。
   - 显式指定 `NS none` / `NS all`：明确赋予跨租户全局超管视界。

```redis
# 超管创建用户未指定 NS，默认是超级用户 (全局视界)
ACL SETUSER super_admin on >secret ~* +@all

# 超管分配独立新租户 (自动分配 ns = 2)
ACL SETUSER tenant_a on >secret ~* +@all NS 0

# 超管创建共享沙箱租户 (指定 ns = 100)
ACL SETUSER tenant_b on >secret ~* +@all NS 100

# 超管显式声明创建新系统管理员 (全局视界)
ACL SETUSER super_auditor on >secret ~* +@all NS none

# 普通租户即使不带 NS 创建子账号，系统强制锁定为本租户沙箱 (ns = 2)
AUTH tenant_a#2 secret
ACL SETUSER sub_worker on >subpwd ~* +@all
# -> sub_worker.namespace 严格锁定为 Some(2)
```

### 3. AUTH 登录凭据语法：`用户名#空间id`

用户身份 = `(名字空间, 用户名)` 复合主键，而登录凭据必须**完整携带二元身份**，
认证退化为单点查 O(1)（内存哈希 + 持久层点查），与全库用户规模彻底解耦：

| 凭据形式 | 归属空间 | 适用用户 |
| --- | --- | --- |
| `username` | 超管全局桶（`None`） | 仅超级用户（`ns none` / `default`） |
| `username#<N>` | 租户沙箱 `Some(N)` | 仅绑定了名字空间 N 的租户用户 |

强约束（实现见 `wedb_acl::parse_user_token`）：

1. **超级用户不带 `#`**：纯用户名仅在全局桶点查，与既有 `AUTH default` 完全兼容。
2. **租户用户必须携带 `#空间id`**：`N` 为数字空间 id（`1..=MAX_TENANT_NAMESPACE`）；
   绑定了名字空间的用户在全局桶不可见，省略 `#N` 或写错 `N` 一律 `WRONGPASS`，
   从语法上杜绝跨空间越权登录。
3. **用户名禁止包含 `#`**：`#` 是登录凭据的保留分隔符（创建用户时即强制校验），
   切分无歧义，无需任何转义规则。
4. `0` 为控制面自动分配保留值；`none`/`all` 属 `ACL SETUSER` 规则语法而非登录语法，均拒绝。
5. 凭据语法非法（空用户名、非法空间值）返回 `ERR`，与口令错误（`WRONGPASS`）严格区分。

```redis
# 超级用户：纯用户名登录（全局视界）
AUTH default secret
AUTH super_admin secret

# 租户用户：必须携带 #空间id（tenant_a 已绑定 ns = 2）
AUTH tenant_a#2 secret

# 租户用户省略 #空间id：只查全局桶，必然 WRONGPASS
AUTH tenant_a secret
# -> WRONGPASS invalid username-password pair or user is disabled.
```

---

## 七、 发布订阅（Pub/Sub）多租户物理隔离

1. 租户频道加签：
   - 普通租户（Some(N)）执行 `SUBSCRIBE channel` 或 `PUBLISH channel msg` 时，接入层透明对频道进行物理前缀加签：
     `channel_phy = [0x00] + [NsVarint(N)] + channel`
   - 租户间频道天然物理分离，避免消息串通窃听。标准 Redis 中 Pub/Sub 不受 SELECT db 影响，频道在同一租户各 DB 间共享，符合 Redis 原生行为。
2. 全局管理员监听：
   - 超管用户可选择订阅无前缀的全局系统广播，或通过显式加签前缀监听指定租户的频道进行运维审计。

---

## 八、 栈分配缓冲区与 CPU 缓存行优化 (TaggedKeyBuf)

### 1. 严格 64 字节单缓存行对齐 (Cache Line Fit)

在高频热点读写路径（`GET`, `SET`, `HGET` 等），为配合 `compio` 原生单线程单核模型发挥硬件极致吞吐，彻底消灭堆分配与跨缓存行内存抖动：
- 真实尺寸精细规整：在 64 位机器上枚举包含 1 字节辨别符并以 8 字节对齐。将内部栈数组容量规整为 `[u8; 62]`。
  `62B (数据) + 1B (len) + 1B (tag) = 64 字节`，配合 `#[repr(C, align(64))]`，总大小严格等于 64 字节，100% 锁定在单条 64B L1 Cache Line 内！
- 业务覆盖度：
  对于 `ns < 128` 且 `db < 128` 的绝大多数租户与数据库，物理前缀仅占 3 字节（`KeyTag: 1B + NsVarint: 1B + DbVarint: 1B`）。
  栈缓冲区可直接容纳长达 59 字节 的裸用户键（覆盖 95% 以上 Redis 实际业务键长），全程 0 堆分配、0 缓存行抖动。
  键长超过 59 字节时平滑回退至小向量堆分配。

### 2. 编解码器 Rust 核心实现参考

```rust
// wedb_record/src/ns_codec.rs

use std::ops::Deref;
use crate::error::{Error, Result};
use crate::tag::KeyTag;

/// 严格 64 字节单缓存行物理键缓冲区
#[repr(C, align(64))]
pub struct TaggedKeyBuf {
  inner: KeyBufRepr,
}

enum KeyBufRepr {
  /// 62 字节栈缓冲区 + 1 字节长度 + 1 字节鉴别符 = 64 字节
  Stack([u8; 62], u8),
  /// 超长键平滑回退到堆分配
  Heap(Vec<u8>),
}

impl Deref for TaggedKeyBuf {
  type Target = [u8];

  #[inline(always)]
  fn deref(&self) -> &Self::Target {
    match &self.inner {
      KeyBufRepr::Stack(buf, len) => unsafe {
        debug_assert!(*len <= 62);
        buf.get_unchecked(..*len as usize)
      },
      KeyBufRepr::Heap(vec) => vec.as_slice(),
    }
  }
}

// 编译期断言尺寸与对齐严格等于 64 字节
const _: () = assert!(core::mem::size_of::<TaggedKeyBuf>() == 64);
const _: () = assert!(core::mem::align_of::<TaggedKeyBuf>() == 64);

/// 紧凑保序无重叠双层变长编码器 (Dual-OPPV)
pub struct NamespaceDbCodec;

impl NamespaceDbCodec {
  pub const MAX_TENANT_NAMESPACE: u64 = u64::MAX - 1024;

  /// 计算单一 u64 变长编码占用字节数 (const fn)
  #[inline(always)]
  pub const fn varint_len(val: u64) -> usize {
    if val < 128 {
      1
    } else if val < 16_512 {
      2
    } else if val < 2_113_664 {
      3
    } else if val < 270_549_120 {
      4
    } else {
      9
    }
  }

  /// 编码单一 u64 为 OPPV 变长字节，返回实际写入字节数
  #[inline]
  pub fn encode_varint(val: u64, dst: &mut [u8]) -> usize {
    if val < 128 {
      dst[0] = val as u8;
      1
    } else if val < 16_512 {
      let offset = (val - 128) as u16;
      dst[0] = 0x80 | ((offset >> 8) as u8);
      dst[1] = (offset & 0xFF) as u8;
      2
    } else if val < 2_113_664 {
      let offset = (val - 16_512) as u32;
      dst[0] = 0xC0 | ((offset >> 16) as u8);
      dst[1] = ((offset >> 8) & 0xFF) as u8;
      dst[2] = (offset & 0xFF) as u8;
      3
    } else if val < 270_549_120 {
      let offset = (val - 2_113_664) as u32;
      dst[0] = 0xE0 | ((offset >> 24) as u8);
      dst[1] = ((offset >> 16) & 0xFF) as u8;
      dst[2] = ((offset >> 8) & 0xFF) as u8;
      dst[3] = (offset & 0xFF) as u8;
      4
    } else {
      dst[0] = 0xFF;
      dst[1..9].copy_from_slice(&val.to_be_bytes());
      9
    }
  }

  /// 单步自定界解码单一 OPPV 数值 (首字节预判长度，零回溯且报错真实精准)
  #[inline]
  pub fn decode_varint(slice: &[u8]) -> Result<(u64, usize)> {
    let first = *slice.first().ok_or(Error::BufferTooShort { expected: 1, actual: 0 })?;
    let expected_len = match first {
      0..=0x7F => 1,
      0x80..=0xBF => 2,
      0xC0..=0xDF => 3,
      0xE0..=0xEF => 4,
      0xFF => 9,
      _ => return Err(Error::NonCanonicalEncoding),
    };

    if slice.len() < expected_len {
      return Err(Error::BufferTooShort {
        expected: expected_len,
        actual: slice.len(),
      });
    }

    match expected_len {
      1 => Ok((first as u64, 1)),
      2 => {
        let offset = (((first & 0x3F) as u64) << 8) | (slice[1] as u64);
        Ok((128 + offset, 2))
      }
      3 => {
        let offset = (((first & 0x1F) as u64) << 16) | ((slice[1] as u64) << 8) | (slice[2] as u64);
        Ok((16_512 + offset, 3))
      }
      4 => {
        let offset = (((first & 0x0F) as u64) << 24)
          | ((slice[1] as u64) << 16)
          | ((slice[2] as u64) << 8)
          | (slice[3] as u64);
        Ok((2_113_664 + offset, 4))
      }
      9 => {
        let val = u64::from_be_bytes(slice[1..9].try_into().unwrap());
        if val < 270_549_120 {
          return Err(Error::NonCanonicalEncoding);
        }
        Ok((val, 9))
      }
      _ => unreachable!(),
    }
  }

  /// 构建方案 A 物理键缓冲区: [NsVarint] + [DbVarint] + [KeyTag: 1B] + [Payload]
  /// 优先利用预计算的会话前缀 (SessionPrefixBuf)，消除重复分支判定与中间栈数组搬运
  #[inline]
  pub fn encode_tagged_key(ns: u64, db: u64, tag: KeyTag, payload: &[u8]) -> TaggedKeyBuf {
    let prefix = SessionPrefixBuf::new(ns, db);
    Self::encode_with_session_prefix(&prefix, tag, payload)
  }

  /// 构造基于指定会话前缀切片的完整物理键缓冲区（复用前缀，零冗余）
  #[inline]
  pub fn encode_with_session_prefix(prefix: &[u8], tag: KeyTag, payload: &[u8]) -> TaggedKeyBuf {
    let prefix_len = prefix.len();
    let total_len = prefix_len + KeyTag::TAG_LEN + payload.len();
    if total_len <= STACK_KEY_CAP {
      let mut buf = [0u8; STACK_KEY_CAP];
      buf[..prefix_len].copy_from_slice(prefix);
      buf[prefix_len] = tag as u8;
      buf[prefix_len + KeyTag::TAG_LEN..total_len].copy_from_slice(payload);
      TaggedKeyBuf::from_stack(buf, total_len as u8)
    } else {
      let mut vec = Vec::with_capacity(total_len);
      vec.extend_from_slice(prefix);
      vec.push(tag as u8);
      vec.extend_from_slice(payload);
      TaggedKeyBuf::from_heap(vec)
    }
  }

  /// 从完整物理键中解码出 (ns, db, tag, payload)
  #[inline]
  pub fn decode_tagged_key(key: &[u8]) -> Result<(u64, u64, KeyTag, &[u8])> {
    let (ns, ns_len) = Self::decode_varint(key)?;
    let db_slice = &key[ns_len..];
    let (db, db_len) = Self::decode_varint(db_slice)?;
    let tag_slice = &db_slice[db_len..];
    let (&tag_byte, payload) = tag_slice.split_first().ok_or(Error::BufferTooShort {
      expected: 1,
      actual: 0,
    })?;
    let tag = KeyTag::try_from(tag_byte)?;
    Ok((ns, db, tag, payload))
  }

  /// 从物理键中快速剥离当前会话前缀，提取 (tag, payload)
  /// 热路径极速过滤：无需任何变长整型解码，底层为单次 SIMD 内存比对与单字节标签解析
  #[inline(always)]
  pub fn strip_session_prefix<'a>(
    key: &'a [u8],
    session_prefix: &[u8],
  ) -> Option<(KeyTag, &'a [u8])> {
    let rest = key.strip_prefix(session_prefix)?;
    let (&tag_byte, payload) = rest.split_first()?;
    let tag = KeyTag::from_u8(tag_byte)?;
    Some((tag, payload))
  }

  /// 从物理键中提取属于当前会话的存活用户逻辑键（String 或 Meta）
  /// 若物理键不属于当前会话，或属于集合内部打平子键，安全返回 None
  #[inline(always)]
  pub fn extract_live_user_key<'a>(
    key: &'a [u8],
    session_prefix: &[u8],
  ) -> Option<(KeyTag, &'a [u8])> {
    match Self::strip_session_prefix(key, session_prefix) {
      Some((tag @ (KeyTag::String | KeyTag::Meta), user_key)) => Some((tag, user_key)),
      _ => None,
    }
  }
}
```

---

## 九、 现有核心微 Crate 适配清单

1. `wedb_record`：
   - 引入 `wedb_record::bftag::BfTag` 独立模块，接管 BfTree 所有单字节紧凑标签。
   - `KeyTag` 维持 0x00..=0x08，专注 HLog 物理键标签。
   - 引入 `NamespaceDbCodec` 与 `TaggedKeyBuf`。
   - 在 `Error` 枚举中补充 `NonCanonicalEncoding` 错误类型。
2. `wedb_compact`：
   - 在识别失效集合及 Meta 记录时，通过 `NamespaceDbCodec::decode_tagged_key(key)` 精准解析出 `(ns, db, tag, payload)`；
   - 若 `tag == KeyTag::Meta`，读取元数据并更新活跃版本号；
   - 若 `tag` 为内部打平子键（`KeyTag::Hash..=KeyTag::SetChunk`），从 `payload` 零拷贝解析 `(key_id, sub_version)` 并判定是否废弃，消除对旧头部固定偏移与单字节猜测的假定。
3. `wedb_store`：
   - `StoreSession` 维护当前租户的命名空间与活跃数据库 `(namespace, active_db)` 及 19B 预计算 `session_prefix`；
   - 顶层通过 `session_string_key` 与 `session_meta_key` 统一调用 `NamespaceDbCodec::encode_with_session_prefix`；
   - `live_user_key`：基于 `NamespaceDbCodec::extract_live_user_key(key, session_prefix)`，单次 SIMD 剥离会话前缀。若标签为 `KeyTag::String` 或 `KeyTag::Meta`（且集合元素未清空）返回存活 `user_key`；内部打平子键或其他库/租户键瞬时过滤丢弃，彻底消除黑名单和长度探测 hack。
   - `FLUSHDB`：通过遍历当前沙箱用户键流式追加墓碑记录。
   - `DBSIZE`：按 `(ns, active_db)` 过滤统计存活键数。
4. `wedb_acl` 与 `wedb_server`：
   - `StoreAclStorage` 全面迁移使用 `BfTag::AclMeta`（物理键 `[0x22]` 即 34）与 `BfTag::AclUser`（前缀 `[0x21]` 即 33），0 堆分配点查与范围扫描；
   - `User` 实体与持久化补充 `namespace: Option<u64>`；
   - `ServerSession` 维护 `active_db: u64`（默认 0），接收并处理 `SELECT <db>` 命令，动态同步到底层的 `StoreSession`；
   - `NamespaceAllocator` 保证分配持久化 `next_id` 至 `[0x20]`（即 32），单调无反转、无重启碰撞。

---

## 十、 架构演进全景图

```
┌─────────────────────────────────────────────────────────────────────────┐
│                               wedb_server                               │
│  - Session 鉴权注入 namespace: Option<u64>                               │
│  - 维护 session.active_db: u64，解析并执行 SELECT <db> (支持全量 u64)    │
│  - Pub/Sub 频道统一加签隔离 [0x00, NsVarint(N), channel]                 │
│  - ACL GETUSER / SETUSER 暴露 NS 语法并执行垂直防提权校验                │
│  - StoreAclStorage 纯 1 字节 BfTag 点查与流式扫描 (AclMeta: 34, User: 33)│
└────────────────────┬────────────────────────────────┬───────────────────┘
                     │                                │
                     ▼                                ▼
┌────────────────────────────────┐       ┌────────────────────────────────┐
│            wedb_acl            │       │           wedb_store           │
│ - User.namespace 实体扩展      │       │ - StoreSession 维护 (ns, db)   │
│ - ACL Parser 支持 NS 参数解析  │       │ - Namespace-First 双 OPPV 包装 │
│ - NamespaceAllocator 串行落盘  │       │ - live_user_key (ns, db) 过滤  │
│   持久化 next_id 至 [0x20]     │       │ - FLUSHDB / DBSIZE 沙箱精准隔离│
└────────────────────┬───────────┘       └────────────────┬───────────────┘
                     │                                    │
                     └─────────────────┬──────────────────┘
                                       ▼
┌─────────────────────────────────────────────────────────────────────────┐
│                               wedb_record                               │
│  - 统一物理键: [NsVarint] + [DbVarint] + [KeyTag: 1B] + [Payload]        │
│  - KeyTag::String (0x00) 与 KeyTag::Meta (0x01) 强类型正交隔离           │
│  - bftag::BfTag (1 字节紧凑枚举):                                       │
│    ├─ 业务有序区 (0..=31，预留 32 槽): ZMember(0), ZScore(1)            │
│    └─ 系统元数据区 (32..=63，预留 32 槽): Watermark(32), Acl(33,34)     │
│  - NamespaceDbCodec: 紧凑保序无重叠双层变长编码器 (Dual-OPPV)           │
│  - TaggedKeyBuf: 64B 单缓存行栈优先缓冲区 (user_key <= 59B 零分配)      │
│  - SessionPrefixBuf: 19B 预计算固定栈缓冲，热路径 SIMD 单次剥离         │
└─────────────────────────────────────────────────────────────────────────┘
```
