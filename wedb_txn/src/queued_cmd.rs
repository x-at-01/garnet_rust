use bytes::Bytes;
use wedb_resp::RespCommand;

use super::{
  error::{Error, Result},
  key_entry::LockType,
};

/// 事务内部排队暂存的命令
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedCommand {
  /// 命令枚举操作码
  pub cmd: RespCommand,
  /// 命令参数列表
  pub args: Vec<Bytes>,
  /// 是否为写修改操作
  pub is_write: bool,
}

#[derive(bitcode::Encode, bitcode::Decode)]
struct QueuedCommandWire {
  cmd: String,
  args: Vec<Vec<u8>>,
  is_write: bool,
}

impl QueuedCommand {
  /// 构造新的排队命令
  #[inline]
  pub fn new(cmd: RespCommand, args: Vec<Bytes>) -> Self {
    let is_write = cmd.is_write();
    Self {
      cmd,
      args,
      is_write,
    }
  }

  /// 使用 bitcode 编码为二进制字节向量
  pub fn encode_bitcode(&self) -> Vec<u8> {
    let wire = QueuedCommandWire {
      cmd: self.cmd.as_str().to_string(),
      args: self.args.iter().map(|b| b.to_vec()).collect(),
      is_write: self.is_write,
    };
    bitcode::encode(&wire)
  }

  /// 从 bitcode 二进制切片解码还原
  pub fn decode_bitcode(src: &[u8]) -> Result<Self> {
    let wire: QueuedCommandWire =
      bitcode::decode(src).map_err(|e| Error::Bitcode(e.to_string()))?;
    let cmd = RespCommand::from_slice(wire.cmd.as_bytes()).unwrap_or(RespCommand::None);
    let args = wire.args.into_iter().map(Bytes::from).collect();
    Ok(Self {
      cmd,
      args,
      is_write: wire.is_write,
    })
  }

  /// 检查命令是否允许在事务内部入队
  ///
  /// 对标 Garnet `AllowedInTxn` 元数据：事务控制命令与 `NoMulti` 管理类命令
  /// （含全部 CLUSTER 子命令）一律禁止入队，入队即中止事务。
  pub const fn is_allowed_in_txn(cmd: RespCommand) -> bool {
    !matches!(
      cmd,
      RespCommand::NONE
        | RespCommand::MULTI
        | RespCommand::EXEC
        | RespCommand::DISCARD
        | RespCommand::WATCH
        | RespCommand::WATCHMS
        | RespCommand::WATCHOS
        | RespCommand::UNWATCH
        | RespCommand::SWAPDB
        // NoMulti 管理类命令（对标 Garnet RespCommandsInfo 的 NoMulti 标记）
        | RespCommand::ASYNC
        | RespCommand::RUNTXP
        | RespCommand::COMMITAOF
        | RespCommand::SAVE
        | RespCommand::EXPDELSCAN
        | RespCommand::REGISTERCS
    ) && !cmd.is_cluster_subcommand()
  }

  /// 解析 `args[start]` 位置的 numkeys 并返回其后的键切片（非法输入返回空切片）
  #[inline]
  fn keys_after_numkeys(args: &[Bytes], start: usize) -> &[Bytes] {
    let Some(num) = args.get(start) else {
      return &[];
    };
    if num.is_empty() {
      return &[];
    }
    let mut n = 0usize;
    for &d in num.iter() {
      if !d.is_ascii_digit() {
        return &[];
      }
      n = n.saturating_mul(10).saturating_add((d - b'0') as usize);
    }
    let from = start + 1;
    let to = from.saturating_add(n).min(args.len());
    args.get(from..to).unwrap_or(&[])
  }

