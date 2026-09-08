use std::sync::Arc;

use itoa::Buffer;
use wedb_net::SendBuffer;
use wedb_resp::{ParseUtils, RespCommand, SessionParseState, consts};
use wkv::{RangeIndexError, ScanReturnField, StorageBackend, TreeTuning};

use crate::{context::ServerContext, error::Result, session::ServerSession};

/// 分发执行 RangeIndex (RI.*) 命令 (1:1 对标 Garnet RespServerSessionRangeIndex.cs)
pub async fn dispatch_range_index(
  ctx: &Arc<ServerContext>,
  session: &mut ServerSession,
  cmd: RespCommand,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  match cmd {
    RespCommand::Ricreate => handle_ri_create(ctx, session, args, buf).await,
    RespCommand::Riset => handle_ri_set(ctx, session, args, buf).await,
    RespCommand::Riget => handle_ri_get(session, args, buf).await,
    RespCommand::Ridel => handle_ri_del(ctx, session, args, buf).await,
    RespCommand::Riscan => handle_ri_scan(session, args, buf).await,
    RespCommand::Rirange => handle_ri_range(session, args, buf).await,
    RespCommand::Riexists => handle_ri_exists(session, args, buf).await,
    RespCommand::Riconfig => handle_ri_config(session, args, buf).await,
    RespCommand::Rimetrics => handle_ri_metrics(session, args, buf).await,
    _ => Ok(()),
  }
}

/// RI.CREATE key [MEMORY | DISK] [CACHESIZE n] [MINRECORD n] [MAXRECORD n] [MAXKEYLEN n] [PAGESIZE n]
async fn handle_ri_create(
  ctx: &Arc<ServerContext>,
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.is_empty() {
    buf.write_error(b"ERR wrong number of arguments for 'ri.create' command");
    return Ok(());
  }

  let key = args[0];

  // 默认参数 (1:1 对标 Garnet)
  let mut storage_backend = StorageBackend::Std; // 0=Disk by default
  let mut cache_size: i64 = 16 * 1024 * 1024; // 16 MiB
  let mut min_record_size: i64 = 64;
  let mut max_record_size: i64 = 1024;
  let mut max_key_len: i64 = 128;
  let mut leaf_page_size: i64 = 0; // 0 = auto-compute

  let mut idx = 1;
  while idx < args.len() {
    let arg = args[idx];
    if arg.eq_ignore_ascii_case(b"MEMORY") {
      storage_backend = StorageBackend::Memory;
      idx += 1;
    } else if arg.eq_ignore_ascii_case(b"DISK") {
      storage_backend = StorageBackend::Std;
      idx += 1;
    } else if arg.eq_ignore_ascii_case(b"CACHESIZE") {
      idx += 1;
      if idx >= args.len() {
        buf.write_error(b"ERR CACHESIZE requires a value");
        return Ok(());
      }
      cache_size = ParseUtils::try_read_long(args[idx]).unwrap_or(0);
      idx += 1;
    } else if arg.eq_ignore_ascii_case(b"MINRECORD") {
      idx += 1;
      if idx >= args.len() {
        buf.write_error(b"ERR MINRECORD requires a value");
        return Ok(());
      }
      min_record_size = ParseUtils::try_read_long(args[idx]).unwrap_or(0);
      idx += 1;
    } else if arg.eq_ignore_ascii_case(b"MAXRECORD") {
      idx += 1;
      if idx >= args.len() {
        buf.write_error(b"ERR MAXRECORD requires a value");
        return Ok(());
      }
      max_record_size = ParseUtils::try_read_long(args[idx]).unwrap_or(0);
      idx += 1;
    } else if arg.eq_ignore_ascii_case(b"MAXKEYLEN") {
      idx += 1;
      if idx >= args.len() {
        buf.write_error(b"ERR MAXKEYLEN requires a value");
        return Ok(());
      }
      max_key_len = ParseUtils::try_read_long(args[idx]).unwrap_or(0);
      idx += 1;
    } else if arg.eq_ignore_ascii_case(b"PAGESIZE") {
      idx += 1;
      if idx >= args.len() {
        buf.write_error(b"ERR PAGESIZE requires a value");
        return Ok(());
      }
      leaf_page_size = ParseUtils::try_read_long(args[idx]).unwrap_or(0);
      idx += 1;
    } else {
      buf.write_error(b"ERR unknown option");
      return Ok(());
    }
  }

  // 参数合法性校验
  if cache_size <= 0 || min_record_size <= 0 || max_record_size <= 0 || max_key_len <= 0 {
    buf.write_error(b"ERR numeric options must be greater than zero");
    return Ok(());
  }

  if min_record_size > max_record_size {
    buf.write_error(b"ERR MINRECORD must not exceed MAXRECORD");
    return Ok(());
  }

  match session
    .store_session
    .range_index_create_with_wal(
      key,
      storage_backend,
      TreeTuning {
        cache_size: cache_size as usize,
        min_record_size: min_record_size as usize,
        max_record_size: max_record_size as usize,
        max_key_len: max_key_len as usize,
        leaf_page_size: leaf_page_size.max(0) as usize,
      },
      |bytes| {
        let mut hist = ctx.repl.history.write();
        hist.replication_offset += bytes.len() as u64;
      },
    )
    .await
  {
    Ok(()) => buf.write_ok(),
    Err(RangeIndexError::AlreadyExists) => buf.write_error(b"ERR index already exists"),
    Err(RangeIndexError::WrongType) => buf.write_error(consts::err::WRONG_TYPE),
    Err(e) => buf.write_error_fmt(format_args!("{e}")),
  }

  Ok(())
}

