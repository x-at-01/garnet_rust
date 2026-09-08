//! 集合 RMW 命令 per-key 独占锁并发正确性测试与 update_key_id_meta 守卫语义测试。
//!
//! 多 OS 线程各自持 compio Runtime 并共享 Arc<WedbStore>（每线程独立会话），
//! 对同一键并发读改写，验证锁串行化后无静默丢更新/删除复活。

use std::{future::Future, sync::Arc, thread};

use aok::{OK, Void};
use compio::runtime::Runtime;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use wedb_zset::ZAddOpt;
use wkv::{StoreConfig, StoreSession, WedbStore};

/// 并发线程数
const N_THREADS: usize = 8;

/// 每线程操作数
const OPS_PER_THREAD: usize = 25;

/// 并发轮数（多轮循环防偶现）
const ROUNDS: usize = 3;

fn create_test_store() -> aok::Result<(tempfile::TempDir, Arc<WedbStore<SegmentedDevice>>)> {
  let dir = tempdir()?;
  let db_path = dir.path().join("concurrent_rmw.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);
  let store = Arc::new(WedbStore::open(
    StoreConfig::new(4096, 64 * 1024, 128, 0.5)?,
    device,
  )?);
  Ok((dir, store))
}

/// 多线程并发驱动器：每线程独立 Runtime 与会话，`f(tid, session)` 产出该线程的异步工作流
///
/// 注意：compio thread-per-core 底座的会话内部 future 非 Send（不跨线程转移），
/// future 仅在本线程内 block_on 消费，故 Fut 不要求 Send
fn run_threads<S, Fut>(store: &Arc<WedbStore<SegmentedDevice>>, f: S) -> Void
where
  S: Fn(usize, StoreSession<SegmentedDevice>) -> Fut + Send + Sync + 'static,
  Fut: Future<Output = aok::Result<()>> + 'static,
{
  let f = Arc::new(f);
  let handles = (0..N_THREADS)
    .map(|tid| {
      let store = Arc::clone(store);
      let f = Arc::clone(&f);
      thread::spawn(move || -> aok::Result<()> {
        let rt = Runtime::new()?;
        rt.block_on(async {
          let session = store.new_session()?;
          f(tid, session).await
        })?;
        Ok(())
      })
    })
    .collect::<Vec<_>>();
  for h in handles {
    h.join().unwrap()?;
  }
  OK
}

/// 并发 HSET 同一 key 的 N_THREADS * OPS_PER_THREAD 个不同 field：
/// 断言 HLEN 精确等于总数且 HGET 全部命中（无丢更新），多轮循环防偶现
async fn hset_worker(
  session: StoreSession<SegmentedDevice>,
  key: Vec<u8>,
  tid: usize,
) -> aok::Result<()> {
  for i in 0..OPS_PER_THREAD {
    let field = format!("f_{tid}_{i}").into_bytes();
    let val = format!("v_{tid}_{i}").into_bytes();
    session.hset(&key, field, val).await?;
  }
  OK
}

#[test]
fn test_concurrent_hset_distinct_fields() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    for round in 0..ROUNDS {
      let key = format!("hset:r{round}").into_bytes();
      let worker_key = key.clone();
      run_threads(&store, move |tid, session| {
        hset_worker(session, worker_key.clone(), tid)
      })?;
      let session = store.new_session()?;
      assert_eq!(session.hlen(&key).await?, N_THREADS * OPS_PER_THREAD);
      for tid in 0..N_THREADS {
        for i in 0..OPS_PER_THREAD {
          let field = format!("f_{tid}_{i}").into_bytes();
          assert_eq!(
            session.hget(&key, &field).await?,
            Some(format!("v_{tid}_{i}").into_bytes()),
            "并发 HSET 丢更新: round={round} field={field:?}"
          );
        }
      }
    }
    OK
  })
}

/// 并发 HINCRBY 同一 field：终值必须精确等于 N_THREADS * OPS_PER_THREAD
async fn hincrby_worker(
  session: StoreSession<SegmentedDevice>,
  key: Vec<u8>,
  tid: usize,
) -> aok::Result<()> {
  let _ = tid;
  for _ in 0..OPS_PER_THREAD {
    session.hincrby(&key, b"ctr", 1).await?;
  }
  OK
}

#[test]
fn test_concurrent_hincrby_same_field() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    for round in 0..ROUNDS {
      let key = format!("hincrby:r{round}").into_bytes();
      let worker_key = key.clone();
      run_threads(&store, move |tid, session| {
        hincrby_worker(session, worker_key.clone(), tid)
      })?;
      let session = store.new_session()?;
      assert_eq!(
        session.hget(&key, b"ctr").await?,
        Some((N_THREADS * OPS_PER_THREAD).to_string().into_bytes()),
        "并发 HINCRBY 丢失自增: round={round}"
      );
    }
    OK
  })
}

/// 并发 SADD 同一 key 的互不相同成员：断言 SCARD 精确且全部成员可命中
async fn sadd_worker(
  session: StoreSession<SegmentedDevice>,
  key: Vec<u8>,
  tid: usize,
) -> aok::Result<()> {
  let members: Vec<Vec<u8>> = (0..OPS_PER_THREAD)
    .map(|i| format!("m_{tid}_{i}").into_bytes())
    .collect();
  session.sadd(&key, members).await?;
  OK
}

