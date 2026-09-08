---
name: gitnexus-area-tests
description: "Skill for the Tests area of garnet_rust. 1706 symbols across 201 files."
---

# Tests

1706 symbols | 201 files | Cohesion: 82%

## When to Use

- Working with code in `wedb_store/`
- Understanding how is_point_within_radius, contains, is_valid work
- Modifying tests-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_zset/src/zset.rs` | cmp, encode, eq, hash, partial_cmp (+107) |
| `wedb_store/src/redis.rs` | hdel, hdel_unlocked, hexists, hexpire, hget (+81) |
| `wedb_store/tests/range_index_compat.rs` | _log_init, new_store, open_store, test_range_index_manager_key_exists, test_ri_concurrent_multi_client (+70) |
| `wedb_hash/src/hash.rs` | cleanup_expiration_if_empty, delete_expired, deserialize, expired_since, from_bitcode (+44) |
| `wedb_record/src/ns_codec.rs` | encode_with_session_prefix, encode_with_session_prefix_to_slice, with_session_prefix, write_key_parts, decode_chunk_key (+31) |
| `wedb_record/src/meta.rs` | bump_version, dec_size, encoding, inc_size, set_encoding (+28) |
| `wedb_resp/src/write.rs` | write_array_item, write_array_len, write_bulk_string, write_bulk_string_chunks, write_double_bulk_string (+28) |
| `wedb_store/src/session.rs` | append_collection_chunks_batch, append_hash_field, append_hash_fields_batch, append_set_member, append_set_members_batch (+27) |
| `wedb_hash/tests/main.rs` | _log_init, test_field_expiration, test_hexpire_option_precedence_over_past_timestamp, test_hexpireat_and_hpexpireat, test_hrandfield_edge_cases (+26) |
| `wedb_record/src/zset.rs` | from_slice, decode_score_key, encode_score_prefix, from_member, from_score (+25) |

## Entry Points

Start here when exploring this area:

- **`is_point_within_radius`** (Function) — `wedb_zset/src/geo.rs:377`
- **`contains`** (Function) — `wedb_zset/src/skiplist.rs:48`
- **`is_valid`** (Function) — `wedb_zset/src/skiplist.rs:36`
- **`count_by_score`** (Function) — `wedb_zset/src/skiplist.rs:850`
- **`delete`** (Function) — `wedb_zset/src/skiplist.rs:334`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `is_point_within_radius` | Function | `wedb_zset/src/geo.rs` | 377 |
| `contains` | Function | `wedb_zset/src/skiplist.rs` | 48 |
| `is_valid` | Function | `wedb_zset/src/skiplist.rs` | 36 |
| `count_by_score` | Function | `wedb_zset/src/skiplist.rs` | 850 |
| `delete` | Function | `wedb_zset/src/skiplist.rs` | 334 |
| `delete_entry` | Function | `wedb_zset/src/skiplist.rs` | 271 |
| `get_by_rank` | Function | `wedb_zset/src/skiplist.rs` | 371 |
| `get_rank` | Function | `wedb_zset/src/skiplist.rs` | 339 |
| `insert` | Function | `wedb_zset/src/skiplist.rs` | 183 |
| `is_empty` | Function | `wedb_zset/src/skiplist.rs` | 169 |
| `iter` | Function | `wedb_zset/src/skiplist.rs` | 159 |
| `len` | Function | `wedb_zset/src/skiplist.rs` | 154 |
| `new` | Function | `wedb_zset/src/skiplist.rs` | 141 |
| `pop_first` | Function | `wedb_zset/src/skiplist.rs` | 599 |
| `pop_last` | Function | `wedb_zset/src/skiplist.rs` | 639 |
| `range_by_rank` | Function | `wedb_zset/src/skiplist.rs` | 467 |
| `range_by_rank_borrowed` | Function | `wedb_zset/src/skiplist.rs` | 394 |
| `range_by_score` | Function | `wedb_zset/src/skiplist.rs` | 584 |
| `range_by_score_borrowed` | Function | `wedb_zset/src/skiplist.rs` | 476 |
| `encode` | Function | `wedb_zset/src/zset.rs` | 74 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Main → Encode_sortable_f64` | cross_community | 8 |
| `Complete → Is_empty` | cross_community | 8 |
| `Complete → Push` | cross_community | 8 |
| `Complete → Promote_immutable_to_read_cache` | cross_community | 8 |
| `Smove → Is_empty` | cross_community | 8 |
| `Smove → Push` | cross_community | 8 |
| `Smove → Promote_immutable_to_read_cache` | cross_community | 8 |
| `Zlexcount → Is_empty` | cross_community | 8 |
| `Zlexcount → Push` | cross_community | 8 |
| `Zlexcount → Promote_immutable_to_read_cache` | cross_community | 8 |

## How to Explore

1. `context({name: "is_point_within_radius"})` — see callers and callees
2. `query({search_query: "tests"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