/// RI.SET key field value
async fn handle_ri_set(
  ctx: &Arc<ServerContext>,
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.len() != 3 {
    buf.write_error(b"ERR wrong number of arguments for 'ri.set' command");
    return Ok(());
  }

  let key = args[0];
  let field = args[1];
  let val = args[2];

  match session
    .store_session
    .range_index_set_with_wal(key, field, val, |bytes| {
      let mut hist = ctx.repl.history.write();
      hist.replication_offset += bytes.len() as u64;
    })
    .await
  {
    Ok(()) => buf.write_ok(),
    Err(RangeIndexError::WrongType) => buf.write_error(consts::err::WRONG_TYPE),
    Err(RangeIndexError::NotFound) => buf.write_error(b"ERR range index not found"),
    Err(e) => buf.write_error_fmt(format_args!("{e}")),
  }

  Ok(())
}

/// RI.GET key field
async fn handle_ri_get(
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.len() != 2 {
    buf.write_error(b"ERR wrong number of arguments for 'ri.get' command");
    return Ok(());
  }

  let key = args[0];
  let field = args[1];

  match session.store_session.range_index_get(key, field).await {
    Ok(Some(val)) => buf.write_bulk_string(&val),
    Ok(None) => buf.write_null(),
    Err(RangeIndexError::WrongType) => buf.write_error(consts::err::WRONG_TYPE),
    Err(RangeIndexError::NotFound) => buf.write_error(b"ERR range index not found"),
    Err(e) => buf.write_error_fmt(format_args!("{e}")),
  }

  Ok(())
}

/// RI.DEL key field
async fn handle_ri_del(
  ctx: &Arc<ServerContext>,
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.len() != 2 {
    buf.write_error(b"ERR wrong number of arguments for 'ri.del' command");
    return Ok(());
  }

  let key = args[0];
  let field = args[1];

  match session
    .store_session
    .range_index_del_with_wal(key, field, |bytes| {
      let mut hist = ctx.repl.history.write();
      hist.replication_offset += bytes.len() as u64;
    })
    .await
  {
    Ok(_) => buf.write_integer(1),
    Err(RangeIndexError::WrongType) => buf.write_error(consts::err::WRONG_TYPE),
    Err(RangeIndexError::NotFound) => buf.write_error(b"ERR range index not found"),
    Err(e) => buf.write_error_fmt(format_args!("{e}")),
  }

  Ok(())
}

/// RI.SCAN key start COUNT n [FIELDS KEY|VALUE|BOTH]
async fn handle_ri_scan(
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.len() < 4 {
    buf.write_error(b"ERR wrong number of arguments for 'ri.scan' command");
    return Ok(());
  }

  let key = args[0];
  let start_key = args[1];

  if !args[2].eq_ignore_ascii_case(b"COUNT") {
    buf.write_error(b"ERR syntax error, expected COUNT");
    return Ok(());
  }

  let count = ParseUtils::try_read_long(args[3]).unwrap_or(0);
  if count <= 0 {
    buf.write_error(b"ERR invalid count");
    return Ok(());
  }

  let mut return_field = ScanReturnField::KeyAndValue;
  if args.len() >= 6 && args[4].eq_ignore_ascii_case(b"FIELDS") {
    let f = args[5];
    if f.eq_ignore_ascii_case(b"KEY") {
      return_field = ScanReturnField::Key;
    } else if f.eq_ignore_ascii_case(b"VALUE") {
      return_field = ScanReturnField::Value;
    }
  }

  match session
    .store_session
    .range_index_scan(key, start_key, count as usize, return_field)
    .await
  {
    Ok(records) => {
      buf.write_array_header(records.len());
      for rec in records {
        match return_field {
          ScanReturnField::Key => buf.write_bulk_string(&rec.key),
          ScanReturnField::Value => buf.write_bulk_string(&rec.value),
          ScanReturnField::KeyAndValue => {
            buf.write_array_header(2);
            buf.write_bulk_string(&rec.key);
            buf.write_bulk_string(&rec.value);
          }
        }
      }
    }
    Err(RangeIndexError::WrongType) => buf.write_error(consts::err::WRONG_TYPE),
    Err(RangeIndexError::NotFound) => buf.write_error(b"ERR range index not found"),
    Err(RangeIndexError::MemoryModeNotSupported) => {
      buf.write_error(b"ERR RI.SCAN is not supported for MEMORY-mode indexes");
    }
    Err(e) => buf.write_error_fmt(format_args!("{e}")),
  }

  Ok(())
}

