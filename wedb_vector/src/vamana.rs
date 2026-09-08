//! Vamana 单层图 ANN 索引 (DiskANN 风格，纯 Rust 实现)
//!
//! 结构：向量池 + 定向邻接表 + 删除墓碑。建图与检索均为束搜索（论文 Algorithm 1），
//! 插入时以 robust prune（Algorithm 4，alpha 启发）选邻居，再做双向链接与出度收缩。
//! 写路径由上层 [`crate::set::VectorSet`] 的锁保护；读路径（检索）无锁并发，
//! 访问去重靠线程本地的「代数标记」，免去每次搜索 memset visited 位图。
//!
//! 参考：Jayaram Subramanya et al., DiskANN: Fast Accurate Billion-point Nearest
//! Neighbor Search on a Single Node (Vamana), NeurIPS 2019, Algorithm 1/3/4。
//! 与论文的差异：剪枝出度不足时按 DiskANN 工程实现递增 α 重试（论文仅提示
//! 「increase α and re-run」），保证低维流形数据上的出度与可导航性。

use std::{
  cell::RefCell,
  cmp::{Ordering, Reverse},
  collections::BinaryHeap,
};

use crate::error::{Error, Result};

/// 小数据集精确检索阈值：其下暴力扫描（更快且无召回损失），其上走图索引
const EXACT_LIMIT: usize = 2048;

/// robust prune 的 alpha 起始系数与重试上限（论文 Algorithm 4；DiskANN 工程
/// 实现在出度不足时递增 α 重试——更大 α 保留更长程的边）
const ALPHA_INIT: f32 = 1.0;
const ALPHA_MAX: f32 = 64.0;

/// 随机取样的墓碑探测上限，超过后线性扫描兜底
const RANDOM_PROBES: u32 = 32;

/// 副本内距离：f32 全序包装，供二叉堆排序（NaN 也有确定序，不 panic）
#[derive(Clone, Copy, PartialEq)]
struct Dist(f32);

impl Eq for Dist {}

impl PartialOrd for Dist {
  fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
    Some(self.cmp(other))
  }
}

impl Ord for Dist {
  fn cmp(&self, other: &Self) -> Ordering {
    self.0.total_cmp(&other.0)
  }
}

/// 候选 (元素 id, 到基准向量的距离)
type Candidate = (u32, f32);

// 线程本地的搜索访问标记：代数 + 每元素最近触碰代数。
// 搜索是只读操作，同一线程内串行执行（读锁共享、线程内互斥），
// 线程本地代数即可精确去重且免原子操作；跨线程各持代数互不干扰。
thread_local! {
  static VISITED: RefCell<(u32, Vec<u32>)> = const { RefCell::new((1, Vec::new())) };
}

/// Vamana 图索引：L2 距离；向量入库时归一化，L2 单调等价于余弦距离
pub struct Vamana {
  /// 向量维度，插入/检索时校验
  dims: usize,
  /// 最大出度 R（论文 Algorithm 4 的 r）
  degree: usize,
  /// 建图束宽 L（论文 Algorithm 1 的 L）
  build_ef: usize,
  vectors: Vec<Box<[f32]>>,
  links: Vec<Vec<u32>>,
  deleted: Vec<bool>,
  /// 存活元素数（不含墓碑）
  live: usize,
  /// 入口点；不变式：为 Some 时必指向存活元素（remove 时转移）
  entry: Option<u32>,
}

/// 建图与检索参数 (对标 VADD 的 M/EF)
#[derive(Debug, Clone, Copy)]
pub struct VamanaParams {
  pub degree: usize,
  pub build_ef: usize,
}

impl Default for VamanaParams {
  fn default() -> Self {
    Self {
      degree: 32,
      build_ef: 64,
    }
  }
}

impl Vamana {
  pub fn new(dims: usize, params: VamanaParams) -> Result<Self> {
    if dims == 0 {
      return Err(Error::InvalidDims(0));
    }
    Ok(Self {
      dims,
      degree: params.degree.max(4),
      build_ef: params.build_ef.max(params.degree.max(4)),
      vectors: Vec::new(),
      links: Vec::new(),
      deleted: Vec::new(),
      live: 0,
      entry: None,
    })
  }

  /// 向量维度
  pub fn dims(&self) -> usize {
    self.dims
  }

