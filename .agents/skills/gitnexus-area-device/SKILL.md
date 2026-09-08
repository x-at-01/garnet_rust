---
name: gitnexus-area-device
description: "Skill for the Device area of garnet_rust. 55 symbols across 13 files."
---

# Device

55 symbols | 13 files | Cohesion: 76%

## When to Use

- Working with code in `wedb_device/`
- Understanding how segmented, make_pattern_data, new work
- Modifying device-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_device/src/segmented_device.rs` | segmented, dir_and_prefix, drop, new, recover (+3) |
| `wedb_device/tests/device/lifecycle.rs` | delete_on_close_cleans_up_files_on_drop, idevice_permission_denied_at_first_write_callback_gets_error, preallocate_sets_segment_file_size, read_only_device_blocks_writes_while_allowing_reads, storage_device_trait_dispatch (+3) |
| `wedb_device/tests/device/round_trip.rs` | idevice_initialize_segment_size_minus_one_unbounded_single_segment, idevice_round_trip_across_segment_boundary, idevice_round_trip_basic_read_write, idevice_round_trip_various_segment_sizes, native_device_test1 (+3) |
| `wedb_device/tests/device/parallel.rs` | concurrent_cold_open_race_without_zombie_revival, high_concurrency_many_threads_no_hang, idevice_parallel_32_concurrent_writes, idevice_parallel_64_concurrent_reads, idevice_parallel_bursty_traffic (+2) |
| `wedb_device/tests/device/truncate.rs` | get_file_size_reflects_writes, remove_segment_removes_persisted_data, reset_closes_segments_and_device_remains_usable, successive_truncations_defend_against_ghost_segments, truncate_until_address_deletes_all_prior_segments (+1) |
| `wedb_device/tests/device/recovery.rs` | recover_files_restores_segment_range_after_gap, recovery_larger_existing_segment_detects_mismatch, recovery_segment_numbering_stays_numeric_beyond_two_digits, recovery_smaller_existing_segment_succeeds, recovery_matching_segment_size_succeeds |
| `wedb_device/tests/device/capacity.rs` | handle_capacity_evicts_oldest_segments_when_bounded, set_capacity_rejects_non_multiple_of_segment_size, system_probes_return_usable_values |
| `wedb_device/src/sys.rs` | detect_cpu_cores, detect_system_memory, GlobalMemoryStatusEx |
| `wedb_device/tests/device/boundary.rs` | truncate_until_address_u64_max_is_clamped_safely, massive_cross_segment_round_trip |
| `wedb_store/src/config.rs` | auto, auto_with_budget |

## Entry Points

Start here when exploring this area:

- **`segmented`** (Function) — `wedb_device/src/segmented_device.rs:184`
- **`make_pattern_data`** (Function) — `wedb_device/tests/support/mod.rs:4`
- **`new`** (Function) — `wedb_device/src/segmented_device.rs:64`
- **`recover`** (Function) — `wedb_device/src/segmented_device.rs:479`
- **`single_file`** (Function) — `wedb_device/src/segmented_device.rs:178`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `segmented` | Function | `wedb_device/src/segmented_device.rs` | 184 |
| `make_pattern_data` | Function | `wedb_device/tests/support/mod.rs` | 4 |
| `new` | Function | `wedb_device/src/segmented_device.rs` | 64 |
| `recover` | Function | `wedb_device/src/segmented_device.rs` | 479 |
| `single_file` | Function | `wedb_device/src/segmented_device.rs` | 178 |
| `with_pool` | Function | `wedb_device/src/segmented_device.rs` | 80 |
| `detect_cpu_cores` | Function | `wedb_device/src/sys.rs` | 95 |
| `detect_system_memory` | Function | `wedb_device/src/sys.rs` | 20 |
| `auto` | Function | `wedb_store/src/config.rs` | 87 |
| `auto_with_budget` | Function | `wedb_store/src/config.rs` | 96 |
| `multi_segment_physical_truncation` | Function | `wedb_compact/tests/main.rs` | 21 |
| `truncate_until_address_u64_max_is_clamped_safely` | Function | `wedb_device/tests/device/boundary.rs` | 118 |
| `handle_capacity_evicts_oldest_segments_when_bounded` | Function | `wedb_device/tests/device/capacity.rs` | 60 |
| `set_capacity_rejects_non_multiple_of_segment_size` | Function | `wedb_device/tests/device/capacity.rs` | 21 |
| `delete_on_close_cleans_up_files_on_drop` | Function | `wedb_device/tests/device/lifecycle.rs` | 224 |
| `idevice_permission_denied_at_first_write_callback_gets_error` | Function | `wedb_device/tests/device/lifecycle.rs` | 74 |
| `preallocate_sets_segment_file_size` | Function | `wedb_device/tests/device/lifecycle.rs` | 194 |
| `concurrent_cold_open_race_without_zombie_revival` | Function | `wedb_device/tests/device/parallel.rs` | 319 |
| `high_concurrency_many_threads_no_hang` | Function | `wedb_device/tests/device/parallel.rs` | 452 |
| `idevice_parallel_32_concurrent_writes` | Function | `wedb_device/tests/device/parallel.rs` | 25 |

## How to Explore

1. `context({name: "segmented"})` — see callers and callees
2. `query({search_query: "device"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
