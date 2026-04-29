//! RFC-020 Wave 9 Spine-B read-model — SQLite port (RFC-023 Phase 3.3).
//!
//! Three read-only methods paralleling
//! `ff-backend-postgres/src/exec_core.rs` §4.1:
//!
//!   * [`read_execution_state_impl`] — single-column projection of
//!     `ff_exec_core.public_state`.
//!   * [`read_execution_info_impl`] — multi-column `ff_exec_core` +
//!     correlated-subquery attempt join into [`ExecutionInfo`].
//!   * [`get_execution_result_impl`] — `ff_exec_core.result` BLOB.
//!
//! Storage literals are a superset of the in-core enum variants
//! (Postgres and SQLite ports share the same authoring sites); the
//! `normalise_*` helpers collapse them to the `serde_snake_case`
//! wire-form expected by `json_enum!`. Unknown tokens surface as
//! `EngineError::Validation(Corruption)` — the `json_enum!` macro
//! deserialise from a quoted literal, so any mismatch is loud.
//!
//! All three paths are read-only; no `BEGIN IMMEDIATE` needed — we
//! execute directly on the pool. Mirrors PG's READ COMMITTED posture
//! for these three methods (§4.1).

use std::collections::HashMap;

use ff_core::contracts::{ExecutionContext, ExecutionInfo};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::state::{
    AttemptState, BlockingReason, EligibilityState, LifecyclePhase, OwnershipState, PublicState,
    StateVector, TerminalOutcome,
};
use ff_core::types::ExecutionId;
use serde_json::Value as JsonValue;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use crate::errors::map_sqlx_error;
use crate::queries::reads as q_read;
use crate::tx_util::split_exec_id;

// ─── normalisation helpers (parity with PG exec_core) ──────────────
//
// Match arms are grounded in actual write sites in this crate + PG
// reference; unknown tokens fall through so `json_enum!` surfaces
// `Corruption` loudly per RFC-020 Rev 7 §4.1.

fn normalise_lifecycle_phase(raw: &str) -> &str {
    match raw {
        "cancelled" | "terminal" => "terminal",
        "pending" | "runnable" | "eligible" | "blocked" => "runnable",
        "active" => "active",
        "suspended" => "suspended",
        "submitted" => "submitted",
        other => other,
    }
}

fn normalise_ownership_state(raw: &str) -> &str {
    match raw {
        "released" | "unowned" => "unowned",
        "leased" => "leased",
        "lease_expired_reclaimable" => "lease_expired_reclaimable",
        "lease_revoked" => "lease_revoked",
        other => other,
    }
}

fn normalise_eligibility_state(raw: &str) -> &str {
    match raw {
        "cancelled" => "not_applicable",
        "pending_claim" => "eligible_now",
        other => other,
    }
}

fn normalise_attempt_state(raw: &str) -> &str {
    match raw {
        "pending" | "pending_claim" => "pending_first_attempt",
        "running" => "running_attempt",
        "cancelled" => "attempt_terminal",
        other => other,
    }
}

fn normalise_public_state(raw: &str) -> &str {
    match raw {
        "running" => "active",
        other => other,
    }
}

macro_rules! json_enum {
    ($ty:ty, $field:expr, $raw:expr) => {{
        let quoted = format!("\"{}\"", $raw);
        serde_json::from_str::<$ty>(&quoted).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!(
                "exec_core: {}: '{}' is not a known value: {}",
                $field, $raw, e
            ),
        })
    }};
}

fn derive_terminal_outcome(
    phase_norm: &str,
    phase_raw: &str,
    attempt_outcome: Option<&str>,
) -> TerminalOutcome {
    if phase_norm != "terminal" {
        return TerminalOutcome::None;
    }
    if phase_raw == "cancelled" {
        return TerminalOutcome::Cancelled;
    }
    match attempt_outcome {
        Some("success") => TerminalOutcome::Success,
        Some("failed") => TerminalOutcome::Failed,
        Some("cancelled") => TerminalOutcome::Cancelled,
        Some("expired") => TerminalOutcome::Expired,
        Some("skipped") => TerminalOutcome::Skipped,
        _ => TerminalOutcome::None,
    }
}

// ─── read_execution_state ──────────────────────────────────────────