#[test]
fn test_concurrent_sadd_distinct_members() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    for round in 0..ROUNDS {
      let key = format!("sadd:r{round}").into_bytes();
      let worker_key = key.clone();
      run_threads(&store, move |tid, session| {
        sadd_worker(session, worker_key.clone(), tid)
      })?;
      let session = store.new_session()?;
      assert_eq!(session.scard(&key).await?, N_THREADS * OPS_PER_THREAD);
      for tid in 0..N_THREADS {
        for i in 0..OPS_PER_THREAD {
          assert!(
            session
              .sismember(&key, format!("m_{tid}_{i}").as_bytes())
              .await?,
            "并发 SADD 丢成员: round={round}"
          );
        }
      }
    }
    OK
  })
}

/// 并发 ZADD 同一 key 的互不相同成员：断言 ZCARD 精确且分值全部正确
async fn zadd_worker(
  session: StoreSession<SegmentedDevice>,
  key: Vec<u8>,
  tid: usize,
) -> aok::Result<()> {
  for i in 0..OPS_PER_THREAD {
    session
      .zadd(
        &key,
        (tid * 1000 + i) as f64,
        format!("zm_{tid}_{i}"),
        ZAddOpt::default(),
      )
      .await?;
  }
  OK
}

#[test]
fn test_concurrent_zadd_distinct_members() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    for round in 0..ROUNDS {
      let key = format!("zadd:r{round}").into_bytes();
      let worker_key = key.clone();
      run_threads(&store, move |tid, session| {
        zadd_worker(session, worker_key.clone(), tid)
      })?;
      let session = store.new_session()?;
      assert_eq!(session.zcard(&key).await?, N_THREADS * OPS_PER_THREAD);
      for tid in 0..N_THREADS {
        for i in 0..OPS_PER_THREAD {
          assert_eq!(
            session
              .zscore(&key, format!("zm_{tid}_{i}").as_bytes())
              .await?,
            Some((tid * 1000 + i) as f64),
            "并发 ZADD 丢成员: round={round}"
          );
        }
      }
    }
    OK
  })
}

/// 并发 ZINCRBY 同一 member：终值必须精确等于 2 * N_THREADS * OPS_PER_THREAD
async fn zincrby_worker(
  session: StoreSession<SegmentedDevice>,
  key: Vec<u8>,
  tid: usize,
) -> aok::Result<()> {
  let _ = tid;
  for _ in 0..OPS_PER_THREAD {
    session.zincrby(&key, 2.0, b"ctr").await?;
  }
  OK
}

#[test]
fn test_concurrent_zincrby_same_member() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    for round in 0..ROUNDS {
      let key = format!("zincrby:r{round}").into_bytes();
      let worker_key = key.clone();
      run_threads(&store, move |tid, session| {
        zincrby_worker(session, worker_key.clone(), tid)
      })?;
      let session = store.new_session()?;
      assert_eq!(
        session.zscore(&key, b"ctr").await?,
        Some((2 * N_THREADS * OPS_PER_THREAD) as f64),
        "并发 ZINCRBY 丢失自增: round={round}"
      );
    }
    OK
  })
}

/// update_key_id_meta 守卫语义（papaya compute 原子化后）：
/// 空缺直插、幂等零写、同版本 alive -> false 收敛、拒绝同版本幽灵复活、拒绝陈旧版本、接受更高版本
#[test]
fn test_update_key_id_meta_guard_semantics() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;

    // 空缺键：直接插入
    store.update_key_id_meta(1001, 5, true);
    assert_eq!(store.get_key_id_meta(1001), Some((5, true)));

    // 幂等：同版本同状态零变更
    store.update_key_id_meta(1001, 5, true);
    assert_eq!(store.get_key_id_meta(1001), Some((5, true)));

    // 同版本 alive -> false 收敛：接受
    store.update_key_id_meta(1001, 5, false);
    assert_eq!(store.get_key_id_meta(1001), Some((5, false)));

    // 同版本幽灵复活：拒绝
    store.update_key_id_meta(1001, 5, true);
    assert_eq!(store.get_key_id_meta(1001), Some((5, false)));

    // 陈旧版本覆盖：拒绝
    store.update_key_id_meta(1001, 4, true);
    assert_eq!(store.get_key_id_meta(1001), Some((5, false)));

    // 更高版本：接受
    store.update_key_id_meta(1001, 6, true);
    assert_eq!(store.get_key_id_meta(1001), Some((6, true)));

    OK
  })
}

/// 并发 update_key_id_meta 版本水位单调性：多线程交错递增版本，
/// 终态版本必须精确等于最大目标版本（原子 compute 守卫下低版本不可回退水位）
#[test]
fn test_update_key_id_meta_concurrent_monotonic() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let (_dir, store) = create_test_store()?;
    store.update_key_id_meta(2001, 0, true);

    let store_ref = Arc::clone(&store);
    let handles: Vec<_> = (1..=N_THREADS)
      .map(|v| {
        let store_ref = Arc::clone(&store_ref);
        thread::spawn(move || {
          store_ref.update_key_id_meta(2001, v as u64, true);
        })
      })
      .collect();
    for h in handles {
      h.join().unwrap();
    }

    // 守卫保证版本水位单调：无论线程交错顺序，终态必为最大版本
    assert_eq!(
      store.get_key_id_meta(2001),
      Some((N_THREADS as u64, true)),
      "并发版本更新水位被回退"
    );
    OK
  })
}
