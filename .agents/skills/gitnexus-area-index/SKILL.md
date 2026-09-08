---
name: gitnexus-area-index
description: "Skill for the Index area of garnet_rust. 90 symbols across 12 files."
---

# Index

90 symbols | 12 files | Cohesion: 86%

## When to Use

- Working with code in `wedb_index/`
- Understanding how find_tag_address, set_overflow_index, address work
- Modifying index-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_index/src/table.rs` | advance, bucket, delete_by_hash, find_or_create_tag_by_hash_with_min_addr, find_tag_batch_by_hash (+24) |
| `wedb_index/src/bucket.rs` | find_tag_address, set_overflow_index, is_latched, try_lock_exclusive, try_lock_shared (+10) |
| `wedb_index/src/entry.rs` | address, fmt, is_read_cache, is_tentative, is_valid (+6) |
| `wedb_index/tests/index/latch_concurrency.rs` | test_bucket_shared_exclusive_latch_lifecycle, test_bucket_index_mask_distribution, test_multi_key_locking_boundary_stress, test_multi_key_locking_large_key_set_heap_and_guard_overflow, test_latch_conflict_spin_drain_and_promote (+3) |
| `wedb_index/tests/index/rcu_and_probe.rs` | test_find_tag_fast_probe, test_find_tag_or_insert_single_pass_and_overflow_penetration, test_concurrent_rcu_atomic_update, test_concurrent_rcu_mixed_workload, test_find_or_create_tag_concurrent_winner_cascade (+1) |
| `wedb_index/src/overflow_pool.rs` | allocate, ensure_chunk, free, get, get_unchecked |
| `wedb_index/tests/index/overflow_and_chain.rs` | test_overflow_pool_free_and_recycle, test_extreme_overflow_depth_1024_plus_buckets, test_overflow_bucket_cascade, test_overflow_chain_cycle_detection, test_overflow_pool_cross_chunk_concurrent_allocation |
| `wedb_index/tests/index/bucket_and_entry.rs` | test_cacheline_alignment_and_stride, test_extreme_tag_and_address_boundaries, test_entry_bit_packing_and_tentative, test_matches_tag_mask_defense |
| `wedb_index/tests/index/support.rs` | make_key, make_address, make_keys |
| `wedb_index/src/split_index.rs` | insert_raw_to_bucket, split_single_chunk |

## Entry Points

Start here when exploring this area:

- **`find_tag_address`** (Function) — `wedb_index/src/bucket.rs:326`
- **`set_overflow_index`** (Function) — `wedb_index/src/bucket.rs:293`
- **`address`** (Function) — `wedb_index/src/entry.rs:61`
- **`is_read_cache`** (Function) — `wedb_index/src/entry.rs:97`
- **`is_tentative`** (Function) — `wedb_index/src/entry.rs:73`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `find_tag_address` | Function | `wedb_index/src/bucket.rs` | 326 |
| `set_overflow_index` | Function | `wedb_index/src/bucket.rs` | 293 |
| `address` | Function | `wedb_index/src/entry.rs` | 61 |
| `is_read_cache` | Function | `wedb_index/src/entry.rs` | 97 |
| `is_tentative` | Function | `wedb_index/src/entry.rs` | 73 |
| `is_valid` | Function | `wedb_index/src/entry.rs` | 85 |
| `matches_tag` | Function | `wedb_index/src/entry.rs` | 113 |
| `tag` | Function | `wedb_index/src/entry.rs` | 67 |
| `tag_from_hash` | Function | `wedb_index/src/entry.rs` | 141 |
| `allocate` | Function | `wedb_index/src/overflow_pool.rs` | 45 |
| `free` | Function | `wedb_index/src/overflow_pool.rs` | 96 |
| `get` | Function | `wedb_index/src/overflow_pool.rs` | 136 |
| `get_unchecked` | Function | `wedb_index/src/overflow_pool.rs` | 168 |
| `bucket` | Function | `wedb_index/src/table.rs` | 1004 |
| `delete_by_hash` | Function | `wedb_index/src/table.rs` | 961 |
| `find_or_create_tag_by_hash_with_min_addr` | Function | `wedb_index/src/table.rs` | 796 |
| `find_tag_batch_by_hash` | Function | `wedb_index/src/table.rs` | 1151 |
| `find_tag_by_hash` | Function | `wedb_index/src/table.rs` | 591 |
| `get_bucket` | Function | `wedb_index/src/table.rs` | 568 |
| `insert_by_hash` | Function | `wedb_index/src/table.rs` | 671 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Read_batch_raw_with → Scalar_fallback_key_eq` | cross_community | 4 |
| `Read_batch_raw_with → Neon_key_eq` | cross_community | 4 |
| `Read_batch_raw_with → Sse2_key_eq` | cross_community | 4 |
| `Read_batch_raw_with → Is_empty` | cross_community | 4 |
| `Read_batch_raw_with → Push` | cross_community | 4 |
| `Read_batch_raw_with → Promote_immutable_to_read_cache` | cross_community | 4 |
| `Create_checkpoint_inner → Address` | cross_community | 3 |
| `Create_checkpoint_inner → Is_tentative` | cross_community | 3 |
| `Insert_by_hash → Ensure_chunk` | intra_community | 3 |
| `Insert_by_hash → Get` | intra_community | 3 |

## How to Explore

1. `context({name: "find_tag_address"})` — see callers and callees
2. `query({search_query: "index"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
