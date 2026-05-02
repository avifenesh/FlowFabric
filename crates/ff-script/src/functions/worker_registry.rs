//! Typed FCALL wrappers for RFC-025 worker-registry functions
//! (`lua/scheduling.lua` → `ff_register_worker`, `ff_heartbeat_worker`).
//!
//! `mark_worker_dead` + `list_workers` + `list_expired_leases` stay as
//! direct commands on the Valkey backend; only `ff_register_worker`
//! and `ff_heartbeat_worker` fold multi-round-trip paths into a single
//! Lua round trip.

use ferriskey::Client;
use ff_core::contracts::{HeartbeatWorkerOutcome, RegisterWorkerOutcome};
use ff_core::types::TimestampMs;

use crate::error::ScriptError;
use crate::result::{FcallResult, FromFcallResult};

impl FromFcallResult for RegisterWorkerOutcome {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        // Lua convention (RFC-010 §4.9): ok("registered") returns
        // `{1, "OK", "registered"}`. `status` = "OK"; the variant tag
        // lives in `fields[0]` (elem[2] of the raw array).
        let r = FcallResult::parse(raw)?.into_success()?;
        let tag = r.field_str(0);
        match tag.as_str() {
            "registered" => Ok(RegisterWorkerOutcome::Registered),
            "refreshed" => Ok(RegisterWorkerOutcome::Refreshed),
            other => Err(ScriptError::Parse {
                fcall: "ff_register_worker".into(),
                execution_id: None,
                message: format!("unexpected outcome tag: {other:?}"),
            }),
        }
    }
}

/// ARGV bundle for [`ff_register_worker`]. All strings are sent
/// verbatim to Valkey; callers are responsible for sorting/dedup of
/// lanes_csv + caps_csv (RFC-025 §5.1 wire shape).
pub struct RegisterWorkerArgv<'a> {
    pub instance_id: &'a str,
    pub worker_id: &'a str,
    pub lanes_csv: &'a str,
    pub caps_csv: &'a str,
    pub ttl_ms: u64,
    pub now_ms: i64,
}

/// KEYS bundle for [`ff_register_worker`]. Order matches the Lua
/// body: `alive`, `caps`, `index` (all namespace-prefixed post-§9.1).
pub struct RegisterWorkerKeys<'a> {
    pub alive_key: &'a str,
    pub caps_key: &'a str,
    pub index_key: &'a str,
}

/// Call `ff_register_worker`. Single FCALL: SET PX alive + HSET caps
/// + PEXPIRE caps + SADD index, returning [`RegisterWorkerOutcome`].
pub async fn ff_register_worker(
    conn: &Client,
    keys: RegisterWorkerKeys<'_>,
    argv: RegisterWorkerArgv<'_>,
) -> Result<RegisterWorkerOutcome, ScriptError> {
    let key_refs: [&str; 3] = [keys.alive_key, keys.caps_key, keys.index_key];
    let ttl = argv.ttl_ms.to_string();
    let now = argv.now_ms.to_string();
    let argv_refs: [&str; 6] = [
        argv.instance_id,
        argv.worker_id,
        argv.lanes_csv,
        argv.caps_csv,
        ttl.as_str(),
        now.as_str(),
    ];

    let raw = conn
        .fcall::<ferriskey::Value>("ff_register_worker", &key_refs, &argv_refs)
        .await
        .map_err(ScriptError::Valkey)?;
    RegisterWorkerOutcome::from_fcall_result(&raw)
}

/// Parsed reply shape for [`ff_heartbeat_worker`]. The Lua body
/// returns `ok("not_registered")` or `ok("refreshed", <ttl_ms>)`.
/// `next_expiry_ms` = `now_ms + ttl_ms` is derived in the Rust wrapper,
/// not in Lua (keeps the FCALL narrow — the Lua layer has no reason to
/// know the caller's clock semantics).
pub struct HeartbeatWorkerReply {
    pub outcome_tag: HeartbeatReplyTag,
}

pub enum HeartbeatReplyTag {
    NotRegistered,
    /// `ttl_ms` parsed from `fields[0]`; wrapper converts to
    /// `next_expiry_ms` using the caller-supplied `now_ms`.
    Refreshed { ttl_ms: u64 },
}

impl FromFcallResult for HeartbeatWorkerReply {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        let tag = r.field_str(0);
        match tag.as_str() {
            "not_registered" => Ok(HeartbeatWorkerReply {
                outcome_tag: HeartbeatReplyTag::NotRegistered,
            }),
            "refreshed" => {
                let ttl_str = r.field_str(1);
                let ttl_ms: u64 = ttl_str.parse().map_err(|e| ScriptError::Parse {
                    fcall: "ff_heartbeat_worker".into(),
                    execution_id: None,
                    message: format!("refreshed: ttl_ms not a u64 ({ttl_str:?}): {e}"),
                })?;
                Ok(HeartbeatWorkerReply {
                    outcome_tag: HeartbeatReplyTag::Refreshed { ttl_ms },
                })
            }
            other => Err(ScriptError::Parse {
                fcall: "ff_heartbeat_worker".into(),
                execution_id: None,
                message: format!("unexpected outcome tag: {other:?}"),
            }),
        }
    }
}

/// KEYS bundle for [`ff_heartbeat_worker`]. Order matches the Lua
/// body: `alive`, `caps`. Both are namespace-prefixed.
pub struct HeartbeatWorkerKeys<'a> {
    pub alive_key: &'a str,
    pub caps_key: &'a str,
}

/// Call `ff_heartbeat_worker`. Single FCALL covering HGET ttl_ms, PEXPIRE
/// alive, PEXPIRE caps, and HSET last_heartbeat_ms. Returns the typed
/// [`HeartbeatWorkerOutcome`] with `next_expiry_ms` derived from the supplied
/// now_ms and Lua-returned ttl_ms.
pub async fn ff_heartbeat_worker(
    conn: &Client,
    keys: HeartbeatWorkerKeys<'_>,
    now_ms: i64,
) -> Result<HeartbeatWorkerOutcome, ScriptError> {
    let key_refs: [&str; 2] = [keys.alive_key, keys.caps_key];
    let now = now_ms.to_string();
    let argv_refs: [&str; 1] = [now.as_str()];

    let raw = conn
        .fcall::<ferriskey::Value>("ff_heartbeat_worker", &key_refs, &argv_refs)
        .await
        .map_err(ScriptError::Valkey)?;
    let reply = HeartbeatWorkerReply::from_fcall_result(&raw)?;
    match reply.outcome_tag {
        HeartbeatReplyTag::NotRegistered => Ok(HeartbeatWorkerOutcome::NotRegistered),
        HeartbeatReplyTag::Refreshed { ttl_ms } => {
            let next_expiry_ms = TimestampMs::from_millis(now_ms.saturating_add(ttl_ms as i64));
            Ok(HeartbeatWorkerOutcome::Refreshed { next_expiry_ms })
        }
    }
}
