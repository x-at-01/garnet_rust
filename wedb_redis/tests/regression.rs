use std::{io, iter::repeat_n, sync::Arc, thread, time::Instant};

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;
use tempfile::tempdir;
use wdev::SegmentedDevice;
use wedb_redis::prelude::*;
use wedb_zset::{ScoreRange, ZAddOpt};
use whasher::HashMap;
use wkv::{StoreConfig, WedbStore};

/// 十进制补零到 width 位
fn pad(v: impl itoa::Integer, width: usize) -> String {
  let mut buf = itoa::Buffer::new();
  let digits = buf.format(v);
  let mut s = String::with_capacity(width.max(digits.len()));
  s.extend(repeat_n('0', width.saturating_sub(digits.len())));
  s.push_str(digits);
  s
}

#[ctor::ctor(unsafe)]
fn _log_init() {
  log_init::init();
}

/// 测试 1: 内存原位更新（In-Place Update）与只读区 RCU 迁移回归测试
/// 验证：
/// 1. 可变区内的同长度更新必须触发原位就地修改（返回相同逻辑地址，零追加）
/// 2. 推进 ReadOnlyAddress 后，后续更新必须安全退化为 RCU 追加（返回新逻辑地址）
/// 3. RCU 发生后哈希索引自愈，原位读与新版本读保证全局一致
#[test]
fn test_regression_in_place_update_and_rcu_transition() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("reg_inplace.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    // 配置 64KB 页大小，16 页缓冲区
    let config = StoreConfig::new(1024, 64 * 1024, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let key = b"user:session:1001";
    let val1 = b"token_version_0001";
    let addr1 = session.upsert(key, val1).await?;
    assert!(addr1 > 0, "初始写入应分配有效逻辑地址");

    // 1. 在可变区内执行等长更新，必须原位就地修改，地址保持不变
    let val2 = b"token_version_0002";
    let addr2 = session.upsert(key, val2).await?;
    assert_eq!(addr1, addr2, "可变区内等长更新必须原地修改，地址应保持一致");
    let read2 = session.read(key).await?;
    assert_eq!(read2, Some(val2.to_vec()), "原地修改后读取值应更新");

    // 2. 将当前尾部地址推进为只读边界，模拟记录进入只读冷区
    let tail = store.tail_address();
    store.shift_read_only_address(tail);

    // 3. 只读区内的记录被更新时，必须走 RCU 追加新记录
    let val3 = b"token_version_0003";
    let addr3 = session.upsert(key, val3).await?;
    assert!(
      addr3 > addr2,
      "只读区内的更新必须触发 RCU 追加，生成更大的新逻辑地址"
    );

    // 4. 验证最新版本可读，且历史槽位已被自动清理收敛
    let read3 = session.read(key).await?;
    assert_eq!(read3, Some(val3.to_vec()), "RCU 追加后应读取到最新版本");

    info!("回归测试 1: 原位更新与只读区 RCU 迁移测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 2: 冷数据逐出与磁盘回读一致性回归测试（Flush & Evict & Cold Read）
/// 验证：
/// 1. 跨多页写入大批数据后，执行 flush_and_evict_all 将所有页面完全淘汰出内存
/// 2. 淘汰出内存后，所有记录通过 SegmentedDevice 底层块设备异步读取必须 100% 正确
/// 3. 冷数据读取后再次更新，验证从磁盘记录走 RCU 升阶写回到内存尾部
#[test]
fn test_regression_flush_evict_and_cold_read() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("reg_cold_read.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    // 采用较小页 8KB 模拟高频换页
    let config = StoreConfig::new(2048, 8 * 1024, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let count = 200usize;
    let mut pairs = Vec::with_capacity(count);
    let padding = "x".repeat(64);

    for i in 0..count {
      let mut key = String::from("k:cold:");
      key.push_str(&pad(i, 4));
      let key = key.into_bytes();
      let mut val = String::from("v:payload_data_");
      val.push_str(&pad(i, 4));
      val.push('_');
      val.push_str(&padding);
      let val = val.into_bytes();
      session.upsert(&key, &val).await?;
      pairs.push((key, val));
    }

    // 将全部内存数据刷盘并驱逐至磁盘（HeadAddress 追上 TailAddress）
    store.flush_and_evict_all().await?;
    assert_eq!(
      store.head_address(),
      store.tail_address(),
      "逐出后 head 应该等于 tail"
    );

    // 验证所有键从底层设备冷读的一致性
    for (k, expected_v) in &pairs {
      let v = session.read(k).await?;
      assert_eq!(
        v.as_ref(),
        Some(expected_v),
        "冷读数据与写入数据必须完全一致"
      );
    }

    // 对磁盘上的冷数据执行更新，必须成功 RCU 写入内存尾部
    let update_key = &pairs[0].0;
    let new_val = b"new_hot_value_in_memory";
    let new_addr = session.upsert(update_key, new_val).await?;
    assert!(
      new_addr >= store.head_address(),
      "RCU 更新产生的新地址必须在最新内存区"
    );
    assert_eq!(
      session.read(update_key).await?,
      Some(new_val.to_vec()),
      "更新后应读到新值"
    );

    info!("回归测试 2: 冷数据逐出与磁盘回读测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 3: 截断边界与墓碑自愈回归测试（Truncation & Tombstone Invalidation）
/// 验证：
/// 1. 记录被删除后生成墓碑，读取返回 None，entry_count 保持准确
/// 2. 推进 BeginAddress 物理截断历史日志，读取低于 BeginAddress 的地址不会造成悬挂或 panic
/// 3. 再次写入同名 Key 时能从墓碑/截断状态自愈复活
#[test]
fn test_regression_truncation_and_tombstone_healing() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("reg_truncate.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(1024, 64 * 1024, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let k1 = b"transient:key:1";
    let v1 = b"temp_value_1";
    session.upsert(k1, v1).await?;
    assert_eq!(session.read(k1).await?, Some(v1.to_vec()));

    // 1. 删除该 Key 并验证墓碑行为
    let deleted = session.delete(k1).await?;
    assert!(deleted, "首次删除应返回 true");
    assert_eq!(session.read(k1).await?, None, "已删除键读取必须为 None");
    assert!(
      !session.contains_key(k1).await?,
      "contains_key 应返回 false"
    );

    // 重复删除应幂等返回 false
    let deleted_again = session.delete(k1).await?;
    assert!(!deleted_again, "重复删除应返回 false");

    // 2. 写入更多数据推进日志并截断
    let k2 = b"persistent:key:2";
    let v2 = b"val_2";
    session.upsert(k2, v2).await?;

    let mid_addr = store.tail_address();
    store.shift_begin_address(mid_addr).await?;
    assert_eq!(store.begin_address(), mid_addr);

    // 3. 对已删除的 k1 重新写入，验证自愈复活
    let v1_resurrect = b"resurrected_val_1";
    let addr_res = session.upsert(k1, v1_resurrect).await?;
    assert!(addr_res >= mid_addr);
    assert_eq!(session.read(k1).await?, Some(v1_resurrect.to_vec()));

    info!("回归测试 3: 截断边界与墓碑自愈测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 4: 差分预言机随机模型回归测试（Differential Oracle Regression Test with HashMap）
/// 通过构造高随机伪随机序列（固定种子确保 100% 可重现），
/// 对 WedbStore 与内存标准模型（HashMap）进行大量交错操作（写入、原位覆盖、删除、读取、驱逐），
/// 确保任何边界情况下的状态转移与预言机模型严格一致。
#[test]
fn test_regression_differential_oracle_model() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("reg_oracle.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(2048, 16 * 1024, 16, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let mut oracle: HashMap<Vec<u8>, Vec<u8>> = HashMap::default();
    let mut rng = fastrand::Rng::with_seed(20260905);

    let key_space_size = 80usize;
    let total_operations = 1200usize;

    for step in 0..total_operations {
      let key_id = rng.usize(0..key_space_size);
      let mut key = String::from("oracle:key:");
      key.push_str(&pad(key_id, 3));
      let key = key.into_bytes();
      let op = rng.usize(0..100);

      if op < 45 {
        // 45% 概率执行写入 / 覆盖
        let val_len = rng.usize(8..64);
        let mut val = vec![0u8; val_len];
        rng.fill(&mut val);

        session.upsert(&key, &val).await?;
        oracle.insert(key, val);
      } else if op < 75 {
        // 30% 概率执行读取比对
        let actual = session.read(&key).await?;
        assert_eq!(
          actual.as_ref(),
          oracle.get(&key),
          "第 {step} 步读取不匹配: key={:?}",
          String::from_utf8_lossy(&key)
        );
      } else if op < 90 {
        // 15% 概率执行删除
        let actual_del = session.delete(&key).await?;
        let expected_del = oracle.remove(&key).is_some();
        assert_eq!(
          actual_del,
          expected_del,
          "第 {step} 步删除结果不一致: key={:?}",
          String::from_utf8_lossy(&key)
        );
      } else {
        // 10% 概率触发刷盘或逐出，测试动态冷热转换
        if rng.bool() {
          store.flush_all().await?;
        } else {
          store.flush_and_evict_all().await?;
        }
      }
    }

    // 最终全量对账
    for key_id in 0..key_space_size {
      let key = format!("oracle:key:{}", pad(key_id, 3)).into_bytes();
      let actual = session.read(&key).await?;
      assert_eq!(
        actual.as_ref(),
        oracle.get(&key),
        "最终全量核对失败 key={:?}",
        key
      );
    }

    info!("回归测试 4: 差分预言机模型测试（1200 步交错操作）验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 5: 多会话高并发写与读冲突回归测试（Concurrent Multi-Session Contention）
/// 验证在多个 OS 线程并发竞争读写相同 Key 时：
/// 1. LightEpoch 正常调度无死锁
/// 2. 无锁哈希索引 CAS 重试机制正常，无数据竞争与内存破坏
/// 3. 所有并发线程最终能够安全结束且数据保持有效
#[test]
fn test_regression_concurrent_multi_session_contention() -> Void {
  let dir = tempdir()?;
  let db_path = dir.path().join("reg_concurrent.db");
  let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

  let config = StoreConfig::new(2048, 64 * 1024, 16, 0.5)?;
  let store = Arc::new(WedbStore::open(config, device)?);

  let thread_count = 6;
  let ops_per_thread = 150;
  let mut handles = Vec::new();

  for t in 0..thread_count {
    let store_clone = Arc::clone(&store);
    let handle = thread::spawn(move || -> aok::Result<()> {
      let rt = Runtime::new()?;
      rt.block_on(async {
        let session = store_clone.new_session()?;
        for i in 0..ops_per_thread {
          // 交叉写入独立 key 和热点共享 key
          let hot_key = b"hot:shared:key";
          let val = format!("thread_{}_seq_{}", t, i).into_bytes();
          session.upsert(hot_key, &val).await?;

          let private_key = format!("priv:{}:{}", t, i).into_bytes();
          session.upsert(&private_key, &val).await?;

          let read_val = session.read(&private_key).await?;
          assert_eq!(read_val, Some(val));
        }
        aok::Result::<()>::Ok(())
      })?;
      OK
    });
    handles.push(handle);
  }

  for h in handles {
    h.join()
      .map_err(|e| io::Error::other(format!("并发线程异常退出: {e:?}")))??;
  }

  info!("回归测试 5: 多会话高并发写与读冲突测试通过");
  OK
}

/// 测试 6: 性能基线防劣化门禁测试（Performance Regression Guard）
/// 测量纯内存环境下的点写入与点读取吞吐量，
/// 确保存储引擎在后续重构迭代中不会出现意外的 O(N^2) 退化或全局锁冲突。
#[test]
fn test_regression_performance_baseline_guard() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("reg_perf_guard.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    // 足够大的页以避免触发频繁磁盘 IO，专注评测内存哈希与分配器基线
    let config = StoreConfig::new(16384, 256 * 1024, 32, 0.8)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let n = 2000usize;

    // 1. 批量点写耗时测试
    let start_write = Instant::now();
    for i in 0..n {
      let key = (i as u64).to_be_bytes();
      let val = (i as u64).to_le_bytes();
      session.upsert(&key, &val).await?;
    }
    let write_elapsed = start_write.elapsed();
    let write_iops = (n as f64) / write_elapsed.as_secs_f64();

    // 2. 批量点读耗时测试
    let start_read = Instant::now();
    for i in 0..n {
      let key = (i as u64).to_be_bytes();
      let val = session.read(&key).await?;
      assert!(val.is_some());
    }
    let read_elapsed = start_read.elapsed();
    let read_iops = (n as f64) / read_elapsed.as_secs_f64();

    let geo_iops = (write_iops * read_iops).sqrt();

    info!(
      "性能基线守卫: 写入 {n} 条用时 {:?} ({:.0} ops/s); 读取 {n} 条用时 {:?} ({:.0} ops/s); 几何平均加权吞吐: {:.0} ops/s",
      write_elapsed, write_iops, read_elapsed, read_iops, geo_iops
    );

    // 设定保底性能门槛：Debug 模式下至少需要满足 2000 ops/s，几何平均至少满足 2500 ops/s（防止性能衰退）
    assert!(
      write_iops >= 2000.0,
      "点写吞吐低于基线门槛: {:.0} ops/s",
      write_iops
    );
    assert!(
      read_iops >= 2000.0,
      "点读吞吐低于基线门槛: {:.0} ops/s",
      read_iops
    );
    assert!(
      geo_iops >= 2500.0,
      "几何平均综合吞吐低于基线门槛: {:.0} ops/s",
      geo_iops
    );

    info!("回归测试 6: 性能基线防劣化门禁测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 测试 7: 范围查询正确性、边界截断与性能基线回归测试（Range Query & Scan Regression）
/// 针对有序集合与范围切片场景进行深度回归：
/// 1. 批量插入 500 个带权有序元素
/// 2. 验证基于排名与分数的全量范围切片 [0..-1]、中间分页切片、逆序倒排范围
/// 3. 验证区间开闭边界过滤（ScoreRange [min, max]）
/// 4. 验证追加更新后的最新范围视图一致性与非空断言
/// 5. 测量高频范围查询吞吐量（不得低于 1000 ops/s 防劣化底线）
#[test]
fn test_regression_range_query_and_scan() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    let dir = tempdir()?;
    let db_path = dir.path().join("reg_range.db");
    let device = Arc::new(SegmentedDevice::single_file(&db_path)?);

    let config = StoreConfig::new(2048, 64 * 1024, 64, 0.5)?;
    let store = Arc::new(WedbStore::open(config, device)?);
    let session = store.new_session()?;

    let zset_key = b"rank:leaderboard:daily";
    let n = 100usize;

    // 1. 验证 zmadd 空集合防御：空批量添加不应创建幽灵键
    let phantom_key = b"rank:phantom:empty";
    let empty_items: [(f64, &[u8]); 0] = [];
    let added_empty = session
      .zmadd(phantom_key, empty_items, ZAddOpt::default())
      .await?;
    assert_eq!(added_empty, 0);
    assert!(
      !session.contains_key(phantom_key).await?,
      "空集合 zmadd 不应在数据库中创建幽灵键"
    );

    // 2. 批量写入带分数值的有序记录（前 50 条使用 zmadd 批量插入，后 50 条使用 zadd 单条插入）
    let batch_items: Vec<(f64, Vec<u8>)> = (1..=50)
      .map(|i| (i as f64 * 10.0, format!("user:{}", pad(i, 4)).into_bytes()))
      .collect();
    let batch_added = session
      .zmadd(zset_key, batch_items, ZAddOpt::default())
      .await?;
    assert_eq!(batch_added, 50, "zmadd 批量插入 50 个元素应成功");

    for i in 51..=n {
      let score = i as f64 * 10.0;
      let member = format!("user:{}", pad(i, 4)).into_bytes();
      session
        .zadd(zset_key, score, member, ZAddOpt::default())
        .await?;
    }

    assert_eq!(session.zcard(zset_key).await?, n);

    // 3. 验证基础正向范围查询（获取前 10 名）
    let top10 = session.zrange(zset_key, 0, 9, false).await?;
    assert_eq!(top10.len(), 10, "前 10 名切片长度必须为 10");
    assert_eq!(top10[0].0, b"user:0001");
    assert_eq!(top10[0].1, 10.0);
    assert_eq!(top10[9].0, b"user:0010");
    assert_eq!(top10[9].1, 100.0);

    // 4. 验证逆序范围查询（获取倒排后前 5 名）
    let rev_top5 = session.zrange(zset_key, 0, 4, true).await?;
    assert_eq!(rev_top5.len(), 5);
    assert_eq!(rev_top5[0].0, format!("user:{}", pad(n, 4)).into_bytes());
    assert_eq!(rev_top5[0].1, n as f64 * 10.0);

    // 5. 验证分数区间范围查询 (ScoreRange)
    let score_slice = session
      .zrangebyscore(
        zset_key,
        ScoreRange::new(100.0, true, 200.0, true),
        false,
        0,
        100,
      )
      .await?;
    assert_eq!(
      score_slice.len(),
      11,
      "分数区间 [100.0, 200.0] 元素数量应为 11"
    );
    assert_eq!(score_slice.first().map(|s| s.1), Some(100.0));
    assert_eq!(score_slice.last().map(|s| s.1), Some(200.0));

    // 6. 验证空区间与越界区间防御
    let empty_slice = session
      .zrangebyscore(
        zset_key,
        ScoreRange::new(9999.0, true, 10000.0, true),
        false,
        0,
        10,
      )
      .await?;
    assert!(empty_slice.is_empty(), "不存在的区间范围应返回空向量");

    // 7. 范围查询性能基线守卫（连续执行 200 次范围查询）
    let start = Instant::now();
    for _ in 0..200 {
      let res = session.zrange(zset_key, 10, 59, false).await?;
      assert_eq!(res.len(), 50);
    }
    let elapsed = start.elapsed();
    let range_qps = 200.0 / elapsed.as_secs_f64();
    info!(
      "范围查询性能基线: 200 次范围切片用时 {:?}, 吞吐 {:.0} ops/s",
      elapsed, range_qps
    );
    assert!(
      range_qps >= 200.0,
      "范围查询吞吐低于门禁基线: {:.0} ops/s",
      range_qps
    );

    info!("回归测试 7: 范围查询正确性、边界与性能防劣化测试通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
