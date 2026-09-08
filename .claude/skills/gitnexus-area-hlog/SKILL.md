---
name: gitnexus-area-hlog
description: "Skill for the Hlog area of garnet_rust. 51 symbols across 8 files."
---

# Hlog

51 symbols | 8 files | Cohesion: 66%

## When to Use

- Working with code in `wedb_hlog/`
- Understanding how calculate_read_only_address, page_id, page_start_address work
- Modifying hlog-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_hlog/src/hlog/io.rs` | clamp_flush_range, flush_all, flush_page, flush_pages_range, requeue_flush_range (+7) |
| `wedb_hlog/src/buffer.rs` | is_page_loaded, write_page, raw_page_ptr_mut, get_physical_address, offset_in_page (+4) |
| `wedb_hlog/src/hlog/inplace.rs` | revivify_record_at, try_mark_tombstone_in_place, try_modify_record_in_place, try_modify_record_with_slack, try_revivify_in_chain (+2) |
| `wedb_hlog/src/hlog/mod.rs` | parse_record_from_slice, reject_pad, encode_at, write_pad_tail, validate_append_args (+2) |
| `wedb_hlog/src/config.rs` | calculate_read_only_address, page_id, page_start_address, page_mask, page_offset |
| `wedb_hlog/src/hlog/shift.rs` | shift_begin_address, shift_head_address, shift_read_only_address, shift_read_only_to_tail |
| `wedb_record/src/codec.rs` | build_header, checked_record_size, encode_to_slice, try_encode_to_vec |
| `wedb_hlog/src/hlog/append.rs` | append, ensure_page_ready, maybe_advance_read_only |

## Entry Points

Start here when exploring this area:

- **`calculate_read_only_address`** (Function) — `wedb_hlog/src/config.rs:170`
- **`page_id`** (Function) — `wedb_hlog/src/config.rs:143`
- **`page_start_address`** (Function) — `wedb_hlog/src/config.rs:155`
- **`flush_all`** (Function) — `wedb_hlog/src/hlog/io.rs:415`
- **`flush_page`** (Function) — `wedb_hlog/src/hlog/io.rs:244`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `calculate_read_only_address` | Function | `wedb_hlog/src/config.rs` | 170 |
| `page_id` | Function | `wedb_hlog/src/config.rs` | 143 |
| `page_start_address` | Function | `wedb_hlog/src/config.rs` | 155 |
| `flush_all` | Function | `wedb_hlog/src/hlog/io.rs` | 415 |
| `flush_page` | Function | `wedb_hlog/src/hlog/io.rs` | 244 |
| `flush_pages_range` | Function | `wedb_hlog/src/hlog/io.rs` | 266 |
| `shift_begin_address` | Function | `wedb_hlog/src/hlog/shift.rs` | 57 |
| `shift_head_address` | Function | `wedb_hlog/src/hlog/shift.rs` | 37 |
| `is_page_loaded` | Function | `wedb_hlog/src/buffer.rs` | 170 |
| `write_page` | Function | `wedb_hlog/src/buffer.rs` | 98 |
| `revivify_record_at` | Function | `wedb_hlog/src/hlog/inplace.rs` | 102 |
| `try_mark_tombstone_in_place` | Function | `wedb_hlog/src/hlog/inplace.rs` | 30 |
| `try_modify_record_in_place` | Function | `wedb_hlog/src/hlog/inplace.rs` | 47 |
| `try_modify_record_with_slack` | Function | `wedb_hlog/src/hlog/inplace.rs` | 63 |
| `try_revivify_in_chain` | Function | `wedb_hlog/src/hlog/inplace.rs` | 76 |
| `try_update_in_place` | Function | `wedb_hlog/src/hlog/inplace.rs` | 11 |
| `page_mask` | Function | `wedb_hlog/src/config.rs` | 125 |
| `page_offset` | Function | `wedb_hlog/src/config.rs` | 149 |
| `iterate_version_chain` | Function | `wedb_hlog/src/hlog/io.rs` | 430 |
| `read_disk_record` | Function | `wedb_hlog/src/hlog/io.rs` | 137 |

## Execution Flows

| Flow | Type | Steps |
|------|------|-------|
| `Flush_all → Page_bits` | cross_community | 4 |
| `Flush_all → Remove_next_adjacent` | cross_community | 4 |
| `Flush_all → Remove_previous_adjacent` | cross_community | 4 |
| `Read_record → Page_idx` | cross_community | 4 |
| `Read_record → Page_slice_unchecked` | cross_community | 4 |
| `Read_record → Page_bits` | cross_community | 4 |
| `With_memory_record → Page_idx` | cross_community | 4 |
| `With_memory_record → Page_slice_unchecked` | cross_community | 4 |
| `With_memory_record → Page_bits` | cross_community | 4 |
| `Append → Checked_record_size` | cross_community | 3 |

## How to Explore

1. `context({name: "calculate_read_only_address"})` — see callers and callees
2. `query({search_query: "hlog"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
