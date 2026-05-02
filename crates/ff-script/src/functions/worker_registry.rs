//! Typed FCALL wrappers for RFC-025 worker-registry functions
//! (`lua/scheduling.lua` → `ff_register_worker`).
//!
//! `heartbeat_worker` + `mark_worker_dead` + `list_workers` +
//! `list_expired_leases` stay as direct commands on the Valkey
//! backend; only `ff_register_worker` needs Lua-side atomicity.

use ferriskey::Client;
use ff_core::contracts::RegisterWorkerOutcome;

use crate::error::ScriptError;
use crate::result::{FcallResult, FromFcallResult};

impl FromFcallResult for RegisterWorkerOutcome {
    fn from_fcall_result(raw: &ferriskey::Value) -> Result<Self, ScriptError> {
        let r = FcallResult::parse(raw)?.into_success()?;
        match r.status.as_str() {
            "registered" => Ok(RegisterWorkerOutcome::Registered),
            "refreshed" => Ok(RegisterWorkerOutcome::Refreshed),
            other => Err(ScriptError::Parse {
                fcall: "ff_register_worker".into(),
                execution_id: None,
                message: format!("unexpected status: {other}"),
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
