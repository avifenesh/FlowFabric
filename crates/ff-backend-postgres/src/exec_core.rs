//! exec_core trait-method family — Postgres impls.
//!
//! **RFC-v0.7 Wave 4 (Agent A).** Implements the five exec-core write
//! + read methods for the Postgres backend:
//!
//!  1. [`create_execution_impl`] — inherent entry on
//!     [`super::PostgresBackend`]. Replaces the Valkey `ff_create_execution`
//!     FCALL with a single `INSERT ... ON CONFLICT DO NOTHING` for
//!     idempotency (idempotency-key replay mirrors the FCALL's
//!     `Duplicate` outcome). Also seeds the global [`ff_lane_registry`]
//!     row on first sight of a lane (Q6 adjudication — dynamic lanes
//!     get a registry entry here as well as at server-boot seeding).
//!  2. [`describe_execution_impl`] — single-row `SELECT` against
//!     `ff_exec_core`, decoded into [`ExecutionSnapshot`] via the
//!     shared [`ff_core::contracts::decode::build_execution_snapshot`]
//!     helper so the snapshot shape matches the Valkey backend
//!     bit-for-bit.
//!  3. [`list_executions_impl`] — cursor-paginated forward scan over
//!     one `partition_key`, `ORDER BY execution_id ASC`. Uses the
//!     N+1 trick (fetch `limit+1` rows) to decide `next_cursor`.
//!  4. [`cancel_impl`] — transactional `SELECT ... FOR UPDATE` +
//!     `UPDATE` to transition the exec row to
//!     `public_state='cancelled'`, `lifecycle_phase='terminal'`,
//!     setting `terminal_at_ms` + `cancellation_reason`. Per Q11 the
//!     default READ COMMITTED isolation suffices; the row lock
//!     narrows the RMW to one writer.
//!  5. [`resolve_execution_flow_id_impl`] — one-column lookup by
//!     `execution_id` (the unique index lets pg skip partition
//!     pruning for this admin-tooling path).
//!
//! Spec authority: `rfcs/drafts/v0.7-migration-master.md` Q5 (partition
//! math), Q11 (isolation), Q14 (dual-backend — no cross-backend
//! fields).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use ff_core::contracts::decode::build_execution_snapshot;
use ff_core::contracts::{
    CreateExecutionArgs, ExecutionContext, ExecutionInfo, ExecutionSnapshot, ListExecutionsPage,
};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::partition::{PartitionConfig, PartitionKey};
use ff_core::state::{
    AttemptState, BlockingReason, EligibilityState, LifecyclePhase, OwnershipState, PublicState,
    StateVector, TerminalOutcome,
};
use ff_core::types::{ExecutionId, FlowId};
use serde_json::Value as JsonValue;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;

/// Extract the raw UUID suffix from an [`ExecutionId`]'s wire form
/// (`"{fp:N}:<uuid>"`). The constructors / [`ExecutionId::parse`]
/// guarantee the `}:` separator exists and the suffix is a valid
/// UUID.
fn eid_uuid(eid: &ExecutionId) -> Uuid {
    let s = eid.as_str();
    // Shape is enforced by ExecutionId constructors: `{fp:N}:<uuid>`.
    let suffix = s
        .split_once("}:")
        .map(|(_, u)| u)
        .expect("ExecutionId has `}:` separator (invariant)");
    Uuid::parse_str(suffix).expect("ExecutionId suffix is a valid UUID (invariant)")
}

/// Build a typed [`FlowId`] / [`ExecutionId`] string back from a
/// partition index + UUID. Keeps the `{fp:N}:<uuid>` wire shape the
/// rest of FF expects.
fn eid_from_parts(partition: u16, uuid: Uuid) -> Result<ExecutionId, EngineError> {
    let s = format!("{{fp:{partition}}}:{uuid}");
    ExecutionId::parse(&s).map_err(|e| EngineError::Validation {
        kind: ValidationKind::Corruption,
        detail: format!("exec_core: execution_id: could not reassemble '{s}': {e}"),
    })
}

fn now_ms() -> i64 {
    let d = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after UNIX_EPOCH");
    (d.as_millis() as i64).max(0)
}

