use std::{
  f32::consts::TAU,
  str::from_utf8,
  sync::{Arc, RwLock},
  thread,
};

use aok::{OK, Void};
use wedb_vector::{Error, Vamana, VamanaParams, VectorManager, VectorSet};

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 简单二维向量
fn v2(x: f32, y: f32) -> Vec<f32> {
  vec![x, y]
}

/// 暴力余弦相似度 top-k（召回率对照基准）
fn brute_top(vectors: &[(usize, Vec<f32>)], q: &[f32], k: usize) -> Vec<usize> {
  let n = q.iter().map(|x| x * x).sum::<f32>().sqrt();
  let mut scored: Vec<(usize, f32)> = vectors
    .iter()
    .map(|(i, v)| {
      let nv = v.iter().map(|x| x * x).sum::<f32>().sqrt();
      let dot = q.iter().zip(v).map(|(a, b)| a * b).sum::<f32>();
      (*i, if nv * n == 0.0 { 0.0 } else { dot / (nv * n) })
    })
    .collect();
  scored.sort_by(|a, b| b.1.total_cmp(&a.1));
  scored.into_iter().take(k).map(|(i, _)| i).collect()
}

/// 检索结果元素名 "eN" 还原为编号
fn hit_ids(hits: &[(Box<[u8]>, f32)]) -> Vec<usize> {
  hits
    .iter()
    .filter_map(|(name, _)| {
      name
        .strip_prefix(b"e".as_slice())
        .and_then(|b| from_utf8(b).ok())
        .and_then(|s| s.parse().ok())
    })
    .collect()
}

/// n 个方向严格互异且均匀分布的圆周方向点（避免归一化后重合导致并列平局）
fn circle(n: usize) -> Vec<Vec<f32>> {
  (0..n)
    .map(|i| {
      let theta = i as f32 * (TAU / n as f32);
      v2(theta.cos(), theta.sin())
    })
    .collect()
}

/// 建一个超过 EXACT_LIMIT 的圆周方向点集，走 Vamana 图检索路径
fn big_circle_set(n: usize) -> (VectorSet, Vec<Vec<f32>>) {
  let mut set = VectorSet::new(
    2,
    VamanaParams {
      degree: 16,
      build_ef: 64,
    },
  )
  .unwrap();
  let vectors = circle(n);
  for (i, v) in vectors.iter().enumerate() {
    assert!(set.add(format!("e{i}").as_bytes(), v).unwrap().is_some());
  }
  (set, vectors)
}

#[test]
fn test_recall_and_order() -> Void {
  let mut set = VectorSet::new(2, VamanaParams::default()).unwrap();
  let vectors = circle(200);
  for (i, v) in vectors.iter().enumerate() {
    assert!(set.add(format!("e{i}").as_bytes(), v).unwrap().is_some());
  }
  assert_eq!(set.len(), 200);

  // 多个查询点的 top-5 召回率应为 100%（小数据集暴力全覆盖）
  let pool: Vec<(usize, Vec<f32>)> = vectors.iter().cloned().enumerate().collect();
  let mut recall_sum = 0.0;
  for q in [v2(3.0, 3.0), v2(-6.0, 5.0), v2(0.1, -0.2), v2(7.0, -6.5)] {
    let hits = set.search(&q, 5, 64).unwrap();
    assert_eq!(hits.len(), 5);
    let got = hit_ids(&hits);
    let expect = brute_top(&pool, &q, 5);
    recall_sum += got.iter().filter(|g| expect.contains(g)).count() as f64 / 5.0;
  }
  assert!(
    recall_sum >= 4.0,
    "4 组查询平均召回率应 >= 100%，实际 {recall_sum}"
  );

  // 相似度降序
  let hits = set.search(&v2(3.0, 3.0), 10, 64).unwrap();
  for w in hits.windows(2) {
    assert!(w[0].1 >= w[1].1, "结果应按相似度降序");
  }
  OK
}

