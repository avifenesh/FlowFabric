//! Direct XREAD / XREAD BLOCK tail for attempt-scoped streams.
//!
//! XREAD BLOCK cannot run inside a Valkey Function (blocking commands are
//! rejected by `FUNCTION`), so tailing is implemented as a direct client
//! command against the same `ferriskey::Client` the server already holds.
//!
//! Semantics:
//! - `block_ms == 0` → non-blocking `XREAD` (returns immediately)
//! - `block_ms > 0`  → `XREAD BLOCK <ms>` (returns nil on timeout → empty result)
//!
//! Cluster-safe: the stream key carries the execution's `{p:N}` hash tag, so
//! XREAD routes to the single owning slot.
//!
//! # Timeout interaction with the ferriskey client
//!
//! The client's configured `request_timeout` (5s on the server default) does
//! NOT cap blocking tail calls. `ferriskey::client::get_request_timeout`
//! inspects every outgoing cmd; for `XREAD`/`XREADGROUP` with a `BLOCK`
//! argument, it returns `RequestTimeoutOption::BlockingCommand(block_ms +
//! 500ms)` via `BLOCKING_CMD_TIMEOUT_EXTENSION`. That means a
//! `block_ms = 30_000` call gets a 30_500ms effective timeout regardless of
//! the client's default. No manual override is needed from this module.
//!
//! # Terminal signal (closed-stream propagation)
//!
//! After every XREAD/XREAD BLOCK we HMGET the sibling `stream_meta` hash
//! to learn whether the producer has closed the stream (`closed_at` /
//! `closed_reason`). This is the terminal-signal contract described in
//! RFC-006 — consumers poll `tail_stream` until `is_closed()` is true,
//! then drain any remaining frames.
//!
//! # Atomicity (XREAD vs HMGET)
//!
//! The XREAD and the follow-up HMGET are separate Valkey round trips. A
//! close can land between them, so a caller can observe a result that
//! was not simultaneously true at any single instant: `frames` reflect
//! the stream as of T1, `closed_at`/`closed_reason` reflect
//! `stream_meta` as of T2 > T1.
//!
//! This is correctness-safe. `ff_append_frame` gates on `closed_at`
//! (see `lua/stream.lua`), so once the stream is closed no new frames
//! can be appended — any frames returned by XREAD are guaranteed to be
//! from before the close. The terminal signal arriving "slightly later"
//! just means a tail loop may take one extra iteration to observe the
//! closure. Consumers should treat `closed_at.is_some()` as the exit
//! condition and perform a final `read_attempt_stream` drain from
//! `last_seen_id` to `+` to pick up any frames that landed before close
//! but after the tail's last read — see RFC-006 Impl Notes
//! §"Terminal-signal drain pattern".

use ff_core::contracts::{StreamFrame, StreamFrames, STREAM_READ_HARD_CAP};
use ff_core::types::TimestampMs;
use ferriskey::{Client, Value};

use crate::error::ScriptError;
use crate::functions::stream::{parse_entries, parse_fields_kv, value_to_string, FieldShape};