// ─── create_execution ─────────────────────────────────────────────

/// Insert one `ff_exec_core` row (idempotent on primary key) + seed
/// the lane registry.
///
/// Idempotent replay: on primary-key conflict we treat the call as a
/// successful duplicate and return `Ok(args.execution_id)` — matching
/// `CreateExecutionResult::Duplicate` from the Valkey FCALL path.
pub(super) async fn create_execution_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    args: CreateExecutionArgs,
) -> Result<ExecutionId, EngineError> {
    let partition_key: i16 = args.execution_id.partition() as i16;
    let execution_id = eid_uuid(&args.execution_id);
    let lane_id = args.lane_id.as_str().to_owned();
    let priority: i32 = args.priority;
    let created_at_ms: i64 = args.now.0;
    let deadline_at_ms: Option<i64> = args.execution_deadline_at.map(|t| t.0);

    // `raw_fields` carries every CreateExecution arg that doesn't map
    // to a typed column today. Describe_execution reads them back via
    // `build_execution_snapshot`, which speaks HashMap<String,String>
    // — so we store strings here.
    let mut raw: serde_json::Map<String, JsonValue> = serde_json::Map::new();
    raw.insert(
        "namespace".into(),
        JsonValue::String(args.namespace.as_str().to_owned()),
    );
    raw.insert("execution_kind".into(), JsonValue::String(args.execution_kind));
    raw.insert(
        "creator_identity".into(),
        JsonValue::String(args.creator_identity),
    );
    if let Some(k) = args.idempotency_key {
        raw.insert("idempotency_key".into(), JsonValue::String(k));
    }
    if let Some(enc) = args.payload_encoding {
        raw.insert("payload_encoding".into(), JsonValue::String(enc));
    }
    // last_mutation_at mirrors Valkey's exec_core field — initialised
    // to created_at on first write.
    raw.insert(
        "last_mutation_at".into(),
        JsonValue::String(created_at_ms.to_string()),
    );
    raw.insert(
        "total_attempt_count".into(),
        JsonValue::String("0".to_owned()),
    );
    // Tags live under raw_fields.tags as a JSON object keyed by tag name.
    let tags_json: serde_json::Map<String, JsonValue> = args
        .tags
        .into_iter()
        .map(|(k, v)| (k, JsonValue::String(v)))
        .collect();
    raw.insert("tags".into(), JsonValue::Object(tags_json));

    let raw_fields = JsonValue::Object(raw);
    let policy_json: Option<JsonValue> = match args.policy {
        Some(p) => Some(serde_json::to_value(&p).map_err(|e| EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("create_execution: policy: serialize failed: {e}"),
        })?),
        None => None,
    };

    // Create exec row + seed lane registry in one transaction so a
    // concurrent lane-list doesn't see an exec without the lane
    // registry row.
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            required_capabilities, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms, deadline_at_ms,
            payload, policy, raw_fields
        ) VALUES (
            $1, $2, NULL, $3,
            '{}'::text[], 0,
            'submitted', 'unowned', 'eligible_now',
            'waiting', 'pending',
            $4, $5, $6,
            $7, $8, $9
        )
        ON CONFLICT (partition_key, execution_id) DO NOTHING
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .bind(&lane_id)
    .bind(priority)
    .bind(created_at_ms)
    .bind(deadline_at_ms)
    .bind(&args.input_payload)
    .bind(policy_json)
    .bind(&raw_fields)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Lane registry: Q6 — dynamic lanes seed here on first use.
    sqlx::query(
        r#"
        INSERT INTO ff_lane_registry (lane_id, registered_at_ms, registered_by)
        VALUES ($1, $2, $3)
        ON CONFLICT (lane_id) DO NOTHING
        "#,
    )
    .bind(&lane_id)
    .bind(created_at_ms)
    .bind("create_execution")
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(args.execution_id)
}

// ─── describe_execution ──────────────────────────────────────────

