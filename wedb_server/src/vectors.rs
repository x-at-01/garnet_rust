//! 向量集命令接线 (VADD / VSIM / VCARD / VDIM / VEMB / VGETATTR / VSETATTR /
//! VREM / VISMEMBER / VRANDMEMBER / VLINKS / VINFO)
//!
//! 向量集与主存储键空间相互独立（纯内存管理器），序列化与 AOF 接入由后续模块补齐；
//! 仅支持 FP32 输入与 COSINE 度量，量化与过滤表达式暂不支持。
//!
//! 墓碑回收设计（wedb_vector 审查遗留项，须跨 crate 联动落地）：
//! VREM 走 Vamana 惰性墓碑（标记删除、邻接表保留墓碑仅作搜索导航），`names` 名录与
//! 邻接表空间均只增不回收，高频写删场景内存单调增长。压缩入口建议以管理命令
//! `VECTORCOMPACT <key>` 落地：临时冻结该键写入 → 遍历存活元素（element, vector）
//! 重建全新 VectorSet（含重算 HNSW 邻接表）→ 原子替换 [`ServerContext::vector`]
//! 中的键条目并丢弃旧集合。依赖 wedb_vector 补充两个 API：存活元素枚举
//! （跳过墓碑流式产出 (element, vector)）与 VectorSet 批量重建构造器；
//! 在此之前 server 不引入半自动回收，避免 与检索导航性（墓碑保召回）冲突。

use std::{result::Result as StdResult, str::from_utf8, sync::Arc};

use wedb_net::SendBuffer;
use wedb_resp::{ParseUtils, RespCommand, SessionParseState};
use wedb_vector::{Error as VectorError, VamanaParams, VectorManager};
use zmij::Buffer;

use crate::{context::ServerContext, error::Result, session::ServerSession};

/// 解析向量结果元组 (向量浮点切片, 消费参数索引)
type ParsedVector = (Vec<f32>, usize);

/// 最大向量维度 (对标 Garnet VectorManager.MaxVectorDimensions = 1 << 16)
const MAX_DIMS: usize = 1 << 16;

/// 默认检索候选数
const DEFAULT_COUNT: usize = 10;

/// 向量集命令族入口
pub fn dispatch_vectors(
  ctx: &Arc<ServerContext>,
  _session: &mut ServerSession,
  cmd: RespCommand,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  let args = args.as_slice();
  match cmd {
    RespCommand::Vadd => vadd(&ctx.vector, args, buf)?,
    RespCommand::Vsim => vsim(&ctx.vector, args, buf)?,
    RespCommand::Vcard => {
      let Some(key) = args.first() else {
        buf.write_error(b"ERR wrong number of arguments for 'vcard' command");
        return Ok(());
      };
      buf.write_integer(match ctx.vector.get(&String::from_utf8_lossy(key)) {
        Some(set) => set.read().len() as i64,
        None => 0,
      });
    }
    RespCommand::Vdim => {
      let Some(key) = args.first() else {
        buf.write_error(b"ERR wrong number of arguments for 'vdim' command");
        return Ok(());
      };
      match ctx.vector.get(&String::from_utf8_lossy(key)) {
        Some(set) => buf.write_integer(set.read().dims() as i64),
        None => buf.write_error(b"ERR Key not found"),
      }
    }
    RespCommand::Vemb => vemb(&ctx.vector, args, buf)?,
    RespCommand::Vgetattr => {
      if args.len() != 2 {
        buf.write_error(b"ERR wrong number of arguments for 'vgetattr' command");
        return Ok(());
      }
      match ctx.vector.get(&String::from_utf8_lossy(args[0])) {
        Some(set) => {
          let set = set.read();
          match set.attr(args[1]) {
            Some(attr) => buf.write_bulk_string(&attr),
            None => buf.write_null(),
          }
        }
        None => buf.write_null(),
      }
    }
    RespCommand::Vsetattr => {
      if args.len() != 3 {
        buf.write_error(b"ERR wrong number of arguments for 'vsetattr' command");
        return Ok(());
      }
      match ctx.vector.get(&String::from_utf8_lossy(args[0])) {
        Some(set) => {
          let mut set = set.write();
          // 空属性视为清除
          let attr = if args[2].is_empty() {
            None
          } else {
            Some(args[2].to_vec())
          };
          buf.write_integer(set.set_attr(args[1], attr) as i64);
        }
        None => buf.write_error(b"ERR Key not found"),
      }
    }
    RespCommand::Vrem => {
      if args.len() != 2 {
        buf.write_error(b"ERR wrong number of arguments for 'vrem' command");
        return Ok(());
      }
      match ctx.vector.get(&String::from_utf8_lossy(args[0])) {
        Some(set) => buf.write_integer(set.write().remove(args[1]) as i64),
        None => buf.write_integer(0),
      }
    }
    RespCommand::Vismember => {
      if args.len() != 2 {
        buf.write_error(b"ERR wrong number of arguments for 'vismember' command");
        return Ok(());
      }
      buf.write_integer(match ctx.vector.get(&String::from_utf8_lossy(args[0])) {
        Some(set) => set.read().contains(args[1]) as i64,
        None => 0,
      });
    }
    RespCommand::Vrandmember => vrandmember(&ctx.vector, args, buf)?,
    RespCommand::Vlinks => vlinks(&ctx.vector, args, buf)?,
    RespCommand::Vinfo => vinfo(&ctx.vector, args, buf)?,
    _ => buf.write_error(b"ERR unknown command"),
  }
  Ok(())
}

