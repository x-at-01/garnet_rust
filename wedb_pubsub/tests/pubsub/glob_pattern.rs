use aok::{OK, Void};
use wedb_pubsub::{glob_match, match_glob, match_glob_nocase};

/// 对标 C# GlobUtils.Match：Glob 基础模式通配匹配（*、?、[...]、[^...]、转义与大小写敏感度）
#[test]
fn glob_matching() -> Void {
  // 精确匹配
  assert!(match_glob(b"abc", b"abc"));
  assert!(!match_glob(b"abc", b"abd"));
  assert!(!match_glob(b"abc", b"abcd"));
  assert!(!match_glob(b"abcd", b"abc"));
  assert!(match_glob(b"", b""));

  // 通配符 *
  assert!(match_glob(b"*", b""));
  assert!(match_glob(b"*", b"anything"));
  assert!(match_glob(b"a*", b"a"));
  assert!(match_glob(b"a*", b"abc"));
  assert!(!match_glob(b"a*", b"bac"));
  assert!(match_glob(b"*c", b"c"));
  assert!(match_glob(b"*c", b"abc"));
  assert!(!match_glob(b"*c", b"abcd"));
  assert!(match_glob(b"a*c", b"ac"));
  assert!(match_glob(b"a*c", b"abc"));
  assert!(match_glob(b"a*c", b"a123c"));
  assert!(!match_glob(b"a*c", b"a123d"));
  assert!(match_glob(b"a*b*c", b"axbxc"));
  assert!(match_glob(b"***", b"hello"));

  // 单字符通配符 ?
  assert!(match_glob(b"h?llo", b"hello"));
  assert!(match_glob(b"h?llo", b"hallo"));
  assert!(!match_glob(b"h?llo", b"hllo"));
  assert!(!match_glob(b"h?llo", b"heello"));

  // 字符集合与范围 [...]
  assert!(match_glob(b"h[ae]llo", b"hello"));
  assert!(match_glob(b"h[ae]llo", b"hallo"));
  assert!(!match_glob(b"h[ae]llo", b"hxllo"));
  assert!(match_glob(b"h[a-c]llo", b"hallo"));
  assert!(match_glob(b"h[a-c]llo", b"hbllo"));
  assert!(match_glob(b"h[a-c]llo", b"hcllo"));
  assert!(!match_glob(b"h[a-c]llo", b"hdllo"));

  // 逆序范围 [c-a]
  assert!(match_glob(b"h[c-a]llo", b"hallo"));
  assert!(match_glob(b"h[c-a]llo", b"hbllo"));

  // 排除集合 [^...]
  assert!(match_glob(b"h[^e]llo", b"hallo"));
  assert!(!match_glob(b"h[^e]llo", b"hello"));

  // 集合内连字符与转义
  assert!(match_glob(b"[-]", b"-"));
  assert!(match_glob(b"[\\]]", b"]"));

  // 非法未闭合括号测试（严格对齐 Redis/Garnet）
  assert!(!match_glob(b"[abc", b"a"));
  assert!(!match_glob(b"h[a", b"ha"));
  assert!(!match_glob(b"h[", b"h"));
  assert!(!match_glob(b"[\\", b"\\"));

  // 转义字符
  assert!(match_glob(b"h\\*llo", b"h*llo"));
  assert!(!match_glob(b"h\\*llo", b"hello"));
  assert!(match_glob(b"h\\?llo", b"h?llo"));
  assert!(!match_glob(b"h\\?llo", b"hallo"));

  // 大小写敏感度测试
  assert!(!match_glob(b"hello", b"HELLO"));
  assert!(match_glob_nocase(b"hello", b"HELLO"));
  assert!(glob_match(b"h[A-Z]llo", b"hello", true));
  assert!(glob_match(b"h[Z-a]llo", b"hello", true));
  assert!(glob_match(b"h[c-A]llo", b"hallo", true));

  // 复杂反向回溯模式（防 ReDoS 恶劣回溯测试）
  let pattern = b"*a*a*a*a*b";
  let target = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaab";
  assert!(glob_match(pattern, target, false));

  let no_match_target = b"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaac";
  assert!(!glob_match(pattern, no_match_target, false));
  assert!(!glob_match(pattern, no_match_target, true));

  OK
}

