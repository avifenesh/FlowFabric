//! Postgres flow-family `EngineBackend` implementations (Wave 4c).
//!
//! Six trait methods over the Wave 3 schema:
//!
//! * [`describe_flow`] — single-flow snapshot (row + edge groups).
//! * [`list_flows`] — cursor-paginated per-partition listing.
//! * [`list_edges`] — fan-in / fan-out adjacency for an execution.
//! * [`describe_edge`] — one edge by flow + edge id.
//! * [`cancel_flow`] — SERIALIZABLE cascade with a 3-attempt retry
//!   loop. Exhaustion surfaces as
//!   [`ContentionKind::RetryExhausted`] (Q11 — one of the three
//!   SERIALIZABLE sites).
//! * [`set_edge_group_policy`] — RFC-016 Stage A policy write;
//!   rejects when edges have already been staged for the downstream
//!   group (parity with the Valkey `invalid_input` error path).
//!
//! Field layout note: `ff_flow_core` only hoists the columns the
//! engine filters on (public_flow_state, graph_revision, created_at).
//! Every other `FlowSnapshot` field — `flow_kind`, `namespace`,
//! `node_count`, `edge_count`, `last_mutation_at_ms`, cancel meta,
//! and namespaced tags — rides in `raw_fields jsonb`. Same story
//! for `ff_edge`: the immutable edge record (dependency_kind,
//! satisfaction_condition, etc.) lives in the `policy jsonb` column
//! (the only jsonb slot on the edge row); the typed columns carry
//! only endpoints + policy_version.

use std::collections::BTreeMap;
use std::time::Duration;

use ff_core::backend::CancelFlowPolicy;
use ff_core::contracts::{
    CancelFlowResult, EdgeDependencyPolicy, EdgeDirection, EdgeGroupSnapshot, EdgeGroupState,
    EdgeSnapshot, FlowSnapshot, FlowStatus, FlowSummary, ListFlowsPage, OnSatisfied,
    SetEdgeGroupPolicyResult,
};
use ff_core::engine_error::{
    ContentionKind, EngineError, ValidationKind,
};
use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
use ff_core::types::{EdgeId, ExecutionId, FlowId, Namespace, TimestampMs};
use serde_json::Value as JsonValue;
use sqlx::postgres::PgRow;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;

/// Max attempts for the SERIALIZABLE retry loop on `cancel_flow`
/// (Q11). After this many 40001 / 40P01 faults the caller sees
/// [`ContentionKind::RetryExhausted`] and falls back to the
/// reconciler.
const CANCEL_FLOW_MAX_ATTEMPTS: u32 = 3;

/// Extract a partition-index byte (0..=255) from a [`PartitionKey`].
/// Returns a typed validation error on malformed keys.
fn partition_index_from_key(key: &PartitionKey) -> Result<i16, EngineError> {
    let p = key.parse().map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("partition_key: {e}"),
    })?;
    Ok(p.index as i16)
}

/// Compute the partition byte (0..=255) for a `FlowId` using the
/// deployment's partition config. Routing parity with the Valkey
/// backend's `flow_partition` — same crc16-CCITT modulo.
fn flow_partition_byte(
    flow_id: &FlowId,
    partition_config: &ff_core::partition::PartitionConfig,
) -> i16 {
    ff_core::partition::flow_partition(flow_id, partition_config).index as i16
}

/// Decode a `raw_fields jsonb` string slot, falling back to `None`
/// for missing / null / non-string values.
fn raw_str<'a>(raw: &'a JsonValue, key: &str) -> Option<&'a str> {
    raw.get(key).and_then(|v| v.as_str())
}

/// Decode a numeric slot from `raw_fields jsonb` as `u64`. Accepts
/// JSON numbers or string-encoded ints (the Lua projector emits
/// numbers; the Wave-4 proc emits the same).
fn raw_u64(raw: &JsonValue, key: &str) -> Option<u64> {
    raw.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
    })
}

