//! Wave 9 operator-control methods — SQLite port (RFC-023 Phase 3.2).
//!
//! Mirrors `ff-backend-postgres/src/operator.rs` Revision 7. Four
//! mutating ops:
//!
//!   * [`cancel_execution_impl`] — §4.2.1 + §4.2.7 outbox matrix
//!     (ff_lease_event `revoked` iff a lease was active; NO
//!     operator_event — the migration 0010 CHECK allow-list excludes
//!     a `cancelled` event_type and the PG reference does not emit
//!     one).
//!   * [`revoke_lease_impl`] — §4.2.2 (ff_lease_event revoked).
//!   * [`change_priority_impl`] — §4.2.4 Rev 7 Fork 3 Option C
//!     (ff_operator_event priority_changed).
//!   * [`replay_execution_impl`] — §4.2.5 Rev 7 Forks 1 + 2 (two
//!     branches: normal vs skipped_flow_member; ff_operator_event
//!     replayed).
//!
//! Each follows the §4.2 shared spine adapted for SQLite:
//!
//!   1. `BEGIN IMMEDIATE` (RESERVED write lock — single-writer §4.1 A3).
//!   2. Pre-read gates + identity fields (no `FOR UPDATE` — the write
//!      lock already covers us).
//!   3. Validate against Valkey-canonical semantics (§5.2 PG parity).
//!   4. Mutate with WHERE-clause CAS fencing + `rows_affected()` check.
//!   5. Emit the outbox row per §4.2.7 matrix via the co-transactional
//!      `INSERT_*_EVENT_SQL` producer that back-fills
//!      namespace + instance_tag.
//!   6. `COMMIT`. The `retry::retry_serializable` wrapper retries up to
//!      3× on SQLITE_BUSY / SQLITE_LOCKED (§4.3).
//!
//! Lifecycle_phase enum literals are lowercase per RFC-020 Rev 4
//! (`'cancelled'`, `'runnable'`, `'terminal'`, `'active'`).

use serde_json::json;
use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use ff_core::contracts::{
    CancelExecutionArgs, CancelExecutionResult, CancelFlowArgs, CancelFlowHeader,
    ChangePriorityArgs, ChangePriorityResult, ReplayExecutionArgs, ReplayExecutionResult,
    RevokeLeaseArgs, RevokeLeaseResult,
};
use ff_core::engine_error::{ContentionKind, EngineError, StateKind, ValidationKind};
use ff_core::state::PublicState;
use ff_core::types::{CancelSource, ExecutionId, FlowId};

use crate::errors::map_sqlx_error;
use crate::pubsub::{OutboxEvent, PubSub};
use crate::queries::operator as q_op;
use crate::retry::retry_serializable;
use crate::tx_util::{
    begin_immediate, commit_or_rollback, now_ms, rollback_quiet, split_exec_id,
};

// ─── shared helpers ────────────────────────────────────────────────

/// Synthetic lease identity for the SQLite backend — same shape as
/// the PG synthetic (`pg:<uuid>:<attempt>:<epoch>`) but tagged
/// `sqlite` so consumers can tell the producer backend apart from
/// logs / traces.
fn synthetic_lease_id(exec_uuid: Uuid, attempt_index: i32, lease_epoch: i64) -> String {
    format!("sqlite:{exec_uuid}:{attempt_index}:{lease_epoch}")
}

