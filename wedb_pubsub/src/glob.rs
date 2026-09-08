/// 十六进制字符转数值
#[inline]
const fn hex_val(b: u8) -> Option<u8> {
  match b {
    b'0'..=b'9' => Some(b - b'0'),
    b'a'..=b'f' => Some(b - b'a' + 10),
    b'A'..=b'F' => Some(b - b'A' + 10),
    _ => None,
  }
}

/// 解析 2 位十六进制字节转义序列 `\xHH`
#[inline]
const fn parse_hex_byte(h: u8, l: u8) -> Option<u8> {
  if let (Some(hi), Some(lo)) = (hex_val(h), hex_val(l)) {
    Some((hi << 4) | lo)
  } else {
    None
  }
}

/// 读取模式串在指定偏移处的下一个有效匹配字符，支持 `\xHH` 十六进制与 `\c` 字符转义
/// 返回 `(解析后的字节, 消耗的模式串字节数)`
#[inline]
const fn next_pat_char(pat: &[u8], offset: usize) -> Option<(u8, usize)> {
  if offset >= pat.len() {
    return None;
  }
  if pat[offset] == b'\\' {
    if offset + 3 < pat.len()
      && pat[offset + 1] == b'x'
      && let Some(b) = parse_hex_byte(pat[offset + 2], pat[offset + 3])
    {
      return Some((b, 4));
    }
    if offset + 1 < pat.len() {
      return Some((pat[offset + 1], 2));
    }
    return Some((b'\\', 1));
  }
  Some((pat[offset], 1))
}

/// Glob 模式匹配算法，纯字节切片零拷贝实现
/// 对应 Microsoft Garnet `GlobUtils.Match`（即 Redis `stringmatchlen` 的移植），发布分发时匹配方向为
/// `glob_match(pattern, channel)`，与 C# `SubscribeBroker.Match(key, pattern)` 一致
///
/// 采用非递归贪心状态机单次遍历，实现 $O(N)$ 线性复杂度与 $O(1)$ 栈空间，杜绝 ReDoS 与递归栈溢出
/// （C# 为指数回溯递归，恶意模式可打爆调用栈；匹配结果对合法模式完全等价）
///
/// 相对 C# 的刻意扩展（均已在测试中固化语义）：
/// - `\xHH` 十六进制字节转义（对齐 Redis 7 glob 扩展）
/// - `[!...]` 与 `[^...]` 等价的双重否定前缀
/// - 未闭合 `[` 以字面量回退（C# 在此路径存在越界读 UB），`-]` 不作为范围终点
///
/// 基础支持：
/// - `*`：匹配 0 个或多个任意字节
/// - `?`：匹配任意单个字节
/// - `[...]`：字符集匹配，支持范围如 `[a-z]`、反向匹配、转义字符如 `[\*]`
/// - `\`：转义字符，支持 `\xHH` 十六进制字节及 `\c` 转义
/// - `ignore_case`：大小写不敏感匹配支持
#[inline]
pub fn glob_match(pattern: &[u8], key: &[u8], ignore_case: bool) -> bool {
  let mut p = 0;
  let mut k = 0;
  let mut star_p: Option<usize> = None;
  let mut star_t = 0;

  while k < key.len() {
    if p < pattern.len() {
      match pattern[p] {
        b'*' => {
          while p + 1 < pattern.len() && pattern[p + 1] == b'*' {
            p += 1;
          }
          star_p = Some(p);
          p += 1;
          star_t = k;
          continue;
        }
        b'?' => {
          p += 1;
          k += 1;
          continue;
        }
        b'[' => {
          let (matched, next_p) = match_bracket(&pattern[p..], key[k], ignore_case);
          if next_p > 0 {
            if matched {
              p += next_p;
              k += 1;
              continue;
            }
          } else if key[k] == b'[' {
            p += 1;
            k += 1;
            continue;
          }
        }
        b'\\' => {
          if let Some((pat_byte, adv)) = next_pat_char(pattern, p) {
            let matched = if ignore_case {
              pat_byte.eq_ignore_ascii_case(&key[k])
            } else {
              pat_byte == key[k]
            };
            if matched {
              p += adv;
              k += 1;
              continue;
            }
          }
        }
        c => {
          let matched = if ignore_case {
            c.eq_ignore_ascii_case(&key[k])
          } else {
            c == key[k]
          };
          if matched {
            p += 1;
            k += 1;
            continue;
          }
        }
      }
    }

    // 字符不匹配，若此前存在星号通配，则回溯星号匹配范围
    if let Some(sp) = star_p {
      p = sp + 1;
      star_t += 1;
      k = star_t;
    } else {
      return false;
    }
  }

  // 跳过模式串尾部所有多余的 '*'
  while p < pattern.len() && pattern[p] == b'*' {
    p += 1;
  }

  p == pattern.len()
}

/// 解析并匹配中括号字符集 `[...]`
///
/// 返回 `(是否匹配, 消耗的模式串字节数)`，若括号非法未闭合返回 `(false, 0)`
#[inline]
fn match_bracket(pat: &[u8], target_byte: u8, ignore_case: bool) -> (bool, usize) {
  if pat.is_empty() || pat[0] != b'[' {
    return (false, 0);
  }
  let mut i = 1;
  let mut not = false;
  if i < pat.len() && (pat[i] == b'^' || pat[i] == b'!') {
    not = true;
    i += 1;
  }
  let target = if ignore_case {
    target_byte.to_ascii_lowercase()
  } else {
    target_byte
  };

  let mut matched = false;
  let mut closed = false;

  while i < pat.len() {
    if pat[i] == b']' {
      closed = true;
      i += 1;
      break;
    }

    let Some((c1, adv1)) = next_pat_char(pat, i) else {
      return (false, 0);
    };

    // 检查范围表达式 `c1-c2`
    let next_idx = i + adv1;
    if next_idx < pat.len()
      && pat[next_idx] == b'-'
      && next_idx + 1 < pat.len()
      && pat[next_idx + 1] != b']'
      && let Some((c2, adv2)) = next_pat_char(pat, next_idx + 1)
    {
      let mut start = if ignore_case {
        c1.to_ascii_lowercase()
      } else {
        c1
      };
      let mut end = if ignore_case {
        c2.to_ascii_lowercase()
      } else {
        c2
      };
      if start > end {
        (start, end) = (end, start);
      }
      if target >= start && target <= end {
        matched = true;
      }
      i = next_idx + 1 + adv2;
      continue;
    }

    let c = if ignore_case {
      c1.to_ascii_lowercase()
    } else {
      c1
    };
    if c == target {
      matched = true;
    }
    i += adv1;
  }

  if !closed {
    return (false, 0);
  }

  (matched != not, i)
}

/// 默认区分大小写的 Glob 匹配
#[inline]
pub fn match_glob(pattern: &[u8], key: &[u8]) -> bool {
  glob_match(pattern, key, false)
}

/// 不区分大小写的 Glob 匹配
#[inline]
pub fn match_glob_nocase(pattern: &[u8], key: &[u8]) -> bool {
  glob_match(pattern, key, true)
}