/// RFC-020 §4.1 — point read of `public_state`. `Ok(None)` when the
/// execution is missing.
pub(crate) async fn read_execution_state_impl(
    pool: &SqlitePool,
    id: &ExecutionId,
) -> Result<Option<PublicState>, EngineError> {
    let (part, exec_uuid) = split_exec_id(id)?;

    let row: Option<(String,)> = sqlx::query_as(q_read::SELECT_PUBLIC_STATE_SQL)
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(pool)
        .await
        .map_err(map_sqlx_error)?;

    let Some((raw,)) = row else {
        return Ok(None);
    };
    let parsed: PublicState =
        json_enum!(PublicState, "public_state", normalise_public_state(&raw))?;
    Ok(Some(parsed))
}

// ─── read_execution_info ───────────────────────────────────────────

/// RFC-020 §4.1 — multi-column projection + current-attempt /
/// first-attempt joins into [`ExecutionInfo`]. `Ok(None)` when the
/// execution is missing.
pub(crate) async fn read_execution_info_impl(
    pool: &SqlitePool,
    id: &ExecutionId,
) -> Result<Option<ExecutionInfo>, EngineError> {
    let (part, exec_uuid) = split_exec_id(id)?;

    let row = sqlx::query(q_read::SELECT_EXECUTION_INFO_SQL)
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(pool)
        .await
        .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let flow_id_blob: Option<Vec<u8>> = row.try_get("flow_id").map_err(map_sqlx_error)?;
    let lane_id: String = row.try_get("lane_id").map_err(map_sqlx_error)?;
    let priority: i64 = row.try_get("priority").map_err(map_sqlx_error)?;
    let lifecycle_phase_raw: String =
        row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let ownership_state_raw: String =
        row.try_get("ownership_state").map_err(map_sqlx_error)?;
    let eligibility_state_raw: String =
        row.try_get("eligibility_state").map_err(map_sqlx_error)?;
    let public_state_raw: String = row.try_get("public_state").map_err(map_sqlx_error)?;
    let attempt_state_raw: String = row.try_get("attempt_state").map_err(map_sqlx_error)?;
    let blocking_reason_opt: Option<String> =
        row.try_get("blocking_reason").map_err(map_sqlx_error)?;
    let attempt_index: i64 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let created_at_ms: i64 = row.try_get("created_at_ms").map_err(map_sqlx_error)?;
    let terminal_at_ms_opt: Option<i64> =
        row.try_get("terminal_at_ms").map_err(map_sqlx_error)?;
    let raw_fields_str: String = row.try_get("raw_fields").map_err(map_sqlx_error)?;
    let attempt_outcome_opt: Option<String> =
        row.try_get("attempt_outcome").map_err(map_sqlx_error)?;
    let started_at_ms_opt: Option<i64> =
        row.try_get("started_at_ms").map_err(map_sqlx_error)?;

    let raw_fields: JsonValue =
        serde_json::from_str(&raw_fields_str).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("exec_core: raw_fields not valid JSON: {e}"),
        })?;

    let lifecycle_phase: LifecyclePhase = json_enum!(
        LifecyclePhase,
        "lifecycle_phase",
        normalise_lifecycle_phase(&lifecycle_phase_raw)
    )?;
    let ownership_state: OwnershipState = json_enum!(
        OwnershipState,
        "ownership_state",
        normalise_ownership_state(&ownership_state_raw)
    )?;
    let eligibility_state: EligibilityState = json_enum!(
        EligibilityState,
        "eligibility_state",
        normalise_eligibility_state(&eligibility_state_raw)
    )?;
    let public_state: PublicState = json_enum!(
        PublicState,
        "public_state",
        normalise_public_state(&public_state_raw)
    )?;
    let attempt_state: AttemptState = json_enum!(
        AttemptState,
        "attempt_state",
        normalise_attempt_state(&attempt_state_raw)
    )?;
    let blocking_reason: BlockingReason = match blocking_reason_opt
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        None => BlockingReason::None,
        Some(raw) => json_enum!(BlockingReason, "blocking_reason", raw)?,
    };
    let terminal_outcome = derive_terminal_outcome(
        normalise_lifecycle_phase(&lifecycle_phase_raw),
        &lifecycle_phase_raw,
        attempt_outcome_opt.as_deref(),
    );

    let state_vector = StateVector {
        lifecycle_phase,
        ownership_state,
        eligibility_state,
        blocking_reason,
        terminal_outcome,
        attempt_state,
        public_state,
    };

    // Scalar raw_fields fields (namespace, execution_kind, blocking_detail).
    let mut namespace = String::new();
    let mut execution_kind = String::new();
    let mut blocking_detail = String::new();
    if let JsonValue::Object(map) = &raw_fields {
        if let Some(JsonValue::String(s)) = map.get("namespace") {
            namespace = s.clone();
        }
        if let Some(JsonValue::String(s)) = map.get("execution_kind") {
            execution_kind = s.clone();
        }
        if let Some(JsonValue::String(s)) = map.get("blocking_detail") {
            blocking_detail = s.clone();
        }
    }

    // `ExecutionInfo::flow_id` is the bare UUID wire form (per PG impl).
    let flow_id = flow_id_blob
        .as_deref()
        .and_then(|bytes| Uuid::from_slice(bytes).ok())
        .map(|u| u.to_string());

    // Checked integer conversions — surface storage-tier corruption
    // loudly instead of silently truncating (copilot review).
    let priority_i32: i32 = i32::try_from(priority).map_err(|e| EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail: format!("exec_core: priority out of i32 range: {priority}: {e}"),
    })?;
    let attempt_index_u32: u32 = u32::try_from(attempt_index.max(0)).map_err(|e| {
        EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!(
                "exec_core: attempt_index out of u32 range: {attempt_index}: {e}"
            ),
        }
    })?;

    Ok(Some(ExecutionInfo {
        execution_id: id.clone(),
        namespace,
        lane_id,
        priority: priority_i32,
        execution_kind,
        state_vector,
        public_state,
        created_at: created_at_ms.to_string(),
        started_at: started_at_ms_opt.map(|v| v.to_string()),
        completed_at: terminal_at_ms_opt.map(|v| v.to_string()),
        current_attempt_index: attempt_index_u32,
        flow_id,
        blocking_detail,
    }))
}

