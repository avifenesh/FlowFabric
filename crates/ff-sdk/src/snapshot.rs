//! Typed read-models that decouple consumers from FF's storage engine.
//!
//! Today the only call here is [`FlowFabricWorker::describe_execution`],
//! which replaces the HGETALL-on-exec_core pattern downstream consumers
//! rely on with a typed snapshot. The engine remains free to rename
//! hash fields or restructure keys under this surface — see issue #58
//! for the strategic context (decoupling ahead of a Postgres backend
//! port).
//!
//! Snapshot types (`ExecutionSnapshot`, `AttemptSummary`, `LeaseSummary`)
//! live in [`ff_core::contracts`] so non-SDK consumers (tests, REST
//! server, future alternate backends) can share them without depending
//! on ff-sdk.

use std::collections::{BTreeMap, HashMap};

use ff_core::contracts::{AttemptSummary, ExecutionSnapshot, LeaseSummary};
use ff_core::keys::ExecKeyContext;
use ff_core::partition::execution_partition;
use ff_core::state::PublicState;
use ff_core::types::{
    AttemptId, AttemptIndex, ExecutionId, FlowId, LaneId, LeaseEpoch, Namespace, TimestampMs,
    WaitpointId, WorkerInstanceId,
};

use crate::worker::FlowFabricWorker;
use crate::SdkError;

impl FlowFabricWorker {
    /// Read a typed snapshot of one execution.
    ///
    /// Returns `Ok(None)` when no execution exists at `id` (exec_core
    /// hash absent). Returns `Ok(Some(snapshot))` on success. Errors
    /// propagate Valkey transport faults and decode failures.
    ///
    /// # Consistency
    ///
    /// The snapshot is assembled from two pipelined `HGETALL`s — one
    /// for `exec_core`, one for the sibling tags hash — issued in a
    /// single round trip against the partition holding the execution.
    /// The two reads share a hash-tag so they always land on the same
    /// Valkey slot in cluster mode. They are NOT MULTI/EXEC-atomic:
    /// a concurrent FCALL that HSETs both keys can interleave, and a
    /// caller may observe exec_core fields from epoch `N+1` alongside
    /// tags from epoch `N` (or vice versa). This matches the
    /// last-write-wins-per-field semantics every existing HGETALL
    /// consumer already assumes.
    ///
    /// # Field semantics
    ///
    /// * `public_state` is the engine-maintained derived label written
    ///   atomically by every state-mutating FCALL. Parsed from the
    ///   snake_case string stored on exec_core.
    /// * `blocking_reason` / `blocking_detail` — `None` when the
    ///   exec_core field is the empty string (cleared on transition).
    /// * `current_attempt` — `None` before the first claim (exec_core
    ///   `current_attempt_id` empty).
    /// * `current_lease` — `None` when no lease is held (typical for
    ///   terminal, suspended, or pre-claim executions).
    /// * `current_waitpoint` — `None` unless an active suspension has
    ///   pinned a waitpoint id.
    /// * `tags` — empty map if the tags hash is absent (common for
    ///   executions created without `tags_json`).
    pub async fn describe_execution(
        &self,
        id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, SdkError> {
        let partition = execution_partition(id, self.partition_config());
        let ctx = ExecKeyContext::new(&partition, id);
        let core_key = ctx.core();
        let tags_key = ctx.tags();

        // Pipeline the two HGETALLs in one round trip. The two keys
        // share `{fp:N}` so cluster mode routes them to the same slot.
        let mut pipe = self.client().pipeline();
        let core_slot = pipe
            .cmd::<HashMap<String, String>>("HGETALL")
            .arg(&core_key)
            .finish();
        let tags_slot = pipe
            .cmd::<HashMap<String, String>>("HGETALL")
            .arg(&tags_key)
            .finish();
        pipe.execute().await.map_err(|e| SdkError::ValkeyContext {
            source: e,
            context: "describe_execution: pipeline HGETALL exec_core + tags".into(),
        })?;

        let core = core_slot.value().map_err(|e| SdkError::ValkeyContext {
            source: e,
            context: "describe_execution: decode HGETALL exec_core".into(),
        })?;
        if core.is_empty() {
            return Ok(None);
        }
        let tags_raw = tags_slot.value().map_err(|e| SdkError::ValkeyContext {
            source: e,
            context: "describe_execution: decode HGETALL tags".into(),
        })?;

        build_execution_snapshot(id.clone(), &core, tags_raw)
    }
}

/// Assemble an [`ExecutionSnapshot`] from the raw HGETALL field maps.
///
/// Kept as a free function so a future unit test can feed synthetic
/// maps without a live Valkey.
fn build_execution_snapshot(
    execution_id: ExecutionId,
    core: &HashMap<String, String>,
    tags_raw: HashMap<String, String>,
) -> Result<Option<ExecutionSnapshot>, SdkError> {
    let public_state = parse_public_state(opt_str(core, "public_state").unwrap_or(""))?;

    let lane_id_str = opt_str(core, "lane_id").unwrap_or("").to_owned();
    let lane_id = LaneId::new(lane_id_str);

    let namespace_str = opt_str(core, "namespace").unwrap_or("").to_owned();
    let namespace = Namespace::new(namespace_str);

    let flow_id = opt_str(core, "flow_id")
        .filter(|s| !s.is_empty())
        .map(|s| {
            FlowId::parse(s).map_err(|e| {
                SdkError::Config(format!(
                    "describe_execution: exec_core.flow_id is not a valid UUID \
                     (key corruption?): {e}"
                ))
            })
        })
        .transpose()?;

    let blocking_reason = opt_str(core, "blocking_reason")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let blocking_detail = opt_str(core, "blocking_detail")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let created_at = parse_ts(core, "created_at")?.unwrap_or(TimestampMs::from_millis(0));
    let last_mutation_at =
        parse_ts(core, "last_mutation_at")?.unwrap_or(TimestampMs::from_millis(0));

    let total_attempt_count: u32 = opt_str(core, "total_attempt_count")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let current_attempt = build_attempt_summary(core)?;
    let current_lease = build_lease_summary(core)?;

    let current_waitpoint = opt_str(core, "current_waitpoint_id")
        .filter(|s| !s.is_empty())
        .map(|s| {
            WaitpointId::parse(s).map_err(|e| {
                SdkError::Config(format!(
                    "describe_execution: exec_core.current_waitpoint_id is not a \
                     valid UUID (key corruption?): {e}"
                ))
            })
        })
        .transpose()?;

    let tags: BTreeMap<String, String> = tags_raw.into_iter().collect();

    Ok(Some(ExecutionSnapshot::new(
        execution_id,
        flow_id,
        lane_id,
        namespace,
        public_state,
        blocking_reason,
        blocking_detail,
        current_attempt,
        current_lease,
        current_waitpoint,
        created_at,
        last_mutation_at,
        total_attempt_count,
        tags,
    )))
}

fn opt_str<'a>(map: &'a HashMap<String, String>, field: &str) -> Option<&'a str> {
    map.get(field).map(String::as_str)
}

