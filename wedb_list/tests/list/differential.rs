// 基于 Vec 参考模型的确定性差分对拍（转写新增，无 C# 对应测试；
// 以 Redis/Garnet 语义实现参考模型，4000 步随机操作序列下逐一比对
// ListObject 与模型的状态一致性，并校验空列表内存彻底释放不变量）。

use std::mem::size_of;

use aok::{OK, Void};
use log::info;
use wedb_list::{InsertPosition, ListObject};

/// 确定性 xorshift64* 伪随机数（避免引入额外随机性依赖，保证回归可复现）
struct XorShift(u64);

impl XorShift {
  fn next_u64(&mut self) -> u64 {
    let mut x = self.0;
    x ^= x >> 12;
    x ^= x << 25;
    x ^= x >> 27;
    self.0 = x;
    x.wrapping_mul(0x2545_F491_4F6C_DD1D)
  }

  /// 返回 [0, n) 的随机数
  fn below(&mut self, n: u64) -> u64 {
    self.next_u64() % n
  }

  /// 返回 [lo, hi] 的随机整数
  fn range(&mut self, lo: isize, hi: isize) -> isize {
    lo + self.below((hi - lo + 1) as u64) as isize
  }
}

/// 参考模型: LREM (Redis 语义)
fn model_lrem(v: &mut Vec<Vec<u8>>, count: isize, val: &[u8]) -> usize {
  if count == 0 {
    let n = v.iter().filter(|i| i.as_slice() == val).count();
    v.retain(|i| i.as_slice() != val);
    n
  } else if count > 0 {
    let limit = count as usize;
    let mut removed = 0usize;
    let mut w = 0usize;
    for r in 0..v.len() {
      if removed < limit && v[r].as_slice() == val {
        removed += 1;
      } else {
        v[w] = v[r].clone();
        w += 1;
      }
    }
    v.truncate(w);
    removed
  } else {
    let limit = count.unsigned_abs();
    let mut removed = 0;
    for r in (0..v.len()).rev() {
      if removed < limit && v[r].as_slice() == val {
        v.remove(r);
        removed += 1;
      }
    }
    removed
  }
}

/// 参考模型: LTRIM (Redis/Garnet 语义)
fn model_ltrim(v: &mut Vec<Vec<u8>>, start: isize, stop: isize) {
  if v.is_empty() {
    return;
  }
  let len = v.len() as isize;
  let s = if start < 0 {
    (len + start).max(0)
  } else {
    start
  };
  let e = if stop < 0 {
    len + stop
  } else {
    stop.min(len - 1)
  };
  if s > e {
    v.clear();
    return;
  }
  v.drain(..s as usize);
  v.truncate((e - s + 1) as usize);
}

/// 参考模型: LPOS
fn model_lpos(
  v: &[Vec<u8>],
  el: &[u8],
  rank: isize,
  count: Option<usize>,
  maxlen: usize,
) -> Vec<usize> {
  if rank == 0 || v.is_empty() {
    return Vec::new();
  }
  let max_cmp = if maxlen == 0 {
    v.len()
  } else {
    maxlen.min(v.len())
  };
  let target = match count {
    Some(0) => usize::MAX,
    Some(c) => c,
    None => 1,
  };
  let mut out = Vec::new();
  if rank > 0 {
    let mut skip = (rank - 1) as usize;
    for (i, it) in v.iter().enumerate().take(max_cmp) {
      if it.as_slice() == el {
        if skip > 0 {
          skip -= 1;
        } else {
          out.push(i);
          if out.len() >= target {
            break;
          }
        }
      }
    }
  } else {
    let mut skip = rank.unsigned_abs().saturating_sub(1);
    for (off, it) in v.iter().rev().enumerate().take(max_cmp) {
      if it.as_slice() == el {
        let i = v.len() - 1 - off;
        if skip > 0 {
          skip -= 1;
        } else {
          out.push(i);
          if out.len() >= target {
            break;
          }
        }
      }
    }
  }
  out
}

/// 元素值池（小值域以保证 LREM/LINSERT/LPOS 高频命中）
const POOL: [&[u8]; 8] = [b"a", b"b", b"c", b"d", b"e", b"f", b"g", b"h"];

