//! Typed FCALL wrapper for stream append function (lua/stream.lua).

use ff_core::contracts::*;
use crate::error::ScriptError;
use ff_core::keys::ExecKeyContext;

use crate::result::{FcallResult, FromFcallResult};

/// Key context for stream operations — only needs exec keys (no indexes).
pub struct StreamOpKeys<'a> {
    pub ctx: &'a ExecKeyContext,
}

// ─── ff_append_frame ──────────────────────────────────────────────────
//
// Lua KEYS (3): exec_core, stream_data, stream_meta
// Lua ARGV (13): execution_id, attempt_index, lease_id, lease_epoch,
//                frame_type, ts, payload, encoding, correlation_id,
//                source, retention_maxlen, attempt_id, max_payload_bytes

ff_function! {
    pub ff_append_frame(args: AppendFrameArgs) -> AppendFrameResult {
        keys(k: &StreamOpKeys<'_>) {
            k.ctx.core(),                                  // 1
            k.ctx.stream(args.attempt_index),              // 2
            k.ctx.stream_meta(args.attempt_index),         // 3
        }
        argv {
            args.execution_id.to_string(),                 // 1
            args.attempt_index.to_string(),                // 2
            args.lease_id.to_string(),                     // 3
            args.lease_epoch.to_string(),                  // 4
            args.frame_type.clone(),                       // 5
            args.timestamp.to_string(),                    // 6
            String::from_utf8_lossy(&args.payload).into_owned(), // 7
            args.encoding.clone().unwrap_or_else(|| "utf8".into()), // 8
            args.correlation_id.clone().unwrap_or_default(), // 9
            args.source.clone().unwrap_or_else(|| "worker".into()), // 10
            args.retention_maxlen.unwrap_or(0).to_string(), // 11
            args.attempt_id.to_string(),                   // 12
            args.max_payload_bytes.unwrap_or(65536).to_string(), // 13
        }
    }
}

impl FromFcallResult for AppendFrameResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // ok(entry_id, frame_count)
        let entry_id = r.field_str(0);
        let frame_count = r.field_str(1).parse::<u64>()
            .map_err(|e| ScriptError::Parse {
                fcall: "ff_append_frame".into(),
                execution_id: None,
                message: format!("bad frame_count: {e}"),
            })?;
        Ok(AppendFrameResult::Appended {
            entry_id,
            frame_count,
        })
    }
}

// ─── ff_read_attempt_stream ───────────────────────────────────────────
//
// Lua KEYS (2): stream_data, stream_meta
// Lua ARGV (3): from_id, to_id, count_limit

ff_function! {
    pub ff_read_attempt_stream(args: ReadFramesArgs) -> ReadFramesResult {
        keys(k: &StreamOpKeys<'_>) {
            k.ctx.stream(args.attempt_index),              // 1
            k.ctx.stream_meta(args.attempt_index),         // 2
        }
        argv {
            args.from_id.clone(),                          // 1
            args.to_id.clone(),                            // 2
            args.count_limit.to_string(),                  // 3
        }
    }
}

impl FromFcallResult for ReadFramesResult {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        // Lua returns `ok(entries, closed_at, closed_reason)`.
        // - entries: array of [entry_id, [f1, v1, ...]]
        // - closed_at: string (ms timestamp) or "" if open
        // - closed_reason: string or "" if open
        let entries = r.fields.first().ok_or_else(|| ScriptError::Parse {
            fcall: "ff_read_attempt_stream".into(),
            execution_id: None,
            message: "missing entries field".into(),
        })?;
        // XRANGE from a Valkey Function passes fields through as raw Lua
        // arrays — no ArrayOfPairs conversion, so always Flat here.
        let frames = parse_entries(entries, FieldShape::Flat)?;

        let closed_at = r.field_str(1);
        let closed_at = if closed_at.is_empty() {
            None
        } else {
            closed_at
                .parse::<i64>()
                .ok()
                .map(ff_core::types::TimestampMs::from_millis)
        };