pub(super) async fn describe_execution_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<Option<ExecutionSnapshot>, EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    let row = sqlx::query(
        r#"
        SELECT flow_id, lane_id, public_state, blocking_reason,
               created_at_ms, raw_fields
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let flow_id_uuid: Option<Uuid> = row.try_get("flow_id").map_err(map_sqlx_error)?;
    let lane_id: String = row.try_get("lane_id").map_err(map_sqlx_error)?;
    let public_state: String = row.try_get("public_state").map_err(map_sqlx_error)?;
    let blocking_reason: Option<String> =
        row.try_get("blocking_reason").map_err(map_sqlx_error)?;
    let created_at_ms: i64 = row.try_get("created_at_ms").map_err(map_sqlx_error)?;
    let raw_fields: JsonValue = row.try_get("raw_fields").map_err(map_sqlx_error)?;

    // Build the HashMap<String,String> shape the shared decoder
    // consumes. The Postgres row projection + JSON raw_fields together
    // carry every field `build_execution_snapshot` reads.
    let mut core: HashMap<String, String> = HashMap::new();
    core.insert("public_state".into(), public_state);
    core.insert("lane_id".into(), lane_id);
    if let Some(fid) = flow_id_uuid {
        // Reassemble `{fp:N}:<uuid>` using the exec's own partition
        // (RFC-011 co-location: exec + flow share a partition).
        core.insert(
            "flow_id".into(),
            format!("{{fp:{part}}}:{fid}", part = id.partition()),
        );
    }
    if let Some(r) = blocking_reason {
        core.insert("blocking_reason".into(), r);
    }
    core.insert("created_at".into(), created_at_ms.to_string());

    // raw_fields-derived scalars. build_execution_snapshot hard-requires
    // `last_mutation_at`; `create_execution_impl` seeds it to
    // `created_at_ms`, subsequent mutators (future waves) bump it.
    if let JsonValue::Object(map) = &raw_fields {
        for key in [
            "namespace",
            "last_mutation_at",
            "total_attempt_count",
            "current_attempt_id",
            "current_attempt_index",
            "current_waitpoint_id",
            "blocking_detail",
        ] {
            if let Some(JsonValue::String(s)) = map.get(key) {
                core.insert(key.to_owned(), s.clone());
            }
        }
    }

    // Tags hash: extract from raw_fields.tags JSON object.
    let tags_raw: HashMap<String, String> = match &raw_fields {
        JsonValue::Object(map) => match map.get("tags") {
            Some(JsonValue::Object(tag_map)) => tag_map
                .iter()
                .filter_map(|(k, v)| {
                    v.as_str().map(|s| (k.clone(), s.to_owned()))
                })
                .collect(),
            _ => HashMap::new(),
        },
        _ => HashMap::new(),
    };

    build_execution_snapshot(id.clone(), &core, tags_raw)
}

// ─── read_execution_context ──────────────────────────────────────

