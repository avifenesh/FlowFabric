//! Postgres flow-staging inherent methods (Wave 4i).
//!
//! Mirrors the ingress-layer Lua FCALLs that the Valkey backend ships
//! under `ff-server::Server` — these are **not** `EngineBackend` trait
//! methods. The Valkey shape is:
//!
//!   * `ff_create_flow`                — INSERT flow_core
//!   * `ff_add_execution_to_flow`      — stamp exec.flow_id + bump counters
//!   * `ff_stage_dependency_edge`      — CAS graph_revision + INSERT edge
//!   * `ff_apply_dependency_to_child`  — mark edge applied + bump edge_group
//!
//! Wave 4c already covers the read surface and `cancel_flow`. This
//! module adds the mutating ingress operations so flow-DAG construction
//! works on Postgres end-to-end. Wave 5a owns the completion-cascade
//! dispatch that consumes what `apply_dependency_to_child` writes.
//!
//! # Storage convention
//!
//! Matches Wave 4c's decoder in `flow.rs`:
//!
//!   * Flow-level extras (`flow_kind`, `namespace`, `node_count`,
//!     `edge_count`, `last_mutation_at_ms`) live in
//!     `ff_flow_core.raw_fields` jsonb.
//!   * Edge-level extras (`dependency_kind`, `data_passing_ref`,
//!     `staged_at_ms`, `applied_at_ms`, `edge_state`, `created_at_ms`,
//!     `created_by`, `satisfaction_condition`) live in
//!     `ff_edge.policy` jsonb (the only jsonb slot on that row).
//!
//! No schema migration is required — every new field fits in the
//! existing jsonb columns. This keeps Wave 4c's decoder and the
//! dispatch queries (which select on typed columns only) unchanged.
//!
//! # Membership
//!
//! `ff_add_execution_to_flow` / `ff_stage_dependency_edge` treat
//! membership as `ff_exec_core.flow_id = flow_id` — the same back-
//! pointer that Wave 4c's `cancel_flow` already queries. No separate
//! member table is introduced; RFC-011 flow/exec co-location makes the
//! back-pointer authoritative on the flow's partition.

use ff_core::contracts::{
    AddExecutionToFlowArgs, AddExecutionToFlowResult, ApplyDependencyToChildArgs,
    ApplyDependencyToChildResult, CreateFlowArgs, CreateFlowResult, StageDependencyEdgeArgs,
    StageDependencyEdgeResult,
};
use ff_core::engine_error::{ConflictKind, ContentionKind, EngineError, ValidationKind};
use ff_core::partition::PartitionConfig;
use ff_core::types::{ExecutionId, FlowId};
use serde_json::json;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;

/// Compute the flow partition byte (0..=255). Matches Wave 4c's helper.
fn flow_partition_byte(flow_id: &FlowId, pc: &PartitionConfig) -> i16 {
    ff_core::partition::flow_partition(flow_id, pc).index as i16
}

/// Parse the UUID suffix from a `{fp:N}:<uuid>` exec id.
fn parse_exec_uuid(eid: &ExecutionId) -> Result<Uuid, EngineError> {
    let s = eid.as_str();
    let Some(colon) = s.rfind("}:") else {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("execution_id missing '}}:' delimiter: {s}"),
        });
    };
    Uuid::parse_str(&s[colon + 2..]).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("execution_id uuid suffix: {e}"),
    })
}