/// 解析向量输入：FP32 裸字节 或 VALUES n v1..vN，返回起始游标
fn parse_vector(args: &[&[u8]], ix: usize) -> StdResult<ParsedVector, ()> {
  let Some(kind) = args.get(ix) else {
    return Err(());
  };
  if kind.eq_ignore_ascii_case(b"FP32") {
    let Some(blob) = args.get(ix + 1) else {
      return Err(());
    };
    if blob.is_empty() || blob.len() % 4 != 0 {
      return Err(());
    }
    let dims = blob.len() / 4;
    if dims > MAX_DIMS {
      return Err(());
    }
    let floats = blob
      .as_chunks::<4>()
      .0
      .iter()
      .map(|c| f32::from_le_bytes(*c))
      .collect();
    Ok((floats, ix + 2))
  } else if kind.eq_ignore_ascii_case(b"VALUES") {
    let Some(n_raw) = args.get(ix + 1) else {
      return Err(());
    };
    let dims: usize = ParseUtils::try_read_long(n_raw).ok_or(())? as usize;
    if dims == 0 || dims > MAX_DIMS || args.len() < ix + 2 + dims {
      return Err(());
    }
    let mut floats = Vec::with_capacity(dims);
    for raw in &args[ix + 2..ix + 2 + dims] {
      let s = from_utf8(raw).map_err(|_| ())?;
      floats.push(s.parse::<f32>().map_err(|_| ())?);
    }
    Ok((floats, ix + 2 + dims))
  } else {
    Err(())
  }
}

/// VADD key (FP32 blob | VALUES n v..) element [CAS] [NOQUANT] [EF n] [M n] [SETATTR attr]
fn vadd(mgr: &VectorManager, args: &[&[u8]], buf: &mut SendBuffer) -> Result<()> {
  if args.len() < 4 {
    buf.write_error(b"ERR wrong number of arguments for 'vadd' command");
    return Ok(());
  }
  let key = String::from_utf8_lossy(args[0]).into_owned();
  let Ok((vector, mut ix)) = parse_vector(args, 1) else {
    buf.write_error(b"ERR invalid vector data or unsupported input format");
    return Ok(());
  };
  let Some(element) = args.get(ix) else {
    buf.write_error(b"ERR wrong number of arguments for 'vadd' command");
    return Ok(());
  };
  let element = element.to_vec();
  ix += 1;

  // 可选参数
  let mut params = VamanaParams::default();
  let mut attr: Option<Vec<u8>> = None;
  while ix < args.len() {
    let opt = args[ix];
    if opt.eq_ignore_ascii_case(b"CAS") || opt.eq_ignore_ascii_case(b"NOQUANT") {
      // 兼容占位：CAS 语义为返回新 id，此处与默认行为一致；NOQUANT 即 FP32 存储
      ix += 1;
    } else if opt.eq_ignore_ascii_case(b"EF") {
      match args.get(ix + 1).and_then(|v| ParseUtils::try_read_long(v)) {
        Some(n) if n > 0 => params.build_ef = (n as usize).min(4096),
        _ => {
          buf.write_error(b"ERR invalid EF");
          return Ok(());
        }
      }
      ix += 2;
    } else if opt.eq_ignore_ascii_case(b"M") {
      match args.get(ix + 1).and_then(|v| ParseUtils::try_read_long(v)) {
        Some(n) if (4..=4096).contains(&n) => params.degree = n as usize,
        _ => {
          buf.write_error(b"ERR invalid M");
          return Ok(());
        }
      }
      ix += 2;
    } else if opt.eq_ignore_ascii_case(b"SETATTR") {
      match args.get(ix + 1) {
        Some(a) => attr = Some(a.to_vec()),
        None => {
          buf.write_error(b"ERR wrong number of arguments for 'vadd' command");
          return Ok(());
        }
      }
      ix += 2;
    } else {
      buf.write_error_fmt(format_args!(
        "ERR unsupported VADD option '{}'",
        String::from_utf8_lossy(opt)
      ));
      return Ok(());
    }
  }

  let dims = vector.len();
  match mgr.get_or_create(&key, dims, params) {
    Ok((set, _created)) => {
      let mut set = set.write();
      if set.dims() != dims {
        buf.write_error_fmt(format_args!(
          "ERR vector dimension mismatch: expected {}, got {}",
          set.dims(),
          dims
        ));
        return Ok(());
      }
      match set.add(&element, &vector) {
        Ok(Some(_)) => {
          if let Some(attr) = attr {
            set.set_attr(&element, Some(attr));
          }
          buf.write_integer(1);
        }
        Ok(None) => buf.write_integer(0),
        Err(VectorError::DimMismatch { expected, got }) => buf.write_error_fmt(format_args!(
          "ERR vector dimension mismatch: expected {expected}, got {got}"
        )),
        Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
      }
    }
    Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
  }
  Ok(())
}

