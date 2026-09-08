//! 存储层零拷贝直读与批量读契约测试（原 store 测试模块，含 MGET 键序/多库隔离契约）
//! 零拷贝读路径测试：read_with 闭包借用、多候选时序、同步内存直读与批量预取。

use aok::{OK, Void};
use compio::runtime::Runtime;
use log::info;

mod support;
use support::{config, open_store, pad};
use wedb_redis::prelude::*;

/// 对标 Garnet Tsavorite InternalRead 零拷贝语义 —— 读路径零拷贝借用与首项快速探针验证
#[test]
fn test_zero_copy_read_with() -> Void {
  let rt = Runtime::new()?;

  rt.block_on(async {
    let env = open_store("zero_copy.db", config(64, 16 * 1024, 16)?)?;
    let store = env.store;
    let session = store.new_session()?;

    // 1. 内存活跃区快速探针零拷贝验证
    let key1 = b"probe_zero_copy_key_001";
    let val1 = b"hello_world_zero_copy_val";
    session.upsert(key1, val1).await?;

    // 闭包直接提取长度，零堆分配
    let len_res = session.read_with(key1, |v| v.len()).await?;
    assert_eq!(len_res, Some(val1.len()));

    // 闭包直接比对内容切片，零堆分配
    let matches_res = session.read_with(key1, |v| v == val1).await?;
    assert_eq!(matches_res, Some(true));

    // read_raw_with 与 read_with 行为一致（read_raw_with 直调物理键，须带会话前缀）
    let str_k = session.session_string_key(key1);
    let raw_len = session.read_raw_with(&str_k, |v| v.len()).await?;
    assert_eq!(raw_len, Some(val1.len()));

    // 2. 不存在与已删除键的边界测试
    let missing_len = session.read_with(b"non_exist_key", |v| v.len()).await?;
    assert_eq!(missing_len, None);

    assert!(session.delete(key1).await?);
    let deleted_len = session.read_with(key1, |v| v.len()).await?;
    assert_eq!(deleted_len, None);

    // 复活键并验证原地与追加
    session.upsert(key1, b"resurrected_val").await?;
    let resurrected_val = session.read_with(key1, |v| v.to_vec()).await?;
    assert_eq!(resurrected_val, Some(b"resurrected_val".to_vec()));

    // 3. 驱逐至磁盘后的异步扇区读取零拷贝借用验证
    let cold_k = b"cold_evicted_zero_copy_key";
    let cold_v = b"cold_payload_long_string_zero_copy_read_with_test_xxxxxxxxxx";
    session.upsert(cold_k, cold_v).await?;

    // 强制刷盘并驱逐至磁盘只读冷区
    store.flush_all().await?;
    store.flush_and_evict_all().await?;
    let tail = store.hlog.tail_address();
    store.shift_read_only_address(tail);
    store.shift_head_address(tail);

    // 验证冷数据零拷贝闭包读取正确性
    let cold_len = session.read_with(cold_k, |v| v.len()).await?;
    assert_eq!(cold_len, Some(cold_v.len()));

    let cold_prefix = session
      .read_with(cold_k, |v| v.starts_with(b"cold_payload"))
      .await?;
    assert_eq!(cold_prefix, Some(true));

    info!("读路径零拷贝借用与首项快速探针验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 无直接 C# 原型 —— 多候选槽位与 Tag 碰撞场景下 read_with 版本时序与零拷贝正确性
#[test]
fn test_multi_candidates_zero_copy_read_with() -> Void {
  let rt = Runtime::new()?;

  rt.block_on(async {
    let env = open_store("multi_cand.db", config(64, 16 * 1024, 16)?)?;
    let store = env.store;
    let session = store.new_session()?;

    let k = b"multi_candidate_test_key";
    let v_old = b"value_version_1_old";
    let v_new = b"value_version_2_new_latest";

    // 写入第一个版本
    let addr_old = session.upsert(k, v_old).await?;
    // 物理层直调（hlog.append / 索引 CAS）必须携带会话前缀的物理键
    let str_k = session.session_string_key(k);

    // 模拟并发更新竞争：在尾部追加新记录，并原子更新索引槽位至新地址（对标 Garnet update_address）
    let addr_new = store.hlog.append(&str_k, v_new, addr_old, false)?;
    assert!(store.index.update_address(&str_k, addr_old, addr_new));

    let candidates = store.index.lookup_candidates(&str_k);
    assert!(!candidates.is_empty(), "索引应存在有效候选地址");

    // 验证 read_with 必须返回最新版本 v_new
    let read_val = session.read_with(k, |v| v.to_vec()).await?;
    assert_eq!(
      read_val.as_deref(),
      Some(&v_new[..]),
      "多候选情况下必须优先匹配最新写入的逻辑地址"
    );

    // 模拟追加墓碑删除记录（更高逻辑地址）
    let addr_tombstone = store.hlog.append(&str_k, b"", addr_new, true)?;
    assert!(store.index.update_address(&str_k, addr_new, addr_tombstone));

    // 验证读取时最新墓碑必须遮蔽旧版本，准确返回 None
    let deleted_val = session.read_with(k, |v| v.len()).await?;
    assert_eq!(
      deleted_val, None,
      "最新记录为墓碑时，read_with 必须判定已删除返回 None"
    );

    info!("多候选槽位版本时序与零拷贝读取验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 对标 Garnet Tsavorite InternalRead 同步内存直读快路径 —— OperationStatus 三态映射
///
/// 返回值三态映射：
/// 1. `Ok(Some(Some(val)))` => Garnet `OperationStatus.SUCCESS`
/// 2. `Ok(Some(None))` => Garnet `OperationStatus.NOTFOUND`
/// 3. `Ok(None)` => Garnet `OperationStatus.RECORD_ON_DISK`
///
/// 验证目标：
/// 1. 内存中存在的键，`try_read_in_memory` 与 `read_in_memory` 纯同步命中并返回 `Some(Some(val))`；
/// 2. 不存在的键或已删除的墓碑键，纯同步返回 `Some(None)`；
/// 3. 数据被全量刷盘驱逐至磁盘后（`flush_and_evict_all`），`try_read_in_memory` 同步返回 `None`（指示需走磁盘），
///    随后通过异步回退路径 `read_with.await` 与 `read.await` 成功从磁盘加载数据；
/// 4. 多候选槽位场景下内存命中与墓碑遮蔽语义。
#[test]
fn test_sync_internal_read() -> Void {
  let rt = Runtime::new()?;

  rt.block_on(async {
    let env = open_store("sync_internal_read.db", config(64, 16 * 1024, 16)?)?;
    let store = env.store;
    let session = store.new_session()?;

    // 1. 内存活跃区存在键的纯同步读取 (SUCCESS)
    let key1 = b"sync_read_key_001";
    let val1 = b"sync_read_val_payload_alpha";
    session.upsert(key1, val1).await?;

    // 零拷贝闭包同步直读
    let sync_res = session.try_read_in_memory(key1, |v| v.to_vec())?;
    assert_eq!(
      sync_res,
      Some(Some(val1.to_vec())),
      "内存中存在的数据必须纯同步返回 Some(Some(val))"
    );

    // 零拷贝借用验证：比对切片与长度，零堆分配
    let len_res = session.try_read_in_memory(key1, |v| v.len())?;
    assert_eq!(len_res, Some(Some(val1.len())));

    // 快捷同步方法验证
    let quick_sync = session.read_in_memory(key1)?;
    assert_eq!(quick_sync, Some(Some(val1.to_vec())));

    // 2. 不存在键的纯同步判定 (NOTFOUND)
    let missing_key = b"non_existent_key_xyz";
    let missing_res = session.try_read_in_memory(missing_key, |v| v.len())?;
    assert_eq!(
      missing_res,
      Some(None),
      "未写入的不存在键必须纯同步返回 Some(None)"
    );
    assert_eq!(session.read_in_memory(missing_key)?, Some(None));

    // 3. 已删除墓碑键的纯同步判定 (NOTFOUND)
    let del_key = b"sync_tombstone_key_002";
    let del_val = b"sync_tombstone_val";
    session.upsert(del_key, del_val).await?;
    assert!(session.delete(del_key).await?);

    let tombstone_res = session.try_read_in_memory(del_key, |v| v.len())?;
    assert_eq!(
      tombstone_res,
      Some(None),
      "最新记录为墓碑的键必须纯同步返回 Some(None)"
    );
    assert_eq!(session.read_in_memory(del_key)?, Some(None));

    // 4. 多版本时序与最新版本墓碑遮蔽（物理层直调统一携带会话前缀物理键）
    let multi_key = b"sync_multi_candidate_key";
    let val_old = b"version_old_v1";
    let val_new = b"version_new_v2";
    let addr_v1 = session.upsert(multi_key, val_old).await?;
    let multi_str_k = session.session_string_key(multi_key);
    let addr_v2 = store.hlog.append(&multi_str_k, val_new, addr_v1, false)?;
    assert!(store.index.update_address(&multi_str_k, addr_v1, addr_v2));

    let multi_res = session.try_read_in_memory(multi_key, |v| v.to_vec())?;
    assert_eq!(
      multi_res,
      Some(Some(val_new.to_vec())),
      "多候选情况下必须优先匹配最新写入的逻辑地址"
    );

    // 追加最新墓碑
    let addr_tomb = store.hlog.append(&multi_str_k, b"", addr_v2, true)?;
    assert!(store.index.update_address(&multi_str_k, addr_v2, addr_tomb));
    let multi_tomb_res = session.try_read_in_memory(multi_key, |v| v.len())?;
    assert_eq!(
      multi_tomb_res,
      Some(None),
      "多候选最新记录为墓碑时必须同步返回 Some(None)"
    );

    // 5. 数据被驱逐落盘后返回 None (RECORD_ON_DISK)，且随后异步回退成功
    let disk_key = b"evicted_disk_key_003";
    let disk_val = b"evicted_disk_val_payload_beta";
    session.upsert(disk_key, disk_val).await?;

    // 刷盘并驱逐所有内存页面至磁盘
    store.flush_all().await?;
    store.flush_and_evict_all().await?;
    let tail = store.hlog.tail_address();
    store.shift_read_only_address(tail);
    store.shift_head_address(tail);

    // 验证此时该记录已不在内存中（索引直查须携带会话前缀物理键）
    let disk_str_k = session.session_string_key(disk_key);
    assert!(!store.hlog.is_in_memory(store.index.lookup(&disk_str_k)[0]));

    // 验证 try_read_in_memory 同步返回 None (指示 RECORD_ON_DISK)
    let on_disk_res = session.try_read_in_memory(disk_key, |v| v.to_vec())?;
    assert_eq!(
      on_disk_res, None,
      "已落盘驱逐的记录 try_read_in_memory 必须返回 None 指示需走磁盘"
    );
    assert_eq!(session.read_in_memory(disk_key)?, None);

    // 验证随后通过异步回退路径能够正确读取出数据
    let async_res = session.read_with(disk_key, |v| v.to_vec()).await?;
    assert_eq!(
      async_res,
      Some(disk_val.to_vec()),
      "异步回退路径 read_with 必须能够正确从磁盘加载数据"
    );

    let async_read = session.read(disk_key).await?;
    assert_eq!(async_read, Some(disk_val.to_vec()));

    info!("对照 C# Garnet InternalRead 同步内存直读快路径验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}

/// 对标 Garnet Tsavorite ContextReadWithPrefetch（12 项流水线预取与批量读取）——
/// Tsavorite.cs ContextReadWithPrefetch (PrefetchSize = 12, Sse.Prefetch0 两级预取)、
/// ArrayCommands.cs storageApi.ReadWithPrefetch 与 MGetReadArgBatch 的兼容验证
///
/// 验证目标：
/// 1. 单批次 1 个键（小批次边界）结果 100% 正确；
/// 2. 单批次 12 个键（恰好填满 12 项硬件流水线窗口）结果 100% 正确；
/// 3. 单批次 100 个键（跨多个 12 项窗口）结果 100% 正确；
/// 4. 混合场景：已存在键、未写入不存在键、墓碑删除键、以及覆盖更新键混合批次；
/// 5. 同步纯内存批量直读 try_read_batch_in_memory 与异步 read_batch_with 行为一致性；
/// 6. 冷热交替键：部分键在内存，部分键被刷盘驱逐至磁盘区，跨内存/磁盘混合批次读出 100% 正确；
/// 7. Redis mget / mget_each 底层流水线两级预取接口兼容性。
#[test]
fn test_context_read_with_prefetch() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    // 配置小页以方便触发换页与落盘驱逐：page_size = 16KB, 16 页
    let env = open_store("prefetch_batch.db", config(2048, 16 * 1024, 16)?)?;
    let store = env.store;
    let session = store.new_session()?;

    // ==========================================
    // 阶段 1: 写入基础数据
    // ==========================================
    let mut all_keys = Vec::new();
    let mut all_vals = Vec::new();
    for i in 0..100 {
      let k = format!("prefetch_k_{}", pad(i, 4)).into_bytes();
      let v = format!("prefetch_v_{}_payload", pad(i, 4)).into_bytes();
      session.upsert(&k, &v).await?;
      all_keys.push(k);
      all_vals.push(v);
    }

    // ==========================================
    // 阶段 2: 单批次 1 个键验证 (边界测试)
    // ==========================================
    {
      let single_key = [&all_keys[0][..]];
      let mut results = Vec::new();
      session
        .read_batch_with(&single_key, |idx, val_opt| {
          assert_eq!(idx, 0);
          results.push(val_opt.map(|b| b.to_vec()));
        })
        .await?;
      assert_eq!(results.len(), 1);
      assert_eq!(results[0], Some(all_vals[0].clone()));

      let mut mem_results = Vec::new();
      session.try_read_batch_in_memory(&single_key, |idx, val_opt| {
        assert_eq!(idx, 0);
        mem_results.push(val_opt.map(|b| b.to_vec()));
      })?;
      assert_eq!(mem_results, results);
    }

    // ==========================================
    // 阶段 3: 单批次 12 个键验证 (恰好 1 个流水线窗口)
    // ==========================================
    {
      let keys_12: Vec<&[u8]> = all_keys[..12].iter().map(|k| k.as_slice()).collect();
      let mut results_12 = Vec::new();
      session
        .read_batch_with(&keys_12, |idx, val_opt| {
          results_12.push((idx, val_opt.map(|b| b.to_vec())));
        })
        .await?;
      assert_eq!(results_12.len(), 12);
      for (i, (idx, val)) in results_12.into_iter().enumerate() {
        assert_eq!(idx, i);
        assert_eq!(val, Some(all_vals[i].clone()));
      }

      // 同步内存直读
      let mut mem_results_12 = Vec::new();
      session.try_read_batch_in_memory(&keys_12, |idx, val_opt| {
        mem_results_12.push((idx, val_opt.map(|b| b.to_vec())));
      })?;
      assert_eq!(mem_results_12.len(), 12);
      for (i, (idx, val)) in mem_results_12.into_iter().enumerate() {
        assert_eq!(idx, i);
        assert_eq!(val, Some(all_vals[i].clone()));
      }
    }

    // ==========================================
    // 阶段 4: 单批次 100 个键验证 (跨多个窗口)
    // ==========================================
    {
      let keys_100: Vec<&[u8]> = all_keys.iter().map(|k| k.as_slice()).collect();
      let mut results_100 = Vec::new();
      session
        .read_batch_with(&keys_100, |idx, val_opt| {
          results_100.push((idx, val_opt.map(|b| b.to_vec())));
        })
        .await?;
      assert_eq!(results_100.len(), 100);
      for (i, (idx, val)) in results_100.into_iter().enumerate() {
        assert_eq!(idx, i);
        assert_eq!(val, Some(all_vals[i].clone()));
      }

      // Redis mget / mget_each 验证
      let mget_vals = session.mget(&keys_100).await?;
      assert_eq!(mget_vals.len(), 100);
      for (i, val) in mget_vals.into_iter().enumerate() {
        assert_eq!(val, Some(all_vals[i].clone()));
      }
    }

    // ==========================================
    // 阶段 5: 混合批次验证 (存在键、不存在键、墓碑删除键)
    // ==========================================
    {
      // 删除第 5 个和第 15 个键（产生墓碑）
      session.delete(&all_keys[5]).await?;
      session.delete(&all_keys[15]).await?;

      let non_existent_1 = b"missing_key_prefetch_001";
      let non_existent_2 = b"missing_key_prefetch_002";

      let mixed_keys: Vec<&[u8]> = vec![
        &all_keys[0],   // 存在
        non_existent_1, // 不存在
        &all_keys[5],   // 墓碑删除
        &all_keys[10],  // 存在
        &all_keys[15],  // 墓碑删除
        non_existent_2, // 不存在
        &all_keys[20],  // 存在
      ];

      let mut mixed_results = Vec::new();
      session
        .read_batch_with(&mixed_keys, |idx, val_opt| {
          mixed_results.push((idx, val_opt.map(|b| b.to_vec())));
        })
        .await?;

      assert_eq!(mixed_results.len(), 7);
      assert_eq!(mixed_results[0], (0, Some(all_vals[0].clone())));
      assert_eq!(mixed_results[1], (1, None));
      assert_eq!(mixed_results[2], (2, None)); // 墓碑应返回 None
      assert_eq!(mixed_results[3], (3, Some(all_vals[10].clone())));
      assert_eq!(mixed_results[4], (4, None)); // 墓碑应返回 None
      assert_eq!(mixed_results[5], (5, None));
      assert_eq!(mixed_results[6], (6, Some(all_vals[20].clone())));

      // Redis mget 混合校验
      let redis_mget = session.mget(&mixed_keys).await?;
      assert_eq!(redis_mget[0], Some(all_vals[0].clone()));
      assert_eq!(redis_mget[1], None);
      assert_eq!(redis_mget[2], None);
      assert_eq!(redis_mget[3], Some(all_vals[10].clone()));
      assert_eq!(redis_mget[4], None);
      assert_eq!(redis_mget[5], None);
      assert_eq!(redis_mget[6], Some(all_vals[20].clone()));
    }

    // ==========================================
    // 阶段 6: 冷热交替键验证（大量写入推进驱逐边界，产生磁盘冷数据与内存热数据）
    // ==========================================
    {
      // 写入足够多的数据使环形缓冲区翻转，将早期数据驱逐至磁盘
      let val_padding = vec![0xEEu8; 1024]; // 1KB 每条
      for i in 0..300 {
        let k = format!("cold_fill_{}", pad(i, 4)).into_bytes();
        session.upsert(&k, &val_padding).await?;
      }

      // 确保早期数据已落盘且驱逐出内存
      let head_addr = store.head_address();
      assert!(head_addr > 0, "HeadAddress 必须已推进跨过初始页");

      // 最新热数据
      let hot_key1 = b"hot_key_live_01";
      let hot_val1 = b"hot_val_live_01";
      session.upsert(hot_key1, hot_val1).await?;

      let hot_key2 = b"hot_key_live_02";
      let hot_val2 = b"hot_val_live_02";
      session.upsert(hot_key2, hot_val2).await?;

      // 混合冷数据（早期 all_keys）与热数据（刚刚写入）
      let cold_hot_keys: Vec<&[u8]> = vec![
        &all_keys[1],     // 冷键（已在磁盘）
        hot_key1,         // 热键（内存最新）
        &all_keys[2],     // 冷键（已在磁盘）
        b"absent_random", // 不存在键
        hot_key2,         // 热键（内存最新）
        &all_keys[3],     // 冷键（已在磁盘）
      ];

      let mut cold_hot_results = Vec::new();
      session
        .read_batch_with(&cold_hot_keys, |idx, val_opt| {
          cold_hot_results.push((idx, val_opt.map(|b| b.to_vec())));
        })
        .await?;

      assert_eq!(cold_hot_results.len(), 6);
      assert_eq!(
        cold_hot_results[0],
        (0, Some(all_vals[1].clone())),
        "冷键 1 必须成功从磁盘异步读出"
      );
      assert_eq!(
        cold_hot_results[1],
        (1, Some(hot_val1.to_vec())),
        "热键 1 必须从内存同步直读读出"
      );
      assert_eq!(
        cold_hot_results[2],
        (2, Some(all_vals[2].clone())),
        "冷键 2 必须成功从磁盘异步读出"
      );
      assert_eq!(cold_hot_results[3], (3, None), "不存在键返回 None");
      assert_eq!(
        cold_hot_results[4],
        (4, Some(hot_val2.to_vec())),
        "热键 2 必须从内存同步直读读出"
      );
      assert_eq!(
        cold_hot_results[5],
        (5, Some(all_vals[3].clone())),
        "冷键 3 必须成功从磁盘异步读出"
      );

      // mget 验证冷热混合
      let mget_cold_hot = session.mget(&cold_hot_keys).await?;
      assert_eq!(mget_cold_hot[0], Some(all_vals[1].clone()));
      assert_eq!(mget_cold_hot[1], Some(hot_val1.to_vec()));
      assert_eq!(mget_cold_hot[2], Some(all_vals[2].clone()));
      assert_eq!(mget_cold_hot[3], None);
      assert_eq!(mget_cold_hot[4], Some(hot_val2.to_vec()));
      assert_eq!(mget_cold_hot[5], Some(all_vals[3].clone()));
    }

    info!("对照 C# Garnet ContextReadWithPrefetch 12 项流水线预取与批量读取验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
/// 多库隔离与跨分块键序契约（严格对照 Redis MGET：结果顺序恒等于请求顺序、缺失键为 nil）
///
/// 验证目标：
/// 1. `set_active_db` 切换后 `mget` / `read_batch_with` 立即按新会话前缀编解码，
///    两库同名键完全物理隔离互不泄漏（库独有热键在另一库中必须为 nil）；
/// 2. 冷热混批跨多个 12 项预取窗口（分块边界 + 批内磁盘候选归并）时，
///    结果序恒等于请求序，缺失键返回 nil，超长键（物理键超 62B 栈上限）
///    经堆回退路径编码后结果同样正确；
/// 3. `read_batch_with` 回调 idx 严格升序且每键恰好回调一次（回调序契约）；
/// 4. 空批量 `mget` 返回空结果。
#[test]
fn test_batch_read_multi_db_isolation_and_key_order() -> Void {
  let rt = Runtime::new()?;
  rt.block_on(async {
    // 16 页 × 16KB = 256KB 环形缓冲区，填充后早期键必然被驱逐至磁盘区
    let env = open_store("batch_multi_db.db", config(2048, 16 * 1024, 16)?)?;
    let store = env.store;
    let session = store.new_session()?;

    const KEY_COUNT: usize = 30;
    let hot0_a: &[u8] = b"mdb_hot_0_a";
    let hot0_b: &[u8] = b"mdb_hot_0_b";
    let hot1_a: &[u8] = b"mdb_hot_1_a";
    let absent1: &[u8] = b"mdb_absent_key";
    let absent2: &[u8] = b"mdb_absent_key_2";

    let keys: Vec<Vec<u8>> = (0..KEY_COUNT)
      .map(|i| format!("mdb_key_{}", pad(i, 3)).into_bytes())
      .collect();
    // 80 字节用户键：加会话前缀后物理键超 62B 栈上限，覆盖 TaggedKeyBuf 堆回退编码
    let long_key = vec![b'L'; 80];

    let val = |db: u8, i: usize| format!("db{db}_val_{}", pad(i, 3)).into_bytes();

    // 每库各自写入：30 个常规键 + 1 个超长键，随后填充驱逐使早期键全部落盘
    for db in 0..2u8 {
      session.set_active_db(db as u64);
      let addr_k0 = session.upsert(&keys[0], &val(db, 0)).await?;
      for (i, k) in keys.iter().enumerate().skip(1) {
        session.upsert(k, &val(db, i)).await?;
      }
      session
        .upsert(&long_key, format!("db{db}_long_val").as_bytes())
        .await?;

      let filler = vec![b'F'; 1024];
      for i in 0..300 {
        session
          .upsert(format!("mdb_filler_{db}_{i}").as_bytes(), &filler)
          .await?;
      }

      // 驱逐线已跨过首个常规键：混批中的磁盘候选确定存在
      assert!(
        store.hlog.is_on_disk(addr_k0) && !store.hlog.is_in_memory(addr_k0),
        "db{db} 首键必须已被驱逐至磁盘区"
      );

      // 热键在驱逐后写入，保证混批中始终存在内存命中项
      if db == 0 {
        session.upsert(hot0_a, b"db0_hot_a").await?;
        session.upsert(hot0_b, b"db0_hot_b").await?;
      } else {
        session.upsert(hot1_a, b"db1_hot_a").await?;
      }
    }

    // 冷热交替 + 缺失键 + 超长键混合编排，使每个预取窗口均为混批
    session.set_active_db(0);
    let mut order_keys: Vec<&[u8]> = Vec::with_capacity(KEY_COUNT + 5);
    order_keys.push(&keys[0]);
    order_keys.push(hot0_a);
    order_keys.push(absent1);
    order_keys.push(&long_key);
    for (i, k) in keys.iter().enumerate().skip(1) {
      order_keys.push(k);
      if i == 10 {
        order_keys.push(hot0_b);
      }
      if i == 20 {
        order_keys.push(absent2);
      }
    }
    order_keys.push(hot1_a);

    // 各键在对应库中的期望值（库独有键在另一库必须为 None）
    let want = |db: u8, k: &[u8]| -> Option<Vec<u8>> {
      if k == absent1 || k == absent2 {
        return None;
      }
      if k == hot0_a {
        return (db == 0).then(|| b"db0_hot_a".to_vec());
      }
      if k == hot0_b {
        return (db == 0).then(|| b"db0_hot_b".to_vec());
      }
      if k == hot1_a {
        return (db == 1).then(|| b"db1_hot_a".to_vec());
      }
      if k == long_key.as_slice() {
        return Some(format!("db{db}_long_val").into_bytes());
      }
      let idx = keys.iter().position(|c| c == k).expect("键必须存在");
      Some(val(db, idx))
    };

    // 1. DB 0：read_batch_with 回调 idx 严格升序且每键恰好一次，值全部对位
    let mut batch_results: Vec<(usize, Option<Vec<u8>>)> = Vec::new();
    session
      .read_batch_with(&order_keys, |idx, v| {
        batch_results.push((idx, v.map(|b| b.to_vec())));
      })
      .await?;
    assert_eq!(
      batch_results.len(),
      order_keys.len(),
      "每键必须恰好回调一次"
    );
    for (want_idx, (idx, got)) in batch_results.into_iter().enumerate() {
      assert_eq!(idx, want_idx, "回调 idx 必须严格升序对位请求键序");
      assert_eq!(
        got,
        want(0, order_keys[want_idx]),
        "DB 0 第 {want_idx} 项批量读结果必须对位"
      );
    }

    // 2. DB 0：mget 结果序恒等于请求序，缺失键为 nil
    let mget_db0 = session.mget(&order_keys).await?;
    assert_eq!(mget_db0.len(), order_keys.len());
    for (i, k) in order_keys.iter().enumerate() {
      assert_eq!(
        &mget_db0[i],
        &want(0, k),
        "DB 0 第 {i} 项结果必须对位请求键"
      );
    }

    // 3. SELECT 1：同名键完全物理隔离，跨分块批量读按 DB 1 前缀取数
    session.set_active_db(1);
    let mget_db1 = session.mget(&order_keys).await?;
    for (i, k) in order_keys.iter().enumerate() {
      assert_eq!(
        &mget_db1[i],
        &want(1, k),
        "DB 1 第 {i} 项结果必须对位请求键"
      );
    }

    // 4. 空批量边界
    assert!(session.mget(&[]).await?.is_empty());

    info!("多库隔离与跨分块键序契约验证通过");
    aok::Result::<()>::Ok(())
  })?;

  OK
}