/// `ff_create_flow` — idempotent INSERT into `ff_flow_core`.
///
/// Mirrors `lua/flow.lua :: ff_create_flow`. Graph_revision starts at 0;
/// `public_flow_state` starts at `'open'`. Typed columns carry what the
/// engine filters on; `flow_kind`, `namespace`, `node_count`,
/// `edge_count`, `last_mutation_at_ms` ride in `raw_fields` so Wave 4c's
/// decoder picks them up.
pub async fn create_flow(
    pool: &PgPool,
    pc: &PartitionConfig,
    args: &CreateFlowArgs,
) -> Result<CreateFlowResult, EngineError> {
    let part = flow_partition_byte(&args.flow_id, pc);
    let flow_uuid: Uuid = args.flow_id.0;
    let now_ms = args.now.0;

    let raw_fields = json!({
        "flow_kind": args.flow_kind,
        "namespace": args.namespace.as_str(),
        "node_count": 0,
        "edge_count": 0,
        "last_mutation_at_ms": now_ms,
    });

    // ON CONFLICT DO NOTHING makes the insert idempotent. `RETURNING`
    // fires only on the insert path; a conflict returns zero rows, so
    // we can branch on the row count.
    let inserted = sqlx::query(
        r#"
        INSERT INTO ff_flow_core
            (partition_key, flow_id, graph_revision, public_flow_state,
             created_at_ms, raw_fields)
        VALUES ($1, $2, 0, 'open', $3, $4)
        ON CONFLICT (partition_key, flow_id) DO NOTHING
        RETURNING flow_id
        "#,
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(now_ms)
    .bind(&raw_fields)
    .fetch_optional(pool)
    .await
    .map_err(map_sqlx_error)?;

    if inserted.is_some() {
        Ok(CreateFlowResult::Created {
            flow_id: args.flow_id.clone(),
        })
    } else {
        Ok(CreateFlowResult::AlreadySatisfied {
            flow_id: args.flow_id.clone(),
        })
    }
}

/// `ff_add_execution_to_flow` — stamp `exec.flow_id` + bump flow counters.
///
/// Single transaction:
///   1. Validate flow exists and is not terminal.
///   2. Validate exec exists.
///   3. If exec already on this flow → idempotent `AlreadyMember`.
///   4. If exec on a different flow → `Validation(InvalidInput)`
///      (RFC-007: an exec belongs to at most one flow).
///   5. Stamp `exec_core.flow_id` + bump `graph_revision` and
///      `raw_fields.node_count` on `flow_core`.
pub async fn add_execution_to_flow(
    pool: &PgPool,
    pc: &PartitionConfig,
    args: &AddExecutionToFlowArgs,
) -> Result<AddExecutionToFlowResult, EngineError> {
    let part = flow_partition_byte(&args.flow_id, pc);
    let flow_uuid: Uuid = args.flow_id.0;
    let exec_uuid = parse_exec_uuid(&args.execution_id)?;
    let now_ms = args.now.0;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 1. Flow must exist + not be terminal.
    let flow_row = sqlx::query(
        "SELECT public_flow_state, raw_fields FROM ff_flow_core \
         WHERE partition_key = $1 AND flow_id = $2 FOR UPDATE",
    )
    .bind(part)
    .bind(flow_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(flow_row) = flow_row else {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "flow_not_found".to_string(),
        });
    };
    let public_flow_state: String = flow_row.get("public_flow_state");
    if matches!(
        public_flow_state.as_str(),
        "cancelled" | "completed" | "failed"
    ) {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "flow_already_terminal".to_string(),
        });
    }

    // 2. Exec must exist. The partition co-location invariant
    //    (RFC-011 §7.3) guarantees the exec row, if any, lives on the
    //    flow's partition; use the flow partition_key directly.
    let exec_row = sqlx::query(
        "SELECT flow_id FROM ff_exec_core \
         WHERE partition_key = $1 AND execution_id = $2 FOR UPDATE",
    )
    .bind(part)
    .bind(exec_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(exec_row) = exec_row else {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "execution_not_found".to_string(),
        });
    };
    let existing_flow_id: Option<Uuid> = exec_row.get("flow_id");

    // 3. Idempotent: already on this flow.
    if existing_flow_id == Some(flow_uuid) {
        let raw: serde_json::Value = flow_row.get("raw_fields");
        let nc = raw
            .get("node_count")
            .and_then(|v| v.as_u64())
            .and_then(|n| u32::try_from(n).ok())
            .unwrap_or(0);
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(AddExecutionToFlowResult::AlreadyMember {
            execution_id: args.execution_id.clone(),
            node_count: nc,
        });
    }

    // 4. Cross-flow refusal.
    if let Some(other) = existing_flow_id
        && other != flow_uuid
    {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("already_member_of_different_flow:{other}"),
        });
    }

    // 5. Stamp exec.flow_id + bump flow counters.
    sqlx::query(
        "UPDATE ff_exec_core SET flow_id = $3 \
         WHERE partition_key = $1 AND execution_id = $2",
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(flow_uuid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let bumped = sqlx::query(
        r#"
        UPDATE ff_flow_core
           SET graph_revision = graph_revision + 1,
               raw_fields = raw_fields
                   || jsonb_build_object(
                       'node_count',
                       COALESCE((raw_fields->>'node_count')::int, 0) + 1,
                       'last_mutation_at_ms', $3::bigint
                   )
         WHERE partition_key = $1 AND flow_id = $2
         RETURNING (raw_fields->>'node_count')::int AS node_count
        "#,
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(now_ms)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    let new_nc: i32 = bumped.get("node_count");

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(AddExecutionToFlowResult::Added {
        execution_id: args.execution_id.clone(),
        new_node_count: u32::try_from(new_nc.max(0)).unwrap_or(0),
    })
}

/// `ff_stage_dependency_edge` — CAS on graph_revision + INSERT into
/// `ff_edge`.
///
/// Parity with `lua/flow.lua :: ff_stage_dependency_edge`:
///
///   * Rejects self-edges, cross-flow membership, terminal flows.
///   * Mismatching `expected_graph_revision` →
///     `Contention(StaleGraphRevision)`.
///   * Duplicate edge_id → `Conflict(DependencyAlreadyExists { .. })`.
///   * Cycle detection: not implemented here (deferred to a future
///     Wave; the Valkey path's `detect_cycle` uses BFS over adjacency
///     SETs that Postgres models as SELECTs — can land in Wave 4j /
///     Wave 5b without re-opening the contract).
///
/// On success: writes `staged_at_ms = now`, `applied_at_ms = null`,
/// `edge_state = 'pending'` into `policy` jsonb; bumps
/// `graph_revision` and `raw_fields.edge_count` on `flow_core`.
pub async fn stage_dependency_edge(
    pool: &PgPool,
    pc: &PartitionConfig,
    args: &StageDependencyEdgeArgs,
) -> Result<StageDependencyEdgeResult, EngineError> {
    if args.upstream_execution_id == args.downstream_execution_id {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "self_referencing_edge".to_string(),
        });
    }

    let part = flow_partition_byte(&args.flow_id, pc);
    let flow_uuid: Uuid = args.flow_id.0;
    let edge_uuid: Uuid = args.edge_id.0;
    let upstream_uuid = parse_exec_uuid(&args.upstream_execution_id)?;
    let downstream_uuid = parse_exec_uuid(&args.downstream_execution_id)?;
    let now_ms = args.now.0;
    let expected_rev = i64::try_from(args.expected_graph_revision).unwrap_or(i64::MAX);

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 1. Flow must exist + not terminal + graph_revision matches. CAS
    //    in one UPDATE: the WHERE clause pins the expected rev, and
    //    the RETURNING tells us whether we won the race.
    let bumped = sqlx::query(
        r#"
        UPDATE ff_flow_core
           SET graph_revision = graph_revision + 1,
               raw_fields = raw_fields
                   || jsonb_build_object(
                       'edge_count',
                       COALESCE((raw_fields->>'edge_count')::int, 0) + 1,
                       'last_mutation_at_ms', $4::bigint
                   )
         WHERE partition_key = $1
           AND flow_id = $2
           AND graph_revision = $3
           AND public_flow_state = 'open'
         RETURNING graph_revision
        "#,
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(expected_rev)
    .bind(now_ms)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(bumped_row) = bumped else {
        // Distinguish "flow missing" / "terminal" / "stale rev".
        let probe = sqlx::query(
            "SELECT graph_revision, public_flow_state FROM ff_flow_core \
             WHERE partition_key = $1 AND flow_id = $2",
        )
        .bind(part)
        .bind(flow_uuid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        let _ = tx.rollback().await;
        return match probe {
            None => Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                detail: "flow_not_found".to_string(),
            }),
            Some(r) => {
                let state: String = r.get("public_flow_state");
                if matches!(state.as_str(), "cancelled" | "completed" | "failed") {
                    Err(EngineError::Validation {
                        kind: ValidationKind::InvalidInput,
                        detail: "flow_already_terminal".to_string(),
                    })
                } else {
                    Err(EngineError::Contention(ContentionKind::StaleGraphRevision))
                }
            }
        };
    };
    let new_rev: i64 = bumped_row.get("graph_revision");

    // 2. Membership — both execs must be on this flow.
    let members: Vec<Uuid> = sqlx::query_scalar(
        "SELECT execution_id FROM ff_exec_core \
         WHERE partition_key = $1 AND flow_id = $2 \
           AND execution_id = ANY($3)",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(&[upstream_uuid, downstream_uuid][..])
    .fetch_all(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;
    if !members.contains(&upstream_uuid) || !members.contains(&downstream_uuid) {
        let _ = tx.rollback().await;
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "execution_not_in_flow".to_string(),
        });
    }

    // 3. Insert edge. `policy jsonb` carries the immutable edge record
    //    (dependency_kind, data_passing_ref, staged_at_ms, etc.) so
    //    Wave 4c's `decode_edge_row` picks it up unchanged.
    let policy = json!({
        "dependency_kind": args.dependency_kind,
        "satisfaction_condition": "all_required",
        "data_passing_ref": args.data_passing_ref.clone().unwrap_or_default(),
        "edge_state": "pending",
        "created_at_ms": now_ms,
        "created_by": "engine",
        "staged_at_ms": now_ms,
        "applied_at_ms": serde_json::Value::Null,
    });

    let insert = sqlx::query(
        r#"
        INSERT INTO ff_edge
            (partition_key, flow_id, edge_id, upstream_eid, downstream_eid,
             policy, policy_version)
        VALUES ($1, $2, $3, $4, $5, $6, 0)
        ON CONFLICT (partition_key, flow_id, edge_id) DO NOTHING
        RETURNING edge_id
        "#,
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(edge_uuid)
    .bind(upstream_uuid)
    .bind(downstream_uuid)
    .bind(&policy)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    if insert.is_none() {
        // Edge already exists — surface the existing snapshot for
        // `ConflictKind::DependencyAlreadyExists` parity. We need to
        // commit the rev bump rollback since the CAS succeeded but the
        // downstream write failed.
        let _ = tx.rollback().await;
        // Read back for the error payload.
        let existing = crate::flow::describe_edge(pool, pc, &args.flow_id, &args.edge_id)
            .await?
            .ok_or_else(|| EngineError::Validation {
                kind: ValidationKind::Corruption,
                detail: "edge vanished between insert and describe".to_string(),
            })?;
        return Err(EngineError::Conflict(
            ConflictKind::DependencyAlreadyExists { existing },
        ));
    }

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(StageDependencyEdgeResult::Staged {
        edge_id: args.edge_id.clone(),
        new_graph_revision: u64::try_from(new_rev).unwrap_or(0),
    })
}

/// `ff_apply_dependency_to_child` — mark an edge applied + bump the
/// downstream's edge-group aggregate.
///
/// Parity with `lua/flow.lua :: ff_apply_dependency_to_child`:
///
///   * Idempotent on already-applied (`policy.applied_at_ms` already
///     non-null) → returns `AlreadyApplied`.
///   * Bumps `ff_edge_group.running_count` (the Stage-A "unsatisfied
///     required count" equivalent). Creates an `ff_edge_group` row
///     with a default `AllOf` policy when none exists — matches the
///     Lua path's inline `SET policy_variant='all_of'` fallback.
///
/// The cascade (downstream eligibility flip + sibling cancellation) is
/// driven by Wave 5a's `dispatch::dispatch_completion` → this function
/// only writes the per-edge state the cascade reads.
pub async fn apply_dependency_to_child(
    pool: &PgPool,
    pc: &PartitionConfig,
    args: &ApplyDependencyToChildArgs,
) -> Result<ApplyDependencyToChildResult, EngineError> {
    let part = flow_partition_byte(&args.flow_id, pc);
    let flow_uuid: Uuid = args.flow_id.0;
    let edge_uuid: Uuid = args.edge_id.0;
    let downstream_uuid = parse_exec_uuid(&args.downstream_execution_id)?;
    let now_ms = args.now.0;

    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // 1. Load + lock the edge row.
    let edge_row = sqlx::query(
        "SELECT policy FROM ff_edge \
         WHERE partition_key = $1 AND flow_id = $2 AND edge_id = $3 \
         FOR UPDATE",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(edge_uuid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(edge_row) = edge_row else {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: "edge_not_found".to_string(),
        });
    };
    let mut policy: serde_json::Value = edge_row.get("policy");

    // 2. Idempotency: already applied? Count this edge in the running
    //    group state if we have to create the edge_group row (fresh
    //    flows), but do NOT double-bump `running_count`.
    let already_applied = policy
        .get("applied_at_ms")
        .and_then(|v| v.as_i64())
        .is_some();
    if already_applied {
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(ApplyDependencyToChildResult::AlreadyApplied);
    }

    // 3. Mark edge applied in the policy jsonb.
    if let Some(obj) = policy.as_object_mut() {
        obj.insert("applied_at_ms".to_string(), json!(now_ms));
        obj.insert("edge_state".to_string(), json!("applied"));
    }
    sqlx::query(
        "UPDATE ff_edge SET policy = $4 \
         WHERE partition_key = $1 AND flow_id = $2 AND edge_id = $3",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(edge_uuid)
    .bind(&policy)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // 4. Upsert the edge_group row — create with default AllOf when
    //    missing, bump `running_count` when present. `running_count`
    //    is the "unsatisfied required" bucket the dispatch cascade
    //    decrements on each completion.
    sqlx::query(
        r#"
        INSERT INTO ff_edge_group
            (partition_key, flow_id, downstream_eid, policy, running_count)
        VALUES ($1, $2, $3, $4, 1)
        ON CONFLICT (partition_key, flow_id, downstream_eid) DO UPDATE
           SET running_count = ff_edge_group.running_count + 1
        "#,
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(downstream_uuid)
    .bind(json!({ "kind": "all_of" }))
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    // Read back the post-update running_count for the result.
    let unsatisfied: i32 = sqlx::query_scalar(
        "SELECT running_count FROM ff_edge_group \
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
    )
    .bind(part)
    .bind(flow_uuid)
    .bind(downstream_uuid)
    .fetch_one(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(ApplyDependencyToChildResult::Applied {
        unsatisfied_count: u32::try_from(unsatisfied.max(0)).unwrap_or(0),
    })
}
