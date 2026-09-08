---
name: gitnexus-area-pubsub
description: "Skill for the Pubsub area of garnet_rust. 43 symbols across 8 files."
---

# Pubsub

43 symbols | 8 files | Cohesion: 82%

## When to Use

- Working with code in `wedb_pubsub/`
- Understanding how publish_now, new, create_session work
- Modifying pubsub-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_pubsub/tests/pubsub/resp_pubsub.rs` | pubsub_channels, pubsub_numpat, pubsub_numsub, command_utils, punsubscribe_all_command_behavior (+6) |
| `wedb_pubsub/tests/pubsub/support.rs` | execute, execute_tokens, is_in_sub_mode, new, subscription_count (+4) |
| `wedb_pubsub/tests/pubsub/consistency.rs` | drain, full_preserves_session_while_disconnected_removes, multi_channel_multi_pattern_delivery_formula, pattern_unsubscribe_reverse_index_cleanup, payload_shared_zero_copy_and_immutable (+2) |
| `wedb_pubsub/tests/pubsub/session_lifecycle.rs` | backpressure_smooth_degradation, concurrent_subscribe_unsubscribe_race, dead_session_cascade_cleanup_during_publish, dead_session_dedup_cascade_cleanup, multi_session_broadcast_isolation (+1) |
| `wedb_pubsub/src/broker.rs` | publish_now, pattern_subscribe, psubscribe, subscribe |
| `wedb_pubsub/tests/main.rs` | core_concurrent_publish_subscribe_stress, core_pattern_subscribe_publish_end_to_end, core_subscribe_publish_end_to_end |
| `wedb_pubsub/src/session.rs` | new, create_session |
| `wedb_pubsub/tests/pubsub/cluster_forward.rs` | cluster_publish_survives_peer_node_shutdown |

## Entry Points

Start here when exploring this area:

- **`publish_now`** (Function) — `wedb_pubsub/src/broker.rs:281`
- **`new`** (Function) — `wedb_pubsub/src/session.rs:38`
- **`create_session`** (Function) — `wedb_pubsub/src/session.rs:76`
- **`execute`** (Function) — `wedb_pubsub/tests/pubsub/support.rs:50`
- **`execute_tokens`** (Function) — `wedb_pubsub/tests/pubsub/support.rs:55`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `publish_now` | Function | `wedb_pubsub/src/broker.rs` | 281 |
| `new` | Function | `wedb_pubsub/src/session.rs` | 38 |
| `create_session` | Function | `wedb_pubsub/src/session.rs` | 76 |
| `execute` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 50 |
| `execute_tokens` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 55 |
| `is_in_sub_mode` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 42 |
| `new` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 25 |
| `subscription_count` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 38 |
| `read_available` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 295 |
| `send_command` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 290 |
| `setup` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 285 |
| `pattern_subscribe` | Function | `wedb_pubsub/src/broker.rs` | 178 |
| `psubscribe` | Function | `wedb_pubsub/src/broker.rs` | 172 |
| `subscribe` | Function | `wedb_pubsub/src/broker.rs` | 164 |
| `subscribe_and_publish` | Function | `wedb_pubsub/tests/pubsub/support.rs` | 300 |
| `core_concurrent_publish_subscribe_stress` | Function | `wedb_pubsub/tests/main.rs` | 148 |
| `core_pattern_subscribe_publish_end_to_end` | Function | `wedb_pubsub/tests/main.rs` | 121 |
| `core_subscribe_publish_end_to_end` | Function | `wedb_pubsub/tests/main.rs` | 94 |
| `cluster_publish_survives_peer_node_shutdown` | Function | `wedb_pubsub/tests/pubsub/cluster_forward.rs` | 11 |
| `drain` | Function | `wedb_pubsub/tests/pubsub/consistency.rs` | 10 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Broadcast_exact_prunes_disconnected_sessions → New` | cross_community | 3 |

## How to Explore

1. `context({name: "publish_now"})` — see callers and callees
2. `query({search_query: "pubsub"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
