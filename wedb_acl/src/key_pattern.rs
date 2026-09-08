use crate::glob::glob_match;

/// 键访问模式规则，支持只读、只写与读写分离控制
#[derive(Clone, Debug, PartialEq, Eq, bitcode::Encode, bitcode::Decode)]
pub struct KeyPattern {
  /// Glob 键匹配模式（如 `*`、`cache:*`、`user:????` 等）
  pub pattern: String,
  /// 是否允许读取键（GET, MGET, EXISTS 等）
  pub read: bool,
  /// 是否允许写入键（SET, DEL, HSET 等）
  pub write: bool,
}

impl KeyPattern {
  /// 创建全权限读写键模式 (`~*`)
  #[inline]
  pub fn all() -> Self {
    Self {
      pattern: "*".to_string(),
      read: true,
      write: true,
    }
  }

  /// 创建指定读写权限的键模式
  #[inline]
  pub fn new(pattern: impl Into<String>, read: bool, write: bool) -> Self {
    Self {
      pattern: pattern.into(),
      read,
      write,
    }
  }

  /// 检查给定键名是否满足当前模式及读写约束
  #[inline]
  pub fn matches(&self, key: &[u8], is_write: bool) -> bool {
    if is_write && !self.write {
      return false;
    }
    if !is_write && !self.read {
      return false;
    }
    glob_match(self.pattern.as_bytes(), key)
  }
}