fn parse_ts(
    map: &HashMap<String, String>,
    field: &str,
) -> Result<Option<TimestampMs>, SdkError> {
    match opt_str(map, field).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(raw) => {
            let ms: i64 = raw.parse().map_err(|e| {
                SdkError::Config(format!(
                    "describe_execution: exec_core.{field} is not a valid ms \
                     timestamp ('{raw}'): {e}"
                ))
            })?;
            Ok(Some(TimestampMs::from_millis(ms)))
        }
    }
}

fn parse_public_state(raw: &str) -> Result<PublicState, SdkError> {
    // exec_core stores the snake_case literal (e.g. "waiting"). PublicState's
    // Deserialize accepts the JSON-quoted form, so wrap + delegate.
    let quoted = format!("\"{raw}\"");
    serde_json::from_str(&quoted).map_err(|e| {
        SdkError::Config(format!(
            "describe_execution: exec_core.public_state '{raw}' is not a known \
             public state: {e}"
        ))
    })
}

fn build_attempt_summary(
    core: &HashMap<String, String>,
) -> Result<Option<AttemptSummary>, SdkError> {
    let attempt_id_str = match opt_str(core, "current_attempt_id").filter(|s| !s.is_empty()) {
        None => return Ok(None),
        Some(s) => s,
    };
    let attempt_id = AttemptId::parse(attempt_id_str).map_err(|e| {
        SdkError::Config(format!(
            "describe_execution: exec_core.current_attempt_id is not a valid \
             UUID: {e}"
        ))
    })?;
    let attempt_index: u32 = opt_str(core, "current_attempt_index")
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(Some(AttemptSummary::new(
        attempt_id,
        AttemptIndex::new(attempt_index),
    )))
}

