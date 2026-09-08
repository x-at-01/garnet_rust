---
name: gitnexus-area-txn
description: "Skill for the Txn area of garnet_rust. 117 symbols across 16 files."
---

# Txn

117 symbols | 16 files | Cohesion: 78%

## When to Use

- Working with code in `wedb_txn/`
- Understanding how fast_hash, fast_hash_u64, fast_hash_with_seed work
- Modifying txn-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_txn/src/manager.rs` | drop, commit, abort, begin_run, commit (+21) |
| `wedb_txn/tests/txn/support.rs` | b, parse, to_resp_command, create_client, send_command (+13) |
| `wedb_txn/src/key_entry.rs` | add_key_bytes, clear, iter, len, unlock_all_keys (+9) |
| `wedb_txn/tests/txn/watch_semantics.rs` | test_exec_clears_watch_state, test_flush_invalidates_all_watches, test_rewatch_refreshes_version_baseline, test_unwatch_noop_inside_multi, test_watched_keys_container (+7) |
| `wedb_txn/tests/txn/transactions.rs` | test_empty_transaction, test_txn_aborted_execabort, test_txn_discard, test_exec_discard_without_multi, test_nested_multi_interception (+6) |
| `wedb_txn/tests/txn/concurrency_and_deadlock.rs` | test_concurrent_watch_version_map_atomicity, test_deadlock_prevention, test_split_conflict_preserves_lockset, test_split_phase_prepare_and_validate, test_version_map_and_lock_lifecycle (+4) |
| `wedb_txn/src/version_map.rs` | bump_all, bump_version, bump_version_key, clear, hash_key (+2) |
| `wedb_txn/src/watched_keys.rs` | reset, validate_versions, watch, save_keys_to_lock, watched_count |
| `wedb_hasher/src/lib.rs` | fast_hash, fast_hash_u64, fast_hash_with_seed |
| `wedb_txn/src/state.rs` | is_in_txn, is_none, is_skipping_operations |

## Entry Points

Start here when exploring this area:

- **`fast_hash`** (Function) — `wedb_hasher/src/lib.rs:250`
- **`fast_hash_u64`** (Function) — `wedb_hasher/src/lib.rs:259`
- **`fast_hash_with_seed`** (Function) — `wedb_hasher/src/lib.rs:265`
- **`on_unknown_command`** (Function) — `wedb_net/src/session.rs:102`
- **`is_cluster_subcommand`** (Function) — `wedb_resp/src/cmd.rs:1254`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `fast_hash` | Function | `wedb_hasher/src/lib.rs` | 250 |
| `fast_hash_u64` | Function | `wedb_hasher/src/lib.rs` | 259 |
| `fast_hash_with_seed` | Function | `wedb_hasher/src/lib.rs` | 265 |
| `on_unknown_command` | Function | `wedb_net/src/session.rs` | 102 |
| `is_cluster_subcommand` | Function | `wedb_resp/src/cmd.rs` | 1254 |
| `add_key_bytes` | Function | `wedb_txn/src/key_entry.rs` | 188 |
| `clear` | Function | `wedb_txn/src/key_entry.rs` | 224 |
| `iter` | Function | `wedb_txn/src/key_entry.rs` | 218 |
| `len` | Function | `wedb_txn/src/key_entry.rs` | 200 |
| `unlock_all_keys` | Function | `wedb_txn/src/key_entry.rs` | 268 |
| `from_bytes` | Function | `wedb_txn/src/key_entry.rs` | 89 |
| `commit` | Function | `wedb_txn/src/manager.rs` | 478 |
| `abort` | Function | `wedb_txn/src/manager.rs` | 243 |
| `begin_run` | Function | `wedb_txn/src/manager.rs` | 417 |
| `commit` | Function | `wedb_txn/src/manager.rs` | 331 |
| `commit_with_options` | Function | `wedb_txn/src/manager.rs` | 314 |
| `discard` | Function | `wedb_txn/src/manager.rs` | 197 |
| `exec` | Function | `wedb_txn/src/manager.rs` | 337 |
| `exec_prepare` | Function | `wedb_txn/src/manager.rs` | 298 |
| `is_in_txn` | Function | `wedb_txn/src/manager.rs` | 99 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Handle_exec → Reset` | cross_community | 6 |
| `Handle_exec → Clear` | cross_community | 5 |
| `Handle_exec → New` | cross_community | 5 |
| `Execute → Is_none` | cross_community | 4 |
| `Handle_exec → Reserve` | cross_community | 4 |
| `Handle_exec → Watched_count` | cross_community | 4 |
| `Main → Fast_hash` | cross_community | 3 |
| `Handle_exec → Sort_by_key_hash` | cross_community | 3 |
| `Delete_index → Fast_hash` | cross_community | 3 |
| `Get_or_open_tree → Fast_hash` | cross_community | 3 |

## How to Explore

1. `context({name: "fast_hash"})` — see callers and callees
2. `query({search_query: "txn"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
