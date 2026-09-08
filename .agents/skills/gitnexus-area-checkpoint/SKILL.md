---
name: gitnexus-area-checkpoint
description: "Skill for the Checkpoint area of garnet_rust. 68 symbols across 9 files."
---

# Checkpoint

68 symbols | 9 files | Cohesion: 90%

## When to Use

- Working with code in `wedb_checkpoint/`
- Understanding how create_checkpoint, create_checkpoint_with_token, find_latest_checkpoint work
- Modifying checkpoint-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_checkpoint/src/manager.rs` | create_checkpoint, create_checkpoint_with_token, find_latest_checkpoint, list_checkpoints, new (+18) |
| `wedb_checkpoint/tests/main.rs` | test_checkpoint_meta_bitcode_roundtrip_and_recovery, test_concurrent_checkpoints_serialize, test_failed_checkpoint_residue_cleanup, test_flushed_until_clamp_and_validation, test_foldover_checkpoint_recovery (+9) |
| `wedb_checkpoint/src/meta.rs` | decode_auto, decode_bitcode, decode_json, build_filename, index_filename (+3) |
| `wedb_checkpoint/tests/checkpoint/index_checkpoint.rs` | test_fuzzy_index_truncation_and_token_dir_cleanup, test_range_index_cpr_stub_healing_recovery, test_batch_buffer_arbitrary_buckets_and_crc32, test_read_cache_pointer_snapshot_resolution, test_read_index_checkpoint_truncated_bit_operations (+1) |
| `wedb_checkpoint/tests/checkpoint/recovery.rs` | pad_str, test_full_checkpoint_periodic, test_read_and_update_info, test_should_recover_begin_address, test_simple_recovery_foldover (+1) |
| `wedb_checkpoint/tests/checkpoint/edge.rs` | test_checkpoint_under_epoch_protected_caller, test_empty_store_checkpoint_and_recovery, test_page_boundary_aligned_checkpoint_recovery |
| `wedb_checkpoint/tests/checkpoint/fault_defense.rs` | test_adversarial_fault_injection, test_corrupted_files_defense, test_meta_integrity_seal_defense |
| `wedb_checkpoint/src/index_ckpt.rs` | read_index_checkpoint, read_index_checkpoint_truncated, write_index_checkpoint |
| `wedb_checkpoint/tests/checkpoint/checkpoint_manager.rs` | test_concurrent_sessions_after_recovery, test_purge_check |

## Entry Points

Start here when exploring this area:

- **`create_checkpoint`** (Function) — `wedb_checkpoint/src/manager.rs:289`
- **`create_checkpoint_with_token`** (Function) — `wedb_checkpoint/src/manager.rs:317`
- **`find_latest_checkpoint`** (Function) — `wedb_checkpoint/src/manager.rs:796`
- **`list_checkpoints`** (Function) — `wedb_checkpoint/src/manager.rs:773`
- **`new`** (Function) — `wedb_checkpoint/src/manager.rs:231`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `create_checkpoint` | Function | `wedb_checkpoint/src/manager.rs` | 289 |
| `create_checkpoint_with_token` | Function | `wedb_checkpoint/src/manager.rs` | 317 |
| `find_latest_checkpoint` | Function | `wedb_checkpoint/src/manager.rs` | 796 |
| `list_checkpoints` | Function | `wedb_checkpoint/src/manager.rs` | 773 |
| `new` | Function | `wedb_checkpoint/src/manager.rs` | 231 |
| `purge` | Function | `wedb_checkpoint/src/manager.rs` | 892 |
| `purge_all` | Function | `wedb_checkpoint/src/manager.rs` | 834 |
| `purge_all_checkpoints` | Function | `wedb_checkpoint/src/manager.rs` | 897 |
| `purge_checkpoint` | Function | `wedb_checkpoint/src/manager.rs` | 815 |
| `purge_outdated` | Function | `wedb_checkpoint/src/manager.rs` | 881 |
| `purge_outdated_checkpoints` | Function | `wedb_checkpoint/src/manager.rs` | 902 |
| `recover` | Function | `wedb_checkpoint/src/manager.rs` | 485 |
| `recover_latest` | Function | `wedb_checkpoint/src/manager.rs` | 750 |
| `recover_latest_store` | Function | `wedb_checkpoint/src/manager.rs` | 802 |
| `recover_store` | Function | `wedb_checkpoint/src/manager.rs` | 731 |
| `decode_auto` | Function | `wedb_checkpoint/src/meta.rs` | 249 |
| `decode_bitcode` | Function | `wedb_checkpoint/src/meta.rs` | 228 |
| `decode_json` | Function | `wedb_checkpoint/src/meta.rs` | 240 |
| `index_filename` | Function | `wedb_checkpoint/src/meta.rs` | 49 |
| `index_tmp_filename` | Function | `wedb_checkpoint/src/meta.rs` | 55 |

## How to Explore

1. `context({name: "create_checkpoint"})` — see callers and callees
2. `query({search_query: "checkpoint"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
