---
name: gitnexus-area-blocking
description: "Skill for the Blocking area of garnet_rust. 39 symbols across 8 files."
---

# Blocking

39 symbols | 8 files | Cohesion: 91%

## When to Use

- Working with code in `wedb_blocking/`
- Understanding how exact_broker, new, push_list_left work
- Modifying blocking-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_blocking/tests/blocking/misc.rs` | test_abandoned_wait_purged_not_swallowing, test_blpop_wrong_type_rejected, test_clean_keys_to_observers, test_extreme_timeout_no_panic, test_midwait_mismatch_keeps_waiter_blocked (+3) |
| `wedb_blocking/tests/blocking/pop.rs` | test_blmpop_multi_pop, test_blpop_fifo_wake_order, test_blpop_multi_key_priority, test_blpop_single_key_wakes_waiter, test_blpop_timeout_returns_empty (+3) |
| `wedb_blocking/tests/blocking/blmove.rs` | test_blmove_blocking_transfer, test_blmove_cascading_wakeup, test_blmove_dst_wrong_type_preserves_src, test_blmove_dst_wrong_type_wakeup_error, test_blmove_same_key_rotation (+2) |
| `wedb_blocking/src/provider.rs` | default, list_entry_for_write, new, push_list_left, push_list_right (+2) |
| `wedb_blocking/tests/blocking/signal.rs` | signal_blpop, test_signal_mode_competitor_reblock, test_signal_mode_lost_wakeup_recovered, test_signal_mode_multi_key_window, test_signal_mode_notify_wakes_waiter |
| `wedb_blocking/src/result.rs` | is_force_unblocked, is_type_mismatch |
| `wedb_blocking/tests/blocking/stress.rs` | test_concurrent_registration_assignment_stress |
| `wedb_blocking/tests/support/mod.rs` | exact_broker |

## Entry Points

Start here when exploring this area:

- **`exact_broker`** (Function) — `wedb_blocking/tests/support/mod.rs:4`
- **`new`** (Function) — `wedb_blocking/src/provider.rs:119`
- **`push_list_left`** (Function) — `wedb_blocking/src/provider.rs:183`
- **`push_list_right`** (Function) — `wedb_blocking/src/provider.rs:188`
- **`zadd`** (Function) — `wedb_blocking/src/provider.rs:218`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `exact_broker` | Function | `wedb_blocking/tests/support/mod.rs` | 4 |
| `new` | Function | `wedb_blocking/src/provider.rs` | 119 |
| `push_list_left` | Function | `wedb_blocking/src/provider.rs` | 183 |
| `push_list_right` | Function | `wedb_blocking/src/provider.rs` | 188 |
| `zadd` | Function | `wedb_blocking/src/provider.rs` | 218 |
| `is_force_unblocked` | Function | `wedb_blocking/src/result.rs` | 150 |
| `is_type_mismatch` | Function | `wedb_blocking/src/result.rs` | 156 |
| `test_blmove_blocking_transfer` | Function | `wedb_blocking/tests/blocking/blmove.rs` | 19 |
| `test_blmove_cascading_wakeup` | Function | `wedb_blocking/tests/blocking/blmove.rs` | 327 |
| `test_blmove_dst_wrong_type_preserves_src` | Function | `wedb_blocking/tests/blocking/blmove.rs` | 139 |
| `test_blmove_dst_wrong_type_wakeup_error` | Function | `wedb_blocking/tests/blocking/blmove.rs` | 213 |
| `test_blmove_same_key_rotation` | Function | `wedb_blocking/tests/blocking/blmove.rs` | 93 |
| `test_blmove_wrong_type_no_cascading_wakeup` | Function | `wedb_blocking/tests/blocking/blmove.rs` | 260 |
| `test_brpoplpush_single_arg_direct_move` | Function | `wedb_blocking/tests/blocking/blmove.rs` | 173 |
| `test_abandoned_wait_purged_not_swallowing` | Function | `wedb_blocking/tests/blocking/misc.rs` | 207 |
| `test_blpop_wrong_type_rejected` | Function | `wedb_blocking/tests/blocking/misc.rs` | 19 |
| `test_clean_keys_to_observers` | Function | `wedb_blocking/tests/blocking/misc.rs` | 115 |
| `test_extreme_timeout_no_panic` | Function | `wedb_blocking/tests/blocking/misc.rs` | 176 |
| `test_midwait_mismatch_keeps_waiter_blocked` | Function | `wedb_blocking/tests/blocking/misc.rs` | 44 |
| `test_notify_skips_disconnected_observer` | Function | `wedb_blocking/tests/blocking/misc.rs` | 259 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Try_move_item → New_papaya_map` | cross_community | 4 |

## How to Explore

1. `context({name: "exact_broker"})` — see callers and callees
2. `query({search_query: "blocking"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