        let closed_reason = r.field_str(2);
        let closed_reason = if closed_reason.is_empty() {
            None
        } else {
            Some(closed_reason)
        };

        Ok(ReadFramesResult::Frames(StreamFrames {
            frames,
            closed_at,
            closed_reason,
        }))
    }
}

/// Explicit shape tag for stream-entry field payloads. Passed from the
/// caller because each code path *knows* which shape ferriskey will emit —
/// heuristic detection ("first entry looks like a 2-elem array") can
/// mis-classify when field values happen to be arrays themselves.
#[derive(Clone, Copy, Debug)]
pub enum FieldShape {
    /// Flat alternating `[k, v, k, v, ...]`. Emitted by raw Lua FCALL
    /// replies and by RESP2 servers that bypass the ArrayOfPairs adapter.
    Flat,
    /// `[[k, v], [k, v], ...]` — ferriskey's `ArrayOfPairs` adapter for
    /// XRANGE / XREAD direct commands.
    Pairs,
    /// RESP3 `{k: v, ...}` map.
    Map,
}

/// Parse an XRANGE/XREAD-shaped `[entry_id, fields]` list.
///
/// Caller must tell us the field shape — see [`FieldShape`]. Inferring
/// shape from data alone is unsound: any call site always knows whether
/// it's reading a raw Lua payload (Flat), a ferriskey-adapted XRANGE/XREAD
/// reply (Pairs), or a RESP3 Map.
///
/// Nil fields → an empty frame (no fields).
pub(crate) fn parse_entries(
    raw: &ferriskey::Value,
    shape: FieldShape,
) -> Result<Vec<StreamFrame>, ScriptError> {
    let entries = match raw {
        ferriskey::Value::Array(arr) => arr,
        ferriskey::Value::Nil => return Ok(Vec::new()),
        other => {
            return Err(ScriptError::Parse {
                fcall: "parse_entries".into(),
                execution_id: None,
                message: format!("XRANGE/XREAD entries: expected Array, got {other:?}"),
            });
        }
    };

    let mut frames = Vec::with_capacity(entries.len());
    for entry in entries.iter() {
        let entry = entry.as_ref().map_err(|e| ScriptError::Parse {
            fcall: "parse_entries".into(),
            execution_id: None,
            message: format!("XRANGE entry error: {e}"),
        })?;
        let parts = match entry {
            ferriskey::Value::Array(a) => a,
            other => {
                return Err(ScriptError::Parse {
                    fcall: "parse_entries".into(),
                    execution_id: None,
                    message: format!("XRANGE entry: expected Array, got {other:?}"),
                });
            }
        };
        if parts.len() != 2 {
            return Err(ScriptError::Parse {
                fcall: "parse_entries".into(),
                execution_id: None,
                message: format!("XRANGE entry: expected 2 elements, got {}", parts.len()),
            });
        }
        let id = value_to_string(parts[0].as_ref().ok()).ok_or_else(|| ScriptError::Parse {
            fcall: "parse_entries".into(),
            execution_id: None,
            message: "XRANGE entry: missing/invalid id".into(),
        })?;

        let field_val = match parts[1].as_ref() {
            Ok(v) => v,
            Err(e) => {
                return Err(ScriptError::Parse {
                    fcall: "parse_entries".into(),
                    execution_id: None,
                    message: format!("XRANGE entry fields error: {e}"),
                });
            }
        };
        let fields = parse_fields_kv(field_val, shape)?;
        frames.push(StreamFrame { id, fields });
    }
    Ok(frames)
}

