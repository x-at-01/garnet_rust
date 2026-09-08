---
name: gitnexus-area-resp
description: "Skill for the Resp area of garnet_rust. 47 symbols across 5 files."
---

# Resp

47 symbols | 5 files | Cohesion: 67%

## When to Use

- Working with code in `wedb_resp/`
- Understanding how try_read_byte_array_with_length_header, try_read_ptr_with_length_header, try_read_ptr_with_signed_length_header work
- Modifying resp-related functionality

## Key Files

| File | Symbols |
|------|---------|
| `wedb_resp/src/read.rs` | try_read_byte_array_with_length_header, try_read_ptr_with_length_header, try_read_ptr_with_signed_length_header, try_read_span_with_length_header, try_read_string_response_with_length_header (+24) |
| `wedb_resp/tests/resp/read_header.rs` | test_read_ptr_with_length_header, test_read_length_header, test_read_bool_with_length_header, test_read_array_length, test_read_array_length_exceptions (+7) |
| `wedb_resp/tests/resp/record_span.rs` | test_get_serialized_record_span_insufficient_header, test_get_serialized_record_span_negative_length, test_get_serialized_record_span_overflow_length, test_get_serialized_record_span_valid |
| `wedb_resp/tests/resp/limits.rs` | test_reject_oversized_lengths |
| `wedb_resp/tests/review_optimizations.rs` | test_read_advancing_apis |

## Entry Points

Start here when exploring this area:

- **`try_read_byte_array_with_length_header`** (Function) — `wedb_resp/src/read.rs:364`
- **`try_read_ptr_with_length_header`** (Function) — `wedb_resp/src/read.rs:377`
- **`try_read_ptr_with_signed_length_header`** (Function) — `wedb_resp/src/read.rs:382`
- **`try_read_span_with_length_header`** (Function) — `wedb_resp/src/read.rs:371`
- **`try_read_string_response_with_length_header`** (Function) — `wedb_resp/src/read.rs:399`

## Key Symbols

| Symbol | Type | File | Line |
|--------|------|------|------|
| `try_read_byte_array_with_length_header` | Function | `wedb_resp/src/read.rs` | 364 |
| `try_read_ptr_with_length_header` | Function | `wedb_resp/src/read.rs` | 377 |
| `try_read_ptr_with_signed_length_header` | Function | `wedb_resp/src/read.rs` | 382 |
| `try_read_span_with_length_header` | Function | `wedb_resp/src/read.rs` | 371 |
| `try_read_string_response_with_length_header` | Function | `wedb_resp/src/read.rs` | 399 |
| `try_skip_byte_array_with_length_header` | Function | `wedb_resp/src/read.rs` | 718 |
| `try_slice_with_length_header` | Function | `wedb_resp/src/read.rs` | 344 |
| `read_length_header` | Function | `wedb_resp/src/read.rs` | 797 |
| `try_read_signed_length_header` | Function | `wedb_resp/src/read.rs` | 282 |
| `try_read_signed_length_header_i32` | Function | `wedb_resp/src/read.rs` | 249 |
| `try_read_signed_map_len` | Function | `wedb_resp/src/read.rs` | 324 |
| `try_read_signed_set_len` | Function | `wedb_resp/src/read.rs` | 330 |
| `try_read_verbatim_string_len` | Function | `wedb_resp/src/read.rs` | 336 |
| `get_serialized_record_span` | Function | `wedb_resp/src/read.rs` | 727 |
| `read_serialized_record_span` | Function | `wedb_resp/src/read.rs` | 877 |
| `read_bool_with_length_header` | Function | `wedb_resp/src/read.rs` | 845 |
| `read_float_with_length_header` | Function | `wedb_resp/src/read.rs` | 861 |
| `try_read_bool_with_length_header` | Function | `wedb_resp/src/read.rs` | 460 |
| `try_read_float_with_length_header` | Function | `wedb_resp/src/read.rs` | 662 |
| `read_unsigned_length_header` | Function | `wedb_resp/src/read.rs` | 805 |

## How to Explore

1. `context({name: "try_read_byte_array_with_length_header"})` — see callers and callees
2. `query({search_query: "resp"})` — find related execution flows
3. Read key files listed above for implementation details
4. `explain({target: "<file or symbol>"})` — persisted taint findings (source→sink data flows), when indexed with `--pdg`
