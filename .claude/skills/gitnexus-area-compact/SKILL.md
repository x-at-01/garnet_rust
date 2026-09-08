---
name: gitnexus-area-compact
description: "Skill for the Compact area of garnet_rust. 57 symbols across 9 files."
---

# Compact

57 symbols | 9 files | Cohesion: 64%

## When to Use

- Working with code in `wedb_compact/`
- Understanding how compact, compact_with_filter, new work
- Modifying compact-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_compact/src/compactor.rs` | compact, with_cas_retries, new, cas_retry_exhausted_retains_live_record, compact_lookup (+11) |
| `wedb_compact/tests/compact/spanbyte_compaction.rs` | spanbyte_compaction_custom_functions_test2_scan, spanbyte_compaction_test2_multilevel_lookup, spanbyte_compaction_test2_multilevel_scan, spanbyte_compaction_test3_with_deletions_lookup, spanbyte_compaction_test3_with_deletions_scan (+5) |
| `wedb_compact/tests/compact/concurrency_and_collision.rs` | forced_hash_collision_chaining, concurrent_write_safety_lookup, concurrent_write_safety_scan, empty_key_and_large_payload, zero_length_value_live (+3) |
| `wedb_compact/tests/compact/more_log_compaction.rs` | more_log_compaction_delete_scan, more_log_compaction_multigeneration_continuous_updates, more_log_compaction_scan_mode_stage2_early_termination, more_log_compaction_delete_lookup, more_log_compaction_entire_log_to_tail (+2) |
| `wedb_compact/tests/main.rs` | edge_cases, exact_page_boundary_and_pad, mid_record_until_snaps_to_boundary, mixed_large_small_zero_payloads, pure_empty_store |
| `wedb_compact/tests/compact/support.rs` | create_custom_store, create_test_store, verify_records, create_read_cache_store |
| `wedb_store/src/store.rs` | get_key_id_meta, remove_key_id_meta, update_key_id_meta |
| `wedb_compact/tests/compact/key_id_meta_gc.rs` | key_id_meta_gc_after_fast_drop_compaction, key_id_meta_gc_death_outside_compaction_scope_kept, key_id_meta_gc_live_entry_kept_and_tombstone_form_collected |
| `wedb_hlog/src/scan.rs` | current_address |

## Entry Points

Start here when exploring this area:

- **`compact`** (Function) — `wedb_compact/src/compactor.rs:762`
- **`compact_with_filter`** (Function) — `wedb_compact/src/compactor.rs:675`
- **`new`** (Function) — `wedb_compact/src/compactor.rs:272`
- **`current_address`** (Function) — `wedb_hlog/src/scan.rs:64`
- **`get_key_id_meta`** (Function) — `wedb_store/src/store.rs:335`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `compact` | Function | `wedb_compact/src/compactor.rs` | 762 |
| `compact_with_filter` | Function | `wedb_compact/src/compactor.rs` | 675 |
| `new` | Function | `wedb_compact/src/compactor.rs` | 272 |
| `current_address` | Function | `wedb_hlog/src/scan.rs` | 64 |
| `get_key_id_meta` | Function | `wedb_store/src/store.rs` | 335 |
| `remove_key_id_meta` | Function | `wedb_store/src/store.rs` | 341 |
| `update_key_id_meta` | Function | `wedb_store/src/store.rs` | 317 |
| `create_custom_store` | Function | `wedb_compact/tests/compact/support.rs` | 32 |
| `create_test_store` | Function | `wedb_compact/tests/compact/support.rs` | 15 |
| `verify_records` | Function | `wedb_compact/tests/compact/support.rs` | 74 |
| `create_read_cache_store` | Function | `wedb_compact/tests/compact/support.rs` | 53 |
| `with_cas_retries` | Function | `wedb_compact/src/compactor.rs` | 281 |
| `new` | Function | `wedb_compact/src/compactor.rs` | 126 |
| `cas_retry_exhausted_retains_live_record` | Function | `wedb_compact/src/compactor.rs` | 788 |
| `forced_hash_collision_chaining` | Function | `wedb_compact/tests/compact/concurrency_and_collision.rs` | 113 |
| `more_log_compaction_delete_scan` | Function | `wedb_compact/tests/compact/more_log_compaction.rs` | 88 |
| `more_log_compaction_multigeneration_continuous_updates` | Function | `wedb_compact/tests/compact/more_log_compaction.rs` | 199 |
| `more_log_compaction_scan_mode_stage2_early_termination` | Function | `wedb_compact/tests/compact/more_log_compaction.rs` | 338 |
| `spanbyte_compaction_custom_functions_test2_scan` | Function | `wedb_compact/tests/compact/spanbyte_compaction.rs` | 543 |
| `spanbyte_compaction_test2_multilevel_lookup` | Function | `wedb_compact/tests/compact/spanbyte_compaction.rs` | 120 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Compact_scan → Skip_to_next_page` | cross_community | 6 |
| `Compact_scan → Varint_len_from_byte` | cross_community | 5 |
| `Compact_scan → Mark_death` | intra_community | 3 |
| `Compact_scan → Get_key_id_meta` | intra_community | 3 |
| `Compact_scan → Update_key_id_meta` | intra_community | 3 |
| `Compact_scan → Current_address` | intra_community | 3 |

## How to Explore

1. `context({name: "compact"})` — see callers and callees
2. `query({search_query: "compact"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
