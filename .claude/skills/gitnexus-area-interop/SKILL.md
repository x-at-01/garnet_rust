---
name: gitnexus-area-interop
description: "Skill for the Interop area of garnet_rust. 49 symbols across 10 files."
---

# Interop

49 symbols | 10 files | Cohesion: 61%

## When to Use

- Working with code in `wedb_bftree/`
- Understanding how open_disk, insert_test_data, new work
- Modifying interop-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_bftree/tests/interop/scan.rs` | test_large_insert_and_scan, test_scan_all_empty_tree, test_scan_with_count_empty_tree, test_scan_all_key_only, test_scan_all_returns_all_entries (+13) |
| `wedb_bftree/tests/interop/point_ops.rs` | test_delete_existing_key, test_delete_non_existent_key_returns_success, test_insert_and_read_basic_round_trip, test_insert_multiple_all_readable, test_insert_overwrite_returns_updated_value (+5) |
| `wedb_bftree/tests/interop/snapshot.rs` | test_memory_only_snapshot_and_recover_round_trip, test_cpr_snapshot_without_use_snapshot_returns_err, test_snapshot_and_recover_round_trip, test_snapshot_and_recover_scan_after_restore, test_memory_only_recover_from_non_existent_file_throws (+2) |
| `wedb_bftree/tests/interop/lifecycle.rs` | test_create_and_dispose, test_double_dispose_does_not_throw, test_operations_on_disposed_tree_throw, test_create_disk_backed_missing_path_throws, test_create_with_custom_config |
| `wedb_bftree/src/service.rs` | open_disk, recover_from_cpr_snapshot |
| `wedb_bench/src/bftree_harness.rs` | new, new_with_budget |
| `wedb_bftree/src/types.rs` | storage_backend, file_path |
| `wedb_bftree/tests/interop/common.rs` | insert_test_data |
| `wedb_bftree/tests/main.rs` | test_record_limits |
| `wedb_bftree/tests/manager_and_stub/service.rs` | test_large_record_read_no_panic |

## Entry Points

Start here when exploring this area:

- **`open_disk`** (Function) — `wedb_bftree/src/service.rs:216`
- **`insert_test_data`** (Function) — `wedb_bftree/tests/interop/common.rs:42`
- **`new`** (Function) — `wedb_bench/src/bftree_harness.rs:67`
- **`new_with_budget`** (Function) — `wedb_bench/src/bftree_harness.rs:41`
- **`storage_backend`** (Function) — `wedb_bftree/src/types.rs:53`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `open_disk` | Function | `wedb_bftree/src/service.rs` | 216 |
| `insert_test_data` | Function | `wedb_bftree/tests/interop/common.rs` | 42 |
| `new` | Function | `wedb_bench/src/bftree_harness.rs` | 67 |
| `new_with_budget` | Function | `wedb_bench/src/bftree_harness.rs` | 41 |
| `storage_backend` | Function | `wedb_bftree/src/types.rs` | 53 |
| `file_path` | Function | `wedb_bftree/src/types.rs` | 43 |
| `recover_from_cpr_snapshot` | Function | `wedb_bftree/src/service.rs` | 686 |
| `test_create_and_dispose` | Function | `wedb_bftree/tests/interop/lifecycle.rs` | 10 |
| `test_double_dispose_does_not_throw` | Function | `wedb_bftree/tests/interop/lifecycle.rs` | 63 |
| `test_operations_on_disposed_tree_throw` | Function | `wedb_bftree/tests/interop/lifecycle.rs` | 73 |
| `test_delete_existing_key` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 126 |
| `test_delete_non_existent_key_returns_success` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 142 |
| `test_insert_and_read_basic_round_trip` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 7 |
| `test_insert_multiple_all_readable` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 41 |
| `test_insert_overwrite_returns_updated_value` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 24 |
| `test_read_after_delete_returns_deleted` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 77 |
| `test_read_into_buffer_contract` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 153 |
| `test_read_into_span_not_found` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 112 |
| `test_read_into_span_zero_alloc` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 94 |
| `test_read_not_found` | Function | `wedb_bftree/tests/interop/point_ops.rs` | 64 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Recover_in_place → Config_error_to_string` | cross_community | 5 |
| `Recover_in_place → With_config` | cross_community | 5 |
| `Get_or_open_tree → File_path` | cross_community | 3 |

## How to Explore

1. `context({name: "open_disk"})` — see callers and callees
2. `query({search_query: "interop"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