fn raw_u32(raw: &JsonValue, key: &str) -> Option<u32> {
    raw.get(key).and_then(|v| {
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.parse::<u64>().ok()))
            .and_then(|n| u32::try_from(n).ok())
    })
}

/// Route `raw_fields` jsonb object keys into a `(typed_fields, tags)`
/// pair — anything matching `^[a-z][a-z0-9_]*\.` is a namespaced tag.
fn extract_tags(raw: &JsonValue) -> BTreeMap<String, String> {
    let mut tags = BTreeMap::new();
    let Some(obj) = raw.as_object() else {
        return tags;
    };
    for (k, v) in obj {
        if !is_namespaced_tag_key(k) {
            continue;
        }
        if let Some(s) = v.as_str() {
            tags.insert(k.clone(), s.to_owned());
        }
    }
    tags
}

fn is_namespaced_tag_key(k: &str) -> bool {
    let mut chars = k.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    let mut saw_dot = false;
    for c in chars {
        if c == '.' {
            saw_dot = true;
            break;
        }
        if !(c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
            return false;
        }
    }
    saw_dot
}

fn parse_on_satisfied(s: &str) -> OnSatisfied {
    match s {
        "let_run" => OnSatisfied::LetRun,
        _ => OnSatisfied::CancelRemaining,
    }
}

/// Decode an `EdgeDependencyPolicy` from the `ff_edge_group.policy`
/// jsonb. Layout mirrors the ff-core `Serialize` impl:
/// `{ "kind": "all_of" | "any_of" | "quorum", "on_satisfied": ..., "k": .. }`.
fn decode_edge_policy(v: &JsonValue) -> EdgeDependencyPolicy {
    let kind = v.get("kind").and_then(|k| k.as_str()).unwrap_or("all_of");
    match kind {
        "any_of" => {
            let on = v
                .get("on_satisfied")
                .and_then(|x| x.as_str())
                .map(parse_on_satisfied)
                .unwrap_or(OnSatisfied::CancelRemaining);
            EdgeDependencyPolicy::AnyOf { on_satisfied: on }
        }
        "quorum" => {
            let k = v
                .get("k")
                .and_then(|x| x.as_u64())
                .and_then(|n| u32::try_from(n).ok())
                .unwrap_or(1);
            let on = v
                .get("on_satisfied")
                .and_then(|x| x.as_str())
                .map(parse_on_satisfied)
                .unwrap_or(OnSatisfied::CancelRemaining);
            EdgeDependencyPolicy::Quorum { k, on_satisfied: on }
        }
        _ => EdgeDependencyPolicy::AllOf,
    }
}

fn encode_edge_policy(p: &EdgeDependencyPolicy) -> JsonValue {
    match p {
        EdgeDependencyPolicy::AllOf => serde_json::json!({ "kind": "all_of" }),
        EdgeDependencyPolicy::AnyOf { on_satisfied } => serde_json::json!({
            "kind": "any_of",
            "on_satisfied": on_satisfied.variant_str(),
        }),
        EdgeDependencyPolicy::Quorum { k, on_satisfied } => serde_json::json!({
            "kind": "quorum",
            "k": k,
            "on_satisfied": on_satisfied.variant_str(),
        }),
        // Forward-compat: unknown variants fall back to the Stage-A
        // `all_of` encoding so a future `#[non_exhaustive]` addition
        // doesn't corrupt the persisted row.
        _ => serde_json::json!({ "kind": "all_of" }),
    }
}