/// Co-transactional `last_insert_rowid()` → `OutboxEvent` for the
/// post-commit broadcast fan-out.
async fn last_outbox_event(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
) -> Result<OutboxEvent, EngineError> {
    let event_id: i64 = sqlx::query_scalar("SELECT last_insert_rowid()")
        .fetch_one(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    Ok(OutboxEvent {
        event_id,
        partition_key: part,
    })
}

/// Insert one `ff_lease_event` outbox row + return the event_id. Uses
/// the co-transactional INSERT that back-fills namespace + instance_tag
/// from exec_core.raw_fields (Phase 3.2 fix).
async fn insert_lease_event(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    event_type: &str,
    now: i64,
) -> Result<OutboxEvent, EngineError> {
    sqlx::query(crate::queries::dispatch::INSERT_LEASE_EVENT_SQL)
        .bind(exec_uuid.to_string())
        .bind(event_type)
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    last_outbox_event(conn, part).await
}

/// Insert one `ff_operator_event` outbox row + return the event_id.
async fn insert_operator_event(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    event_type: &str,
    details: Option<String>,
    now: i64,
) -> Result<OutboxEvent, EngineError> {
    sqlx::query(q_op::INSERT_OPERATOR_EVENT_SQL)
        .bind(exec_uuid.to_string())
        .bind(event_type)
        .bind(details)
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    last_outbox_event(conn, part).await
}

fn dispatch_lease(pubsub: &PubSub, ev: OutboxEvent) {
    PubSub::emit(&pubsub.lease_history, ev);
}
fn dispatch_operator(pubsub: &PubSub, ev: OutboxEvent) {
    PubSub::emit(&pubsub.operator_event, ev);
}

// ─── cancel_execution ──────────────────────────────────────────────

async fn cancel_execution_once(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: &CancelExecutionArgs,
) -> Result<CancelExecutionResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let now = args.now.0;

    let mut conn = begin_immediate(pool).await?;

    // Inline body — on any error, rollback + bail. On success, commit +
    // emit wakeup. Mirrors PG `cancel_execution_once`.
    let result = async {
        let row = sqlx::query(q_op::SELECT_CANCEL_PRE_SQL)
            .bind(part)
            .bind(exec_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let Some(row) = row else {
            return Err(EngineError::NotFound {
                entity: "execution",
            });
        };

        let lifecycle_phase: String = row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
        let public_state: String = row.try_get("public_state").map_err(map_sqlx_error)?;
        let attempt_index: i64 = row.try_get("attempt_index").map_err(map_sqlx_error)?;
        let worker_instance_id: Option<String> =
            row.try_get("worker_instance_id").map_err(map_sqlx_error)?;
        let lease_epoch: Option<i64> = row.try_get("lease_epoch").map_err(map_sqlx_error)?;

        // Terminal handling — idempotent if already cancelled; hard
        // conflict otherwise.
        if matches!(lifecycle_phase.as_str(), "terminal" | "cancelled") {
            return if public_state == "cancelled" {
                Ok((
                    CancelExecutionResult::Cancelled {
                        execution_id: args.execution_id.clone(),
                        public_state: PublicState::Cancelled,
                    },
                    None,
                ))
            } else {
                Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: format!(
                        "cancel_execution: execution_id={}: already terminal in state '{}'",
                        args.execution_id, public_state
                    ),
                })
            };
        }

        // Lease fence validation.
        let lease_active = worker_instance_id
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        if !matches!(args.source, CancelSource::OperatorOverride) && lease_active {
            let Some(expected_epoch) = args.lease_epoch.as_ref() else {
                return Err(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: format!(
                        "cancel_execution: execution_id={}: lease_epoch required when source != operator_override and execution is active",
                        args.execution_id
                    ),
                });
            };
            let expected = i64::try_from(expected_epoch.0).unwrap_or(i64::MAX);
            if lease_epoch.unwrap_or(0) != expected {
                return Err(EngineError::State(StateKind::StaleLease));
            }
        }

        // Flip exec_core → cancelled.
        sqlx::query(q_op::UPDATE_EXEC_CORE_CANCELLED_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(now)
            .bind(&args.reason)
            .bind(args.source.as_str())
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        // If a lease was active, clear it and emit lease_event revoked.
        // Per RFC-020 §4.2.7 matrix, `cancel_execution` emits ONLY
        // `ff_lease_event` (if lease active) — no `ff_operator_event`
        // row (the migration 0010 CHECK allow-list does not include a
        // `cancelled` event_type). Valkey parity: the `released`
        // history event rides the lease stream too.
        let lease_ev = if lease_active {
            sqlx::query(q_op::UPDATE_ATTEMPT_CANCELLED_SQL)
                .bind(part)
                .bind(exec_uuid)
                .bind(attempt_index)
                .bind(now)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;

            Some(insert_lease_event(&mut conn, part, exec_uuid, "revoked", now).await?)
        } else {
            None
        };

        Ok((
            CancelExecutionResult::Cancelled {
                execution_id: args.execution_id.clone(),
                public_state: PublicState::Cancelled,
            },
            lease_ev,
        ))
    }
    .await;

    match result {
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
        Ok((ret, lease_ev)) => {
            commit_or_rollback(&mut conn).await?;
            if let Some(ev) = lease_ev {
                dispatch_lease(pubsub, ev);
            }
            Ok(ret)
        }
    }
}

pub(crate) async fn cancel_execution_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: CancelExecutionArgs,
) -> Result<CancelExecutionResult, EngineError> {
    retry_serializable(|| cancel_execution_once(pool, pubsub, &args)).await
}

