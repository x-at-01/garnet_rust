---
name: gitnexus-area-chunked-serializer
description: "Skill for the Chunked_serializer area of garnet_rust. 89 symbols across 13 files."
---

# Chunked_serializer

89 symbols | 13 files | Cohesion: 67%

## When to Use

- Working with code in `wedb_bftree/`
- Understanding how derive_temp_migration_path, from_root, path work
- Modifying chunked_serializer-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_bftree/tests/chunked_serializer/error.rs` | corrupted_checksum_detected, corrupted_file_data_fails_checksum_in_trailer, declared_size_smaller_than_actual_file_emits_truncated_prefix, dispose_cleans_temp_file, double_dispose_is_idempotent (+15) |
| `wedb_bftree/tests/chunked_serializer/reader.rs` | reader_destination_below_minimum_throws, reader_non_positive_chunk_size_throws, reader_truncated_file_throws, round_trip_exact_min_chunk_size_reader, round_trip_exact_min_chunk_size_serializer (+10) |
| `wedb_bftree/tests/chunked_serializer/boundary.rs` | buffer_too_small_for_trailer_defers_to_next_chunk, empty_chunk_at_waiting_for_key_header, empty_chunk_during_receiving_file_data, empty_chunk_during_waiting_for_file_header, empty_chunk_during_waiting_for_trailer (+8) |
| `wedb_bftree/src/chunk.rs` | file_data_remaining, is_complete, move_next, needs_file_data, new (+4) |
| `wedb_bftree/tests/chunked_serializer/common.rs` | path, build_payload, create_stub, create_buffer, serializer_move_next (+3) |
| `wedb_bftree/tests/chunked_serializer/round_trip.rs` | file_data_exactly_fills_chunk_trailer_in_next_chunk, file_data_one_byte_per_chunk, key_spanning_multiple_chunks_round_trip, single_chunk_round_trip, small_buffer_round_trip (+3) |
| `wedb_store/tests/ttl.rs` | open, test_collection_lazy_expiry, test_delete_and_rename_ttl_consistency, test_persist, test_set_clears_ttl |
| `wedb_bftree/tests/chunked_serializer/streaming.rs` | test_range_index_chunked_streaming, test_range_index_extreme_1byte_chunk_streaming, test_range_index_streaming_with_zero_checksum |
| `wedb_bftree/src/manager.rs` | derive_temp_migration_path, from_root |
| `wedb_bftree/src/stub.rs` | encode, encode_into |

## Entry Points

Start here when exploring this area:

- **`derive_temp_migration_path`** (Function) — `wedb_bftree/src/manager.rs:291`
- **`from_root`** (Function) — `wedb_bftree/src/manager.rs:259`
- **`path`** (Function) — `wedb_bftree/tests/chunked_serializer/common.rs:30`
- **`build_payload`** (Function) — `wedb_bftree/tests/chunked_serializer/common.rs:111`
- **`create_stub`** (Function) — `wedb_bftree/tests/chunked_serializer/common.rs:56`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `derive_temp_migration_path` | Function | `wedb_bftree/src/manager.rs` | 291 |
| `from_root` | Function | `wedb_bftree/src/manager.rs` | 259 |
| `path` | Function | `wedb_bftree/tests/chunked_serializer/common.rs` | 30 |
| `build_payload` | Function | `wedb_bftree/tests/chunked_serializer/common.rs` | 111 |
| `create_stub` | Function | `wedb_bftree/tests/chunked_serializer/common.rs` | 56 |
| `compute_checksum` | Function | `wedb_hasher/src/lib.rs` | 202 |
| `file_data_remaining` | Function | `wedb_bftree/src/chunk.rs` | 123 |
| `is_complete` | Function | `wedb_bftree/src/chunk.rs` | 109 |
| `move_next` | Function | `wedb_bftree/src/chunk.rs` | 139 |
| `needs_file_data` | Function | `wedb_bftree/src/chunk.rs` | 115 |
| `new` | Function | `wedb_bftree/src/chunk.rs` | 70 |
| `new_with_checksum` | Function | `wedb_bftree/src/chunk.rs` | 86 |
| `supply_file_data` | Function | `wedb_bftree/src/chunk.rs` | 130 |
| `is_complete` | Function | `wedb_bftree/src/chunk.rs` | 550 |
| `read_next_chunk` | Function | `wedb_bftree/src/chunk.rs` | 561 |
| `encode` | Function | `wedb_bftree/src/stub.rs` | 158 |
| `encode_into` | Function | `wedb_bftree/src/stub.rs` | 176 |
| `create_buffer` | Function | `wedb_bftree/tests/chunked_serializer/common.rs` | 60 |
| `serializer_move_next` | Function | `wedb_bftree/tests/chunked_serializer/common.rs` | 74 |
| `complete` | Function | `wedb_repl/src/range_index.rs` | 209 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Complete → Is_empty` | cross_community | 8 |
| `Complete → Push` | cross_community | 8 |
| `Complete → Promote_immutable_to_read_cache` | cross_community | 8 |
| `Complete → Current_thread_id` | cross_community | 7 |
| `Complete → EpochGuard` | cross_community | 7 |
| `Complete → From_heap` | cross_community | 7 |
| `Complete → From_stack` | cross_community | 7 |
| `Complete → Write_key_parts` | cross_community | 7 |
| `Complete → New` | cross_community | 7 |
| `Complete → As_slice` | cross_community | 6 |

## How to Explore

1. `context({name: "derive_temp_migration_path"})` — see callers and callees
2. `query({search_query: "chunked_serializer"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
