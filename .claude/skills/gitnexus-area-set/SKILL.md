---
name: gitnexus-area-set
description: "Skill for the Set area of garnet_rust. 63 symbols across 10 files."
---

# Set

63 symbols | 10 files | Cohesion: 65%

## When to Use

- Working with code in `wedb_set/`
- Understanding how sadd, smismember, srem work
- Modifying set-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_set/tests/set/resp_commands.rs` | test_sampling_distribution_uniformity, build, check, test_scard_basic, test_sismember_basic (+15) |
| `wedb_set/tests/set/algebra_and_store.rs` | test_aggregate_symmetry_edges, test_in_place_extreme_edges, test_probes_overflow_smooth_heap_expansion, test_sdiff_basic, test_sdiff_empty_and_self (+8) |
| `wedb_set/src/set.rs` | sadd, smismember, srem, to_vec, spop (+6) |
| `wedb_set/tests/set/codec_and_extensible.rs` | test_codec_and_deserialize_partial_chunk_safeguards, test_deserialize_trailing_garbage, test_deserialize_corruption_never_panics, test_empty_set_serialization, test_large_scale_roundtrip (+3) |
| `wedb_set/tests/set/support.rs` | make_set, make_seq_set, sorted_members |
| `wedb_object/tests/main.rs` | test_concurrent_deserialize, test_trailing_garbage_payload_tolerance |
| `wedb_set/tests/main.rs` | test_smoke_pop_and_rand, test_smoke_serialization_and_bitcode |
| `wedb_zset/src/zset.rs` | zrandmember_impl, to_item |
| `wedb_record/src/sample.rs` | sample_distinct_indices |
| `wedb_record/src/compact_set.rs` | clear |

## Entry Points

Start here when exploring this area:

- **`sadd`** (Function) — `wedb_set/src/set.rs:207`
- **`smismember`** (Function) — `wedb_set/src/set.rs:253`
- **`srem`** (Function) — `wedb_set/src/set.rs:230`
- **`to_vec`** (Function) — `wedb_set/src/set.rs:813`
- **`make_set`** (Function) — `wedb_set/tests/set/support.rs:5`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `sadd` | Function | `wedb_set/src/set.rs` | 207 |
| `smismember` | Function | `wedb_set/src/set.rs` | 253 |
| `srem` | Function | `wedb_set/src/set.rs` | 230 |
| `to_vec` | Function | `wedb_set/src/set.rs` | 813 |
| `make_set` | Function | `wedb_set/tests/set/support.rs` | 5 |
| `sample_distinct_indices` | Function | `wedb_record/src/sample.rs` | 11 |
| `spop` | Function | `wedb_set/src/set.rs` | 270 |
| `srandmember` | Function | `wedb_set/src/set.rs` | 375 |
| `srandmember_ref` | Function | `wedb_set/src/set.rs` | 313 |
| `clear` | Function | `wedb_record/src/compact_set.rs` | 461 |
| `make_seq_set` | Function | `wedb_set/tests/set/support.rs` | 12 |
| `members_ref` | Function | `wedb_set/src/set.rs` | 199 |
| `srandmember_dup_ref` | Function | `wedb_set/src/set.rs` | 339 |
| `sorted_members` | Function | `wedb_set/tests/set/support.rs` | 26 |
| `serialize` | Function | `wedb_set/src/set.rs` | 795 |
| `encode_bitcode` | Function | `wedb_set/src/set.rs` | 821 |
| `test_concurrent_deserialize` | Function | `wedb_object/tests/main.rs` | 609 |
| `test_trailing_garbage_payload_tolerance` | Function | `wedb_object/tests/main.rs` | 525 |
| `test_codec_and_deserialize_partial_chunk_safeguards` | Function | `wedb_set/tests/set/codec_and_extensible.rs` | 441 |
| `test_deserialize_trailing_garbage` | Function | `wedb_set/tests/set/codec_and_extensible.rs` | 352 |

## How to Explore

1. `context({name: "sadd"})` — see callers and callees
2. `query({search_query: "set"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
