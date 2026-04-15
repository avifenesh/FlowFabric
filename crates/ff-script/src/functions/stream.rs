//! Typed FCALL wrapper for stream append function (lua/stream.lua).

use ff_core::contracts::*;
use ff_core::error::ScriptError;
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
            .map_err(|e| ScriptError::Parse(format!("bad frame_count: {e}")))?;
        Ok(AppendFrameResult::Appended {
            entry_id,
            frame_count,
        })
    }
}
