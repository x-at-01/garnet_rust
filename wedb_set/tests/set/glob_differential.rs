//! glob_match 与差分参考实现 reference_match 的确定性差分验证
//!
//! 基准为 Microsoft Garnet `GlobUtils.Match`（libs/server/GlobUtils.cs）的递归参考实现，
//! wedb_set::glob_match 与其语义逐点对齐：
//!
//! 1. 空目标：主循环入口条件为双串非空（C# L19），末尾仅双串耗尽才命中（C# L158），故 ("*", "") 返回 false；
//! 2. 取反符：仅 '^' 视为取反（C# L49），'!' 为字面集合成员；
//! 3. 区间右端点：区间分支消费 start '-' end 后继续扫描（C# L75-95），'a-]' 的 ']' 仅作区间
//!    上界，不兼作字符集结束符，循环持续到真实 ']' 或模式串耗尽。

use std::mem::swap;

use aok::{OK, Void};
use log::info;
use wedb_set::glob_match;

/// 差分模糊测试基准：递归参考实现（对标 Garnet GlobUtils.Match C# 递归逻辑）
fn reference_match(mut p: &[u8], mut k: &[u8]) -> bool {
  while !p.is_empty() && !k.is_empty() {
    match p[0] {
      b'*' => {
        // C# L24: while (patternLen > 0 && pattern[1] == '*')
        while p.len() > 1 && p[1] == b'*' {
          p = &p[1..];
        }
        if p.len() == 1 {
          return true;
        }
        let mut kk = k;
        while !kk.is_empty() {
          if reference_match(&p[1..], kk) {
            return true;
          }
          kk = &kk[1..];
        }
        return false;
      }
      b'?' => {}
      b'[' => {
        p = &p[1..];
        // C# L49: 仅 '^' 为取反前缀
        let not = !p.is_empty() && p[0] == b'^';
        if not {
          p = &p[1..];
        }
        let mut matched = false;
        loop {
          if p.is_empty() {
            // 未闭合 '['：全部消耗
            break;
          } else if p[0] == b'\\' && p.len() >= 2 {
            p = &p[1..];
            if p[0] == k[0] {
              matched = true;
            }
          } else if p[0] == b']' {
            break;
          } else if p.len() >= 3 && p[1] == b'-' {
            // C# L75-95: 区间右端点可为 ']'，不兼作集合终止
            let (mut start, mut end) = (p[0], p[2]);
            if start > end {
              swap(&mut start, &mut end);
            }
            if k[0] >= start && k[0] <= end {
              matched = true;
            }
            p = &p[2..];
          } else {
            if p[0] == k[0] {
              matched = true;
            }
          }
          p = &p[1..];
        }
        if not {
          matched = !matched;
        }
        if !matched {
          return false;
        }
      }
      b'\\' => {
        if p.len() >= 2 {
          p = &p[1..];
        }
        if p[0] != k[0] {
          return false;
        }
      }
      c => {
        if c != k[0] {
          return false;
        }
      }
    }
    // 共享底部：模式串推进 1 字节
    if !p.is_empty() {
      p = &p[1..];
    }
    k = &k[1..];
    if k.is_empty() {
      // 目标在循环内耗尽时跳过尾部 '*'
      while !p.is_empty() && p[0] == b'*' {
        p = &p[1..];
      }
      break;
    }
  }
  p.is_empty() && k.is_empty()
}

