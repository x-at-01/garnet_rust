//! 模块加载规范解析 (对标 Garnet ModuleUtils::TryParseModuleSpec)
//!
//! 规范形如 `<module-path> [arg0 arg1 ...]`，路径与参数以空白分隔；
//! 含空白的路径必须用双引号包裹。

use crate::error::{Error, Result};

/// 解析单条模块规范，零拷贝返回 (路径, 参数列表)，生命周期同 `spec`。
///
/// 两种形式：
/// - 引号形式 `"<path>" [args...]`：路径可含空白，两端 ASCII 空白去除、内部保留；
///   闭合引号与参数间不要求分隔符（宽松解析）；未闭合或路径为空则拒绝
/// - 裸形式：首个空白分词即路径，其余分词为参数
///
/// 引号外的连续空白（含 tab）视为分隔符；参数不做引号转义（引号为字面字符）。
/// 裁剪与分词使用同一 ASCII 空白集（与 [`str::split_ascii_whitespace`] 一致）。
pub fn parse_module_spec(spec: &str) -> Result<(&str, Vec<&str>)> {
  let spec = spec.trim_ascii();
  if spec.is_empty() {
    return Err(Error::InvalidModuleSpec);
  }

  let Some(rest) = spec.strip_prefix('"') else {
    let mut parts = spec.split_ascii_whitespace();
    let Some(path) = parts.next() else {
      return Err(Error::InvalidModuleSpec);
    };
    return Ok((path, parts.collect()));
  };

  // 引号形式：定位闭合引号，其余分词为参数
  let Some(close) = rest.find('"') else {
    return Err(Error::InvalidModuleSpec);
  };
  let path = rest[..close].trim_ascii();
  if path.is_empty() {
    return Err(Error::InvalidModuleSpec);
  }
  Ok((path, rest[close + 1..].split_ascii_whitespace().collect()))
}
