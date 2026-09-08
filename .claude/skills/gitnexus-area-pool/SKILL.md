---
name: gitnexus-area-pool
description: "Skill for the Pool area of garnet_rust. 50 symbols across 9 files."
---

# Pool

50 symbols | 9 files | Cohesion: 78%

## When to Use

- Working with code in `wedb_mem/`
- Understanding how new, with_budgets, class_capacity_bytes work
- Modifying pool-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_mem/src/pool/mod.rs` | issue_new, new, with_budgets, class_capacity_bytes, class_capacity_sectors (+19) |
| `wedb_mem/src/pool/tls.rs` | drain_and_release, drop, process_node, new, current_thread_id (+2) |
| `wedb_wal/src/ring_buffer.rs` | ring_offset, write_bytes, write_record, drop |
| `wedb_mem/tests/suite/pool_budget.rs` | budget_bounds_reusable_bytes_and_returns_to_zero, small_budget_isolated_from_large_exhaustion, closed_pool_serves_uncached_buffers_and_holds_no_budget |
| `wedb_mem/tests/suite/pool_ladder.rs` | ladder_is_monotonic_and_bounded, class_capacities_const_table_matches_fn, local_retention_bounded_by_per_class_cache_cap |
| `wedb_mem/src/pool/budget.rs` | release, used, total |
| `wedb_mem/src/pool/depot.rs` | push, clear, total_cached |
| `wedb_mem/src/aligned_buf.rs` | from_cached, drop |
| `wedb_mem/src/pool/inbox.rs` | next |

## Entry Points

Start here when exploring this area:

- **`new`** (Function) — `wedb_mem/src/pool/mod.rs:213`
- **`with_budgets`** (Function) — `wedb_mem/src/pool/mod.rs:222`
- **`class_capacity_bytes`** (Function) — `wedb_mem/src/pool/mod.rs:133`
- **`class_capacity_sectors`** (Function) — `wedb_mem/src/pool/mod.rs:96`
- **`release`** (Function) — `wedb_mem/src/pool/budget.rs:42`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `new` | Function | `wedb_mem/src/pool/mod.rs` | 213 |
| `with_budgets` | Function | `wedb_mem/src/pool/mod.rs` | 222 |
| `class_capacity_bytes` | Function | `wedb_mem/src/pool/mod.rs` | 133 |
| `class_capacity_sectors` | Function | `wedb_mem/src/pool/mod.rs` | 96 |
| `release` | Function | `wedb_mem/src/pool/budget.rs` | 42 |
| `push` | Function | `wedb_mem/src/pool/depot.rs` | 88 |
| `budget_for` | Function | `wedb_mem/src/pool/mod.rs` | 644 |
| `return_buf` | Function | `wedb_mem/src/pool/mod.rs` | 520 |
| `get_with_policy` | Function | `wedb_mem/src/pool/mod.rs` | 382 |
| `class_of_sectors` | Function | `wedb_mem/src/pool/mod.rs` | 140 |
| `from_cached` | Function | `wedb_mem/src/aligned_buf.rs` | 253 |
| `ring_offset` | Function | `wedb_wal/src/ring_buffer.rs` | 56 |
| `write_bytes` | Function | `wedb_wal/src/ring_buffer.rs` | 102 |
| `write_record` | Function | `wedb_wal/src/ring_buffer.rs` | 66 |
| `drain_and_release` | Function | `wedb_mem/src/pool/tls.rs` | 41 |
| `current_thread_id` | Function | `wedb_mem/src/pool/tls.rs` | 156 |
| `dealloc` | Function | `wedb_mem/src/pool/mod.rs` | 530 |
| `used` | Function | `wedb_mem/src/pool/budget.rs` | 48 |
| `large_reserved_bytes` | Function | `wedb_mem/src/pool/mod.rs` | 284 |
| `reserved_bytes` | Function | `wedb_mem/src/pool/mod.rs` | 270 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Get_with_policy → Class_capacity_sectors` | cross_community | 5 |
| `Issue_new → Class_capacity_sectors` | intra_community | 5 |
| `Get_with_policy → Ring_offset` | cross_community | 4 |
| `Return_buf → Release` | intra_community | 4 |
| `Return_buf → Push` | intra_community | 4 |
| `Return_buf → Budget_for` | intra_community | 4 |
| `Get_with_policy → CachedBuf` | intra_community | 3 |
| `Write_record → Ring_offset` | intra_community | 3 |

## How to Explore

1. `context({name: "new"})` — see callers and callees
2. `query({search_query: "pool"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