/// differential_against_vec_model：4000 步全操作随机对拍与空列表释放不变量
#[test]
fn differential_against_vec_model() -> Void {
  let mut rng = XorShift(0x5EED_1234_ABCD_9876);
  let mut model: Vec<Vec<u8>> = Vec::new();
  let mut list = ListObject::new();
  // 辅助列表 (LMOVE/transfer_to 对拍)
  let mut aux_model: Vec<Vec<u8>> = Vec::new();
  let mut aux = ListObject::new();

  for step in 0..4000u64 {
    let rand_val = |rng: &mut XorShift| POOL[rng.below(POOL.len() as u64) as usize];

    // 偶发批量推入，驱动列表规模跨越多页（页容量 64）
    if rng.below(10) == 0 {
      let n = rng.range(40, 160) as usize;
      let vals: Vec<Vec<u8>> = (0..n).map(|_| rand_val(&mut rng).to_vec()).collect();
      if rng.below(2) == 0 {
        model.extend(vals.iter().cloned());
        list.rpush(vals.iter().map(Vec::as_slice));
      } else {
        let mut front = vals.clone();
        front.append(&mut model);
        model = front;
        list.lpush(vals.iter().rev().map(Vec::as_slice));
      }
    }

    match rng.below(16) {
      // RPUSH
      0 => {
        let k = rng.range(1, 4) as usize;
        let vals: Vec<Vec<u8>> = (0..k).map(|_| rand_val(&mut rng).to_vec()).collect();
        model.extend(vals.iter().cloned());
        assert_eq!(list.rpush(vals.iter().map(Vec::as_slice)), model.len());
      }
      // LPUSH
      1 => {
        let k = rng.range(1, 4) as usize;
        let vals: Vec<Vec<u8>> = (0..k).map(|_| rand_val(&mut rng).to_vec()).collect();
        let mut front = vals.clone();
        front.append(&mut model);
        model = front;
        assert_eq!(
          list.lpush(vals.iter().rev().map(Vec::as_slice)),
          model.len()
        );
      }
      // LPOP
      2 => {
        let k = rng.range(0, 5) as usize;
        let expect: Vec<Vec<u8>> = model.drain(..k.min(model.len())).collect();
        assert_eq!(list.lpop(k), expect, "step {step} lpop({k})");
      }
      // RPOP
      3 => {
        let k = rng.range(0, 5) as usize;
        let take = k.min(model.len());
        let expect: Vec<Vec<u8>> = model
          .split_off(model.len() - take)
          .into_iter()
          .rev()
          .collect();
        assert_eq!(list.rpop(k), expect, "step {step} rpop({k})");
      }
      // LINDEX + LSET
      4 => {
        let idx = rng.range(-20, 250);
        let abs = idx.unsigned_abs();
        let norm = if idx >= 0 {
          ((idx as usize) < model.len()).then_some(idx as usize)
        } else if abs <= model.len() && abs > 0 {
          Some(model.len() - abs)
        } else {
          None
        };
        assert_eq!(
          list.lindex(idx).map(Vec::from),
          norm.map(|i| model[i].clone()),
          "step {step} lindex({idx})"
        );
        if let Some(i) = norm {
          let nv = format!("s{}", step).into_bytes();
          model[i] = nv.clone();
          list
            .lset(idx, nv)
            .map_err(|e| format!("lset 失败: {e}"))
            .unwrap();
        } else {
          assert!(
            list.lset(idx, b"x").is_err(),
            "step {step} lset({idx}) 应越界"
          );
        }
      }
      // LINSERT
      5 => {
        let pivot = rand_val(&mut rng);
        let val = format!("i{}", step).into_bytes();
        let pos = if rng.below(2) == 0 {
          InsertPosition::Before
        } else {
          InsertPosition::After
        };
        let expect = match model.iter().position(|v| v.as_slice() == pivot) {
          Some(i) => {
            let at = match pos {
              InsertPosition::Before => i,
              InsertPosition::After => i + 1,
            };
            model.insert(at, val.clone());
            model.len() as isize
          }
          None => -1,
        };
        assert_eq!(list.linsert(pivot, val, pos), expect, "step {step} linsert");
      }
      // LREM
      6 => {
        let count = match rng.below(6) {
          0 => 0,
          1 => rng.range(-3, 3),
          2 => rng.range(-10, 10),
          3 => isize::MAX,
          4 => isize::MIN,
          _ => rng.range(-70, 70),
        };
        let val = rand_val(&mut rng);
        assert_eq!(
          list.lrem(count, val),
          model_lrem(&mut model, count, val),
          "step {step} lrem({count}, {val:?})"
        );
      }
      // LTRIM
      7 => {
        let s = rng.range(-200, 250);
        let e = rng.range(-200, 250);
        model_ltrim(&mut model, s, e);
        list.ltrim(s, e);
        assert_eq!(list.lrange(0, -1), model, "step {step} ltrim({s},{e})");
      }
      // LPOS
      8 => {
        let el = rand_val(&mut rng);
        let rank = rng.range(-3, 3);
        let count = match rng.below(3) {
          0 => None,
          1 => Some(rng.range(0, 4) as usize),
          _ => Some(0),
        };
        let maxlen = if rng.below(2) == 0 {
          rng.range(0, 150) as usize
        } else {
          0
        };
        assert_eq!(
          list.lpos(el, rank, count, maxlen),
          model_lpos(&model, el, rank, count, maxlen),
          "step {step} lpos({rank:?},{count:?},{maxlen})"
        );
      }
      // LRANGE
      9 => {
        let s = rng.range(-180, 180);
        let e = rng.range(-180, 180);
        let actual_s = if s < 0 {
          (model.len() as isize + s).max(0)
        } else {
          s
        };
        let actual_e = if e < 0 {
          model.len() as isize + e
        } else {
          e.min(model.len() as isize - 1)
        };
        let expect: Vec<Vec<u8>> =
          if actual_s <= actual_e && actual_s < model.len() as isize && actual_e >= 0 {
            model[actual_s as usize..=actual_e as usize].to_vec()
          } else {
            Vec::new()
          };
        assert_eq!(list.lrange(s, e), expect, "step {step} lrange({s},{e})");
      }
      // LMOVE 到辅助列表（等价 transfer_to）
      10 => {
        let k = rng.range(1, 6) as usize;
        let from_left = rng.below(2) == 0;
        let to_left = rng.below(2) == 0;
        let moved = k.min(model.len());
        assert_eq!(
          list.transfer_to(&mut aux, k, from_left, to_left),
          moved,
          "step {step} transfer"
        );
        for _ in 0..moved {
          let val = if from_left {
            model.remove(0)
          } else {
            model.pop().unwrap()
          };
          if to_left {
            aux_model.insert(0, val);
          } else {
            aux_model.push(val);
          }
        }
        assert_eq!(aux.lrange(0, -1), aux_model, "step {step} aux 不一致");
      }
      // rotate（同键 LMOVE）
      11 => {
        let from_left = rng.below(2) == 0;
        let to_left = rng.below(2) == 0;
        let expect = if model.is_empty() {
          None
        } else {
          match (from_left, to_left) {
            (true, false) => {
              let v = model.remove(0);
              model.push(v.clone());
              Some(v)
            }
            (false, true) => {
              let v = model.pop().unwrap();
              model.insert(0, v.clone());
              Some(v)
            }
            (true, true) => model.first().cloned(),
            (false, false) => model.last().cloned(),
          }
        };
        assert_eq!(
          list.rotate(from_left, to_left).map(Vec::from),
          expect,
          "step {step} rotate({from_left},{to_left})"
        );
      }
      // drain
      12 => {
        let k = rng.range(1, 8) as usize;
        let take = k.min(model.len());
        if rng.below(2) == 0 {
          let expect: Vec<Vec<u8>> = model.drain(..take).collect();
          assert_eq!(
            list.drain_left(k).collect::<Vec<_>>(),
            expect,
            "step {step} drain_left"
          );
        } else {
          let expect: Vec<Vec<u8>> = model
            .split_off(model.len() - take)
            .into_iter()
            .rev()
            .collect();
          assert_eq!(
            list.drain_right(k).rev().collect::<Vec<_>>(),
            expect,
            "step {step} drain_right"
          );
        }
      }
      // rpoplpush 到辅助列表
      13 => {
        let expect = model.pop().inspect(|v| {
          aux_model.insert(0, v.clone());
        });
        assert_eq!(
          ListObject::rpoplpush(&mut list, &mut aux),
          expect,
          "step {step} rpoplpush"
        );
        assert_eq!(aux.lrange(0, -1), aux_model);
      }
      // 双向迭代对拍
      14 => {
        assert_eq!(
          list.iter().collect::<Vec<_>>(),
          model.iter().map(Vec::as_slice).collect::<Vec<_>>(),
          "step {step} iter"
        );
        assert_eq!(
          list.iter().rev().collect::<Vec<_>>(),
          model.iter().rev().map(Vec::as_slice).collect::<Vec<_>>(),
          "step {step} iter rev"
        );
        assert_eq!(list.iter().len(), model.len());
      }
      // 序列化往返
      _ => {
        let mut buf = Vec::new();
        list.serialize(&mut buf);
        let restored = ListObject::deserialize(&buf)
          .map_err(|e| e.to_string())
          .unwrap();
        assert_eq!(
          restored.lrange(0, -1),
          model,
          "step {step} serialize roundtrip"
        );
        let bc = ListObject::from_bitcode(&list.to_bitcode())
          .map_err(|e| e.to_string())
          .unwrap();
        assert_eq!(bc.lrange(0, -1), model, "step {step} bitcode roundtrip");
      }
    }

    // 每步不变量: 长度一致; 列表变空时页目录连同环形缓冲区彻底释放
    assert_eq!(list.len(), model.len(), "step {step} len 不一致");
    if model.is_empty() {
      assert_eq!(list.capacity(), 0, "step {step} 空列表应释放页目录");
      assert_eq!(
        list.byte_size(),
        size_of::<ListObject>(),
        "step {step} 空列表应彻底释放内存"
      );
    }
    assert_eq!(list.lrange(0, -1), model, "step {step} 全量对拍");
  }

  // 收尾: 辅助列表回灌并校验内存释放
  aux_model.append(&mut model);
  list.transfer_to(&mut aux, usize::MAX, true, false);
  assert!(list.is_empty());
  assert_eq!(list.capacity(), 0);
  assert_eq!(aux.len(), aux_model.len());
  assert_eq!(aux.lrange(0, -1), aux_model);

  info!("differential_against_vec_model 通过：4000 步全操作对拍一致");
  OK
}