#[test]
fn test_graph_recall_on_large_set() -> Void {
  // 3000 > EXACT_LIMIT，走 Vamana 图检索路径而非暴力
  let (set, vectors) = big_circle_set(3000);
  let pool: Vec<(usize, Vec<f32>)> = vectors.iter().cloned().enumerate().collect();
  let mut recall_sum = 0.0;
  for q in [
    v2(3.0, 3.0),
    v2(-6.0, 5.0),
    v2(0.1, -0.2),
    v2(7.0, -6.5),
    v2(-1.0, -9.0),
  ] {
    let hits = set.search(&q, 10, 96).unwrap();
    assert_eq!(hits.len(), 10);
    let got = hit_ids(&hits);
    let expect = brute_top(&pool, &q, 10);
    recall_sum += got.iter().filter(|g| expect.contains(g)).count() as f64 / 10.0;
  }
  assert!(
    recall_sum >= 4.5,
    "图检索 5 组查询平均召回率应 >= 90%，实际 {recall_sum}"
  );
  OK
}

#[test]
fn test_connectivity_and_invariants() -> Void {
  let degree = 16;
  let mut v = Vamana::new(
    2,
    VamanaParams {
      degree,
      build_ef: 48,
    },
  )
  .unwrap();
  // 随机方向 + 天然乱序的插入序：剪枝后任意插入序都应保持从入口可达
  let n = 2000u32;
  let dirs: Vec<[f32; 2]> = (0..n)
    .map(|_| {
      let t = fastrand::f32() * TAU;
      [t.cos(), t.sin()]
    })
    .collect();
  for d in &dirs {
    v.insert(d).unwrap();
  }
  // 随机删 30%
  for id in 0..n {
    if fastrand::f32() < 0.3 {
      v.remove(id);
    }
  }
  // 不变式：出度不超上限、无自环、无重边
  for id in 0..n {
    let links = v.links(id);
    assert!(links.len() <= degree, "节点 {id} 出度超上限");
    assert!(!links.contains(&id), "节点 {id} 不应有自环");
    let mut dedup = links.to_vec();
    dedup.sort_unstable();
    dedup.dedup();
    assert_eq!(dedup.len(), links.len(), "节点 {id} 邻接表有重边");
  }
  // 从入口 BFS：全部节点（含墓碑，搜索会路过）必须可达
  let entry = v.entry().expect("未删空时入口不应为空");
  let mut seen = vec![false; n as usize];
  let mut stack = vec![entry];
  seen[entry as usize] = true;
  while let Some(cur) = stack.pop() {
    for &nb in v.links(cur) {
      if !seen[nb as usize] {
        seen[nb as usize] = true;
        stack.push(nb);
      }
    }
  }
  let reached = seen.iter().filter(|&&s| s).count();
  assert_eq!(
    reached, n as usize,
    "随机插入序 + 随机删除后，全部节点仍应从入口可达"
  );
  OK
}

#[test]
fn test_tombstones_not_returned() -> Void {
  let n = 2500;
  let (mut set, vectors) = big_circle_set(n);
  let pool: Vec<(usize, Vec<f32>)> = vectors.iter().cloned().enumerate().collect();
  let q = v2(1.0, 0.0);
  // 删除暴力 top-5 后，检索结果不得再返回它们
  let top5 = brute_top(&pool, &q, 5);
  for i in &top5 {
    assert!(set.remove(format!("e{i}").as_bytes()));
  }
  let hits = set.search(&q, 5, 96).unwrap();
  assert_eq!(hits.len(), 5);
  let got = hit_ids(&hits);
  assert!(
    got.iter().all(|g| !top5.contains(g)),
    "已删元素不应出现在检索结果"
  );
  for w in hits.windows(2) {
    assert!(w[0].1 >= w[1].1, "结果应按相似度降序");
  }
  // k 大于存活数时仍不返回墓碑，且存活元素尽数可达
  let hits = set.search(&q, n, 96).unwrap();
  assert!(hits.iter().all(|(name, _)| {
    !top5.contains(
      &name
        .strip_prefix(b"e".as_slice())
        .and_then(|b| from_utf8(b).ok())
        .and_then(|s| s.parse().ok())
        .unwrap_or(usize::MAX),
    )
  }));
  assert!((hits.len() * 2) > n - top5.len(), "存活元素应大部分可达");
  OK
}