/// Point-read of `(input_payload, execution_kind, tags)` from
/// `ff_exec_core`. Used by the SDK worker to populate a freshly
/// claimed [`ClaimedTask`](ff_sdk::ClaimedTask). Returns
/// [`EngineError::Validation { kind: InvalidInput, .. }`](ff_core::engine_error::EngineError::Validation)
/// when the execution does not exist (the SDK only calls this
/// post-claim, so a missing row is an invariant violation).
///
/// Single `SELECT payload, raw_fields` on `(partition_key, execution_id)`;
/// `execution_kind` + `tags` are projected out of `raw_fields`. The
/// payload lives in its own `BYTEA` column so we don't round-trip the
/// bytes through JSON encoding.
pub(super) async fn read_execution_context_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<ExecutionContext, EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    let row = sqlx::query(
        r#"
        SELECT payload, raw_fields
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
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
    let raw_fields: JsonValue = row.try_get("raw_fields").map_err(map_sqlx_error)?;

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

// ─── read_current_attempt_index ──────────────────────────────────

/// Point-read of `attempt_index` from `ff_exec_core`. Used by the SDK
/// worker on the `claim_from_resume_grant` path before dispatching
/// `claim_resumed_execution`. Missing row → `InvalidInput` (same
/// convention as [`read_execution_context_impl`]).
pub(super) async fn read_current_attempt_index_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<ff_core::types::AttemptIndex, EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    let row = sqlx::query(
        r#"
        SELECT attempt_index
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
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
    let attempt_index_i: i32 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
    // `.max(0) as u32`: column is `integer NOT NULL DEFAULT 0`, but a
    // negative value (from a hand-edited row, say) would wrap to a very
    // large u32 under a direct cast. Clamp to 0 — matches the PG
    // convention used elsewhere in this file for attempt-index reads
    // (see `attempt.rs::claim_impl`).
    let attempt_index =
        ff_core::types::AttemptIndex::new(attempt_index_i.max(0) as u32);
    Ok(attempt_index)
}

// ─── read_total_attempt_count ────────────────────────────────────

/// Point-read of `total_attempt_count` from `ff_exec_core.raw_fields`
/// JSONB. Used by the SDK worker's `claim_from_grant` path to compute
/// the next fresh attempt index (retry-path fix, v0.12 PR-5.5). The
/// field is seeded to `"0"` by `create_execution_impl` and bumped by
/// the same SQL transactions that advance the attempt row — it lives
/// in the JSON bag rather than a column, matching the SQLite sibling.
///
/// Missing row → `InvalidInput`. Missing/non-numeric field → `0` (the
/// FCALL surface handles the loud error case; this pre-read is
/// best-effort).
pub(super) async fn read_total_attempt_count_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<ff_core::types::AttemptIndex, EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    let row = sqlx::query(
        r#"
        SELECT raw_fields ->> 'total_attempt_count' AS total_attempt_count
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
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
    let raw: Option<String> = row
        .try_get("total_attempt_count")
        .map_err(map_sqlx_error)?;
    let count = raw
        .as_deref()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    Ok(ff_core::types::AttemptIndex::new(count))
}

// ─── list_executions ─────────────────────────────────────────────

pub(super) async fn list_executions_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    partition: PartitionKey,
    cursor: Option<ExecutionId>,
    limit: usize,
) -> Result<ListExecutionsPage, EngineError> {
    if limit == 0 {
        return Ok(ListExecutionsPage::new(Vec::new(), None));
    }
    // Parse the partition tag into a partition index (u16).
    let parsed = partition.parse().map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("list_executions: partition: '{partition}': {e}"),
    })?;
    let partition_key: i16 = parsed.index as i16;
    let cursor_uuid: Option<Uuid> = cursor.as_ref().map(eid_uuid);

    // N+1 trick: fetch one extra row to decide `next_cursor`.
    let effective_limit = limit.min(1000);
    let fetch_limit: i64 = (effective_limit as i64).saturating_add(1);

    let rows = sqlx::query(
        r#"
        SELECT execution_id
        FROM ff_exec_core
        WHERE partition_key = $1
          AND ($2::uuid IS NULL OR execution_id > $2)
        ORDER BY execution_id ASC
        LIMIT $3
        "#,
    )
    .bind(partition_key)
    .bind(cursor_uuid)
    .bind(fetch_limit)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut ids: Vec<ExecutionId> = Vec::with_capacity(rows.len());
    for row in &rows {
        let u: Uuid = row.try_get("execution_id").map_err(map_sqlx_error)?;
        ids.push(eid_from_parts(parsed.index, u)?);
    }

    let has_more = ids.len() > effective_limit;
    if has_more {
        ids.truncate(effective_limit);
    }
    let next_cursor = if has_more { ids.last().cloned() } else { None };
    Ok(ListExecutionsPage::new(ids, next_cursor))
}

// ─── cancel ──────────────────────────────────────────────────────