/// Decode one `ff_edge_group` row into an [`EdgeGroupSnapshot`].
fn decode_edge_group_row(row: &PgRow) -> Result<EdgeGroupSnapshot, EngineError> {
    let downstream_uuid: Uuid = row.get("downstream_eid");
    let policy_raw: JsonValue = row.get("policy");
    let k_target: i32 = row.get("k_target");
    let success_count: i32 = row.get("success_count");
    let fail_count: i32 = row.get("fail_count");
    let skip_count: i32 = row.get("skip_count");
    let running_count: i32 = row.get("running_count");

    // Round-trip: exec id lives in the flow partition. Reconstruct
    // the full `{fp:N}:<uuid>` form using the row's partition_key.
    let part: i16 = row.get("partition_key");
    let downstream_id = ExecutionId::parse(&format!("{{fp:{part}}}:{downstream_uuid}"))
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_edge_group.downstream_eid: {e}"),
        })?;

    let policy = decode_edge_policy(&policy_raw);
    // Stage A: total_deps reported as success+fail+skip+running
    // (equivalent to the RFC-007 `n` counter on the legacy hash).
    let total: u32 = success_count.max(0) as u32
        + fail_count.max(0) as u32
        + skip_count.max(0) as u32
        + running_count.max(0) as u32;

    // group_state — Stage A derivation: satisfied when k_target met.
    let state = if k_target > 0 && success_count >= k_target {
        EdgeGroupState::Satisfied
    } else {
        EdgeGroupState::Pending
    };

    Ok(EdgeGroupSnapshot::new(
        downstream_id,
        policy,
        total,
        success_count.max(0) as u32,
        fail_count.max(0) as u32,
        skip_count.max(0) as u32,
        running_count.max(0) as u32,
        state,
    ))
}

/// Decode one `ff_flow_core` row + its edge_group rows into a
/// [`FlowSnapshot`].
fn decode_flow_row(
    flow_id: FlowId,
    row: &PgRow,
    edge_groups: Vec<EdgeGroupSnapshot>,
) -> Result<FlowSnapshot, EngineError> {
    let public_flow_state: String = row.get("public_flow_state");
    let graph_revision_i: i64 = row.get("graph_revision");
    let created_at_ms: i64 = row.get("created_at_ms");
    let terminal_at_ms: Option<i64> = row.get("terminal_at_ms");
    let raw_fields: JsonValue = row.get("raw_fields");

    let flow_kind = raw_str(&raw_fields, "flow_kind")
        .unwrap_or("")
        .to_owned();
    let namespace_str = raw_str(&raw_fields, "namespace").unwrap_or("default");
    let namespace = Namespace::new(namespace_str.to_owned());
    let node_count = raw_u32(&raw_fields, "node_count").unwrap_or(0);
    let edge_count = raw_u32(&raw_fields, "edge_count").unwrap_or(0);
    let last_mutation_at_ms =
        raw_u64(&raw_fields, "last_mutation_at_ms").map(|n| n as i64).unwrap_or(created_at_ms);

    let cancelled_at = terminal_at_ms.map(TimestampMs);
    let cancel_reason = raw_str(&raw_fields, "cancel_reason").map(str::to_owned);
    let cancellation_policy = raw_str(&raw_fields, "cancellation_policy").map(str::to_owned);

    let tags = extract_tags(&raw_fields);

    let graph_revision = u64::try_from(graph_revision_i).unwrap_or(0);

    Ok(FlowSnapshot::new(
        flow_id,
        flow_kind,
        namespace,
        public_flow_state,
        graph_revision,
        node_count,
        edge_count,
        TimestampMs(created_at_ms),
        TimestampMs(last_mutation_at_ms),
        cancelled_at,
        cancel_reason,
        cancellation_policy,
        tags,
        edge_groups,
    ))
}

