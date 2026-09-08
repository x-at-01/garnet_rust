---
name: gitnexus-area-epoch
description: "Skill for the Epoch area of garnet_rust. 57 symbols across 7 files."
---

# Epoch

57 symbols | 7 files | Cohesion: 80%

## When to Use

- Working with code in `wedb_epoch/`
- Understanding how bump_and_wait, bump_current_epoch, bump_current_epoch_action work
- Modifying epoch-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_epoch/src/epoch.rs` | bump_and_wait, bump_current_epoch, bump_current_epoch_action, bump_epoch, compute_safe_to_reclaim_epoch (+23) |
| `wedb_epoch/tests/epoch/protection.rs` | protected_scope_thread_affinity_and_debug, protected_thread_owns_valid_slot, refresh_republishes_latest_epoch_every_time, suspend_resume_keeps_thread_protected, protection_survives_repeated_resume_suspend_cycles (+4) |
| `wedb_epoch/tests/epoch/drain.rs` | action_runs_immediately_when_nobody_else_is_protected, many_threads_registering_actions_all_run_exactly_once, actions_run_in_epoch_order, every_action_runs_exactly_once_when_drain_list_fills, refresh_path_executes_pending_action (+2) |
| `wedb_epoch/tests/epoch/concurrency.rs` | multi_instance_thread_local_isolation, adversarial_concurrent_claim_and_reserve_race, adversarial_slot_exhaustion_and_recycling, concurrent_readers_and_epoch_advance_stress, high_concurrency_heavy_stress |
| `wedb_epoch/tests/epoch/support.rs` | assert_protected_at, new, join_all, drop, leave_and_join |
| `wedb_epoch/tests/epoch/user_word.rs` | concurrent_user_word_allocation_race, user_word_capacity_limits_and_errors |
| `example/examples/epoch.rs` | main |

## Entry Points

Start here when exploring this area:

- **`bump_and_wait`** (Function) — `wedb_epoch/src/epoch.rs:755`
- **`bump_current_epoch`** (Function) — `wedb_epoch/src/epoch.rs:558`
- **`bump_current_epoch_action`** (Function) — `wedb_epoch/src/epoch.rs:586`
- **`bump_epoch`** (Function) — `wedb_epoch/src/epoch.rs:571`
- **`compute_safe_to_reclaim_epoch`** (Function) — `wedb_epoch/src/epoch.rs:664`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `bump_and_wait` | Function | `wedb_epoch/src/epoch.rs` | 755 |
| `bump_current_epoch` | Function | `wedb_epoch/src/epoch.rs` | 558 |
| `bump_current_epoch_action` | Function | `wedb_epoch/src/epoch.rs` | 586 |
| `bump_epoch` | Function | `wedb_epoch/src/epoch.rs` | 571 |
| `compute_safe_to_reclaim_epoch` | Function | `wedb_epoch/src/epoch.rs` | 664 |
| `current_epoch` | Function | `wedb_epoch/src/epoch.rs` | 653 |
| `drain` | Function | `wedb_epoch/src/epoch.rs` | 693 |
| `is_safe_to_reclaim` | Function | `wedb_epoch/src/epoch.rs` | 744 |
| `new` | Function | `wedb_epoch/src/epoch.rs` | 321 |
| `protect_and_drain` | Function | `wedb_epoch/src/epoch.rs` | 529 |
| `protected_scope` | Function | `wedb_epoch/src/epoch.rs` | 550 |
| `suspend_resume` | Function | `wedb_epoch/src/epoch.rs` | 523 |
| `assert_protected_at` | Function | `wedb_epoch/tests/epoch/support.rs` | 79 |
| `set_this_thread_user_word` | Function | `wedb_epoch/src/epoch.rs` | 916 |
| `test_hook_this_thread_announced_epoch` | Function | `wedb_epoch/src/epoch.rs` | 797 |
| `this_thread_user_word` | Function | `wedb_epoch/src/epoch.rs` | 906 |
| `this_thread_user_word_atomic` | Function | `wedb_epoch/src/epoch.rs` | 888 |
| `new` | Function | `wedb_epoch/tests/epoch/support.rs` | 23 |
| `join_all` | Function | `wedb_epoch/tests/epoch/support.rs` | 72 |
| `leave_and_join` | Function | `wedb_epoch/tests/epoch/support.rs` | 57 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Main → Get_thread_entry` | cross_community | 6 |
| `Main → Compute_safe_to_reclaim_epoch` | intra_community | 6 |
| `Main → Current_epoch` | intra_community | 5 |
| `Bump_and_wait → Compute_safe_to_reclaim_epoch` | intra_community | 5 |
| `Main → Current_thread_id` | cross_community | 5 |
| `Main → New` | intra_community | 3 |

## How to Explore

1. `context({name: "bump_and_wait"})` — see callers and callees
2. `query({search_query: "epoch"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