/// 固定边界用例：空目标入口判定、转义、字符集与星号回溯
/// 对标 Garnet GlobUtils.Match 边界用例
#[test]
fn test_glob_fixed_edges() -> Void {
  // 入口判定：目标为空时仅空模式命中 (C# GlobUtils L19/L158)
  assert!(!glob_match(b"*", b""));
  assert!(!glob_match(b"**", b""));
  assert!(!glob_match(b"*a*", b""));
  assert!(!glob_match(b"?", b""));
  assert!(glob_match(b"", b""));

  // 未闭合 '[' 与字符集边界
  assert!(glob_match(b"[abc", b"a"));
  assert!(!glob_match(b"[abc", b"d"));
  assert!(glob_match(b"[^abc", b"d"));
  assert!(!glob_match(b"[", b"["));

  // 'a-]' 的 ']' 为区间上界：集合 {']'..'a'}
  assert!(glob_match(b"[a-]", b"]"));
  assert!(glob_match(b"[z-a]", b"m"));

  // 区间后仍有成员：继续扫描真实 ']'
  assert!(glob_match(b"[a-]x]", b"x"));
  assert!(!glob_match(b"[a-]x]", b"!"));

  // '!' 为字面集合成员 (C# L49 仅认 '^')
  assert!(!glob_match(b"[!0-9]ello", b"hello"));
  assert!(glob_match(b"[!0-9]ello", b"!ello"));

  // 转义
  assert!(glob_match(b"\\*bold\\*", b"*bold*"));
  assert!(glob_match(b"a\\", b"a\\"));
  assert!(!glob_match(b"a\\", b"ab"));

  // 星号回溯跨越字符集
  assert!(glob_match(b"*[ab]x", b"zzzxax"));
  assert!(!glob_match(b"*[ab]x", b"zzzcx"));

  info!("test_glob_fixed_edges 通过");
  OK
}

/// 确定性随机差分：模式串/目标全组合枚举，双方结果逐一相等
/// 对标 Garnet GlobUtils.Match 差分模糊测试验证
#[test]
fn test_glob_differential_fuzz() -> Void {
  use fastrand::Rng;

  const ALPHABET: &[u8] = b"*?[]^!a-\\xyz0";
  let mut rng = Rng::with_seed(20260907);
  let mut cases = 0usize;

  // 使用栈缓冲区代替 Vec 堆分配，14 万次测试完全零堆内存分配
  let mut pattern_buf = [0u8; 6];
  let mut target_buf = [0u8; 4];

  for plen in 0..=6usize {
    for _ in 0..4000 {
      for b in pattern_buf[..plen].iter_mut() {
        *b = ALPHABET[rng.usize(0..ALPHABET.len())];
      }
      let pattern = &pattern_buf[..plen];
      for klen in 0..=4usize {
        for b in target_buf[..klen].iter_mut() {
          *b = ALPHABET[rng.usize(0..ALPHABET.len())];
        }
        let target = &target_buf[..klen];
        assert_eq!(
          glob_match(pattern, target),
          reference_match(pattern, target),
          "差分分歧: pattern={pattern:?}, target={target:?}"
        );
        cases += 1;
      }
    }
  }
  assert!(cases > 100_000);

  info!("test_glob_differential_fuzz 通过 (共验证 {cases} 组随机用例)");
  OK
}

/// 转义字符与字符集取反
/// 对标 Garnet GlobUtils.Match 转义字符与字符集取反
#[test]
fn test_glob_escape_and_negation() -> Void {
  // 基础通配
  assert!(glob_match(b"*", b"anything"));
  assert!(glob_match(b"s?me", b"some"));

  // 反斜杠转义
  assert!(glob_match(b"\\*bold\\*", b"*bold*"));
  assert!(!glob_match(b"\\*bold\\*", b"xxboldxx"));
  assert!(glob_match(b"100\\%", b"100%"));
  assert!(glob_match(b"a\\", b"a\\"));
  assert!(!glob_match(b"a\\", b"ab"));

  // 字符集取反仅支持 '^'（'!' 为字面字符）
  assert!(glob_match(b"[^0-9]x", b"ax"));
  assert!(!glob_match(b"[^0-9]x", b"5x"));
  assert!(!glob_match(b"[!0-9]x", b"ax"));
  assert!(glob_match(b"[!0-9]x", b"!x"));
  assert!(glob_match(b"[!0-9]x", b"5x"));
  assert!(glob_match(b"[\\a\\-z]x", b"-x"));

  info!("test_glob_escape_and_negation 通过");
  OK
}

