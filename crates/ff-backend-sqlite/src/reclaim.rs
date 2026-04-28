//! RFC-024 PR-E — SQLite `issue_reclaim_grant` + `reclaim_execution`
//! impls.
//!
//! Mirrors the Valkey `ff_issue_reclaim_grant` + `ff_reclaim_execution`
//! FCALL semantics (`flowfabric.lua:2985`, `:3898`) on SQLite's flat
//! schema. Both impls run inside a single `BEGIN IMMEDIATE` txn per
//! RFC-023 §4.3 — SQLite's RESERVED lock covers the full read-modify-
//! write window, so no explicit `FOR UPDATE` is needed.
//!
//! Outbox semantics (RFC-019): successful `reclaim_execution` emits a
//! `ff_lease_event` row with event_type `reclaimed`, dispatched via
//! the post-commit broadcast to `PubSub::lease_history`. The
//! `ff_operator_event` CHECK constraint (migration 0010) does not
//! admit a `reclaimed` event type, matching PG's RFC-020 matrix
//! where reclaim fires only on the lease channel.

use sqlx::{Row, SqlitePool};
use uuid::Uuid;

use ff_core::backend::HandleKind;
use ff_core::contracts::{
    IssueReclaimGrantArgs, IssueReclaimGrantOutcome, ReclaimExecutionArgs,
    ReclaimExecutionOutcome, ReclaimGrant,
};
use ff_core::engine_error::{EngineError, ValidationKind};
use ff_core::handle_codec::HandlePayload;
use ff_core::partition::{Partition, PartitionFamily, PartitionKey};
use ff_core::types::{AttemptIndex, ExecutionId, LeaseEpoch};

use crate::errors::map_sqlx_error;
use crate::handle_codec::encode_handle;
use crate::pubsub::{OutboxEvent, PubSub};
use crate::queries::attempt as q_attempt;
use crate::queries::claim_grant as q_grant;
use crate::queries::dispatch as q_dispatch;
use crate::tx_util::{begin_immediate, commit_or_rollback, now_ms, rollback_quiet, split_exec_id};

/// Rust-surface default for `max_reclaim_count` per RFC-024 §4.6. The
/// Lua fallback is 100 (scheduler-scanner ceiling); the Rust surface
/// is 1000 (pull-mode consumer ceiling). The two-default coexistence
/// is documented in the RFC.
const DEFAULT_MAX_RECLAIM_COUNT: u32 = 1000;

/// Build the `PartitionKey` for an execution from its hash-tag
/// partition index.
fn partition_key_for_exec(execution_id: &ExecutionId) -> PartitionKey {
    PartitionKey::from(Partition {
        family: PartitionFamily::Flow,
        index: execution_id.partition(),
    })
}

/// Co-transactional `last_insert_rowid()` to `OutboxEvent` for the
/// post-commit broadcast fan-out. Mirrors `operator.rs::last_outbox_event`.
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

