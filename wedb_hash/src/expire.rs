use bitcode::{Decode, Encode};

/// 字段过期选项 (HEXPIRE options)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Encode, Decode)]
pub struct ExpireOpt {
  /// 仅当未设置过期时设置 (NX)
  pub nx: bool,
  /// 仅当已设置过期时设置 (XX)
  pub xx: bool,
  /// 仅当新过期时间大于当前过期时间时设置 (GT)
  pub gt: bool,
  /// 仅当新过期时间小于当前过期时间时设置 (LT)
  pub lt: bool,
}

impl ExpireOpt {
  pub const NONE: Self = Self {
    nx: false,
    xx: false,
    gt: false,
    lt: false,
  };
}

/// 字段过期操作返回状态码 (对标 Garnet ExpireResult)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Encode, Decode)]
#[repr(i32)]
pub enum ExpireResult {
  /// 字段不存在
  KeyNotFound = -2,
  /// 字段未设置过期
  NoExpirationSet = -1,
  /// 过期条件不满足 (例如 NX/XX/GT/LT 冲突)
  ExpireConditionNotMet = 0,
  /// 成功设置过期
  Ok = 1,
  /// 字段已过期并被删除
  KeyAlreadyExpired = 2,
}
