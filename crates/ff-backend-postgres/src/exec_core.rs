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
use ff_core::contracts::{CreateExecutionArgs, ExecutionSnapshot, ListExecutionsPage};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::partition::{PartitionConfig, PartitionKey};
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