// ─── get_execution_result ──────────────────────────────────────────

/// RFC-020 §4.1 + §7.8 — `ff_exec_core.result` BLOB point read.
/// Current-attempt semantics (Fork 3 Rev 7). `Ok(None)` when the
/// execution is missing or `result` is NULL.
pub(crate) async fn get_execution_result_impl(
    pool: &SqlitePool,
    id: &ExecutionId,
) -> Result<Option<Vec<u8>>, EngineError> {
    let (part, exec_uuid) = split_exec_id(id)?;

    let row: Option<(Option<Vec<u8>>,)> = sqlx::query_as(q_read::SELECT_EXECUTION_RESULT_SQL)
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(pool)
        .await
        .map_err(map_sqlx_error)?;

    match row {
        None => Ok(None),
        Some((payload,)) => Ok(payload),
    }
}

// ─── read_execution_context (v0.12 agnostic-SDK prep, PR-1) ────────

/// Point-read of `(input_payload, execution_kind, tags)` from
/// `ff_exec_core`. Mirrors the Postgres impl
/// (`ff-backend-postgres::exec_core::read_execution_context_impl`):
/// payload is the `payload` BLOB column; `execution_kind` + `tags`
/// live inside the `raw_fields` JSON text column.
///
/// Returns [`EngineError::Validation { kind: InvalidInput, .. }`](EngineError::Validation)
/// when the execution does not exist — the SDK worker only calls this
/// post-claim, so a missing row is an invariant violation rather than
/// a routine `Ok(None)`.
pub(crate) async fn read_execution_context_impl(
    pool: &SqlitePool,
    id: &ExecutionId,
) -> Result<ExecutionContext, EngineError> {
    let (part, exec_uuid) = split_exec_id(id)?;

    let row = sqlx::query(
        r#"
        SELECT payload, raw_fields
          FROM ff_exec_core
         WHERE partition_key = ?1 AND execution_id = ?2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("read_execution_context: execution not found: {id}"),
        });
    };

    let payload: Option<Vec<u8>> = row.try_get("payload").map_err(map_sqlx_error)?;
    let raw_fields_str: String = row.try_get("raw_fields").map_err(map_sqlx_error)?;
    let raw_fields: JsonValue =
        serde_json::from_str(&raw_fields_str).map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!("exec_core: raw_fields not valid JSON: {e}"),
        })?;

    let input_payload = payload.unwrap_or_default();

    let (execution_kind, tags) = match &raw_fields {
        JsonValue::Object(map) => {
            let kind = map
                .get("execution_kind")
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned())
                .unwrap_or_default();
            let tags: HashMap<String, String> = match map.get("tags") {
                Some(JsonValue::Object(tag_map)) => tag_map
                    .iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                    .collect(),
                _ => HashMap::new(),
            };
            (kind, tags)
        }
        _ => (String::new(), HashMap::new()),
    };

    Ok(ExecutionContext::new(input_payload, execution_kind, tags))
}