/// 对标 C# GlobUtils.Match：Glob 十六进制转义字符匹配 (\xHH)
#[test]
fn glob_hex_escapes() -> Void {
  assert!(match_glob(b"\\x68ello", b"hello"));
  assert!(match_glob(b"hel\\x6co", b"hello"));
  assert!(match_glob(b"hello\\x20world", b"hello world"));

  // 字符集括号内的 \xHH 转义与范围
  assert!(match_glob(b"h[\\x61-\\x63]llo", b"hallo"));
  assert!(match_glob(b"h[\\x61-\\x63]llo", b"hbllo"));
  assert!(match_glob(b"h[\\x61-\\x63]llo", b"hcllo"));
  assert!(!match_glob(b"h[\\x61-\\x63]llo", b"hdllo"));

  // 不完整的 \x 或非 hex 字符当做普通 \x 转义
  assert!(match_glob(b"\\x", b"x"));
  assert!(match_glob(b"\\xg", b"xg"));

  OK
}

/// 对标 C# GlobUtils.Match：Glob 极端边界与防御性 ReDoS 回溯测试
#[test]
fn glob_boundary_and_redos() -> Void {
  // 1. 空模式与空文本
  assert!(match_glob(b"", b""));
  assert!(!match_glob(b"", b"a"));
  assert!(!match_glob(b"a", b""));

  // 2. 连续多星号在不同位置
  assert!(match_glob(b"************", b"hello"));
  assert!(match_glob(b"a************b", b"ab"));
  assert!(match_glob(b"a************b", b"axxxxxxxxxxxb"));
  assert!(!match_glob(b"a************b", b"axxxxxxxxxxxbc"));

  // 3. 字符集内连字符的极端位置
  assert!(match_glob(b"[-abc]", b"-"));
  assert!(match_glob(b"[-abc]", b"a"));
  assert!(match_glob(b"[abc-]", b"-"));
  assert!(match_glob(b"[abc-]", b"c"));
  assert!(match_glob(b"[-]", b"-"));
  assert!(!match_glob(b"[-]", b"a"));

  // 4. 未闭合中括号作为字面量回退
  assert!(match_glob(b"[", b"["));
  assert!(!match_glob(b"[", b"a"));
  assert!(match_glob(b"[abc", b"[abc"));
  assert!(!match_glob(b"[abc", b"a"));
  assert!(match_glob(b"h[a", b"h[a"));
  assert!(!match_glob(b"h[a", b"ha"));
  assert!(match_glob(b"[\x61", b"[\x61"));

  // 5. 字符集反向范围 [z-a]
  assert!(match_glob(b"[z-a]", b"m"));
  assert!(match_glob(b"[z-a]", b"a"));
  assert!(match_glob(b"[z-a]", b"z"));
  assert!(!match_glob(b"[z-a]", b"1"));

  // 6. 字符集否定 [!a-z] 与 [^a-z]
  assert!(match_glob(b"[!a-z]", b"1"));
  assert!(!match_glob(b"[!a-z]", b"b"));
  assert!(match_glob(b"[^0-9]", b"x"));
  assert!(!match_glob(b"[^0-9]", b"5"));

  // 7. 字符集中包含转义字符
  assert!(match_glob(b"[\\]]", b"]"));
  assert!(match_glob(b"[\\*]", b"*"));
  assert!(match_glob(b"[\\?]", b"?"));
  assert!(match_glob(b"[\\x41-\\x43]", b"B"));

  // 8. 不完整或非 hex 转义序列回退
  assert!(match_glob(b"\\x", b"x"));
  assert!(match_glob(b"\\x1", b"x1"));
  assert!(match_glob(b"\\xGG", b"xGG"));

  // 9. 防 ReDoS 恶劣回溯测试（高密度问号与星号组合）
  let pattern = b"*?*?*?*?*?*?*?*?*?*?a";
  let target = b"xxxxxxxxxxxxxxxxxxxxa";
  assert!(match_glob(pattern, target));
  let no_match = b"xxxxxxxxxxxxxxxxxxxxb";
  assert!(!match_glob(pattern, no_match));
  OK
}
