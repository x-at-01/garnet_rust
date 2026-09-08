---
name: gitnexus-area-manager-and-stub
description: "Skill for the Manager_and_stub area of garnet_rust. 75 symbols across 11 files."
---

# Manager_and_stub

75 symbols | 11 files | Cohesion: 83%

## When to Use

- Working with code in `wedb_bftree/`
- Understanding how read, write, create_bftree work
- Modifying manager_and_stub-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_bftree/src/manager.rs` | read, write, create_bftree, data_file_path, data_file_path_for_key (+28) |
| `wedb_bftree/tests/manager_and_stub/manager.rs` | test_create_bftree_duplicate_prevention, test_delete_index_cleans_disk_file, test_flush_file_name_strict_parsing, test_get_or_open_tree_copy_failure_propagates, test_get_or_open_tree_preserves_persisted_data (+11) |
| `wedb_bftree/src/stub.rs` | is_transferred, mark_recovered_from_checkpoint, recreate_index, set_flushed, set_recovered (+7) |
| `wedb_bftree/tests/manager_and_stub/service.rs` | test_bftree_service_read_stack_and_large_heap_value, test_bftree_service_is_disposed_lifecycle, test_range_index_empty_value_safe_rejection, test_scan_callback_reentrant_access |
| `wedb_bftree/tests/manager_and_stub/locks.rs` | test_cache_aligned_lock_alignment, test_range_index_locks |
| `wedb_bftree/tests/manager_and_stub/stub.rs` | test_range_index_stub_serialization, test_range_index_stub_slice_helpers |
| `wedb_bftree/src/service.rs` | open_memory, preset_config |
| `wedb_repl/src/range_index.rs` | new |
| `wedb_store/src/checkpoint.rs` | recover_range_indexes |
| `wedb_store/tests/range_index_compat.rs` | test_ri_dispose_tree_under_lock_no_ops_on_transferred_source |

## Entry Points

Start here when exploring this area:

- **`read`** (Function) — `wedb_bftree/src/manager.rs:119`
- **`write`** (Function) — `wedb_bftree/src/manager.rs:127`
- **`create_bftree`** (Function) — `wedb_bftree/src/manager.rs:487`
- **`data_file_path`** (Function) — `wedb_bftree/src/manager.rs:346`
- **`data_file_path_for_key`** (Function) — `wedb_bftree/src/manager.rs:355`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `read` | Function | `wedb_bftree/src/manager.rs` | 119 |
| `write` | Function | `wedb_bftree/src/manager.rs` | 127 |
| `create_bftree` | Function | `wedb_bftree/src/manager.rs` | 487 |
| `data_file_path` | Function | `wedb_bftree/src/manager.rs` | 346 |
| `data_file_path_for_key` | Function | `wedb_bftree/src/manager.rs` | 355 |
| `delete_index` | Function | `wedb_bftree/src/manager.rs` | 721 |
| `dispose_tree` | Function | `wedb_bftree/src/manager.rs` | 736 |
| `dispose_tree_under_lock` | Function | `wedb_bftree/src/manager.rs` | 745 |
| `enumerate_files_for_replication` | Function | `wedb_bftree/src/manager.rs` | 940 |
| `get_or_open_tree` | Function | `wedb_bftree/src/manager.rs` | 579 |
| `get_replication_file_names` | Function | `wedb_bftree/src/manager.rs` | 999 |
| `get_tree` | Function | `wedb_bftree/src/manager.rs` | 384 |
| `hash_prefix_of` | Function | `wedb_bftree/src/manager.rs` | 323 |
| `key_hash_of` | Function | `wedb_bftree/src/manager.rs` | 330 |
| `key_id_of` | Function | `wedb_bftree/src/manager.rs` | 317 |
| `on_flush` | Function | `wedb_bftree/src/manager.rs` | 775 |
| `on_flush_address` | Function | `wedb_bftree/src/manager.rs` | 780 |
| `on_truncate` | Function | `wedb_bftree/src/manager.rs` | 920 |
| `pre_stage_and_register_pending` | Function | `wedb_bftree/src/manager.rs` | 679 |
| `register_tree` | Function | `wedb_bftree/src/manager.rs` | 760 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Get_or_open_tree → New` | cross_community | 4 |
| `Register_tree → Hash128` | cross_community | 4 |
| `Get_or_open_tree → Data_file_path` | cross_community | 3 |
| `Get_or_open_tree → Cache_only` | cross_community | 3 |
| `Get_or_open_tree → File_path` | cross_community | 3 |
| `Register_tree → Hex_padded_128` | cross_community | 3 |
| `Delete_index → Fast_hash` | cross_community | 3 |
| `Get_or_open_tree → Fast_hash` | cross_community | 3 |
| `Register_tree → Fast_hash` | cross_community | 3 |

## How to Explore

1. `context({name: "read"})` — see callers and callees
2. `query({search_query: "manager_and_stub"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