/// Parse a stream-entry field payload into a sorted map, using an explicit
/// [`FieldShape`] tag supplied by the caller.
///
/// `Value::Nil` always yields an empty map (entry exists but has no
/// fields) regardless of `shape` — both XRANGE and XREAD can produce this
/// for empty writes.
pub(crate) fn parse_fields_kv(
    v: &ferriskey::Value,
    shape: FieldShape,
) -> Result<std::collections::BTreeMap<String, String>, ScriptError> {
    let mut out = std::collections::BTreeMap::new();
    if matches!(v, ferriskey::Value::Nil) {
        return Ok(out);
    }
    match shape {
        FieldShape::Flat => {
            let arr = match v {
                ferriskey::Value::Array(arr) => arr,
                other => {
                    return Err(ScriptError::Parse {
                        fcall: "parse_fields_kv".into(),
                        execution_id: None,
                        message: format!(
                            "stream fields (Flat): expected Array, got {other:?}"
                        ),
                    });
                }
            };
            if !arr.len().is_multiple_of(2) {
                return Err(ScriptError::Parse {
                    fcall: "parse_fields_kv".into(),
                    execution_id: None,
                    message: format!(
                        "stream fields (Flat): odd element count {}",
                        arr.len()
                    ),
                });
            }
            let mut i = 0;
            while i < arr.len() {
                let k = value_to_string(arr[i].as_ref().ok())
                    .ok_or_else(|| ScriptError::Parse {
                        fcall: "parse_fields_kv".into(),
                        execution_id: None,
                        message: "stream field: bad key".into(),
                    })?;
                let val = value_to_string(arr[i + 1].as_ref().ok()).unwrap_or_default();
                out.insert(k, val);
                i += 2;
            }
        }
        FieldShape::Pairs => {
            let arr = match v {
                ferriskey::Value::Array(arr) => arr,
                other => {
                    return Err(ScriptError::Parse {
                        fcall: "parse_fields_kv".into(),
                        execution_id: None,
                        message: format!(
                            "stream fields (Pairs): expected Array, got {other:?}"
                        ),
                    });
                }
            };
            for pair in arr.iter() {
                let inner = match pair.as_ref() {
                    Ok(ferriskey::Value::Array(inner)) => inner,
                    _ => {
                        return Err(ScriptError::Parse {
                            fcall: "parse_fields_kv".into(),
                            execution_id: None,
                            message: "stream fields (Pairs): expected 2-element Array per entry"
                                .into(),
                        });
                    }
                };
                if inner.len() != 2 {
                    return Err(ScriptError::Parse {
                        fcall: "parse_fields_kv".into(),
                        execution_id: None,
                        message: format!(
                            "stream fields (Pairs): expected len=2, got {}",
                            inner.len()
                        ),
                    });
                }
                let k = value_to_string(inner[0].as_ref().ok())
                    .ok_or_else(|| ScriptError::Parse {
                        fcall: "parse_fields_kv".into(),
                        execution_id: None,
                        message: "stream field: bad key".into(),
                    })?;
                let val = value_to_string(inner[1].as_ref().ok()).unwrap_or_default();
                out.insert(k, val);
            }
        }
        FieldShape::Map => {
            let pairs = match v {
                ferriskey::Value::Map(pairs) => pairs,
                other => {
                    return Err(ScriptError::Parse {
                        fcall: "parse_fields_kv".into(),
                        execution_id: None,
                        message: format!(
                            "stream fields (Map): expected Map, got {other:?}"
                        ),
                    });
                }
            };
            for (k, vv) in pairs {
                let key = value_to_string(Some(k))
                    .ok_or_else(|| ScriptError::Parse {
                        fcall: "parse_fields_kv".into(),
                        execution_id: None,
                        message: "stream field: bad key".into(),
                    })?;
                let val = value_to_string(Some(vv)).unwrap_or_default();
                out.insert(key, val);
            }
        }
    }
    Ok(out)
}

pub(crate) fn value_to_string(v: Option<&ferriskey::Value>) -> Option<String> {
    match v? {
        ferriskey::Value::BulkString(b) => Some(String::from_utf8_lossy(b).into_owned()),
        ferriskey::Value::SimpleString(s) => Some(s.clone()),
        ferriskey::Value::Int(n) => Some(n.to_string()),
        ferriskey::Value::Okay => Some("OK".into()),
        _ => None,
    }
}