  /// 零分配遍历该命令涉及的所有键及加锁类型
  ///
  /// 与 Garnet 元数据驱动的 `LockKeys`（`TxnKeyManager.cs`，按 KeySpecifications 的
  /// RO 标志决定共享 / 排他）等价：读键共享锁、写键排他锁。
  ///
  /// 与 C# 的差异：C# 逐命令声明键规格；本实现按命令族归类——
  /// 显式多键命令精确登记，其余数据命令套用"首参数为键"的单键模式，
  /// 非数据命令（管理 / 连接 / 事务控制 / 普通发布订阅等）对标 Garnet 无键规格，不加锁。
  #[inline]
  pub fn for_each_key<F>(&self, mut f: F)
  where
    F: FnMut(&Bytes, LockType),
  {
    let count = self.args.len();
    if count == 0 {
      return;
    }

    match self.cmd {
      // 批量键读命令：所有参数皆为共享读锁（含 SDIFF/SINTER/SUNION 等多键只读命令；
      // 以及 SSUBSCRIBE 分片订阅频道——对标 Garnet 键规格 Index=1 LastKey=-1 RO）
      RespCommand::MGET
      | RespCommand::SDIFF
      | RespCommand::SINTER
      | RespCommand::SUNION
      | RespCommand::Ssubscribe => {
        for k in &self.args {
          f(k, LockType::Shared);
        }
      }

      // PFCOUNT：多键基数估计。C# 键规格标记 RW（稀疏编码转密集的内部表示变异
      // 会写键并传播副本），须加排他锁防止并发变异同一键的编码（对标 Garnet
      // PFCOUNT keyspec "RW, Access"）
      RespCommand::PFCOUNT => {
        for k in &self.args {
          f(k, LockType::Exclusive);
        }
      }

      // 批量键删除或存在性检查命令
      RespCommand::DEL | RespCommand::UNLINK | RespCommand::EXISTS => {
        let lock = if self.is_write {
          LockType::Exclusive
        } else {
          LockType::Shared
        };
        for k in &self.args {
          f(k, lock);
        }
      }

      // 键值对批量写命令：偶数位置参数为键，排他写锁
      RespCommand::MSET | RespCommand::MSETNX => {
        for k in self.args.iter().step_by(2) {
          f(k, LockType::Exclusive);
        }
      }

      // 双键写操作（源键与目标键均需排他写锁）
      RespCommand::RENAME
      | RespCommand::RENAMENX
      | RespCommand::RPOPLPUSH
      | RespCommand::LMOVE
      | RespCommand::BLMOVE
      | RespCommand::BRPOPLPUSH
      | RespCommand::SMOVE => {
        f(&self.args[0], LockType::Exclusive);
        if count > 1 {
          f(&self.args[1], LockType::Exclusive);
        }
      }

      // 目标-源型存储命令：首个参数为目标写键（排他锁），其余全部为源读键（共享锁）
      // SDIFFSTORE dst src ...、PFMERGE destkey src [src ...]
      RespCommand::SDIFFSTORE
      | RespCommand::SINTERSTORE
      | RespCommand::SUNIONSTORE
      | RespCommand::PFMERGE => {
        f(&self.args[0], LockType::Exclusive);
        for src_key in &self.args[1..] {
          f(src_key, LockType::Shared);
        }
      }

      // 有序集合存储型命令：目标键排他 + 按 numkeys 提取源键（共享锁）
      RespCommand::ZDIFFSTORE | RespCommand::ZINTERSTORE | RespCommand::ZUNIONSTORE => {
        f(&self.args[0], LockType::Exclusive);
        for src_key in Self::keys_after_numkeys(&self.args, 1) {
          f(src_key, LockType::Shared);
        }
      }

      // 目标-源型存储命令：ZRANGESTORE dst src min max、GEOSEARCHSTORE dst src ...选项
      RespCommand::ZRANGESTORE | RespCommand::GEOSEARCHSTORE => {
        if count > 1 {
          f(&self.args[0], LockType::Exclusive);
          f(&self.args[1], LockType::Shared);
        }
      }

      // 位运算命令：BITOP op destkey srckey [srckey ...]（操作符占据首参数）
      RespCommand::Bitop
      | RespCommand::BitopAnd
      | RespCommand::BitopOr
      | RespCommand::BitopXor
      | RespCommand::BitopNot
      | RespCommand::BitopDiff => {
        if count > 1 {
          f(&self.args[1], LockType::Exclusive);
          for src_key in &self.args[2..] {
            f(src_key, LockType::Shared);
          }
        }
      }

      // 地理空间查询：GEORADIUS key lon lat radius unit [STORE dst | STOREDIST dst]。
      // 主键共享读（对标 Garnet RO 规格）；带 STORE / STOREDIST 选项时目标键排他写
      // （对标 Garnet STORE / STOREDIST 关键字键规格 OW），杜绝向存储键写入的欠锁
      RespCommand::Georadius | RespCommand::Georadiusbymember => {
        f(&self.args[0], LockType::Shared);
        for opt in self.args[1..].windows(2) {
          if opt[0].eq_ignore_ascii_case(b"STORE") || opt[0].eq_ignore_ascii_case(b"STOREDIST") {
            f(&opt[1], LockType::Exclusive);
          }
        }
      }

      // 阻塞弹出命令：除末尾超时参数外的所有键（弹出即写，排他锁）
      RespCommand::BLPOP | RespCommand::BRPOP | RespCommand::BZPOPMIN | RespCommand::BZPOPMAX => {
        for k in &self.args[..count - 1] {
          f(k, LockType::Exclusive);
        }
      }

      // MIGRATE host port key db timeout [COPY] [REPLACE] [KEYS key ...]：
      // 主变体迁移键位于第 3 参数（对标 Garnet 键规格 FirstKey=3），排他写锁；
      // KEYS 关键字变体（对标 Garnet BeginSearchKeyword "KEYS" StartFrom=-2，
      // 自尾部向前定位关键字）其后的全部键同为排他写锁。
      // get(3..) 防御残缺参数：args 不足 3 个时范围切片会越界 panic（网络输入可达）
      RespCommand::MIGRATE => {
        if count > 2 {
          f(&self.args[2], LockType::Exclusive);
        }
        if let Some(tail) = self.args.get(3..)
          && let Some(keys_pos) = tail.iter().rposition(|a| a.eq_ignore_ascii_case(b"KEYS"))
        {
          for k in &tail[keys_pos + 1..] {
            f(k, LockType::Exclusive);
          }
        }
      }

      // LMPOP numkeys key [key ...]、ZMPOP numkeys key [key ...]：按 numkeys 提取，弹出即写（排他锁）
      RespCommand::LMPOP | RespCommand::ZMPOP => {
        for k in Self::keys_after_numkeys(&self.args, 0) {
          f(k, LockType::Exclusive);
        }
      }

      // 阻塞变体与脚本命令：首参数（timeout / script）之后为 numkeys，
      // 按其提取键；弹出与脚本写访问保守加排他写锁
      RespCommand::BLMPOP | RespCommand::BZMPOP | RespCommand::EVAL | RespCommand::EVALSHA => {
        for k in Self::keys_after_numkeys(&self.args, 1) {
          f(k, LockType::Exclusive);
        }
      }

      // numkeys 多键读命令：numkeys key [key ...] [修饰参数]，全部共享读锁
      RespCommand::Zdiff
      | RespCommand::Zinter
      | RespCommand::Zunion
      | RespCommand::Zintercard
      | RespCommand::Sintercard => {
        for k in Self::keys_after_numkeys(&self.args, 0) {
          f(k, LockType::Shared);
        }
      }

      // 双键只读比较命令：两个键均为共享读锁
      RespCommand::Lcs => {
        f(&self.args[0], LockType::Shared);
        if count > 1 {
          f(&self.args[1], LockType::Shared);
        }
      }

      // 对象内省命令：OBJECT <sub> key 的存储键位于第 2 参数（对标 Garnet
      // OBJECT|ENCODING 键规格 Index=2 RO），首参为子命令 token，
      // 若套用默认单键模式将把 token 误登记为幻影键
      RespCommand::ObjectEncoding
      | RespCommand::ObjectFreq
      | RespCommand::ObjectIdletime
      | RespCommand::ObjectRefcount => {
        if count > 1 {
          f(&self.args[1], LockType::Shared);
        }
      }

      // CUSTOMOBJECTSCAN indexregx slotsize：自定义管理扫描命令，无存储键参与
      // （对标 Garnet 自定义命令无键规格），正则等参数不得误登记
      RespCommand::Coscan => {}

      // 数据命令默认单键模式：首参数为键，写排他 / 读共享
      _ if self.cmd.is_data() => {
        let lock = if self.is_write {
          LockType::Exclusive
        } else {
          LockType::Shared
        };
        f(&self.args[0], lock);
      }

      // 非数据命令（PING/ECHO 等连接管理、SELECT 切库、普通发布订阅、SCRIPT / ACL / CLIENT 等）：
      // 无存储键参与，对标 Garnet 无键规格的命令，不登记任何键
      _ => {}
    }
  }

  /// 提取该命令涉及的所有键及其加锁类型
  pub fn extract_keys(&self) -> Vec<(Bytes, LockType)> {
    let mut keys = Vec::with_capacity(self.args.len().min(4));
    self.for_each_key(|k, lock| keys.push((k.clone(), lock)));
    keys
  }
}