/// Decode one `ff_edge` row into an [`EdgeSnapshot`].
///
/// The immutable edge record (dependency_kind, satisfaction_condition,
/// data_passing_ref, edge_state, created_at, created_by) lives inside
/// the `policy jsonb` column — the only jsonb slot on `ff_edge`.
fn decode_edge_row(row: &PgRow, flow_id: &FlowId) -> Result<EdgeSnapshot, EngineError> {
    let edge_uuid: Uuid = row.get("edge_id");
    let upstream_uuid: Uuid = row.get("upstream_eid");
    let downstream_uuid: Uuid = row.get("downstream_eid");
    let part: i16 = row.get("partition_key");
    let policy_raw: JsonValue = row.get("policy");

    let upstream = ExecutionId::parse(&format!("{{fp:{part}}}:{upstream_uuid}"))
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_edge.upstream_eid: {e}"),
        })?;
    let downstream = ExecutionId::parse(&format!("{{fp:{part}}}:{downstream_uuid}"))
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("ff_edge.downstream_eid: {e}"),
        })?;

    let dependency_kind = raw_str(&policy_raw, "dependency_kind")
        .unwrap_or("success_only")
        .to_owned();
    let satisfaction_condition = raw_str(&policy_raw, "satisfaction_condition")
        .unwrap_or("all_required")
        .to_owned();
    let data_passing_ref = raw_str(&policy_raw, "data_passing_ref")
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let edge_state = raw_str(&policy_raw, "edge_state")
        .unwrap_or("pending")
        .to_owned();
    let created_at_ms =
        raw_u64(&policy_raw, "created_at_ms").map(|n| n as i64).unwrap_or(0);
    let created_by = raw_str(&policy_raw, "created_by")
        .unwrap_or("engine")
        .to_owned();

    Ok(EdgeSnapshot::new(
        EdgeId::from_uuid(edge_uuid),
        flow_id.clone(),
        upstream,
        downstream,
        dependency_kind,
        satisfaction_condition,
        data_passing_ref,
        edge_state,
        TimestampMs(created_at_ms),
        created_by,
    ))
}