// ─── revoke_lease ──────────────────────────────────────────────────

async fn revoke_lease_once(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: &RevokeLeaseArgs,
) -> Result<RevokeLeaseResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let now = now_ms();

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let ec_row = sqlx::query(q_op::SELECT_EXEC_ATTEMPT_INDEX_SQL)
            .bind(part)
            .bind(exec_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let Some(ec_row) = ec_row else {
            return Err(EngineError::NotFound {
                entity: "execution",
            });
        };
        let attempt_index: i64 = ec_row.try_get("attempt_index").map_err(map_sqlx_error)?;

        let att_row = sqlx::query(q_op::SELECT_ATTEMPT_OWNER_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let (worker_instance_id, lease_epoch): (Option<String>, Option<i64>) = match att_row {
            Some(r) => (
                r.try_get("worker_instance_id").map_err(map_sqlx_error)?,
                r.try_get("lease_epoch").map_err(map_sqlx_error)?,
            ),
            None => (None, None),
        };

        let lease_active = worker_instance_id
            .as_deref()
            .is_some_and(|s| !s.is_empty());
        if !lease_active {
            return Ok((
                RevokeLeaseResult::AlreadySatisfied {
                    reason: "no_active_lease".to_owned(),
                },
                None,
            ));
        }

        // Targeted-revoke fence (Valkey parity).
        let caller_wiid = args.worker_instance_id.as_str();
        if !caller_wiid.is_empty() && worker_instance_id.as_deref() != Some(caller_wiid) {
            return Ok((
                RevokeLeaseResult::AlreadySatisfied {
                    reason: "different_worker_instance_id".to_owned(),
                },
                None,
            ));
        }

        let prior_epoch = lease_epoch.unwrap_or(0);

        // Optional expected_lease_id fence.
        if let Some(expected) = args
            .expected_lease_id
            .as_ref()
            .filter(|s| !s.is_empty())
        {
            let current_id =
                synthetic_lease_id(exec_uuid, attempt_index as i32, prior_epoch);
            if expected != &current_id {
                return Ok((
                    RevokeLeaseResult::AlreadySatisfied {
                        reason: "lease_id_mismatch".to_owned(),
                    },
                    None,
                ));
            }
        }

        // CAS on lease_epoch.
        let affected = sqlx::query(q_op::UPDATE_ATTEMPT_REVOKE_CAS_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .bind(prior_epoch)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?
            .rows_affected();

        if affected == 0 {
            return Ok((
                RevokeLeaseResult::AlreadySatisfied {
                    reason: "epoch_moved".to_owned(),
                },
                None,
            ));
        }

        // Flip exec_core back to reclaimable-runnable.
        sqlx::query(q_op::UPDATE_EXEC_CORE_RECLAIMABLE_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(now)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let lease_ev = insert_lease_event(&mut conn, part, exec_uuid, "revoked", now).await?;

        Ok((
            RevokeLeaseResult::Revoked {
                lease_id: synthetic_lease_id(exec_uuid, attempt_index as i32, prior_epoch),
                lease_epoch: (prior_epoch + 1).to_string(),
            },
            Some(lease_ev),
        ))
    }
    .await;

    match result {
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
        Ok((ret, lease_ev)) => {
            commit_or_rollback(&mut conn).await?;
            if let Some(ev) = lease_ev {
                dispatch_lease(pubsub, ev);
            }
            Ok(ret)
        }
    }
}

pub(crate) async fn revoke_lease_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: RevokeLeaseArgs,
) -> Result<RevokeLeaseResult, EngineError> {
    retry_serializable(|| revoke_lease_once(pool, pubsub, &args)).await
}

// ─── change_priority ───────────────────────────────────────────────

async fn change_priority_once(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: &ChangePriorityArgs,
) -> Result<ChangePriorityResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let now = args.now.0;

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let row = sqlx::query(q_op::SELECT_CHANGE_PRIORITY_PRE_SQL)
            .bind(part)
            .bind(exec_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let Some(row) = row else {
            return Err(EngineError::NotFound {
                entity: "execution",
            });
        };

        let lifecycle_phase: String = row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
        let eligibility_state: String =
            row.try_get("eligibility_state").map_err(map_sqlx_error)?;
        let old_priority: i64 = row.try_get("priority").map_err(map_sqlx_error)?;

        if lifecycle_phase != "runnable" || eligibility_state != "eligible_now" {
            return Err(EngineError::Contention(ContentionKind::ExecutionNotEligible));
        }

        // Clamp to [0, 9000] per Valkey `ff_change_priority`.
        let new_priority = args.new_priority.clamp(0, 9000);

        let affected = sqlx::query(q_op::UPDATE_EXEC_CORE_PRIORITY_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(new_priority)
            .bind(now)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?
            .rows_affected();

        if affected == 0 {
            return Err(EngineError::Contention(ContentionKind::ExecutionNotEligible));
        }

        let details = json!({
            "old_priority": old_priority,
            "new_priority": new_priority,
        });
        let ev = insert_operator_event(
            &mut conn,
            part,
            exec_uuid,
            "priority_changed",
            Some(details.to_string()),
            now,
        )
        .await?;

        Ok((
            ChangePriorityResult::Changed {
                execution_id: args.execution_id.clone(),
            },
            ev,
        ))
    }
    .await;

    match result {
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
        Ok((ret, ev)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_operator(pubsub, ev);
            Ok(ret)
        }
    }
}

pub(crate) async fn change_priority_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: ChangePriorityArgs,
) -> Result<ChangePriorityResult, EngineError> {
    retry_serializable(|| change_priority_once(pool, pubsub, &args)).await
}