/// Cancel one execution by handle (single-execution cancel; flow-wide
/// cancel is the sibling Wave-4c agent's lane).
///
/// Q11: runs under READ COMMITTED with an explicit row lock via
/// `SELECT ... FOR UPDATE` to serialise the state check + UPDATE.
/// Already-terminal executions are a successful no-op (idempotent
/// replay); mismatched lease surfaces as `EngineError::Contention`.
pub(super) async fn cancel_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    execution_id: &ExecutionId,
    reason: &str,
) -> Result<(), EngineError> {
    let partition_key: i16 = execution_id.partition() as i16;
    let eid_uuid = eid_uuid(execution_id);
    let now = now_ms();

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    let current: Option<(String, String)> = sqlx::query_as(
        r#"
        SELECT lifecycle_phase, public_state
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        FOR UPDATE
        "#,
    )
    .bind(partition_key)
    .bind(eid_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some((lifecycle_phase, public_state)) = current else {
        // Not-found on a cancel is an operator-visible state error —
        // matches the Valkey FCALL's `execution_not_found` code.
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "cancel: execution_id={execution_id}: row not found on partition_key={partition_key}"
            ),
        });
    };

    // Terminal-state replay is a successful no-op. Mirrors Valkey's
    // `reconcile_terminal_replay` path.
    if lifecycle_phase == "terminal" {
        tx.rollback().await.map_err(map_sqlx_error)?;
        // Idempotent success iff already cancelled; other terminal states
        // (completed/failed/expired/skipped) surface as a state conflict
        // so operators don't silently squash a real terminal outcome.
        return if public_state == "cancelled" {
            Ok(())
        } else {
            Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: format!(
                    "cancel: execution_id={execution_id}: already terminal in state '{public_state}'"
                ),
            })
        };
    }

    sqlx::query(
        r#"
        UPDATE ff_exec_core
        SET lifecycle_phase     = 'terminal',
            ownership_state     = 'unowned',
            eligibility_state   = 'not_applicable',
            public_state        = 'cancelled',
            attempt_state       = 'cancelled',
            terminal_at_ms      = $3,
            cancellation_reason = $4,
            cancelled_by        = 'worker',
            raw_fields          = jsonb_set(raw_fields, '{last_mutation_at}', to_jsonb($3::text))
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(eid_uuid)
    .bind(now)
    .bind(reason)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // #355: clear the current attempt's `outcome` so a later
    // `read_execution_info` doesn't surface a stale
    // `retry`/`interrupted` terminal-outcome on a cancelled row.
    // Mirrors the equivalent clear on the `cancel_flow` member loop
    // (`flow.rs`).
    sqlx::query(
        r#"
        UPDATE ff_attempt
           SET outcome = NULL
         WHERE partition_key = $1
           AND execution_id  = $2
           AND attempt_index = (SELECT attempt_index FROM ff_exec_core
                                 WHERE partition_key = $1 AND execution_id = $2)
        "#,
    )
    .bind(partition_key)
    .bind(eid_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;
    Ok(())
}

// ─── resolve_execution_flow_id ───────────────────────────────────

pub(super) async fn resolve_execution_flow_id_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    eid: &ExecutionId,
) -> Result<Option<FlowId>, EngineError> {
    let partition_key: i16 = eid.partition() as i16;
    let execution_id = eid_uuid(eid);

    let row: Option<(Option<Uuid>,)> = sqlx::query_as(
        r#"
        SELECT flow_id
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some((maybe_fid,)) = row else {
        return Ok(None);
    };
    let Some(fid_uuid) = maybe_fid else {
        return Ok(None);
    };
    let s = fid_uuid.to_string();
    FlowId::parse(&s)
        .map(Some)
        .map_err(|e| EngineError::Validation {
            kind: ValidationKind::Corruption,
            detail: format!(
                "resolve_execution_flow_id: exec_core.flow_id='{s}' is not a valid FlowId: {e}"
            ),
        })
}

// ─── RFC-020 Wave 9 Spine-B — read model (3 methods) ─────────────
//
// All three reads project from the `ff_exec_core` row for the target
// `(partition_key, execution_id)`; `read_execution_info` additionally
// LATERAL-joins `ff_attempt` for the current-attempt row (outcome).
// READ COMMITTED is sufficient — all three are single-query, read-only,
// no CAS. Per RFC §4.1 + §7.8, `get_execution_result` is
// current-attempt semantics (matches Valkey's `GET ctx.result()`
// primitive; `result` column on `ff_exec_core`).
//
// ── Column-literal alphabet: the read-boundary adapter (#354) ────
//
// The Postgres `ff_exec_core` state columns — `lifecycle_phase`,
// `ownership_state`, `eligibility_state`, `public_state`,
// `attempt_state` — encode `(phase × eligibility × terminal-outcome)`
// in a **richer private alphabet** than the canonical `ff_core::state`
// enums (`LifecyclePhase`, `OwnershipState`, `EligibilityState`,
// `PublicState`, `AttemptState`). Write sites in this crate legitimately
// produce Postgres-specific literals that are **not** members of the
// public enums:
//
//   * `flow.rs:672-687` (cancel-member loop) writes `cancelled` to
//     `lifecycle_phase`, `eligibility_state`, and `public_state` on
//     flow-cancel.
//   * `operator.rs:211-235` (`cancel_execution`) likewise writes
//     `cancelled`.
//   * `exec_core::cancel_impl` writes `cancelled` to `public_state` +
//     `attempt_state` and the sentinel `terminal` to `lifecycle_phase`
//     (the Handle-level single-exec cancel path).
//   * `suspend_ops.rs:958` (resume-claim) writes bare `running` to
//     `public_state` (canonical form is `active`).
//   * `suspend_ops.rs:585` writes `released` to `ownership_state`
//     (canonical form is `unowned`).
//   * Scheduler-transitional paths write `pending_claim` to
//     `eligibility_state` and `attempt_state`; legacy dispatch paths
//     write `blocked` to `lifecycle_phase`.
//
// **This module is the documented read-boundary adapter** (Option B
// per owner decision on #354 / RFC-020 §4.1 Revision 8). The
// `normalise_*` helpers below collapse each column literal to the
// closest public-enum variant before `json_enum!` deserialises the
// string into the enum type; unknown tokens fall through the match arm
// unchanged and `json_enum!` surfaces them as
// `ValidationKind::Corruption` rather than silently defaulting.
//
// **Invariant for new code:**
//
//   * New read paths against these columns MUST call through the
//     `normalise_*` helpers before constructing a public enum.
//   * New write paths MAY introduce new column literals (the column is
//     the authoritative audit trail of which backend phase produced the
//     row), but MUST update the corresponding `normalise_*` arm in the
//     same PR so the read path stays total.
//   * Option A — migrating write paths to canonical literals only —
//     was considered and rejected (RFC-020 §4.1 Revision 8): it would
//     relocate the `(phase × eligibility × terminal-outcome)` encoding
//     from SQL columns into per-write computation without reducing
//     mapping-layer complexity, and would lose the column-level
//     distinction between e.g. `cancelled` (terminal-by-cancel) and
//     `terminal` (terminal-by-success/fail/expire/skip) that the
//     `derive_terminal_outcome` helper below depends on.

// Normalisation maps: `ff_exec_core` literal → closest
// serde-snake_case enum variant. Each arm is grounded in an actual
// write site in this crate; unknown tokens fall through so
// `json_enum!` surfaces `Corruption` loudly.

fn normalise_lifecycle_phase(raw: &str) -> &str {
    match raw {
        // Terminal family — `cancelled` is Postgres-specific
        // (flow.rs:674 writes it directly), `terminal` is canonical.
        "cancelled" | "terminal" => "terminal",
        // Pre-runnable + runnable variants collapse to `Runnable`
        // (the user-facing enum only distinguishes 5 phases). `blocked`
        // is a legacy literal still referenced in dispatch.rs:352's
        // CASE clause but no longer written by current paths.
        "pending" | "runnable" | "eligible" | "blocked" => "runnable",
        "active" => "active",
        "suspended" => "suspended",
        "submitted" => "submitted",
        other => other,
    }
}

fn normalise_ownership_state(raw: &str) -> &str {
    match raw {
        // `released` is a Postgres-specific post-suspension marker
        // (suspend_ops.rs:585); nearest enum variant is `Unowned`.
        "released" | "unowned" => "unowned",
        "leased" => "leased",
        "lease_expired_reclaimable" => "lease_expired_reclaimable",
        "lease_revoked" => "lease_revoked",
        other => other,
    }
}

fn normalise_eligibility_state(raw: &str) -> &str {
    match raw {
        // Terminal-cancelled rows → `NotApplicable` (Valkey parity).
        "cancelled" => "not_applicable",
        // Scheduler ClaimGrant transitional state; nearest user-facing
        // variant is still `EligibleNow` (the exec is about to be
        // claimed — scheduler has picked it but the worker hasn't
        // acknowledged yet).
        "pending_claim" => "eligible_now",
        other => other,
    }
}

fn normalise_attempt_state(raw: &str) -> &str {
    match raw {
        // `pending` is the Postgres initial-insert literal
        // (exec_core.rs:166); `pending_claim` is the scheduler transitional
        // write. Both collapse to `PendingFirstAttempt`.
        "pending" | "pending_claim" => "pending_first_attempt",
        // Bare `running` from the suspension-resume claim path
        // (suspend_ops.rs:958); canonical form is `running_attempt`.
        "running" => "running_attempt",
        // `cancelled` attempt_state (flow.rs cancel path) has no direct
        // enum variant; nearest is `AttemptTerminal`.
        "cancelled" => "attempt_terminal",
        other => other,
    }
}

/// Collapse Postgres `public_state` literals to the
/// `PublicState` snake_case serde form.
fn normalise_public_state(raw: &str) -> &str {
    match raw {
        // Postgres writes the bare `running` literal on the
        // resume-claim path (suspend_ops.rs:958). Valkey / the
        // `PublicState` enum spell it `active`.
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

/// Map an `ff_attempt.outcome` string to a [`TerminalOutcome`]. Only
/// meaningful when `lifecycle_phase` is terminal/cancelled; otherwise
/// returns `TerminalOutcome::None`.
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

/// RFC-020 §4.1 — `read_execution_state`: single-column point read of
/// `public_state`. `Ok(None)` when the execution is missing.
pub(super) async fn read_execution_state_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<Option<PublicState>, EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    let row: Option<(String,)> = sqlx::query_as(
        r#"
        SELECT public_state
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
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

/// RFC-020 §4.1 — `read_execution_info`: multi-column projection of
/// `ff_exec_core` + `LEFT JOIN LATERAL` on `ff_attempt` (current attempt
/// row) to build [`ExecutionInfo`]. `Ok(None)` when the execution is
/// missing. Partition-local (both tables co-located on `partition_key`,
/// RFC-011).
pub(super) async fn read_execution_info_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<Option<ExecutionInfo>, EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    // LATERAL join: `cur` pins to the current attempt (for
    // `outcome` — drives `TerminalOutcome`). Since migration 0016
    // (#356) `ff_exec_core.started_at_ms` is a set-once column
    // populated on first claim, so the `ExecutionInfo.started_at`
    // field reads directly from the base row — the second LATERAL
    // join on `ff_attempt.started_at_ms` (earliest non-NULL) is gone.
    let row = sqlx::query(
        r#"
        SELECT ec.flow_id,
               ec.lane_id,
               ec.priority,
               ec.lifecycle_phase,
               ec.ownership_state,
               ec.eligibility_state,
               ec.public_state,
               ec.attempt_state,
               ec.blocking_reason,
               ec.attempt_index,
               ec.created_at_ms,
               ec.terminal_at_ms,
               ec.started_at_ms,
               ec.raw_fields,
               cur.outcome       AS attempt_outcome
        FROM ff_exec_core ec
        LEFT JOIN LATERAL (
            SELECT outcome
            FROM ff_attempt
            WHERE partition_key = ec.partition_key
              AND execution_id  = ec.execution_id
              AND attempt_index = ec.attempt_index
        ) cur ON TRUE
        WHERE ec.partition_key = $1 AND ec.execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        return Ok(None);
    };

    let flow_id_uuid: Option<Uuid> = row.try_get("flow_id").map_err(map_sqlx_error)?;
    let lane_id: String = row.try_get("lane_id").map_err(map_sqlx_error)?;
    let priority: i32 = row.try_get("priority").map_err(map_sqlx_error)?;
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
    let attempt_index: i32 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let created_at_ms: i64 = row.try_get("created_at_ms").map_err(map_sqlx_error)?;
    let terminal_at_ms_opt: Option<i64> =
        row.try_get("terminal_at_ms").map_err(map_sqlx_error)?;
    let raw_fields: JsonValue = row.try_get("raw_fields").map_err(map_sqlx_error)?;
    let attempt_outcome_opt: Option<String> =
        row.try_get("attempt_outcome").map_err(map_sqlx_error)?;
    let started_at_ms_opt: Option<i64> =
        row.try_get("started_at_ms").map_err(map_sqlx_error)?;

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

    // Scalar fields from raw_fields JSON (namespace, execution_kind,
    // blocking_detail). Same shape as `describe_execution_impl` reads.
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

    // `ExecutionInfo.flow_id` is the bare UUID wire form of `FlowId`
    // (per `uuid_id!` macro — no hash-tag prefix). Valkey stores the
    // bare UUID in `exec_core["flow_id"]` too (Valkey parity).
    let flow_id = flow_id_uuid.map(|fid| fid.to_string());

    Ok(Some(ExecutionInfo {
        execution_id: id.clone(),
        namespace,
        lane_id,
        priority,
        execution_kind,
        state_vector,
        public_state,
        created_at: created_at_ms.to_string(),
        started_at: started_at_ms_opt.map(|v| v.to_string()),
        completed_at: terminal_at_ms_opt.map(|v| v.to_string()),
        current_attempt_index: attempt_index.max(0) as u32,
        flow_id,
        blocking_detail,
    }))
}

/// RFC-020 §4.1 + §7.8 — `get_execution_result`: current-attempt
/// semantics (matches Valkey's `GET ctx.result()`). Reads the `result`
/// column from `ff_exec_core`; `Ok(None)` when the execution is missing
/// or the result is `NULL` (not yet terminal, or cancelled without a
/// payload).
pub(super) async fn get_execution_result_impl(
    pool: &PgPool,
    _partition_config: &PartitionConfig,
    id: &ExecutionId,
) -> Result<Option<Vec<u8>>, EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    let row: Option<(Option<Vec<u8>>,)> = sqlx::query_as(
        r#"
        SELECT result
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    match row {
        None => Ok(None),
        Some((payload,)) => Ok(payload),
    }
}


// ─── set_execution_tag / get_execution_tag (issue #433) ──────────

/// Upsert a single namespaced tag into `raw_fields->'tags'->><key>`.
/// Matches the `EngineBackend::set_execution_tag` wire semantics: key
/// is assumed pre-validated by `ff_core::engine_backend::validate_tag_key`.
///
/// Missing execution → `EngineError::NotFound { entity: "execution" }`
/// (mirrors Valkey's `execution_not_found` FCALL mapping).
pub(super) async fn set_execution_tag_impl(
    pool: &PgPool,
    id: &ExecutionId,
    key: &str,
    value: &str,
) -> Result<(), EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    let result = sqlx::query(
        r#"
        UPDATE ff_exec_core
        SET raw_fields = jsonb_set(
            COALESCE(raw_fields, '{}'::jsonb),
            ARRAY['tags', $3::text],
            to_jsonb($4::text),
            true
        )
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .bind(key)
    .bind(value)
    .execute(pool)
    .await
    .map_err(map_sqlx_error)?;

    if result.rows_affected() == 0 {
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    }
    Ok(())
}

/// Point-read of `raw_fields->'tags'->><key>`. `Ok(None)` covers
/// missing-tag and missing-execution alike — matches Valkey's `HGET`
/// collapse (see `EngineBackend::get_execution_tag` rustdoc).
pub(super) async fn get_execution_tag_impl(
    pool: &PgPool,
    id: &ExecutionId,
    key: &str,
) -> Result<Option<String>, EngineError> {
    let partition_key: i16 = id.partition() as i16;
    let execution_id = eid_uuid(id);

    let row: Option<(Option<String>,)> = sqlx::query_as(
        r#"
        SELECT raw_fields->'tags'->>$3
        FROM ff_exec_core
        WHERE partition_key = $1 AND execution_id = $2
        "#,
    )
    .bind(partition_key)
    .bind(execution_id)
    .bind(key)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    Ok(row.and_then(|(tag,)| tag))
}