/// VSIM key (ELE element | FP32 blob | VALUES n v..) [WITHSCORES] [COUNT n] [EF n]
fn vsim(mgr: &VectorManager, args: &[&[u8]], buf: &mut SendBuffer) -> Result<()> {
  if args.len() < 3 {
    buf.write_error(b"ERR wrong number of arguments for 'vsim' command");
    return Ok(());
  }
  let key = String::from_utf8_lossy(args[0]).into_owned();
  let set_arc = mgr.get(&key);
  let set_guard = set_arc.as_ref().map(|s| s.read());

  let is_ele = args[1].eq_ignore_ascii_case(b"ELE");
  let query_res: StdResult<ParsedVector, ()> = if is_ele {
    match set_guard.as_ref().and_then(|set| {
      let id = set.id_of(args[2])?;
      set.vector_of(id).map(<[f32]>::to_vec)
    }) {
      Some(v) => Ok((v, 3)),
      // 键或元素不存在：返回空数组
      None => {
        buf.write_array_header(0);
        return Ok(());
      }
    }
  } else {
    parse_vector(args, 1)
  };
  let (query, mut ix) = match query_res {
    Ok(v) => v,
    Err(()) => {
      buf.write_error(b"ERR invalid vector data or unsupported input format");
      return Ok(());
    }
  };

  let mut with_scores = false;
  let mut count = DEFAULT_COUNT;
  let mut ef = 100usize;
  while ix < args.len() {
    let opt = args[ix];
    if opt.eq_ignore_ascii_case(b"WITHSCORES") {
      with_scores = true;
      ix += 1;
    } else if opt.eq_ignore_ascii_case(b"COUNT") {
      match args.get(ix + 1).and_then(|v| ParseUtils::try_read_long(v)) {
        Some(n) if (0..=100_000_000).contains(&n) => count = n as usize,
        _ => {
          buf.write_error(b"ERR invalid COUNT");
          return Ok(());
        }
      }
      ix += 2;
    } else if opt.eq_ignore_ascii_case(b"EF") {
      match args.get(ix + 1).and_then(|v| ParseUtils::try_read_long(v)) {
        Some(n) if (1..=1_000_000).contains(&n) => ef = n as usize,
        _ => {
          buf.write_error(b"ERR invalid EF");
          return Ok(());
        }
      }
      ix += 2;
    } else {
      buf.write_error_fmt(format_args!(
        "ERR unsupported VSIM option '{}'",
        String::from_utf8_lossy(opt)
      ));
      return Ok(());
    }
  }

  let Some(set) = set_guard else {
    buf.write_array_header(0);
    return Ok(());
  };
  if set.dims() != query.len() {
    buf.write_error_fmt(format_args!(
      "ERR vector dimension mismatch: expected {}, got {}",
      set.dims(),
      query.len()
    ));
    return Ok(());
  }
  match set.search(&query, count, ef) {
    Ok(hits) => {
      buf.write_array_header(if with_scores {
        hits.len() * 2
      } else {
        hits.len()
      });
      let mut num = Buffer::new();
      for (name, score) in hits {
        buf.write_bulk_string(&name);
        if with_scores {
          buf.write_bulk_string(num.format(score).as_bytes());
        }
      }
    }
    Err(VectorError::DimMismatch { .. }) => unreachable!("维度已预检"),
    Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
  }
  Ok(())
}