// ─── replay_execution ──────────────────────────────────────────────

async fn replay_execution_once(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: &ReplayExecutionArgs,
) -> Result<ReplayExecutionResult, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;
    let now = args.now.0;

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let ec_row = sqlx::query(q_op::SELECT_REPLAY_PRE_SQL)
            .bind(part)
            .bind(exec_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let Some(ec_row) = ec_row else {
            return Err(EngineError::NotFound {
                entity: "execution",
            });
        };

        let lifecycle_phase: String = ec_row
            .try_get("lifecycle_phase")
            .map_err(map_sqlx_error)?;
        let flow_id: Option<Vec<u8>> = ec_row.try_get("flow_id").map_err(map_sqlx_error)?;
        let attempt_index: i64 = ec_row.try_get("attempt_index").map_err(map_sqlx_error)?;

        // Gate: lifecycle_phase = 'terminal' (Valkey canonical).
        if lifecycle_phase != "terminal" {
            return Err(EngineError::State(StateKind::ExecutionNotTerminal));
        }

        let att_row = sqlx::query(q_op::SELECT_ATTEMPT_OUTCOME_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(attempt_index)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let attempt_outcome: Option<String> = match att_row.as_ref() {
            Some(r) => r.try_get("outcome").map_err(map_sqlx_error)?,
            None => None,
        };
        let has_attempt_row = att_row.is_some();

        // Branch: skipped flow member (Valkey `flowfabric.lua:8555`).
        let is_skipped_flow_member =
            attempt_outcome.as_deref() == Some("skipped") && flow_id.is_some();

        // Reset downstream edge-group counters for skipped-flow-member.
        let groups_reset: i64 = if is_skipped_flow_member {
            sqlx::query(q_op::RESET_EDGE_GROUP_COUNTERS_SQL)
                .bind(part)
                .bind(exec_uuid)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?
                .rows_affected() as i64
        } else {
            0
        };

        let (eligibility_state, public_state) = if is_skipped_flow_member {
            ("blocked_by_dependencies", "waiting_children")
        } else {
            ("eligible_now", "waiting")
        };

        sqlx::query(q_op::UPDATE_EXEC_CORE_REPLAY_SQL)
            .bind(part)
            .bind(exec_uuid)
            .bind(eligibility_state)
            .bind(public_state)
            .bind(now)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        if has_attempt_row {
            sqlx::query(q_op::UPDATE_ATTEMPT_REPLAY_RESET_SQL)
                .bind(part)
                .bind(exec_uuid)
                .bind(attempt_index)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;
        }

        let details = if is_skipped_flow_member {
            json!({
                "branch": "skipped_flow_member",
                "groups_reset": groups_reset,
            })
        } else {
            json!({
                "branch": "normal",
            })
        };
        let ev = insert_operator_event(
            &mut conn,
            part,
            exec_uuid,
            "replayed",
            Some(details.to_string()),
            now,
        )
        .await?;

        let ps = if is_skipped_flow_member {
            PublicState::WaitingChildren
        } else {
            PublicState::Waiting
        };
        Ok((ReplayExecutionResult::Replayed { public_state: ps }, ev))
    }
    .await;

    match result {
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
        Ok((ret, ev)) => {
            commit_or_rollback(&mut conn).await?;
            dispatch_operator(pubsub, ev);
            Ok(ret)
        }
    }
}