#[test]
fn test_concurrent_search() -> Void {
  let (set, vectors) = big_circle_set(3000);
  let pool: Vec<(usize, Vec<f32>)> = vectors.iter().cloned().enumerate().collect();
  // 预先算好每个查询的暴力 top-1（圆周方向点最近邻唯一）
  let queries: Vec<(Vec<f32>, usize)> = [
    v2(3.0, 3.0),
    v2(-6.0, 5.0),
    v2(0.1, -0.2),
    v2(7.0, -6.5),
    v2(-1.0, -9.0),
  ]
  .into_iter()
  .map(|q| {
    let top1 = brute_top(&pool, &q, 1)[0];
    (q, top1)
  })
  .collect();
  // 多线程并发只读检索：验证代数标记 visited 在并发下无重复结果
  let set = Arc::new(RwLock::new(set));
  let handles: Vec<_> = (0..4)
    .map(|_| {
      let (set, queries) = (set.clone(), queries.clone());
      thread::spawn(move || {
        for (q, top1) in &queries {
          let hits = set.read().unwrap().search(q, 5, 96).unwrap();
          assert_eq!(hits.len(), 5);
          let got = hit_ids(&hits);
          assert_eq!(got[0], *top1, "并发检索 top-1 应与暴力一致");
          // 无重复 id
          let mut dedup = got.clone();
          dedup.sort_unstable();
          dedup.dedup();
          assert_eq!(dedup.len(), got.len(), "并发检索不应出现重复结果");
        }
      })
    })
    .collect();
  for h in handles {
    h.join().unwrap();
  }
  OK
}

#[test]
fn test_finite_and_dims_defense() -> Void {
  assert!(matches!(
    VectorSet::new(0, VamanaParams::default()),
    Err(Error::InvalidDims(0))
  ));
  let mut set = VectorSet::new(2, VamanaParams::default()).unwrap();
  // NaN/Inf 向量拒绝入库
  assert!(matches!(
    set.add(b"nan", &[f32::NAN, 1.0]),
    Err(Error::InvalidVector)
  ));
  assert!(matches!(
    set.add(b"inf", &[f32::INFINITY, 1.0]),
    Err(Error::InvalidVector)
  ));
  assert!(set.is_empty(), "被拒向量不应消耗 id");
  // 维度不匹配拒绝入库，且不消耗 id
  assert!(matches!(
    set.add(b"short", &[1.0]),
    Err(Error::DimMismatch {
      expected: 2,
      got: 1
    })
  ));
  set.add(b"ok", &v2(1.0, 0.0)).unwrap();
  // NaN 查询与维度不符查询
  assert!(matches!(
    set.search(&[f32::NAN, 1.0], 1, 8),
    Err(Error::InvalidVector)
  ));
  assert!(matches!(
    set.search(&[1.0], 1, 8),
    Err(Error::DimMismatch {
      expected: 2,
      got: 1
    })
  ));
  // k=0 返回空
  assert!(set.search(&v2(1.0, 0.0), 0, 8).unwrap().is_empty());
  OK
}

#[test]
fn test_duplicate_add_and_remove() -> Void {
  let mut set = VectorSet::new(2, VamanaParams::default()).unwrap();
  set.add(b"a", &v2(1.0, 0.0)).unwrap();
  // 重复插入不生效
  assert!(set.add(b"a", &v2(0.0, 1.0)).unwrap().is_none());
  assert_eq!(set.len(), 1);

  // 维度不匹配
  assert!(matches!(
    set.add(b"b", [1.0, 2.0, 3.0].as_slice()),
    Err(Error::DimMismatch {
      expected: 2,
      got: 3
    })
  ));

  assert!(set.remove(b"a"));
  assert!(!set.remove(b"a"));
  assert!(set.is_empty());

  // 空集检索返回空
  assert!(set.search(&v2(1.0, 0.0), 3, 32).unwrap().is_empty());
  // 删除后同名可重插，拿到新 id
  assert!(set.add(b"a", &v2(0.0, 1.0)).unwrap().is_some());
  assert!(set.contains(b"a"));
  OK
}

