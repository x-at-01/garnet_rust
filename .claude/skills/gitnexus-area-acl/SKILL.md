---
name: gitnexus-area-acl
description: "Skill for the Acl area of garnet_rust. 101 symbols across 14 files."
---

# Acl

101 symbols | 14 files | Cohesion: 64%

## When to Use

- Working with code in `wedb_acl/`
- Understanding how list_users, set_user, add_user_handle work
- Modifying acl-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_acl/tests/acl/user.rs` | test_add_and_remove_category, test_bad_input_unknown_operation, test_delete_multiple_user, test_delete_no_user, test_delete_single_user (+13) |
| `wedb_acl/src/acl.rs` | list_users, set_user, add_user_handle, del_users, auth_default (+10) |
| `wedb_acl/src/user.rs` | with_user, add_password, add_password_hash, authenticate, authenticate (+10) |
| `wedb_acl/tests/acl/commands.rs` | test_basic_list, test_basic_users, test_basic_whoami, test_client_setinfo_denied_returns_noperm, test_denied_command_returns_noperm (+3) |
| `wedb_acl/tests/acl/namespace.rs` | test_ns_binding_value_boundaries, test_admin_scope_and_cross_namespace_rules, test_global_listing_includes_all_namespaces, test_namespace_describe_roundtrip, test_same_name_users_isolated_across_namespaces (+2) |
| `wedb_acl/tests/acl/storage.rs` | test_storage_pagination_and_count, test_storage_point_read_write_and_del, test_acl_manager_lazy_loading, test_cold_start_incremental_update_and_deletion, test_cross_namespace_set_user_async_targets_lookup_bucket (+2) |
| `wedb_acl/src/ns.rs` | prefix_len, test_user_key_roundtrip, test_with_user_key_matches_user_key, user_key, with_user_key (+1) |
| `wedb_acl/src/scope.rs` | del_users, set_user_async, find_user, set_user, del_user (+1) |
| `wedb_acl/tests/main.rs` | test_batch_del_users_and_protection, test_acl_parser_ops, test_acl_password, test_user_authentication_and_authorization, test_garnet_rule_reductions |
| `wedb_acl/src/command_set.rs` | category_commands, get_category_bitmap, lookup_category, normalize_category_name |

## Entry Points

Start here when exploring this area:

- **`list_users`** (Function) — `wedb_acl/src/acl.rs:247`
- **`set_user`** (Function) — `wedb_acl/src/acl.rs:214`
- **`add_user_handle`** (Function) — `wedb_acl/src/acl.rs:143`
- **`del_users`** (Function) — `wedb_acl/src/acl.rs:234`
- **`user_key`** (Function) — `wedb_acl/src/ns.rs:50`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `list_users` | Function | `wedb_acl/src/acl.rs` | 247 |
| `set_user` | Function | `wedb_acl/src/acl.rs` | 214 |
| `add_user_handle` | Function | `wedb_acl/src/acl.rs` | 143 |
| `del_users` | Function | `wedb_acl/src/acl.rs` | 234 |
| `user_key` | Function | `wedb_acl/src/ns.rs` | 50 |
| `with_user_key` | Function | `wedb_acl/src/ns.rs` | 61 |
| `del_users` | Function | `wedb_acl/src/scope.rs` | 224 |
| `with_user` | Function | `wedb_acl/src/user.rs` | 652 |
| `from_cleartext` | Function | `wedb_acl/src/password.rs` | 51 |
| `from_hash_hex` | Function | `wedb_acl/src/password.rs` | 68 |
| `to_hex` | Function | `wedb_acl/src/password.rs` | 80 |
| `add_password` | Function | `wedb_acl/src/user.rs` | 316 |
| `add_password_hash` | Function | `wedb_acl/src/user.rs` | 321 |
| `authenticate` | Function | `wedb_acl/src/user.rs` | 172 |
| `auth_default` | Function | `wedb_acl/src/acl.rs` | 194 |
| `authenticate` | Function | `wedb_acl/src/user.rs` | 688 |
| `can_access_channel` | Function | `wedb_acl/src/user.rs` | 712 |
| `can_access_key` | Function | `wedb_acl/src/user.rs` | 706 |
| `can_execute` | Function | `wedb_acl/src/user.rs` | 694 |
| `can_execute_custom` | Function | `wedb_acl/src/user.rs` | 700 |

## How to Explore

1. `context({name: "list_users"})` — see callers and callees
2. `query({search_query: "acl"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