  /// 存活元素数（不含墓碑）
  pub fn len(&self) -> usize {
    self.live
  }

  /// 是否没有存活元素
  pub fn is_empty(&self) -> bool {
    self.live == 0
  }

  /// 图入口元素 id
  pub fn entry(&self) -> Option<u32> {
    self.entry
  }

  /// 插入向量（内部归一化），返回分配的元素 id
  ///
  /// 对应论文建图流程：束搜索取候选集 (Algorithm 1)，robust prune 选邻居
  /// (Algorithm 4)，再对被连边的邻居做双向链接与出度收缩。
  pub fn insert(&mut self, vector: &[f32]) -> Result<u32> {
    if vector.len() != self.dims {
      return Err(Error::DimMismatch {
        expected: self.dims,
        got: vector.len(),
      });
    }
    // 拒绝非有限值：NaN/Inf 距离虽有序不 panic，但会永久污染图导航
    if !vector.iter().all(|x| x.is_finite()) {
      return Err(Error::InvalidVector);
    }

    let id = self.vectors.len() as u32;
    self.vectors.push(normalize(vector));
    self.links.push(Vec::with_capacity(self.degree));
    self.deleted.push(false);
    self.live += 1;

    let Some(entry) = self.entry else {
      self.entry = Some(id);
      return Ok(id);
    };

    // 候选到新向量的距离在束搜索时已算出，剪枝直接复用，免二次计算。
    // 双种子启动：全局入口保证连通导向，「插入前沿」（上一个元素，可能已是
    // 墓碑，仅作导航提示）保证顺序流式写入时新元素总能触及真实局部邻域——
    // 否则纯路径形态的图上束宽耗尽，候选退化为入口旁的紧簇，图不可导航。
    let query = &self.vectors[id as usize];
    let tip = id - 1;
    let seeds: &[u32] = if entry == tip {
      &[entry]
    } else {
      &[entry, tip]
    };
    let candidates = self.greedy_search(seeds, query, self.build_ef);
    let neighbors = self.prune(candidates);
    for &n in &neighbors {
      // 新 id 入邻接表必是新边，不会重复
      let overflow = {
        let ln = &mut self.links[n as usize];
        ln.push(id);
        ln.len() > self.degree
      };
      if overflow {
        self.shrink(n);
      }
    }
    self.links[id as usize] = neighbors;
    Ok(id)
  }

  /// 标记删除（惰性墓碑），返回是否确实删除
  pub fn remove(&mut self, id: u32) -> bool {
    if id as usize >= self.vectors.len() || self.deleted[id as usize] {
      return false;
    }
    self.deleted[id as usize] = true;
    self.live -= 1;
    if self.entry == Some(id) {
      // 维持「入口必存活」不变式，保证后续插入总能拿到有效搜索起点
      self.entry = (0..self.vectors.len() as u32).find(|&i| !self.deleted[i as usize]);
    }
    true
  }

  /// 元素是否存在（非墓碑）
  pub fn contains(&self, id: u32) -> bool {
    (id as usize) < self.vectors.len() && !self.deleted[id as usize]
  }

  /// 元素向量（已归一化）
  pub fn vector(&self, id: u32) -> Option<&[f32]> {
    self
      .contains(id)
      .then(|| self.vectors[id as usize].as_ref())
  }

  /// 元素邻接表（内部 id；可能含墓碑，搜索时会路过以维持导航性）
  pub fn links(&self, id: u32) -> &[u32] {
    self.links.get(id as usize).map_or(&[], |l| l)
  }

  /// 最近邻检索：返回按距离升序的至多 k 个存活 (id, 距离)
  pub fn search(&self, query: &[f32], k: usize, ef: usize) -> Result<Vec<Candidate>> {
    if query.len() != self.dims {
      return Err(Error::DimMismatch {
        expected: self.dims,
        got: query.len(),
      });
    }
    if k == 0 {
      return Ok(Vec::new());
    }
    if !query.iter().all(|x| x.is_finite()) {
      return Err(Error::InvalidVector);
    }
    let q = normalize(query);
    if self.vectors.len() <= EXACT_LIMIT {
      return Ok(self.search_exact(&q, k));
    }
    let Some(entry) = self.entry else {
      return Ok(Vec::new());
    };
    // 墓碑会占据束宽与搜索路径，按墓碑数放大束宽以保证存活元素召回；
    // 饱和加法防极端入参（usize 上限）溢出
    let dead = self.vectors.len() - self.live;
    let ef = ef.max(k).saturating_add(dead).min(self.vectors.len());
    Ok(
      self
        .greedy_search(&[entry], &q, ef)
        .into_iter()
        .filter(|&(id, _)| !self.deleted[id as usize])
        .take(k)
        .collect(),
    )
  }