// ─── read_current_attempt_index (v0.12 agnostic-SDK prep, PR-3) ────

/// Point-read of `attempt_index` from `ff_exec_core`. Used by the SDK
/// worker on the `claim_from_resume_grant` path before dispatching
/// `claim_resumed_execution`. Missing row → `InvalidInput` (same
/// convention as [`read_execution_context_impl`]).
pub(crate) async fn read_current_attempt_index_impl(
    pool: &SqlitePool,
    id: &ExecutionId,
) -> Result<ff_core::types::AttemptIndex, EngineError> {
    let (part, exec_uuid) = split_exec_id(id)?;

    let row = sqlx::query(
        r#"
        SELECT attempt_index
          FROM ff_exec_core
         WHERE partition_key = ?1 AND execution_id = ?2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "read_current_attempt_index: execution not found: {id}"
            ),
        });
    };
    let attempt_index_i: i64 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let attempt_index_u: u32 = u32::try_from(attempt_index_i.max(0)).map_err(|e| {
        EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!(
                "exec_core: attempt_index out of u32 range: {attempt_index_i}: {e}"
            ),
        }
    })?;
    Ok(ff_core::types::AttemptIndex::new(attempt_index_u))
}

// ─── read_total_attempt_count (v0.12 PR-5.5 retry-path fix) ────────

/// Point-read of `total_attempt_count` from `ff_exec_core.raw_fields`
/// JSON. The field is stored as a JSON string inside the raw_fields
/// bag (seeded to `"0"` by `build_raw_fields`) — mirrors PG.
///
/// SQL uses `CAST(json_extract(raw_fields, '$.total_attempt_count')
/// AS INTEGER)`; the same idiom `queries/operator.rs` uses for
/// `replay_count`. Missing row → `InvalidInput`; missing/null field →
/// `0` (FCALL surface handles the loud error case).
pub(crate) async fn read_total_attempt_count_impl(
    pool: &SqlitePool,
    id: &ExecutionId,
) -> Result<ff_core::types::AttemptIndex, EngineError> {
    let (part, exec_uuid) = split_exec_id(id)?;

    let row = sqlx::query(
        r#"
        SELECT CAST(json_extract(raw_fields, '$.total_attempt_count')
                    AS INTEGER) AS total_attempt_count
          FROM ff_exec_core
         WHERE partition_key = ?1 AND execution_id = ?2
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "read_total_attempt_count: execution not found: {id}"
            ),
        });
    };
    let total_i: Option<i64> = row
        .try_get("total_attempt_count")
        .map_err(map_sqlx_error)?;
    let count = total_i
        .and_then(|v| u32::try_from(v.max(0)).ok())
        .unwrap_or(0);
    Ok(ff_core::types::AttemptIndex::new(count))
}

/// Point-read of a waitpoint's HMAC token by `(partition, waitpoint_id)`
/// for the signal-bridge resume path. Mirrors the Postgres impl at
/// `ff-backend-postgres/src/suspend_ops.rs::read_waitpoint_token_impl`.
///
/// Missing row → `Ok(None)`. Empty `token` → `Ok(None)` (parity with
/// the Valkey HGET mapping, even though the column is `NOT NULL`).
pub(crate) async fn read_waitpoint_token_impl(
    pool: &SqlitePool,
    partition: &ff_core::partition::PartitionKey,
    waitpoint_id: &ff_core::types::WaitpointId,
) -> Result<Option<String>, EngineError> {
    let part: i64 = partition
        .parse()
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("partition_key: {e}"),
        })?
        .index as i64;
    let wp_uuid_val = Uuid::parse_str(&waitpoint_id.to_string()).map_err(|e| {
        EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("waitpoint_id not a UUID: {e}"),
        }
    })?;
    let row: Option<(String,)> = sqlx::query_as(
        crate::queries::waitpoint::SELECT_WAITPOINT_TOKEN_BY_ID_SQL,
    )
    .bind(part)
    .bind(wp_uuid_val)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;
    Ok(row.map(|(t,)| t).filter(|s| !s.is_empty()))
}