pub(crate) async fn replay_execution_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: ReplayExecutionArgs,
) -> Result<ReplayExecutionResult, EngineError> {
    retry_serializable(|| replay_execution_once(pool, pubsub, &args)).await
}

// ─── cancel_flow_header (§4.2.3, Phase 3.3) ─────────────────────────

/// Format a member execution UUID as the wire-form `ExecutionId`
/// string (`{fp:N}:<uuid>`). SQLite uses `part=0` for all flows
/// (single-writer, no partitioning).
fn member_wire_id(partition_key: i64, exec_uuid: Uuid) -> String {
    format!("{{fp:{partition_key}}}:{exec_uuid}")
}

/// Insert one operator-event row keyed on a flow_id (not
/// execution_id). Used by `flow_cancel_requested` per RFC-020 §4.2.3.
/// Reuses `last_outbox_event` for consistency with the other outbox
/// insertion helpers in this module (gemini review).
async fn insert_operator_event_flow(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    flow_uuid: Uuid,
    event_type: &str,
    details: Option<String>,
    now: i64,
) -> Result<OutboxEvent, EngineError> {
    sqlx::query(q_op::INSERT_OPERATOR_EVENT_FLOW_SQL)
        .bind(flow_uuid.to_string())
        .bind(event_type)
        .bind(details)
        .bind(now)
        .bind(part)
        .bind(flow_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    last_outbox_event(conn, part).await
}

async fn cancel_flow_header_once(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: &CancelFlowArgs,
) -> Result<CancelFlowHeader, EngineError> {
    let part: i64 = 0; // SQLite single-partition (mirrors cancel_flow_impl).
    let flow_uuid: Uuid = args.flow_id.0;
    let now = args.now.0;

    let mut conn = begin_immediate(pool).await?;

    let result = async {
        let flow_row = sqlx::query(q_op::SELECT_FLOW_CORE_FOR_CANCEL_SQL)
            .bind(part)
            .bind(flow_uuid)
            .fetch_optional(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let Some(flow_row) = flow_row else {
            return Err(EngineError::NotFound { entity: "flow" });
        };

        let public_flow_state: String = flow_row
            .try_get("public_flow_state")
            .map_err(map_sqlx_error)?;
        let raw_fields_str: String = flow_row.try_get("raw_fields").map_err(map_sqlx_error)?;

        // Idempotent-replay path — already terminal. Return stored
        // policy / reason + enumerated members. Mirrors PG.
        if matches!(public_flow_state.as_str(), "cancelled" | "completed" | "failed") {
            let raw: serde_json::Value = serde_json::from_str(&raw_fields_str).map_err(|e| {
                EngineError::Validation {
                    kind: ValidationKind::Corruption,
                    detail: format!("flow_core: raw_fields not valid JSON: {e}"),
                }
            })?;
            let stored_cancellation_policy = raw
                .get("cancellation_policy")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let stored_cancel_reason = raw
                .get("cancel_reason")
                .and_then(|v| v.as_str())
                .map(str::to_owned);

            let backlog_members: Vec<String> =
                sqlx::query_scalar::<_, String>(q_op::SELECT_CANCEL_BACKLOG_MEMBERS_SQL)
                    .bind(part)
                    .bind(flow_uuid)
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(map_sqlx_error)?;

            let members: Vec<String> = if backlog_members.is_empty() {
                // Pre-E2-shape flow: enumerate live exec_core rows.
                let live = sqlx::query_scalar::<_, Uuid>(q_op::SELECT_FLOW_ALL_MEMBERS_SQL)
                    .bind(part)
                    .bind(flow_uuid)
                    .fetch_all(&mut *conn)
                    .await
                    .map_err(map_sqlx_error)?;
                live.into_iter().map(|u| member_wire_id(part, u)).collect()
            } else {
                backlog_members
            };

            return Ok((
                CancelFlowHeader::AlreadyTerminal {
                    stored_cancellation_policy,
                    stored_cancel_reason,
                    member_execution_ids: members,
                },
                None,
            ));
        }

        // Fresh cancel path.
        sqlx::query(q_op::UPDATE_FLOW_CORE_CANCEL_WITH_REASON_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(now)
            .bind(&args.cancellation_policy)
            .bind(&args.reason)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        sqlx::query(q_op::INSERT_CANCEL_BACKLOG_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(now)
            .bind(&args.reason)
            .bind(&args.cancellation_policy)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        let member_uuids: Vec<Uuid> =
            sqlx::query_scalar::<_, Uuid>(q_op::SELECT_FLOW_INFLIGHT_MEMBERS_SQL)
                .bind(part)
                .bind(flow_uuid)
                .fetch_all(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;

        let member_execution_ids: Vec<String> = member_uuids
            .iter()
            .map(|u| member_wire_id(part, *u))
            .collect();

        // SQLite has no bulk UNNEST — loop. Under single-writer
        // BEGIN IMMEDIATE each statement is cheap and membership is
        // bounded; matches `cancel_flow_impl` shape at backend.rs:2090.
        for (wire_id, exec_uuid) in member_execution_ids.iter().zip(member_uuids.iter()) {
            sqlx::query(q_op::INSERT_CANCEL_BACKLOG_MEMBER_SQL)
                .bind(part)
                .bind(flow_uuid)
                .bind(wire_id)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;

            sqlx::query(q_op::UPDATE_EXEC_CORE_CANCEL_FROM_HEADER_SQL)
                .bind(part)
                .bind(exec_uuid)
                .bind(now)
                .bind(&args.reason)
                .execute(&mut *conn)
                .await
                .map_err(map_sqlx_error)?;
        }

        // Outbox emit — flow-level `flow_cancel_requested`.
        let details = json!({
            "flow_id": flow_uuid.to_string(),
            "cancellation_policy": &args.cancellation_policy,
            "reason": &args.reason,
            "member_count": member_execution_ids.len(),
        });
        let ev = insert_operator_event_flow(
            &mut conn,
            part,
            flow_uuid,
            "flow_cancel_requested",
            Some(details.to_string()),
            now,
        )
        .await?;

        Ok((
            CancelFlowHeader::Cancelled {
                cancellation_policy: args.cancellation_policy.clone(),
                member_execution_ids,
            },
            Some(ev),
        ))
    }
    .await;

    match result {
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
        Ok((ret, ev)) => {
            commit_or_rollback(&mut conn).await?;
            if let Some(ev) = ev {
                dispatch_operator(pubsub, ev);
            }
            Ok(ret)
        }
    }
}

pub(crate) async fn cancel_flow_header_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: CancelFlowArgs,
) -> Result<CancelFlowHeader, EngineError> {
    retry_serializable(|| cancel_flow_header_once(pool, pubsub, &args)).await
}

// ─── ack_cancel_member (§4.2.3, Phase 3.3) ──────────────────────────

async fn ack_cancel_member_once(
    pool: &SqlitePool,
    flow_id: &FlowId,
    execution_id: &ExecutionId,
) -> Result<(), EngineError> {
    let part: i64 = 0;
    let flow_uuid: Uuid = flow_id.0;
    let member_wire = execution_id.as_str();

    let mut conn = begin_immediate(pool).await?;

    // Intra-ack drain: delete the member row, then delete the parent
    // iff empty. Under SQLite single-writer (BEGIN IMMEDIATE) the two
    // statements share a serialised window, so the final-ack path
    // observes post-delete state; concurrent acks from other processes
    // race at BEGIN IMMEDIATE and are re-sequenced by
    // `retry_serializable` — matches PG SERIALIZABLE CTE shape.
    //
    // NO outbox emit — RFC-020 §4.2.7 matrix marks this path silent
    // for Valkey parity.
    let result = async {
        sqlx::query(q_op::DELETE_CANCEL_BACKLOG_MEMBER_SQL)
            .bind(part)
            .bind(flow_uuid)
            .bind(member_wire)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        sqlx::query(q_op::DELETE_CANCEL_BACKLOG_IF_EMPTY_SQL)
            .bind(part)
            .bind(flow_uuid)
            .execute(&mut *conn)
            .await
            .map_err(map_sqlx_error)?;

        Ok::<(), EngineError>(())
    }
    .await;

    match result {
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
        Ok(()) => {
            commit_or_rollback(&mut conn).await?;
            Ok(())
        }
    }
}

pub(crate) async fn ack_cancel_member_impl(
    pool: &SqlitePool,
    flow_id: FlowId,
    execution_id: ExecutionId,
) -> Result<(), EngineError> {
    retry_serializable(|| ack_cancel_member_once(pool, &flow_id, &execution_id)).await
}