  /// 精确暴力检索：小数据集上比图检索更快且无召回损失
  fn search_exact(&self, q: &[f32], k: usize) -> Vec<Candidate> {
    let mut scored: Vec<Candidate> = self
      .vectors
      .iter()
      .enumerate()
      .filter(|&(i, _)| !self.deleted[i])
      .map(|(i, v)| (i as u32, l2_sq(v, q)))
      .collect();
    let k = k.min(scored.len());
    // O(n) 部分选择出前 k，再仅对前 k 排序，避免全量 O(n log n)
    if k < scored.len() {
      scored.select_nth_unstable_by(k, |a, b| a.1.total_cmp(&b.1));
      scored.truncate(k);
    }
    scored.sort_by(|a, b| a.1.total_cmp(&b.1));
    scored
  }

  /// 随机取一个存活元素 id
  pub fn random(&self) -> Option<u32> {
    if self.live == 0 {
      return None;
    }
    let total = self.vectors.len() as u32;
    // 蓄水池式随机起点探测，避免墓碑稠密时退化
    for _ in 0..RANDOM_PROBES {
      let id = fastrand::u32(..total);
      if self.deleted[id as usize] {
        continue;
      }
      return Some(id);
    }
    (0..total).find(|&i| !self.deleted[i as usize])
  }

  /// 束搜索（论文 Algorithm 1）：从种子集合出发，维护至多 ef 个最近结果，
  /// 返回按距离升序的候选（含墓碑，墓碑由调用方按语义过滤）
  fn greedy_search(&self, seeds: &[u32], query: &[f32], ef: usize) -> Vec<Candidate> {
    VISITED.with_borrow_mut(|st| self.greedy_with(st, seeds, query, ef))
  }

  /// [`Self::greedy_search`] 的实现体，标记状态由调用方注入以便借用检查
  fn greedy_with(
    &self,
    st: &mut (u32, Vec<u32>),
    seeds: &[u32],
    query: &[f32],
    ef: usize,
  ) -> Vec<Candidate> {
    // 代数标记去重：标记值等于本次代数即已访问。
    // 回绕处理：u32 代数耗尽（同线程 2^32 次搜索）时全量清零重来，
    // 单次 O(n)、物理上数年一遇；初值 1 与槽位初值 0 错开。
    if st.0 == u32::MAX {
      st.1.fill(0);
      st.0 = 1;
    } else {
      st.0 += 1;
    }
    let epoch = st.0;
    if st.1.len() < self.vectors.len() {
      st.1.resize(self.vectors.len(), 0);
    }
    let marks = &mut st.1;

    // 候选堆（小根堆，按距离）与结果堆（大根堆，容量 ef）
    let mut candidates: BinaryHeap<Reverse<(Dist, u32)>> = BinaryHeap::new();
    let mut results: BinaryHeap<(Dist, u32)> = BinaryHeap::new();

    for &seed in seeds {
      // 安全性：seeds 均来自图内已分配元素，恒小于池长度
      if marks[seed as usize] == epoch {
        continue;
      }
      marks[seed as usize] = epoch;
      let d = l2_sq(&self.vectors[seed as usize], query);
      candidates.push(Reverse((Dist(d), seed)));
      results.push((Dist(d), seed));
    }

    while let Some(Reverse((Dist(d), cur))) = candidates.pop() {
      // 候选最近者比结果最远者还远 -> 收敛（论文终止条件）
      let Some(&(Dist(mut worst), _)) = results.peek() else {
        break;
      };
      if d > worst {
        break;
      }
      for &n in &self.links[cur as usize] {
        // 安全性：links 中 id 与 vectors 同步增长，恒小于池长度
        if unsafe { *marks.get_unchecked(n as usize) } == epoch {
          continue;
        }
        // marks 长度已与向量池对齐，直接按下标写入
        unsafe { *marks.get_unchecked_mut(n as usize) = epoch };
        let nd = l2_sq(unsafe { self.vectors.get_unchecked(n as usize) }, query);
        if results.len() < ef || nd < worst {
          candidates.push(Reverse((Dist(nd), n)));
          results.push((Dist(nd), n));
          if results.len() > ef {
            results.pop();
            worst = results.peek().map_or(f32::INFINITY, |&(Dist(w), _)| w);
          }
        }
      }
    }

    let mut out: Vec<Candidate> = results.into_iter().map(|(Dist(d), id)| (id, d)).collect();
    out.sort_by(|a, b| a.1.total_cmp(&b.1));
    out
  }