/// VEMB key element
fn vemb(mgr: &VectorManager, args: &[&[u8]], buf: &mut SendBuffer) -> Result<()> {
  if args.len() != 2 {
    buf.write_error(b"ERR wrong number of arguments for 'vemb' command");
    return Ok(());
  }
  let Some(set_arc) = mgr.get(&String::from_utf8_lossy(args[0])) else {
    buf.write_array_header(0);
    return Ok(());
  };
  let set = set_arc.read();
  let Some(id) = set.id_of(args[1]) else {
    buf.write_array_header(0);
    return Ok(());
  };
  match set.vector_of(id) {
    Some(v) => {
      buf.write_array_header(v.len());
      let mut num = Buffer::new();
      for f in v {
        buf.write_bulk_string(num.format(*f).as_bytes());
      }
    }
    None => buf.write_array_header(0),
  }
  Ok(())
}

/// VRANDMEMBER key [count]
fn vrandmember(mgr: &VectorManager, args: &[&[u8]], buf: &mut SendBuffer) -> Result<()> {
  if args.is_empty() || args.len() > 2 {
    buf.write_error(b"ERR wrong number of arguments for 'vrandmember' command");
    return Ok(());
  }
  let pick = match args.get(1).and_then(|v| ParseUtils::try_read_long(v)) {
    Some(n) => n,
    None => {
      // 无 count：返回单个元素
      match mgr.get(&String::from_utf8_lossy(args[0])) {
        Some(set) => match set.read().random() {
          Some(name) => buf.write_bulk_string(name),
          None => buf.write_null(),
        },
        None => buf.write_null(),
      }
      return Ok(());
    }
  };
  match mgr.get(&String::from_utf8_lossy(args[0])) {
    Some(set) => {
      let set = set.read();
      let n = pick.unsigned_abs() as usize;
      let mut out: Vec<&[u8]> = Vec::with_capacity(n.min(set.len()));
      // 简单多次随机采样去重
      let mut tries = n * 8;
      while out.len() < n && tries > 0 {
        tries -= 1;
        if let Some(name) = set.random() {
          if !out.contains(&name) {
            out.push(name);
          }
        } else {
          break;
        }
      }
      // 负数 count 允许重复
      if pick < 0 {
        out.clear();
        for _ in 0..n {
          if let Some(name) = set.random() {
            out.push(name);
          } else {
            break;
          }
        }
      }
      buf.write_array_header(out.len());
      for name in out {
        buf.write_bulk_string(name);
      }
    }
    None => {
      buf.write_array_header(0);
    }
  }
  Ok(())
}

/// VLINKS key element [WITHSCORES]
fn vlinks(mgr: &VectorManager, args: &[&[u8]], buf: &mut SendBuffer) -> Result<()> {
  if args.len() < 2 {
    buf.write_error(b"ERR wrong number of arguments for 'vlinks' command");
    return Ok(());
  }
  let with_scores = args.len() > 2 && args[2].eq_ignore_ascii_case(b"WITHSCORES");
  match mgr.get(&String::from_utf8_lossy(args[0])) {
    Some(set) => {
      let set = set.read();
      let links = if with_scores {
        set.links_with_scores(args[1])
      } else {
        set
          .links_of(args[1])
          .map(|names| names.into_iter().map(|n| (n, 0.0)).collect())
      };
      match links {
        Ok(links) => {
          buf.write_array_header(if with_scores {
            links.len() * 2
          } else {
            links.len()
          });
          let mut num = zmij::Buffer::new();
          for (name, score) in links {
            buf.write_bulk_string(&name);
            if with_scores {
              buf.write_bulk_string(num.format(score).as_bytes());
            }
          }
        }
        Err(e) => buf.write_error_fmt(format_args!("ERR {e}")),
      }
    }
    None => buf.write_error(b"ERR Key not found"),
  }
  Ok(())
}

/// VINFO key：最小字段集（quant-type / dims / cardinality）
fn vinfo(mgr: &VectorManager, args: &[&[u8]], buf: &mut SendBuffer) -> Result<()> {
  if args.len() != 1 {
    buf.write_error(b"ERR wrong number of arguments for 'vinfo' command");
    return Ok(());
  }
  match mgr.get(&String::from_utf8_lossy(args[0])) {
    Some(set) => {
      let set = set.read();
      buf.write_array_header(6);
      buf.write_bulk_string(b"quant-type");
      buf.write_bulk_string(b"f32");
      buf.write_bulk_string(b"vector-dim");
      buf.write_integer(set.dims() as i64);
      buf.write_bulk_string(b"size");
      buf.write_integer(set.len() as i64);
    }
    None => buf.write_error(b"ERR Key not found"),
  }
  Ok(())
}