/// `describe_flow` — single flow snapshot + edge_groups.
pub async fn describe_flow(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    id: &FlowId,
) -> Result<Option<FlowSnapshot>, EngineError> {
    let part = flow_partition_byte(id, partition_config);
    let flow_uuid: Uuid = id.0;

    let flow_row_opt = sqlx::query(
        "SELECT partition_key, flow_id, graph_revision, public_flow_state, \
                created_at_ms, terminal_at_ms, raw_fields \
         FROM ff_flow_core \
         WHERE partition_key = $1 AND flow_id = $2",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(flow_row) = flow_row_opt else {
        return Ok(None);
    };

    let eg_rows = sqlx::query(
        "SELECT partition_key, flow_id, downstream_eid, policy, \
                k_target, success_count, fail_count, skip_count, running_count \
         FROM ff_edge_group \
         WHERE partition_key = $1 AND flow_id = $2 \
         ORDER BY downstream_eid",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut edge_groups = Vec::with_capacity(eg_rows.len());
    for row in &eg_rows {
        edge_groups.push(decode_edge_group_row(row)?);
    }

    decode_flow_row(id.clone(), &flow_row, edge_groups).map(Some)
}

/// `list_flows` — cursor-paginated per-partition flow listing.
pub async fn list_flows(
    pool: &PgPool,
    partition: PartitionKey,
    cursor: Option<FlowId>,
    limit: usize,
) -> Result<ListFlowsPage, EngineError> {
    if limit == 0 {
        return Ok(ListFlowsPage::new(Vec::new(), None));
    }
    let part = partition_index_from_key(&partition)?;
    let cursor_uuid: Option<Uuid> = cursor.as_ref().map(|f| f.0);
    // +1 row to decide whether next_cursor should be set.
    let fetch_limit = (limit + 1) as i64;

    let rows = sqlx::query(
        "SELECT flow_id, created_at_ms, public_flow_state \
         FROM ff_flow_core \
         WHERE partition_key = $1 \
           AND ($2::uuid IS NULL OR flow_id > $2) \
         ORDER BY flow_id \
         LIMIT $3",
    )
    .bind(part)
    .bind(cursor_uuid)
    .bind(fetch_limit)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut flows: Vec<FlowSummary> = Vec::with_capacity(rows.len().min(limit));
    let mut next_cursor: Option<FlowId> = None;
    for (idx, row) in rows.iter().enumerate() {
        if idx >= limit {
            // The (limit+1)-th row signals there is at least one more;
            // we don't include it in the page, but the last included
            // row's flow_id becomes the cursor.
            if let Some(last) = flows.last() {
                next_cursor = Some(last.flow_id.clone());
            }
            break;
        }
        let flow_uuid: Uuid = row.get("flow_id");
        let created_at_ms: i64 = row.get("created_at_ms");
        let public_state: String = row.get("public_flow_state");
        let status = FlowStatus::from_public_flow_state(&public_state);
        flows.push(FlowSummary::new(
            FlowId::from_uuid(flow_uuid),
            TimestampMs(created_at_ms),
            status,
        ));
    }

    Ok(ListFlowsPage::new(flows, next_cursor))
}

/// `list_edges` — direction-keyed adjacency listing.
pub async fn list_edges(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    flow_id: &FlowId,
    direction: EdgeDirection,
) -> Result<Vec<EdgeSnapshot>, EngineError> {
    let part = flow_partition_byte(flow_id, partition_config);
    let flow_uuid: Uuid = flow_id.0;
    let (column_filter, subject_eid) = match &direction {
        EdgeDirection::Outgoing { from_node } => ("upstream_eid", from_node),
        EdgeDirection::Incoming { to_node } => ("downstream_eid", to_node),
    };
    // Parse the UUID suffix from the `{fp:N}:<uuid>` id.
    let subject_uuid = parse_exec_uuid(subject_eid)?;

    let sql = format!(
        "SELECT partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy \
         FROM ff_edge \
         WHERE partition_key = $1 AND flow_id = $2 AND {column_filter} = $3 \
         ORDER BY edge_id"
    );
    let rows = sqlx::query(&sql)
        .bind(part)
        .bind(flow_uuid)
        .bind(subject_uuid)
        .fetch_all(pool)
        .await
        .map_err(map_sqlx_error)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in &rows {
        out.push(decode_edge_row(row, flow_id)?);
    }
    Ok(out)
}

/// `describe_edge` — one edge by flow + edge id.
pub async fn describe_edge(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    flow_id: &FlowId,
    edge_id: &EdgeId,
) -> Result<Option<EdgeSnapshot>, EngineError> {
    let part = flow_partition_byte(flow_id, partition_config);
    let row_opt = sqlx::query(
        "SELECT partition_key, flow_id, edge_id, upstream_eid, downstream_eid, policy \
         FROM ff_edge \
         WHERE partition_key = $1 AND flow_id = $2 AND edge_id = $3",
    )
    .bind(part)
    .bind(flow_id.0)
    .bind(edge_id.0)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    match row_opt {
        Some(row) => decode_edge_row(&row, flow_id).map(Some),
        None => Ok(None),
    }
}

/// Parse the UUID suffix out of a hash-tagged `{fp:N}:<uuid>` exec id.
fn parse_exec_uuid(eid: &ExecutionId) -> Result<Uuid, EngineError> {
    let s = eid.as_str();
    let Some(colon) = s.rfind("}:") else {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("execution_id missing '}}:' delimiter: {s}"),
        });
    };
    let tail = &s[colon + 2..];
    Uuid::parse_str(tail).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id uuid suffix parse: {e}"),
    })
}

fn cancel_policy_to_str(p: CancelFlowPolicy) -> &'static str {
    match p {
        CancelFlowPolicy::FlowOnly => "cancel_flow_only",
        CancelFlowPolicy::CancelAll => "cancel_all",
        CancelFlowPolicy::CancelPending => "cancel_pending",
        // Forward-compat for future additive `CancelFlowPolicy` variants
        // — encode as `cancel_all` (safest conservative default).
        _ => "cancel_all",
    }
}