/// Tail frames from `stream_key`, blocking up to `block_ms` (0 = no block).
///
/// `last_id` is the exclusive cursor — XREAD returns entries with id > last_id.
/// Pass `"0-0"` (or `"0"`) to read from the beginning.
///
/// Returns a [`StreamFrames`] with `frames=Vec::new()` on timeout or when
/// the stream has no new entries. The `closed_at`/`closed_reason` fields
/// are populated from `stream_meta` on every call so consumers can stop
/// polling when the producer finalizes the stream.
///
/// `count_limit` must be `>= 1` and `<= STREAM_READ_HARD_CAP`. This
/// mirrors [`crate::functions::stream::ff_read_attempt_stream`] and the
/// REST/SDK boundaries; callers that silently clamped to 0 previously must
/// now pass `STREAM_READ_HARD_CAP` explicitly.
///
/// # Timeout handling
///
/// For blocking calls (`block_ms > 0`), the ferriskey client automatically
/// extends its `request_timeout` to `block_ms + 500ms` for the duration of
/// this command. Callers do not need to (and should not) pass a custom
/// client with a larger `request_timeout` just to accommodate tail. See the
/// module-level docs for the exact ferriskey code path.
pub async fn xread_block(
    client: &Client,
    stream_key: &str,
    stream_meta_key: &str,
    last_id: &str,
    block_ms: u64,
    count_limit: u64,
) -> Result<StreamFrames, ScriptError> {
    // These are input-validation errors — `InvalidInput` is the right
    // class (Terminal per `ScriptError::class()`). `Parse` is reserved for
    // FCALL-result parse failures per the contract, so using it here
    // would mislead callers that route retry decisions off the variant.
    if count_limit == 0 {
        return Err(ScriptError::InvalidInput(
            "xread_block: count_limit must be >= 1".into(),
        ));
    }
    if count_limit > STREAM_READ_HARD_CAP {
        return Err(ScriptError::InvalidInput(format!(
            "xread_block: count_limit exceeds STREAM_READ_HARD_CAP ({STREAM_READ_HARD_CAP})"
        )));
    }
    debug_assert!(
        STREAM_READ_HARD_CAP as i128 <= i64::MAX as i128,
        "STREAM_READ_HARD_CAP must fit in i64 — RESP COUNT arg"
    );

    let cmd = client.cmd("XREAD").arg("COUNT").arg(count_limit);
    let cmd = if block_ms > 0 {
        cmd.arg("BLOCK").arg(block_ms)
    } else {
        cmd
    };
    let cmd = cmd.arg("STREAMS").arg(stream_key).arg(last_id);

    let raw: Value = cmd.execute().await.map_err(ScriptError::Valkey)?;

    let frames = parse_xread_reply(&raw, stream_key)?;

    // Fetch terminal markers separately. HMGET on a missing key returns
    // [nil, nil] which normalizes to (None, None) — an in-progress /
    // never-written attempt is indistinguishable from "still open" here,
    // which is the intended semantics.
    let (closed_at, closed_reason) = fetch_closed_meta(client, stream_meta_key).await?;

    Ok(StreamFrames { frames, closed_at, closed_reason })
}

async fn fetch_closed_meta(
    client: &Client,
    stream_meta_key: &str,
) -> Result<(Option<TimestampMs>, Option<String>), ScriptError> {
    let values: Vec<Option<String>> = client
        .cmd("HMGET")
        .arg(stream_meta_key)
        .arg("closed_at")
        .arg("closed_reason")
        .execute()
        .await
        .map_err(ScriptError::Valkey)?;

    let closed_at = values
        .first()
        .and_then(|v| v.as_deref())
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse::<i64>().ok())
        .map(TimestampMs::from_millis);
    let closed_reason = values
        .get(1)
        .and_then(|v| v.clone())
        .filter(|s| !s.is_empty());
    Ok((closed_at, closed_reason))
}

