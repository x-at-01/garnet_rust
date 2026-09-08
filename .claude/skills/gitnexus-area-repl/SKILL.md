---
name: gitnexus-area-repl
description: "Skill for the Repl area of garnet_rust. 101 symbols across 14 files."
---

# Repl

101 symbols | 14 files | Cohesion: 76%

## When to Use

- Working with code in `wedb_repl/`
- Understanding how clear, feed, first_byte_offset work
- Modifying repl-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_repl/src/manager.rs` | new, with_history, adopt_primary_identity, append_stream, read_backlog (+14) |
| `wedb_repl/src/history.rs` | default, empty, generate, failover_update, new (+12) |
| `wedb_repl/tests/repl/handshake_and_psync.rs` | psync_decision_matrix, replication_history_failover_and_persistence, handshake_bare_continue_keeps_cached_replid, handshake_half_packet_and_sticky_packet, protocol_frames_encode_decode (+7) |
| `wedb_repl/src/protocol.rs` | encode_continue, encode_fullresync, encode_pong, encode_psync, encode_replconf_ack (+6) |
| `wedb_repl/src/backlog.rs` | clear, default, feed, first_byte_offset, is_offset_in_range (+5) |
| `wedb_repl/src/client.rs` | connect_and_handshake, connect_with_retry, new, reset, send_ack (+5) |
| `wedb_repl/tests/repl/backlog_and_replayer.rs` | backlog_circular_wrapping_and_offset_calc, backlog_high_frequency_multi_round_monotonic_overwrites, backlog_zero_copy_slices, reconnect_resume_backlog_psync, replica_collection_commands_replay_to_store (+3) |
| `wedb_repl/tests/repl/cluster_replication.rs` | adopt_primary_identity_atomic_adoption, cluster_sr_primary_restart, cluster_replication_stale_replica_eviction_and_demote, replica_ack_offset_monotonic_under_stale_acks, cluster_replication_safe_aof_address_multi_replica (+1) |
| `wedb_repl/tests/main.rs` | test_repl_id_smoke, test_e2e_replica_client_smoke |
| `wedb_repl/tests/repl/support.rs` | fill_hash_field_bytes, make_hash_field_bytes |

## Entry Points

Start here when exploring this area:

- **`clear`** (Function) — `wedb_repl/src/backlog.rs:156`
- **`feed`** (Function) — `wedb_repl/src/backlog.rs:82`
- **`first_byte_offset`** (Function) — `wedb_repl/src/backlog.rs:67`
- **`is_offset_in_range`** (Function) — `wedb_repl/src/backlog.rs:73`
- **`master_offset`** (Function) — `wedb_repl/src/backlog.rs:55`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `clear` | Function | `wedb_repl/src/backlog.rs` | 156 |
| `feed` | Function | `wedb_repl/src/backlog.rs` | 82 |
| `first_byte_offset` | Function | `wedb_repl/src/backlog.rs` | 67 |
| `is_offset_in_range` | Function | `wedb_repl/src/backlog.rs` | 73 |
| `master_offset` | Function | `wedb_repl/src/backlog.rs` | 55 |
| `new` | Function | `wedb_repl/src/backlog.rs` | 30 |
| `read_bytes` | Function | `wedb_repl/src/backlog.rs` | 143 |
| `set_master_offset` | Function | `wedb_repl/src/backlog.rs` | 61 |
| `slices` | Function | `wedb_repl/src/backlog.rs` | 118 |
| `new` | Function | `wedb_repl/src/manager.rs` | 41 |
| `with_history` | Function | `wedb_repl/src/manager.rs` | 46 |
| `empty` | Function | `wedb_repl/src/history.rs` | 49 |
| `generate` | Function | `wedb_repl/src/history.rs` | 33 |
| `failover_update` | Function | `wedb_repl/src/history.rs` | 170 |
| `new` | Function | `wedb_repl/src/history.rs` | 153 |
| `save_to_file` | Function | `wedb_repl/src/history.rs` | 255 |
| `to_byte_array` | Function | `wedb_repl/src/history.rs` | 203 |
| `as_str` | Function | `wedb_repl/src/history.rs` | 73 |
| `adopt_primary_identity` | Function | `wedb_repl/src/manager.rs` | 390 |
| `append_stream` | Function | `wedb_repl/src/manager.rs` | 281 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Start → Bind` | cross_community | 3 |

## How to Explore

1. `context({name: "clear"})` — see callers and callees
2. `query({search_query: "repl"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