/// `cancel_flow` — SERIALIZABLE cascade with a 3-attempt retry loop.
///
/// Transaction steps:
///   1. SELECT + UPDATE `ff_flow_core.public_flow_state = 'cancelled'`
///      (atomic state flip).
///   2. For `CancelAll`/`CancelPending`: SELECT member execution ids
///      from `ff_exec_core` filtered by flow_id and state, UPDATE
///      each to `cancelled`, INSERT a `ff_completion_event` row per
///      member (the trigger fires NOTIFY at COMMIT).
///   3. For `CancelPending` under `CancelRemaining` semantics (Stage
///      C), INSERT a `ff_pending_cancel_groups` bookkeeping row for
///      every running sibling group that the Wave-5 dispatcher needs
///      to follow up on. Wave 4c only writes the bookkeeping row —
///      the actual dispatch is Wave 5.
///
/// On `SQLSTATE 40001` / `40P01` (serialization / deadlock) the
/// transaction ROLLBACKs, we sleep a tiny backoff, and retry. After
/// [`CANCEL_FLOW_MAX_ATTEMPTS`] failed attempts we return
/// [`ContentionKind::RetryExhausted`].
pub async fn cancel_flow(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    id: &FlowId,
    policy: CancelFlowPolicy,
) -> Result<CancelFlowResult, EngineError> {
    let part = flow_partition_byte(id, partition_config);
    let flow_uuid: Uuid = id.0;
    let policy_str = cancel_policy_to_str(policy);

    let mut last_transport: Option<EngineError> = None;
    for attempt in 0..CANCEL_FLOW_MAX_ATTEMPTS {
        match cancel_flow_once(pool, part, flow_uuid, policy, policy_str).await {
            Ok(result) => return Ok(result),
            Err(err) => {
                if is_serialization_conflict(&err) {
                    // Tiny exponential backoff: 0, 5ms, 15ms.
                    if attempt + 1 < CANCEL_FLOW_MAX_ATTEMPTS {
                        let ms = 5u64 * (1u64 << attempt).saturating_sub(0);
                        tokio::time::sleep(Duration::from_millis(ms)).await;
                    }
                    last_transport = Some(err);
                    continue;
                }
                return Err(err);
            }
        }
    }
    // Exhausted all attempts on serialization failures.
    let _ = last_transport; // drop; retained only for trace-level diagnosis
    Err(EngineError::Contention(ContentionKind::RetryExhausted))
}

/// Return `true` when an `EngineError` was produced by the 40001 /
/// 40P01 sqlx mapping (Contention(LeaseConflict)). We re-map that
/// specific kind into the retry loop; other `Contention` variants
/// (e.g. honest lease contention from a future extension) propagate
/// untouched.
fn is_serialization_conflict(err: &EngineError) -> bool {
    matches!(
        err,
        EngineError::Contention(ContentionKind::LeaseConflict)
    )
}