  /// 为新节点选邻居：候选剔除墓碑后做 robust prune
  fn prune(&self, candidates: Vec<Candidate>) -> Vec<u32> {
    let cands: Vec<Candidate> = candidates
      .into_iter()
      .filter(|&(id, _)| !self.deleted[id as usize])
      .collect();
    Self::select_neighbors(&self.vectors, cands, self.degree)
  }

  /// 收缩节点出度：邻接表剔除墓碑后重新 robust prune（双向链接溢出时调用）
  fn shrink(&mut self, id: u32) {
    let base = &self.vectors[id as usize];
    let cands: Vec<Candidate> = self.links[id as usize]
      .iter()
      .filter(|&&n| !self.deleted[n as usize])
      .map(|&n| (n, l2_sq(&self.vectors[n as usize], base)))
      .collect();
    self.links[id as usize] = Self::select_neighbors(&self.vectors, cands, self.degree);
  }

  /// robust prune（论文 Algorithm 4）：候选按到基准距离升序，反复取最近者 p*
  /// 入选，并剔除对 p* 冗余的候选（α·d(p*,p') ≤ d(p',p)）。
  /// 低维流形（如共线/共圆）上 α=1 会退化到极小出度，按 DiskANN 工程做法
  /// 递增 α 重试直到选满 r 或达上限——更大 α 保留长程边，保证可导航性。
  fn select_neighbors(vectors: &[Box<[f32]>], mut cands: Vec<Candidate>, r: usize) -> Vec<u32> {
    cands.sort_by(|a, b| a.1.total_cmp(&b.1));
    let full = r.min(cands.len());
    let mut alpha = ALPHA_INIT;
    loop {
      // 候选池按距离升序，retain 保序，可反复取队首
      let mut pool = cands.clone();
      let mut selected: Vec<u32> = Vec::with_capacity(r);
      while !pool.is_empty() && selected.len() < r {
        let (pstar, _) = pool.remove(0);
        // 安全性：候选 id 均来自图内已分配元素，恒小于向量池长度
        let sv = unsafe { vectors.get_unchecked(pstar as usize) };
        selected.push(pstar);
        pool
          .retain(|&(p, dp)| l2_sq(sv, unsafe { vectors.get_unchecked(p as usize) }) * alpha > dp);
      }
      if selected.len() >= full || alpha >= ALPHA_MAX {
        return selected;
      }
      alpha *= 2.0;
    }
  }
}

/// 归一化（零向量原样返回）。先除以最大绝对值再求范数：极大分量的平方会
/// 溢出为 inf、极小分量的平方会下溢为 0，二者都使范数失真、不同方向坍缩成
/// 同一（零）向量。缩放后分量绝对值 ≤ 1，平方和必有限；且最大分量绝对值
/// 恰为 1，范数 ∈ [1, √dims]，除法安全。输入已保证有限，结果必有限。
fn normalize(v: &[f32]) -> Box<[f32]> {
  let max = v.iter().fold(0.0f32, |m, &x| m.max(x.abs()));
  if max == 0.0 {
    return v.into();
  }
  // 单趟求缩放后的范数，再单趟写出，免去中间 Vec 分配（语义与两段式一致）
  let norm = v
    .iter()
    .map(|&x| {
      let s = x / max;
      s * s
    })
    .sum::<f32>()
    .sqrt();
  v.iter().map(|&x| x / max / norm).collect::<Box<[f32]>>()
}

/// 平方 L2；`d*d` 展开利于 LLVM 自动向量化（避免 powi 逐元素调用）
#[inline]
pub(crate) fn l2_sq(a: &[f32], b: &[f32]) -> f32 {
  a.iter()
    .zip(b)
    .map(|(x, y)| {
      let d = x - y;
      d * d
    })
    .sum()
}