/// 中括号边界语义
/// 对标 Garnet GlobUtils.Match 中括号字符集边界语义
#[test]
fn test_glob_bracket_edges() -> Void {
  // 未闭合 '['：剩余模式串整体视作字符集并消耗至末尾
  assert!(glob_match(b"[abc", b"a"));
  assert!(glob_match(b"[abc", b"c"));
  assert!(!glob_match(b"[abc", b"d"));

  // 未闭合取反集
  assert!(glob_match(b"[^abc", b"d"));
  assert!(!glob_match(b"[^abc", b"a"));

  // 孤立 '[' 永不匹配
  assert!(!glob_match(b"[", b"["));
  assert!(!glob_match(b"[", b"a"));

  // 空集 `[]a]`：']' 紧随 '[' 即闭合，空集不匹配任何字节
  assert!(!glob_match(b"[]a]", b"a"));
  assert!(!glob_match(b"[]a]", b"]"));

  // 范围端点允许为 ']'
  assert!(glob_match(b"[a-]", b"]"));
  assert!(glob_match(b"[a-]", b"_"));
  assert!(glob_match(b"[a-]", b"a"));
  assert!(!glob_match(b"[a-]", b"b"));

  // 区间后继字节并入集合
  assert!(!glob_match(b"[a-]z", b"]z"));
  assert!(!glob_match(b"[a-]z", b"az"));
  assert!(glob_match(b"[a-]z", b"]"));

  // 逆序范围自动交换起止
  assert!(glob_match(b"[z-a]", b"m"));

  // ']' 前的转义按字面比较
  assert!(glob_match(b"[\\]]", b"]"));
  assert!(!glob_match(b"[\\]]", b"a"));

  // 连续 '-' 非范围位置按字面处理
  assert!(glob_match(b"[-a]", b"-"));
  assert!(glob_match(b"[-a]", b"a"));
  assert!(!glob_match(b"[-a]", b"b"));

  // 多字节成员模式
  assert!(glob_match(b"h?llo", b"hello"));
  assert!(glob_match(b"h\\*o", b"h*o"));
  assert!(!glob_match(b"h\\*o", b"hello"));

  info!("test_glob_bracket_edges 通过");
  OK
}

/// 星号回溯残余模式、多星组合与空目标
/// 对标 Garnet GlobUtils.Match 星号回溯残余模式与空目标
#[test]
fn test_glob_star_backtrack_edges() -> Void {
  // 目标耗尽后模式仍残留非星号字节
  assert!(!glob_match(b"a*b", b"a"));
  assert!(!glob_match(b"abc*", b"ab"));
  assert!(glob_match(b"abc*", b"abc"));
  assert!(glob_match(b"abc*", b"abcd"));

  // 多星组合回溯
  assert!(glob_match(b"*a*b*", b"xaybz"));
  assert!(!glob_match(b"*a*b*", b"xbza"));
  let mut ends_ab = [b'a'; 32];
  ends_ab[31] = b'b';
  assert!(glob_match(b"a*a*b", &ends_ab));

  // 空目标语义：仅空模式命中空目标
  assert!(!glob_match(b"*", b""));
  assert!(glob_match(b"", b""));
  assert!(!glob_match(b"", b"a"));
  assert!(!glob_match(b"?", b""));
  assert!(!glob_match(b"**", b""));
  assert!(!glob_match(b"*a*", b""));
  assert!(glob_match(b"*", b"a"));

  // ReDoS 压力：多星叠加对长不匹配目标保持多项式回溯
  let long_a = [b'a'; 512];
  assert!(!glob_match(b"*a*a*a*a*a*a*a*a*a*a*z", &long_a));
  let mut ends_with_b = [b'a'; 512];
  ends_with_b[511] = b'b';
  assert!(glob_match(b"*a*a*a*a*a*a*a*a*a*a*b", &ends_with_b));

  info!("test_glob_star_backtrack_edges 通过");
  OK
}
