---
name: gitnexus-area-list
description: "Skill for the List area of garnet_rust. 106 symbols across 13 files."
---

# List

106 symbols | 13 files | Cohesion: 75%

## When to Use

- Working with code in `wedb_list/`
- Understanding how clear, rpush, shrink_to_fit work
- Modifying list-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_list/src/list.rs` | clear, rpush, shrink_to_fit, from_bitcode_rejects_broken_page_invariants, clear_physical (+33) |
| `wedb_list/src/page.rs` | from_items_unchecked, into_iter, clear, get_mut, is_full (+5) |
| `wedb_list/tests/list/insert_remove.rs` | basic_rpush_and_linsert, basic_rpush_and_lrem, can_do_linsert_before_and_after_lc, can_do_linsert_with_no_element_lc, lrem_all_modes_differential (+4) |
| `wedb_list/tests/list/move_rotate.rs` | can_do_basic_lmove, can_do_rpop_lpush, can_do_rpop_lpush_gc, can_use_lmove, rotate_same_direction_and_empty (+3) |
| `wedb_list/tests/list/paged_scale.rs` | iter_and_memory_lifecycle, paged_sparse_reclaim_merge, clear_physical_and_memory_lifecycle, page_boundary_indexing_and_range, page_split_on_full_insert (+3) |
| `wedb_list/tests/list/trim_range.rs` | can_do_lrange_basic, can_do_lrange_correct, basic_lpush_and_lrange, multi_lpush_and_ltrim_with_memory_check, multi_rpush_and_ltrim (+1) |
| `wedb_list/tests/list/differential.rs` | below, next_u64, range, differential_against_vec_model, rand_val (+1) |
| `wedb_list/tests/list/lpos.rs` | lpos_with_list_position, lpos_with_options, lpos_without_options, push, lpos_with_invalid_key |
| `wedb_list/tests/list/push_pop.rs` | lpop_and_rpop_with_zero_count_return_empty_array, basic_lpush_and_lpop, can_do_lpop_multiple_values, basic_lpush_and_ltrim, can_handle_no_prexistent_key |
| `wedb_list/tests/list/serialization.rs` | bitcode_serialization, paged_serialization_round_trip, serialization_round_trip, serialization_truncation_robustness |

## Entry Points

Start here when exploring this area:

- **`clear`** (Function) — `wedb_list/src/list.rs:111`
- **`rpush`** (Function) — `wedb_list/src/list.rs:357`
- **`shrink_to_fit`** (Function) — `wedb_list/src/list.rs:117`
- **`from_items_unchecked`** (Function) — `wedb_list/src/page.rs:200`
- **`clear_physical`** (Function) — `wedb_list/src/list.rs:103`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `clear` | Function | `wedb_list/src/list.rs` | 111 |
| `rpush` | Function | `wedb_list/src/list.rs` | 357 |
| `shrink_to_fit` | Function | `wedb_list/src/list.rs` | 117 |
| `from_items_unchecked` | Function | `wedb_list/src/page.rs` | 200 |
| `clear_physical` | Function | `wedb_list/src/list.rs` | 103 |
| `deserialize` | Function | `wedb_list/src/list.rs` | 925 |
| `drain_left` | Function | `wedb_list/src/list.rs` | 416 |
| `drain_right` | Function | `wedb_list/src/list.rs` | 429 |
| `is_empty` | Function | `wedb_list/src/list.rs` | 78 |
| `iter` | Function | `wedb_list/src/list.rs` | 467 |
| `lpop` | Function | `wedb_list/src/list.rs` | 397 |
| `lpop_one` | Function | `wedb_list/src/list.rs` | 386 |
| `lpos` | Function | `wedb_list/src/list.rs` | 684 |
| `lpush` | Function | `wedb_list/src/list.rs` | 347 |
| `lpushx` | Function | `wedb_list/src/list.rs` | 367 |
| `new` | Function | `wedb_list/src/list.rs` | 57 |
| `rotate` | Function | `wedb_list/src/list.rs` | 775 |
| `rpop` | Function | `wedb_list/src/list.rs` | 405 |
| `rpop_one` | Function | `wedb_list/src/list.rs` | 392 |
| `rpushx` | Function | `wedb_list/src/list.rs` | 377 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Main → Locate` | cross_community | 5 |
| `Main → Normalize_index` | cross_community | 5 |
| `Main → New` | cross_community | 4 |
| `Lpos → Slices` | cross_community | 4 |
| `Lpos → Locate` | cross_community | 4 |
| `Lpop → Drain` | intra_community | 3 |
| `Lrem → With_capacity` | cross_community | 3 |
| `Lrem → New` | cross_community | 3 |
| `Iter_range → Locate` | cross_community | 3 |
| `Rotate → New` | intra_community | 3 |

## How to Explore

1. `context({name: "clear"})` — see callers and callees
2. `query({search_query: "list"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