/// Insert one `ff_lease_event` outbox row + return the event_id.
async fn insert_lease_event(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    event_type: &str,
    now: i64,
) -> Result<OutboxEvent, EngineError> {
    sqlx::query(q_dispatch::INSERT_LEASE_EVENT_SQL)
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

fn dispatch_lease(pubsub: &PubSub, ev: OutboxEvent) {
    PubSub::emit(&pubsub.lease_history, ev);
}

// -- issue_reclaim_grant -------------------------------------------------

pub(crate) async fn issue_reclaim_grant_impl(
    pool: &SqlitePool,
    args: &IssueReclaimGrantArgs,
) -> Result<IssueReclaimGrantOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;

    let mut conn = begin_immediate(pool).await?;
    let result = issue_reclaim_grant_inner(&mut conn, part, exec_uuid, args).await;
    match result {
        Ok(outcome) => {
            commit_or_rollback(&mut conn).await?;
            Ok(outcome)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

async fn issue_reclaim_grant_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    args: &IssueReclaimGrantArgs,
) -> Result<IssueReclaimGrantOutcome, EngineError> {
    let row = sqlx::query(q_grant::SELECT_EXEC_CORE_FOR_RECLAIM_SQL)
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let Some(row) = row else {
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    };

    let lifecycle_phase: String = row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let ownership_state: String = row.try_get("ownership_state").map_err(map_sqlx_error)?;

    if lifecycle_phase != "active" {
        return Ok(IssueReclaimGrantOutcome::NotReclaimable {
            execution_id: args.execution_id.clone(),
            detail: format!("lifecycle_phase={lifecycle_phase} (expected active)"),
        });
    }
    if ownership_state != "lease_expired_reclaimable" && ownership_state != "lease_revoked" {
        return Ok(IssueReclaimGrantOutcome::NotReclaimable {
            execution_id: args.execution_id.clone(),
            detail: format!(
                "ownership_state={ownership_state} (expected lease_expired_reclaimable | lease_revoked)"
            ),
        });
    }

    let now = now_ms();
    let ttl = i64::try_from(args.grant_ttl_ms).unwrap_or(i64::MAX);
    let expires_at = now.saturating_add(ttl);

    let grant_uuid = Uuid::new_v4();
    let worker_caps_json = serde_json::to_string(&args.worker_capabilities).map_err(|e| {
        EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!("issue_reclaim_grant: worker_capabilities serialize failed: {e}"),
        }
    })?;

    sqlx::query(q_grant::INSERT_RECLAIM_GRANT_SQL)
        .bind(part)
        .bind(grant_uuid)
        .bind(exec_uuid)
        .bind(args.worker_id.as_str())
        .bind(args.worker_instance_id.as_str())
        .bind(args.lane_id.as_str())
        .bind(args.capability_hash.as_deref())
        .bind(&worker_caps_json)
        .bind(args.route_snapshot_json.as_deref())
        .bind(args.admission_summary.as_deref())
        .bind(ttl)
        .bind(now)
        .bind(expires_at)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    let partition_key = partition_key_for_exec(&args.execution_id);
    let expires_at_u64 = u64::try_from(expires_at).unwrap_or(0);

    Ok(IssueReclaimGrantOutcome::Granted(ReclaimGrant::new(
        args.execution_id.clone(),
        partition_key,
        grant_uuid.to_string(),
        expires_at_u64,
        args.lane_id.clone(),
    )))
}

// -- reclaim_execution ---------------------------------------------------

pub(crate) async fn reclaim_execution_impl(
    pool: &SqlitePool,
    pubsub: &PubSub,
    args: &ReclaimExecutionArgs,
) -> Result<ReclaimExecutionOutcome, EngineError> {
    let (part, exec_uuid) = split_exec_id(&args.execution_id)?;

    let mut conn = begin_immediate(pool).await?;
    let result = reclaim_execution_inner(&mut conn, part, exec_uuid, args).await;
    match result {
        Ok((outcome, lease_ev)) => {
            commit_or_rollback(&mut conn).await?;
            if let Some(ev) = lease_ev {
                dispatch_lease(pubsub, ev);
            }
            Ok(outcome)
        }
        Err(e) => {
            rollback_quiet(&mut conn).await;
            Err(e)
        }
    }
}

async fn reclaim_execution_inner(
    conn: &mut sqlx::pool::PoolConnection<sqlx::Sqlite>,
    part: i64,
    exec_uuid: Uuid,
    args: &ReclaimExecutionArgs,
) -> Result<(ReclaimExecutionOutcome, Option<OutboxEvent>), EngineError> {
    // 1. Locate + validate the reclaim grant for this execution.
    let grant_row = sqlx::query(q_grant::SELECT_RECLAIM_GRANT_BY_EXEC_SQL)
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let Some(grant_row) = grant_row else {
        return Ok((
            ReclaimExecutionOutcome::GrantNotFound {
                execution_id: args.execution_id.clone(),
            },
            None,
        ));
    };

    let grant_id: Vec<u8> = grant_row.try_get("grant_id").map_err(map_sqlx_error)?;
    let grant_uuid = Uuid::from_slice(&grant_id).map_err(|e| EngineError::Validation {
        kind: ValidationKind::InvalidInput,
        detail: format!("reclaim_execution: grant_id invalid UUID bytes: {e}"),
    })?;

    let grant_worker: String = grant_row.try_get("worker_id").map_err(map_sqlx_error)?;
    // Lua (flowfabric.lua:3088): grant.worker_id == args.worker_id.
    // worker_instance_id is NOT checked (cairn per-request-spawn).
    if grant_worker != args.worker_id.as_str() {
        return Err(EngineError::Validation {
            kind: ValidationKind::InvalidInput,
            detail: format!(
                "reclaim_execution: grant worker_id={grant_worker} mismatches caller {}",
                args.worker_id
            ),
        });
    }

    let grant_expires_at: i64 = grant_row.try_get("expires_at_ms").map_err(map_sqlx_error)?;
    let now = now_ms();
    if grant_expires_at <= now {
        // Expired grant: consume it so retries do not hit the same
        // stale row, then report GrantNotFound.
        sqlx::query(q_grant::DELETE_RECLAIM_GRANT_SQL)
            .bind(part)
            .bind(grant_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        return Ok((
            ReclaimExecutionOutcome::GrantNotFound {
                execution_id: args.execution_id.clone(),
            },
            None,
        ));
    }

    let grant_lane: Option<String> = grant_row.try_get("lane_id").map_err(map_sqlx_error)?;

    // 2. Reclaim-time re-validation: read authoritative exec_core
    //    fields under the tx (lifecycle/ownership gate + server-
    //    derived attempt_index + prior lease_epoch for monotonic
    //    bump). Mirrors PG PR-D `claim_grant.rs::reclaim_execution_once`
    //    lines 463-484 and Lua `flowfabric.lua:3049+`.
    let gate_row = sqlx::query(q_grant::SELECT_EXEC_CORE_RECLAIM_GATE_SQL)
        .bind(part)
        .bind(exec_uuid)
        .fetch_optional(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;
    let Some(gate_row) = gate_row else {
        return Err(EngineError::NotFound {
            entity: "execution",
        });
    };
    let current_count_i: i64 = gate_row
        .try_get("lease_reclaim_count")
        .map_err(map_sqlx_error)?;
    let current_count = u32::try_from(current_count_i.max(0)).unwrap_or(0);
    let lifecycle_phase: String = gate_row.try_get("lifecycle_phase").map_err(map_sqlx_error)?;
    let ownership_state: String = gate_row.try_get("ownership_state").map_err(map_sqlx_error)?;
    // Authoritative attempt_index: ignore caller-supplied
    // `args.current_attempt_index` (F7). Caller arg is informational
    // only; the server-side value is the truth used for both prior-
    // attempt targeting and new_attempt_index derivation.
    let stored_attempt_index_i: i64 = gate_row.try_get("attempt_index").map_err(map_sqlx_error)?;
    let prior_lease_epoch_i: i64 = gate_row
        .try_get("prior_lease_epoch")
        .map_err(map_sqlx_error)?;

    // 2a. Gate: lifecycle must be `active` + ownership must still be
    //     reclaimable. Between grant issuance and consumption the
    //     exec could have gone terminal/cancelled via another path.
    if lifecycle_phase != "active" {
        return Ok((
            ReclaimExecutionOutcome::NotReclaimable {
                execution_id: args.execution_id.clone(),
                detail: format!(
                    "lifecycle_phase={lifecycle_phase} (expected active); exec transitioned \
                     after grant issuance"
                ),
            },
            None,
        ));
    }
    if ownership_state != "lease_expired_reclaimable" && ownership_state != "lease_revoked" {
        return Ok((
            ReclaimExecutionOutcome::NotReclaimable {
                execution_id: args.execution_id.clone(),
                detail: format!(
                    "ownership_state={ownership_state} (expected \
                     lease_expired_reclaimable | lease_revoked); exec transitioned after \
                     grant issuance"
                ),
            },
            None,
        ));
    }

    // 3. Cap check BEFORE new attempt. Lua (`flowfabric.lua:3049`)
    //    fires terminal_failed when `reclaim_count >= max_reclaim`
    //    BEFORE the bump.
    let cap = args.max_reclaim_count.unwrap_or(DEFAULT_MAX_RECLAIM_COUNT);
    if current_count >= cap {
        // Flip exec_core to terminal_failed.
        sqlx::query(q_grant::UPDATE_EXEC_CORE_RECLAIM_CAP_EXCEEDED_SQL)
            .bind(now)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        // Clear the prior attempt's lease fields + mark interrupted.
        // Mirrors Lua :3064-3079 (clears current_lease_id /
        // current_worker_* on exec_core + DEL lease_current). On
        // SQLite the lease-fencing fields live on the attempt row.
        sqlx::query(q_grant::CLEAR_PRIOR_ATTEMPT_LEASE_ON_CAP_EXCEEDED_SQL)
            .bind(now)
            .bind(part)
            .bind(exec_uuid)
            .bind(stored_attempt_index_i)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        // Consume the grant.
        sqlx::query(q_grant::DELETE_RECLAIM_GRANT_SQL)
            .bind(part)
            .bind(grant_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        // RFC-019 §4.2.7 outbox matrix: every terminal transition
        // emits both a completion_event and a lease_event. Cap-
        // exceeded is terminal_failed, so both fire.
        sqlx::query(q_attempt::INSERT_COMPLETION_EVENT_SQL)
            .bind("failed")
            .bind(now)
            .bind(part)
            .bind(exec_uuid)
            .execute(&mut **conn)
            .await
            .map_err(map_sqlx_error)?;
        let ev = insert_lease_event(conn, part, exec_uuid, "revoked", now).await?;
        return Ok((
            ReclaimExecutionOutcome::ReclaimCapExceeded {
                execution_id: args.execution_id.clone(),
                reclaim_count: current_count,
            },
            Some(ev),
        ));
    }

    // 4. Mark prior attempt `interrupted_reclaimed` using the
    //    server-authoritative prior index.
    sqlx::query(q_grant::UPDATE_PRIOR_ATTEMPT_INTERRUPTED_RECLAIMED_SQL)
        .bind(now)
        .bind(part)
        .bind(exec_uuid)
        .bind(stored_attempt_index_i)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    // 5. Insert new attempt row. `new_attempt_index = stored + 1`
    //    (F7: ignore caller arg). `new_lease_epoch = prior + 1`
    //    (F4/F5: matches Lua :3106, preserves fencing monotonicity).
    let new_attempt_index_i = stored_attempt_index_i.saturating_add(1);
    let new_lease_epoch_i = prior_lease_epoch_i.saturating_add(1);
    let new_lease_epoch_u = u64::try_from(new_lease_epoch_i.max(0)).unwrap_or(0);
    let lease_ttl_ms_i = i64::try_from(args.lease_ttl_ms).unwrap_or(0);
    let new_expires_at = now.saturating_add(lease_ttl_ms_i);
    sqlx::query(q_grant::INSERT_NEW_RECLAIM_ATTEMPT_SQL)
        .bind(part)
        .bind(exec_uuid)
        .bind(new_attempt_index_i)
        .bind(args.worker_id.as_str())
        .bind(args.worker_instance_id.as_str())
        .bind(new_lease_epoch_i)
        .bind(new_expires_at)
        .bind(now)
        .bind(&args.attempt_policy_json)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    // 6. Flip `ff_exec_core` to active/leased + bump reclaim counter +
    //    pin `attempt_index` to the new attempt.
    sqlx::query(q_grant::UPDATE_EXEC_CORE_FOR_NEW_RECLAIM_ATTEMPT_SQL)
        .bind(new_attempt_index_i)
        .bind(part)
        .bind(exec_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    // 7. Consume the grant row.
    sqlx::query(q_grant::DELETE_RECLAIM_GRANT_SQL)
        .bind(part)
        .bind(grant_uuid)
        .execute(&mut **conn)
        .await
        .map_err(map_sqlx_error)?;

    // 8. Emit RFC-019 `reclaimed` lease event.
    let ev = insert_lease_event(conn, part, exec_uuid, "reclaimed", now).await?;

    // 9. Mint the Reclaimed-kind handle. F4: use caller-supplied
    //    `attempt_id` + `lease_id` (PR-D parity @ PG
    //    `claim_grant.rs:633-652` — Valkey PR-F Lua round-trips these
    //    same identifiers via ARGV[5]/[7]). LeaseEpoch is the newly
    //    derived monotonic value, NOT 0.
    let lane_id = grant_lane
        .map(ff_core::types::LaneId::new)
        .unwrap_or_else(|| args.lane_id.clone());
    let payload = HandlePayload::new(
        args.execution_id.clone(),
        AttemptIndex::new(u32::try_from(new_attempt_index_i.max(0)).unwrap_or(0)),
        args.attempt_id.clone(),
        args.lease_id.clone(),
        LeaseEpoch(new_lease_epoch_u),
        args.lease_ttl_ms,
        lane_id,
        args.worker_instance_id.clone(),
    );
    Ok((
        ReclaimExecutionOutcome::Claimed(encode_handle(&payload, HandleKind::Reclaimed)),
        Some(ev),
    ))
}