#[test]
fn test_attrs_and_membership() -> Void {
  let mut set = VectorSet::new(2, VamanaParams::default()).unwrap();
  set.add(b"x", &v2(1.0, 1.0)).unwrap();
  assert!(set.contains(b"x"));
  assert!(set.set_attr(b"x", Some(b"json-data".to_vec())));
  assert_eq!(set.attr(b"x").unwrap().as_ref(), b"json-data");
  // 覆盖与删除
  assert!(set.set_attr(b"x", Some(b"v2".to_vec())));
  assert_eq!(set.attr(b"x").unwrap().as_ref(), b"v2");
  assert!(set.set_attr(b"x", None));
  assert!(set.attr(b"x").is_none());
  // 不存在元素
  assert!(!set.set_attr(b"zz", Some(b"v".to_vec())));
  assert!(!set.contains(b"zz"));
  OK
}

#[test]
fn test_links_and_random() -> Void {
  let mut set = VectorSet::new(2, VamanaParams::default()).unwrap();
  for i in 0..30 {
    set
      .add(format!("e{}", i).as_bytes(), &v2(i as f32, (i * i) as f32))
      .unwrap();
  }
  let links = set.links_of(b"e0").unwrap();
  assert!(!links.is_empty(), "入口元素也应有邻居");

  // 随机取样总能返回存活元素
  assert!(set.random().is_some());
  set.remove(b"e0");
  // 已删元素返回错误
  assert!(matches!(set.links_of(b"e0"), Err(Error::ElementNotFound)));
  OK
}

#[test]
fn test_links_with_scores_sorted() -> Void {
  let mut set = VectorSet::new(2, VamanaParams::default()).unwrap();
  for i in 0..40 {
    set
      .add(format!("e{}", i).as_bytes(), &v2(i as f32, (i * i) as f32))
      .unwrap();
  }
  let links = set.links_with_scores(b"e5").unwrap();
  assert!(!links.is_empty());
  // 相似度降序，且对角线（自身相似度）为 1
  for w in links.windows(2) {
    assert!(w[0].1 >= w[1].1);
  }
  assert!(links.iter().all(|(_, s)| (-1.0..=1.0).contains(s)));
  OK
}

#[test]
fn test_zero_vector() -> Void {
  let mut set = VectorSet::new(2, VamanaParams::default()).unwrap();
  set.add(b"zero", &[0.0, 0.0]).unwrap();
  set.add(b"one", &v2(1.0, 0.0)).unwrap();
  // 零向量参与检索不 panic、不越界
  let hits = set.search(&v2(1.0, 0.0), 2, 16).unwrap();
  assert!(!hits.is_empty());
  OK
}

#[test]
fn test_normalize_numerical_stability() -> Void {
  let mut v = Vamana::new(2, VamanaParams::default()).unwrap();
  // 巨大分量：平方溢出为 inf；极小分量：平方下溢为 0。归一化须先按最大
  // 绝对值缩放，方向信息不得丢失（不得坍缩成同一零向量）
  let hp = [3.0e19, 3.0e19];
  let hn = [3.0e19, -3.0e19];
  let tp = [1.0e-30, 1.0e-30];
  let tn = [1.0e-30, -1.0e-30];
  let mx = [f32::MAX, 0.0];
  let a = v.insert(&hp).unwrap();
  let b = v.insert(&hn).unwrap();
  let c = v.insert(&tp).unwrap();
  let d = v.insert(&tn).unwrap();
  let e = v.insert(&mx).unwrap();
  let unit = 1.0 / 2.0f32.sqrt();
  assert_eq!(v.vector(a).unwrap(), [unit, unit].as_slice());
  assert_eq!(v.vector(b).unwrap(), [unit, -unit].as_slice());
  assert_eq!(v.vector(c).unwrap(), [unit, unit].as_slice());
  assert_eq!(v.vector(d).unwrap(), [unit, -unit].as_slice());
  assert_eq!(v.vector(e).unwrap(), [1.0, 0.0].as_slice());
  // 同向元素归一化后重合（距离 0）：top-1 必属同向集合，± 对称方向不得
  // 与其他方向打成平手（坍缩 bug 下所有向量距离全为 0 的平局）
  for (i, (q, same_dir)) in [
    (&hp, [a, c]),
    (&hn, [b, d]),
    (&tp, [a, c]),
    (&tn, [b, d]),
    (&mx, [e, e]),
  ]
  .into_iter()
  .enumerate()
  {
    let hits = v.search(q, 1, 16).unwrap();
    assert!(
      hits[0].0 == same_dir[0] || hits[0].0 == same_dir[1],
      "查询 {i} 的 top-1 应为同向元素，实际 e{}",
      hits[0].0
    );
  }
  OK
}