fn build_lease_summary(
    core: &HashMap<String, String>,
) -> Result<Option<LeaseSummary>, SdkError> {
    // A lease is "held" when the worker_instance_id field is populated
    // AND lease_expires_at is set. Both clear together on revoke/expire
    // (see clear_lease_and_indexes in lua/helpers.lua).
    let wid_str = match opt_str(core, "current_worker_instance_id").filter(|s| !s.is_empty()) {
        None => return Ok(None),
        Some(s) => s,
    };
    let expires_at = match parse_ts(core, "lease_expires_at")? {
        None => return Ok(None),
        Some(ts) => ts,
    };
    let epoch: u64 = opt_str(core, "current_lease_epoch")
        .filter(|s| !s.is_empty())
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    Ok(Some(LeaseSummary::new(
        LeaseEpoch::new(epoch),
        WorkerInstanceId::new(wid_str.to_owned()),
        expires_at,
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ff_core::partition::PartitionConfig;
    use ff_core::types::FlowId;

    fn eid() -> ExecutionId {
        let config = PartitionConfig::default();
        ExecutionId::for_flow(&FlowId::new(), &config)
    }

    fn minimal_core(public_state: &str) -> HashMap<String, String> {
        let mut m = HashMap::new();
        m.insert("public_state".to_owned(), public_state.to_owned());
        m.insert("lane_id".to_owned(), "default".to_owned());
        m.insert("namespace".to_owned(), "ns".to_owned());
        m.insert("created_at".to_owned(), "1000".to_owned());
        m.insert("last_mutation_at".to_owned(), "2000".to_owned());
        m.insert("total_attempt_count".to_owned(), "0".to_owned());
        m
    }

    #[test]
    fn waiting_exec_no_attempt_no_lease_no_tags() {
        let snap = build_execution_snapshot(eid(), &minimal_core("waiting"), HashMap::new())
            .unwrap()
            .expect("should build");
        assert_eq!(snap.public_state, PublicState::Waiting);
        assert!(snap.current_attempt.is_none());
        assert!(snap.current_lease.is_none());
        assert!(snap.current_waitpoint.is_none());
        assert_eq!(snap.tags.len(), 0);
        assert_eq!(snap.created_at.0, 1000);
        assert_eq!(snap.last_mutation_at.0, 2000);
        assert!(snap.flow_id.is_none());
        assert!(snap.blocking_reason.is_none());
    }

    #[test]
    fn tags_flow_through_sorted() {
        let mut tags = HashMap::new();
        tags.insert("cairn.task_id".to_owned(), "t-1".to_owned());
        tags.insert("cairn.project".to_owned(), "proj".to_owned());
        let snap = build_execution_snapshot(eid(), &minimal_core("waiting"), tags)
            .unwrap()
            .unwrap();
        let keys: Vec<_> = snap.tags.keys().cloned().collect();
        assert_eq!(keys, vec!["cairn.project".to_owned(), "cairn.task_id".to_owned()]);
    }

    #[test]
    fn invalid_public_state_fails_loud() {
        let err = build_execution_snapshot(eid(), &minimal_core("bogus"), HashMap::new())
            .unwrap_err();
        match err {
            SdkError::Config(msg) => assert!(msg.contains("public_state"), "msg: {msg}"),
            other => panic!("expected Config, got {other:?}"),
        }
    }

    #[test]
    fn lease_summary_requires_both_wid_and_expires_at() {
        // Only wid → no lease (lease expired + cleared lease_expires_at
        // but wid not yet rewritten is not a real state; defensive).
        let mut core = minimal_core("active");
        core.insert(
            "current_worker_instance_id".to_owned(),
            "w-inst-1".to_owned(),
        );
        let snap = build_execution_snapshot(eid(), &core, HashMap::new())
            .unwrap()
            .unwrap();
        assert!(snap.current_lease.is_none());

        core.insert("lease_expires_at".to_owned(), "9000".to_owned());
        core.insert("current_lease_epoch".to_owned(), "3".to_owned());
        let snap = build_execution_snapshot(eid(), &core, HashMap::new())
            .unwrap()
            .unwrap();
        let lease = snap.current_lease.expect("lease present");
        assert_eq!(lease.lease_epoch, LeaseEpoch::new(3));
        assert_eq!(lease.expires_at.0, 9000);
        assert_eq!(lease.worker_instance_id.as_str(), "w-inst-1");
    }
}
