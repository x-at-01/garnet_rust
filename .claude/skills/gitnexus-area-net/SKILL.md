---
name: gitnexus-area-net
description: "Skill for the Net area of garnet_rust. 163 symbols across 21 files."
---

# Net

163 symbols | 21 files | Cohesion: 88%

## When to Use

- Working with code in `wedb_server/`
- Understanding how parse_module_spec, unparsed_slice, finish_array work
- Modifying net-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_server/src/dispatcher.rs` | auth_and_bind_session, dispatch, execute_command, add_key, execute_single_command (+30) |
| `wedb_net/src/buffer.rs` | unparsed_slice, finish_array, new, recycle, start_array (+24) |
| `wedb_resp/src/parse_state.rs` | deref, read_double, read_int, try_read_double, try_read_int (+10) |
| `wedb_server/src/range_index.rs` | dispatch_range_index, handle_ri_config, handle_ri_create, handle_ri_del, handle_ri_exists (+5) |
| `wedb_server/src/vectors.rs` | dispatch_vectors, parse_vector, vadd, vemb, vinfo (+3) |
| `wedb_net/tests/net/protocol_robustness.rs` | test_large_value_roundtrip, test_max_connections_enforced, test_empty_frames_silently_ignored, test_unknown_command_keeps_connection_alive, make_multibulk_del (+3) |
| `wedb_server/src/modules.rs` | builtin_module, dispatch_modules, handle_module_load, handle_module_unload, parse_module_spec_checked (+2) |
| `wedb_net/src/session.rs` | bump_key_version, execute, execute_single_command, refresh_pubsub_mode, parse_range (+1) |
| `wedb_net/tests/net/buffer_pool.rs` | test_send_buffer_dynamic_array, test_send_buffer_recycle, test_buffer_pool_edge_and_capacity_behavior, test_limited_fixed_buffer_pool, test_pooled_receive_buffer (+1) |
| `wedb_server/src/scripts.rs` | new, dispatch_script_sub, dispatch_scripts, split_keys_argv, write_lua_error (+1) |

## Entry Points

Start here when exploring this area:

- **`parse_module_spec`** (Function) — `wedb_module/src/utils.rs:16`
- **`unparsed_slice`** (Function) — `wedb_net/src/buffer.rs:44`
- **`finish_array`** (Function) — `wedb_net/src/buffer.rs:400`
- **`new`** (Function) — `wedb_net/src/buffer.rs:223`
- **`recycle`** (Function) — `wedb_net/src/buffer.rs:255`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `parse_module_spec` | Function | `wedb_module/src/utils.rs` | 16 |
| `unparsed_slice` | Function | `wedb_net/src/buffer.rs` | 44 |
| `finish_array` | Function | `wedb_net/src/buffer.rs` | 400 |
| `new` | Function | `wedb_net/src/buffer.rs` | 223 |
| `recycle` | Function | `wedb_net/src/buffer.rs` | 255 |
| `start_array` | Function | `wedb_net/src/buffer.rs` | 392 |
| `take` | Function | `wedb_net/src/buffer.rs` | 245 |
| `write_array_header` | Function | `wedb_net/src/buffer.rs` | 380 |
| `write_bulk_string` | Function | `wedb_net/src/buffer.rs` | 345 |
| `write_double_bulk` | Function | `wedb_net/src/buffer.rs` | 360 |
| `write_error` | Function | `wedb_net/src/buffer.rs` | 316 |
| `write_error_fmt` | Function | `wedb_net/src/buffer.rs` | 325 |
| `write_integer` | Function | `wedb_net/src/buffer.rs` | 334 |
| `write_null` | Function | `wedb_net/src/buffer.rs` | 295 |
| `write_null_array` | Function | `wedb_net/src/buffer.rs` | 301 |
| `write_ok` | Function | `wedb_net/src/buffer.rs` | 277 |
| `write_pong` | Function | `wedb_net/src/buffer.rs` | 283 |
| `write_queued` | Function | `wedb_net/src/buffer.rs` | 289 |
| `write_raw` | Function | `wedb_net/src/buffer.rs` | 271 |
| `write_simple_string` | Function | `wedb_net/src/buffer.rs` | 307 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `New → Level_index_pow2` | intra_community | 4 |
| `Execute → Is_none` | cross_community | 4 |
| `Dispatch_range_index → New` | intra_community | 4 |
| `Run → Ensure_read_capacity` | cross_community | 3 |
| `Run → Put_after_read` | cross_community | 3 |
| `Run → Take_for_read` | cross_community | 3 |
| `Run → Unparsed_slice` | cross_community | 3 |
| `Dispatch_range_index → Write_error` | intra_community | 3 |
| `Dispatch_range_index → Is_empty` | intra_community | 3 |
| `Dispatch_range_index → Len` | intra_community | 3 |

## How to Explore

1. `context({name: "parse_module_spec"})` — see callers and callees
2. `query({search_query: "net"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