#[test]
fn test_entry_replacement_and_empty_recovery() -> Void {
  // > EXACT_LIMIT，全程走图检索路径
  let mut v = Vamana::new(
    2,
    VamanaParams {
      degree: 16,
      build_ef: 64,
    },
  )
  .unwrap();
  let dirs = circle(2200);
  for d in &dirs {
    v.insert(d).unwrap();
  }
  // 入口是首插元素；删除入口后应替换为存活元素，检索仍可用
  assert_eq!(v.entry(), Some(0));
  assert!(v.remove(0));
  let entry = v.entry().expect("删除入口后应替换为存活元素");
  assert_ne!(entry, 0);
  assert!(v.contains(entry));
  let hits = v.search(&dirs[500], 1, 96).unwrap();
  assert_eq!(hits[0].0, 500, "入口替换后检索应照常命中");

  // 全部删除：入口清空、图路径检索返回空
  for i in 1..2200u32 {
    assert!(v.remove(i));
  }
  assert!(v.is_empty());
  assert_eq!(v.entry(), None);
  assert!(v.search(&dirs[42], 1, 96).unwrap().is_empty());

  // 空图重插恢复：入口重建，检索可用
  let id = v.insert(&dirs[42]).unwrap();
  assert_eq!(v.entry(), Some(id));
  let hits = v.search(&dirs[42], 1, 64).unwrap();
  assert_eq!(hits[0].0, id, "空图恢复后应命中重插元素");
  OK
}

#[test]
fn test_reinsert_after_remove_no_residue() -> Void {
  let mut set = VectorSet::new(2, VamanaParams::default()).unwrap();
  set.add(b"a", &v2(1.0, 0.0)).unwrap();
  assert!(set.set_attr(b"a", Some(b"old".to_vec())));
  assert!(set.remove(b"a"));
  // 同名重插拿新 id，存新向量：旧向量/属性不得残留
  assert!(set.add(b"a", &v2(0.0, 1.0)).unwrap().is_some());
  let id = set.id_of(b"a").unwrap();
  assert_eq!(set.vector_of(id).unwrap(), [0.0, 1.0].as_slice());
  assert!(set.attr(b"a").is_none(), "重插后旧属性不得残留");
  // 检索按新向量生效
  let hits = set.search(&v2(0.0, 2.0), 1, 16).unwrap();
  assert_eq!(hits.len(), 1);
  assert_eq!(hits[0].0.as_ref(), b"a");
  OK
}

#[test]
fn test_manager_lifecycle() -> Void {
  let m = VectorManager::new();
  assert!(m.is_empty());

  let (set, created) = m.get_or_create("vs1", 3, VamanaParams::default()).unwrap();
  assert!(created);
  set.write().add(b"a", &[1.0, 2.0, 3.0]).unwrap();
  assert_eq!(m.len(), 1);

  // get_or_create 拿到的是同一个集
  let (same, created) = m.get_or_create("vs1", 3, VamanaParams::default()).unwrap();
  assert!(!created);
  assert!(Arc::ptr_eq(&set, &same));
  assert_eq!(same.read().len(), 1);

  // drop 后可重建
  assert!(m.drop_set("vs1"));
  assert!(!m.drop_set("vs1"));
  let (fresh, created) = m.get_or_create("vs1", 3, VamanaParams::default()).unwrap();
  assert!(created);
  assert_eq!(fresh.read().len(), 0);
  OK
}