/// RI.RANGE key start end [FIELDS KEY|VALUE|BOTH]
async fn handle_ri_range(
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.len() < 3 {
    buf.write_error(b"ERR wrong number of arguments for 'ri.range' command");
    return Ok(());
  }

  let key = args[0];
  let start_key = args[1];
  let end_key = args[2];

  let mut return_field = ScanReturnField::KeyAndValue;
  if args.len() >= 5 && args[3].eq_ignore_ascii_case(b"FIELDS") {
    let f = args[4];
    if f.eq_ignore_ascii_case(b"KEY") {
      return_field = ScanReturnField::Key;
    } else if f.eq_ignore_ascii_case(b"VALUE") {
      return_field = ScanReturnField::Value;
    }
  }

  match session
    .store_session
    .range_index_range(key, start_key, end_key, return_field)
    .await
  {
    Ok(records) => {
      buf.write_array_header(records.len());
      for rec in records {
        match return_field {
          ScanReturnField::Key => buf.write_bulk_string(&rec.key),
          ScanReturnField::Value => buf.write_bulk_string(&rec.value),
          ScanReturnField::KeyAndValue => {
            buf.write_array_header(2);
            buf.write_bulk_string(&rec.key);
            buf.write_bulk_string(&rec.value);
          }
        }
      }
    }
    Err(RangeIndexError::WrongType) => buf.write_error(consts::err::WRONG_TYPE),
    Err(RangeIndexError::NotFound) => buf.write_error(b"ERR range index not found"),
    Err(RangeIndexError::MemoryModeNotSupported) => {
      buf.write_error(b"ERR RI.RANGE is not supported for MEMORY-mode indexes");
    }
    Err(e) => buf.write_error_fmt(format_args!("{e}")),
  }

  Ok(())
}

/// RI.EXISTS key
async fn handle_ri_exists(
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.len() != 1 {
    buf.write_error(b"ERR wrong number of arguments for 'ri.exists' command");
    return Ok(());
  }

  let key = args[0];
  let exists = session
    .store_session
    .range_index_exists(key)
    .await
    .unwrap_or(false);

  buf.write_integer(if exists { 1 } else { 0 });
  Ok(())
}

/// RI.CONFIG key
async fn handle_ri_config(
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.len() != 1 {
    buf.write_error(b"ERR wrong number of arguments for 'ri.config' command");
    return Ok(());
  }

  let key = args[0];
  match session.store_session.range_index_config(key).await {
    Ok(stub) => {
      buf.write_array_header(12);

      let mut b = Buffer::new();

      buf.write_bulk_string(b"storage_backend");
      buf.write_bulk_string(if stub.storage_backend == 0 {
        b"DISK"
      } else {
        b"MEMORY"
      });

      buf.write_bulk_string(b"cache_size");
      buf.write_bulk_string(b.format(stub.cache_size).as_bytes());

      buf.write_bulk_string(b"min_record_size");
      buf.write_bulk_string(b.format(stub.min_record_size).as_bytes());

      buf.write_bulk_string(b"max_record_size");
      buf.write_bulk_string(b.format(stub.max_record_size).as_bytes());

      buf.write_bulk_string(b"max_key_len");
      buf.write_bulk_string(b.format(stub.max_key_len).as_bytes());

      buf.write_bulk_string(b"leaf_page_size");
      buf.write_bulk_string(b.format(stub.leaf_page_size).as_bytes());
    }
    Err(RangeIndexError::WrongType) => buf.write_error(consts::err::WRONG_TYPE),
    Err(RangeIndexError::NotFound) => buf.write_error(b"ERR range index not found"),
    Err(e) => buf.write_error_fmt(format_args!("{e}")),
  }

  Ok(())
}

/// RI.METRICS key
async fn handle_ri_metrics(
  session: &mut ServerSession,
  args: &SessionParseState<'_>,
  buf: &mut SendBuffer,
) -> Result<()> {
  if args.len() != 1 {
    buf.write_error(b"ERR wrong number of arguments for 'ri.metrics' command");
    return Ok(());
  }

  let key = args[0];
  match session.store_session.range_index_metrics(key).await {
    Ok((tree_handle, is_live, is_flushed, is_recovered)) => {
      buf.write_array_header(8);

      let mut b = Buffer::new();

      buf.write_bulk_string(b"tree_handle");
      buf.write_bulk_string(b.format(tree_handle).as_bytes());

      buf.write_bulk_string(b"is_live");
      buf.write_bulk_string(if is_live { b"true" } else { b"false" });

      buf.write_bulk_string(b"is_flushed");
      buf.write_bulk_string(if is_flushed { b"true" } else { b"false" });

      buf.write_bulk_string(b"is_recovered");
      buf.write_bulk_string(if is_recovered { b"true" } else { b"false" });
    }
    Err(RangeIndexError::WrongType) => buf.write_error(consts::err::WRONG_TYPE),
    Err(RangeIndexError::NotFound) => buf.write_error(b"ERR range index not found"),
    Err(e) => buf.write_error_fmt(format_args!("{e}")),
  }

  Ok(())
}