/// Parse an XREAD reply into frames for a single stream.
///
/// Handles every shape ferriskey can produce:
/// - `Value::Nil` on BLOCK timeout or no new entries → `Vec::new()`
/// - `Value::Map({stream_key: Map({entry_id: fields, ...})})` — RESP3
/// - `Value::Map({stream_key: Array([[entry_id, fields], ...])})` — mixed
/// - `Value::Array([[stream_key, [[entry_id, fields], ...]], ...])` — RESP2
///
/// Per-entry field payloads are always decoded with `FieldShape::Pairs`
/// because ferriskey's XREAD adapter (`ArrayOfPairs`) is unconditional for
/// the XREAD command family — see `ferriskey::client::value_conversion`.
fn parse_xread_reply(raw: &Value, stream_key: &str) -> Result<Vec<StreamFrame>, ScriptError> {
    let outer = match raw {
        Value::Nil => return Ok(Vec::new()),
        Value::Map(m) => m,
        // RESP2 fallback: array of [stream_key, entries] pairs.
        Value::Array(arr) => {
            let mut non_match_count: usize = 0;
            for entry in arr {
                let Ok(Value::Array(pair)) = entry.as_ref() else {
                    non_match_count += 1;
                    continue;
                };
                if pair.len() != 2 {
                    non_match_count += 1;
                    continue;
                }
                let matches_key = match pair[0].as_ref() {
                    Ok(Value::BulkString(b)) => b.as_ref() == stream_key.as_bytes(),
                    Ok(Value::SimpleString(s)) => s == stream_key,
                    _ => false,
                };
                if !matches_key {
                    non_match_count += 1;
                    continue;
                }
                let entries_val = match pair[1].as_ref() {
                    Ok(v) => v,
                    Err(e) => {
                        return Err(ScriptError::Parse(format!(
                            "XREAD entries (RESP2): {e}"
                        )));
                    }
                };
                return parse_entries_any(entries_val);
            }
            if non_match_count > 0 {
                tracing::trace!(
                    non_match = non_match_count,
                    stream_key,
                    "XREAD RESP2 reply had entries but none matched the requested stream key"
                );
            }
            return Ok(Vec::new());
        }
        other => {
            return Err(ScriptError::Parse(format!(
                "XREAD: expected Map/Nil/Array, got {other:?}"
            )));
        }
    };

    let mut non_match_count: usize = 0;
    for (k, v) in outer.iter() {
        let matches_key = match k {
            Value::BulkString(b) => b.as_ref() == stream_key.as_bytes(),
            Value::SimpleString(s) => s == stream_key,
            _ => false,
        };
        if !matches_key {
            non_match_count += 1;
            continue;
        }
        let frames = match v {
            // ferriskey's XREAD adapter (`Map { key: BulkString, value: Map
            // { key: BulkString, value: ArrayOfPairs }}`) produces a
            // `Map<entry_id, ArrayOfPairs-fields>` here — keys lift to Map
            // but field payloads retain the Pairs shape.
            Value::Map(entries) => {
                let mut frames = Vec::with_capacity(entries.len());
                for (id_v, field_v) in entries {
                    let id = value_to_string(Some(id_v))
                        .ok_or_else(|| ScriptError::Parse("XREAD entry: bad id".into()))?;
                    let fields = parse_fields_kv(field_v, FieldShape::Pairs)?;
                    frames.push(StreamFrame { id, fields });
                }
                frames
            }
            Value::Array(arr) => {
                if arr.is_empty() {
                    tracing::trace!(
                        stream_key,
                        "XREAD reply matched stream key but entries Array was empty — possible \
                         malformed reply"
                    );
                }
                parse_entries_any(v)?
            }
            Value::Nil => Vec::new(),
            other => {
                return Err(ScriptError::Parse(format!(
                    "XREAD entries: expected Map/Array, got {other:?}"
                )));
            }
        };
        return Ok(frames);
    }
    if non_match_count > 0 {
        tracing::trace!(
            non_match = non_match_count,
            stream_key,
            "XREAD Map reply had entries but none matched the requested stream key"
        );
    }
    Ok(Vec::new())
}

/// Parse an Array-of-entries (or Map-of-entries) payload. Used for the
/// inner XREAD entry list — field shape is always `Pairs` because that's
/// what ferriskey's `ArrayOfPairs` XREAD adapter emits even when the outer
/// stream/entry wrappers are lifted to `Map`.
fn parse_entries_any(raw: &Value) -> Result<Vec<StreamFrame>, ScriptError> {
    match raw {
        Value::Nil => Ok(Vec::new()),
        Value::Array(_) => parse_entries(raw, FieldShape::Pairs),
        Value::Map(map) => {
            let mut frames = Vec::with_capacity(map.len());
            for (id_v, field_v) in map {
                let id = value_to_string(Some(id_v))
                    .ok_or_else(|| ScriptError::Parse("XREAD entry: bad id".into()))?;
                let fields = parse_fields_kv(field_v, FieldShape::Pairs)?;
                frames.push(StreamFrame { id, fields });
            }
            Ok(frames)
        }
        other => Err(ScriptError::Parse(format!(
            "XREAD entries: expected Array/Map/Nil, got {other:?}"
        ))),
    }
}