async fn cancel_flow_once(
    pool: &PgPool,
    part: i16,
    flow_uuid: Uuid,
    policy: CancelFlowPolicy,
    policy_str: &str,
) -> Result<CancelFlowResult, EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    sqlx::query("SET TRANSACTION ISOLATION LEVEL SERIALIZABLE")
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

    // Step 1 — atomically flip the flow_core state. If the flow
    // doesn't exist we return `Cancelled { members: [] }` with empty
    // members (idempotent retry after the row has already been
    // cancelled returns the same shape).
    let flow_found = sqlx::query(
        "UPDATE ff_flow_core \
         SET public_flow_state = 'cancelled', \
             terminal_at_ms = COALESCE(terminal_at_ms, \
                 (extract(epoch from clock_timestamp())*1000)::bigint), \
             raw_fields = raw_fields \
                 || jsonb_build_object('cancellation_policy', $3::text) \
         WHERE partition_key = $1 AND flow_id = $2 \
         RETURNING flow_id",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(policy_str)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if flow_found.is_none() {
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(CancelFlowResult::Cancelled {
            cancellation_policy: policy_str.to_owned(),
            member_execution_ids: Vec::new(),
        });
    }

    // Step 2 — collect + cancel member executions under the policy.
    let member_rows = if matches!(policy, CancelFlowPolicy::FlowOnly) {
        Vec::new()
    } else {
        let state_filter: &str = match policy {
            CancelFlowPolicy::CancelAll => {
                "lifecycle_phase NOT IN ('completed','failed','cancelled','expired')"
            }
            CancelFlowPolicy::CancelPending => {
                "lifecycle_phase IN ('pending','blocked','eligible')"
            }
            _ => "lifecycle_phase NOT IN ('completed','failed','cancelled','expired')",
        };
        let sql = format!(
            "SELECT execution_id FROM ff_exec_core \
             WHERE partition_key = $1 AND flow_id = $2 AND {state_filter}"
        );
        sqlx::query(&sql)
            .bind(part)
            .bind(flow_uuid)
            .fetch_all(&mut *tx)
            .await
            .map_err(map_sqlx_error)?
    };

    let mut member_execution_ids: Vec<String> = Vec::with_capacity(member_rows.len());
    for row in &member_rows {
        let exec_uuid: Uuid = row.get("execution_id");
        // Flip the member to `cancelled` lifecycle + eligibility.
        sqlx::query(
            "UPDATE ff_exec_core \
             SET lifecycle_phase = 'cancelled', \
                 eligibility_state = 'cancelled', \
                 public_state = 'cancelled', \
                 terminal_at_ms = COALESCE(terminal_at_ms, \
                     (extract(epoch from clock_timestamp())*1000)::bigint), \
                 cancellation_reason = COALESCE(cancellation_reason, 'flow_cancelled'), \
                 cancelled_by = COALESCE(cancelled_by, 'cancel_flow') \
             WHERE partition_key = $1 AND execution_id = $2",
        )
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // #355: clear the current attempt's `outcome` so a later
        // `read_execution_info` doesn't surface a stale
        // `retry`/`interrupted` terminal-outcome on the cancelled row.
        // Mirror of the SQLite companion statement in
        // `ff-backend-sqlite/src/queries/flow.rs`
        // (`UPDATE_ATTEMPT_CLEAR_OUTCOME_FOR_CURRENT_SQL`).
        sqlx::query(
            "UPDATE ff_attempt \
                SET outcome = NULL \
              WHERE partition_key = $1 \
                AND execution_id  = $2 \
                AND attempt_index = (SELECT attempt_index FROM ff_exec_core \
                                      WHERE partition_key = $1 AND execution_id = $2)",
        )
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // Emit a completion outbox row (trigger fires NOTIFY).
        sqlx::query(
            "INSERT INTO ff_completion_event \
             (partition_key, execution_id, flow_id, outcome, occurred_at_ms) \
             VALUES ($1, $2, $3, 'cancelled', \
                     (extract(epoch from clock_timestamp())*1000)::bigint)",
        )
        .bind(part)
        .bind(exec_uuid)
        .bind(flow_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        // RFC-019 Stage B outbox: lease revoked on every cancelled
        // member. Members that never held a live lease surface a
        // no-op revoke to consumers — same shape as the Valkey side.
        sqlx::query(
            "INSERT INTO ff_lease_event \
             (execution_id, lease_id, event_type, occurred_at_ms, partition_key) \
             VALUES ($1, NULL, 'revoked', \
                     (extract(epoch from clock_timestamp())*1000)::bigint, $2)",
        )
        .bind(exec_uuid.to_string())
        .bind(i32::from(part))
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        member_execution_ids.push(format!("{{fp:{part}}}:{exec_uuid}"));
    }

    // Step 3 — for CancelPending, record any running sibling groups
    // into ff_pending_cancel_groups so the Wave-5 dispatcher can
    // follow up. Wave 4c only does the bookkeeping write.
    if matches!(policy, CancelFlowPolicy::CancelPending) {
        sqlx::query(
            "INSERT INTO ff_pending_cancel_groups \
             (partition_key, flow_id, downstream_eid, enqueued_at_ms) \
             SELECT partition_key, flow_id, downstream_eid, \
                    (extract(epoch from clock_timestamp())*1000)::bigint \
             FROM ff_edge_group \
             WHERE partition_key = $1 AND flow_id = $2 AND running_count > 0 \
             ON CONFLICT DO NOTHING",
        )
        .bind(part)
        .bind(flow_uuid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(CancelFlowResult::Cancelled {
        cancellation_policy: policy_str.to_owned(),
        member_execution_ids,
    })
}

/// `set_edge_group_policy` — RFC-016 Stage A.
///
/// Reject when at least one edge has already been staged for
/// `downstream_execution_id` (the ordering rule: policy must be set
/// BEFORE the first `add_dependency`). Parity with the Valkey Lua
/// path, which returns `invalid_input` → `EngineError::Validation`
/// in the same situation (not `Conflict`, per in-tree behavior).
///
/// Stage A supports `AllOf` fully; `AnyOf` / `Quorum` flow through
/// the same write path (Stage B's resolver is backend-specific —
/// Valkey's Lua honours them, Postgres Wave 4c stores them and Wave
/// 5 wires the resolver). Callers that need the Stage-A ordering
/// check can rely on this method regardless of the variant.
pub async fn set_edge_group_policy(
    pool: &PgPool,
    partition_config: &ff_core::partition::PartitionConfig,
    flow_id: &FlowId,
    downstream_execution_id: &ExecutionId,
    policy: EdgeDependencyPolicy,
) -> Result<SetEdgeGroupPolicyResult, EngineError> {
    // Light input validation mirroring the Valkey Stage B impl.
    match &policy {
        EdgeDependencyPolicy::AllOf => {}
        EdgeDependencyPolicy::AnyOf { .. } => {}
        EdgeDependencyPolicy::Quorum { k, .. } => {
            if *k == 0 {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: "quorum k must be >= 1".to_string(),
                });
            }
        }
        _ => {
            return Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "unknown EdgeDependencyPolicy variant".to_string(),
            });
        }
    }

    let part = flow_partition_byte(flow_id, partition_config);
    let flow_uuid: Uuid = flow_id.0;
    let downstream_uuid = parse_exec_uuid(downstream_execution_id)?;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // Ordering guard: edges already staged for this group?
    let already_staged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM ff_edge \
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(downstream_uuid)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if already_staged > 0 {
        let _ = tx.rollback().await;
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "edge_group_policy_already_fixed: dependencies already staged".to_string(),
        });
    }

    // Idempotent restate: if an identical policy is already stored,
    // return AlreadySet without touching the row.
    let existing: Option<JsonValue> = sqlx::query_scalar(
        "SELECT policy FROM ff_edge_group \
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(downstream_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let encoded = encode_edge_policy(&policy);
    if let Some(existing_policy) = existing
        && existing_policy == encoded
    {
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(SetEdgeGroupPolicyResult::AlreadySet);
    }

    sqlx::query(
        "INSERT INTO ff_edge_group \
         (partition_key, flow_id, downstream_eid, policy) \
         VALUES ($1, $2, $3, $4) \
         ON CONFLICT (partition_key, flow_id, downstream_eid) \
         DO UPDATE SET policy = EXCLUDED.policy",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(downstream_uuid)
    .bind(&encoded)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(SetEdgeGroupPolicyResult::Set)
}

// Silence an unused-import lint if PartitionFamily/Partition imports
// aren't otherwise referenced after trimming.
#[allow(dead_code)]
fn _unused_imports_anchor(_p: Partition, _f: PartitionFamily) {}
