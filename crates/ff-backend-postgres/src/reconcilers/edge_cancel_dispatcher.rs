//! RFC-016 Stage C — Postgres sibling-cancel dispatcher.
//!
//! Drains `ff_pending_cancel_groups` rows: for each row points at an
//! `ff_edge_group` with `cancel_siblings_pending_flag = true` and a
//! `cancel_siblings_pending_members` array of still-running sibling
//! execution ids. The dispatcher:
//!
//! 1. `SELECT ... FROM ff_pending_cancel_groups LIMIT $BATCH` to get
//!    the candidate set for this tick.
//! 2. Per row, open one tx, `SELECT ... FROM ff_edge_group
//!    WHERE ... FOR UPDATE SKIP LOCKED` — a concurrent dispatcher
//!    that already owns the row is left alone (coalescing is safe
//!    because the losing tick will observe an empty/already-cleared
//!    group on its next pass).
//! 3. If the group is missing → the pending row is an orphan; DELETE
//!    it and commit.
//! 4. If `cancel_siblings_pending_flag = false` → reconciler
//!    territory; skip without touching (the reconciler will SREM).
//! 5. Otherwise: cancel every listed sibling via an `UPDATE
//!    ff_exec_core ... WHERE lifecycle_phase NOT IN ('terminal',
//!    'cancelled')` (idempotent — already-terminal siblings are a
//!    silent no-op), then clear the flag + members array and DELETE
//!    the pending row. One atomic commit per group.
//!
//! `FOR UPDATE SKIP LOCKED` is the coalescing primitive: two
//! dispatchers pulled from the same `ff_pending_cancel_groups` batch
//! can race on the same edge_group row, but only one acquires the
//! row lock; the loser sees no row and continues. No duplicate
//! cancels are issued.

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::EngineError;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;

/// Per-tick batch size. Matches the Valkey dispatcher's `BATCH_SIZE`
/// (50) so the two backends bound per-tick work identically.
const BATCH_SIZE: i64 = 50;

/// Counters surfaced by one `dispatcher_tick` call.
#[derive(Debug, Default, Clone)]
pub struct DispatchReport {
    /// Groups drained (cancels issued + flag cleared + pending row
    /// DELETEd).
    pub dispatched: u64,
    /// Pending rows that pointed at a missing edge_group; DELETEd
    /// without issuing cancels.
    pub orphaned: u64,
    /// Pending rows that belonged to an already-cleared group (flag
    /// == false); left for the reconciler to SREM.
    pub skipped_for_reconciler: u64,
    /// Groups a concurrent dispatcher already held (`SKIP LOCKED`);
    /// left for the next tick.
    pub skipped_locked: u64,
    /// Transient transport / state errors encountered per group.
    pub errors: u64,
}

/// RFC-016 Stage C dispatcher. Pulls a batch of
/// `ff_pending_cancel_groups` rows and drains each in its own
/// transaction.
///
/// `filter` is accepted for trait parity with the Valkey scanner
/// runner. Today only `namespace` is honoured (cheapest to check via
/// a join on `ff_exec_core.raw_fields->>'namespace'`); richer filters
/// are a follow-up if multi-tenant deployments start using the
/// Postgres backend.
pub async fn dispatcher_tick(
    pool: &PgPool,
    _filter: &ScannerFilter,
) -> Result<DispatchReport, EngineError> {
    let pending: Vec<(i16, Uuid, Uuid)> = sqlx::query_as(
        r#"
        SELECT partition_key, flow_id, downstream_eid
        FROM ff_pending_cancel_groups
        ORDER BY enqueued_at_ms
        LIMIT $1
        "#,
    )
    .bind(BATCH_SIZE)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut report = DispatchReport::default();

    for (partition_key, flow_id, downstream_eid) in pending {
        match dispatch_one_group(pool, partition_key, flow_id, downstream_eid).await {
            Ok(GroupOutcome::Dispatched) => report.dispatched += 1,
            Ok(GroupOutcome::Orphaned) => report.orphaned += 1,
            Ok(GroupOutcome::SkippedForReconciler) => report.skipped_for_reconciler += 1,
            Ok(GroupOutcome::SkippedLocked) => report.skipped_locked += 1,
            Err(e) => {
                tracing::warn!(
                    partition_key,
                    %flow_id,
                    %downstream_eid,
                    error = %e,
                    "pg edge_cancel_dispatcher: group drain failed; retry next tick"
                );
                report.errors += 1;
            }
        }
    }

    Ok(report)
}

enum GroupOutcome {
    Dispatched,
    Orphaned,
    SkippedForReconciler,
    SkippedLocked,
}

async fn dispatch_one_group(
    pool: &PgPool,
    partition_key: i16,
    flow_id: Uuid,
    downstream_eid: Uuid,
) -> Result<GroupOutcome, EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // `FOR UPDATE SKIP LOCKED` — concurrent dispatcher coalescing. If
    // another tick already holds the lock, we return None and leave
    // the pending row alone; the other tick will drain and DELETE.
    let row = sqlx::query(
        r#"
        SELECT cancel_siblings_pending_flag,
               cancel_siblings_pending_members
        FROM ff_edge_group
        WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3
        FOR UPDATE SKIP LOCKED
        "#,
    )
    .bind(partition_key)
    .bind(flow_id)
    .bind(downstream_eid)
    .fetch_optional(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    let Some(row) = row else {
        // Two cases collapse here: (a) no such edge_group (orphan)
        // vs (b) row locked by a concurrent dispatcher. Distinguish
        // with a non-locking probe.
        let exists: Option<(bool,)> = sqlx::query_as(
            "SELECT true FROM ff_edge_group
             WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
        )
        .bind(partition_key)
        .bind(flow_id)
        .bind(downstream_eid)
        .fetch_optional(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        if exists.is_some() {
            // Locked by a sibling dispatcher — leave everything
            // alone; the lock-holder owns drain.
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(GroupOutcome::SkippedLocked);
        }

        // Orphan: DELETE the pending row and commit.
        sqlx::query(
            "DELETE FROM ff_pending_cancel_groups
             WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
        )
        .bind(partition_key)
        .bind(flow_id)
        .bind(downstream_eid)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
        tx.commit().await.map_err(map_sqlx_error)?;
        return Ok(GroupOutcome::Orphaned);
    };

    let flag: bool = row.get("cancel_siblings_pending_flag");
    let members_raw: Vec<String> = row.get("cancel_siblings_pending_members");

    if !flag {
        // Reconciler territory: flag already cleared but pending row
        // lingered. Leave for `reconciler_tick` to SREM (it needs to
        // emit the `sremmed_stale` metric).
        tx.rollback().await.map_err(map_sqlx_error)?;
        return Ok(GroupOutcome::SkippedForReconciler);
    }

    // Reason defaults to `sibling_quorum_satisfied` — the Postgres
    // writer path (dispatch.rs Decision::Satisfied branch) only
    // raises the flag when a CancelRemaining policy is satisfied. If
    // a future writer distinguishes `_impossible` it will need to
    // persist a discriminator column on ff_edge_group.
    let reason: &str = "sibling_quorum_satisfied";

    let now = now_ms();
    let members: Vec<Uuid> = members_raw
        .iter()
        .filter_map(|s| Uuid::parse_str(s).ok())
        .collect();
    for sib in &members {
        sqlx::query(
            r#"
            UPDATE ff_exec_core
            SET lifecycle_phase     = 'terminal',
                ownership_state     = 'unowned',
                eligibility_state   = 'not_applicable',
                public_state        = 'cancelled',
                attempt_state       = 'cancelled',
                terminal_at_ms      = COALESCE(terminal_at_ms, $3),
                cancellation_reason = COALESCE(cancellation_reason, $4),
                cancelled_by        = COALESCE(cancelled_by, 'engine'),
                raw_fields          = jsonb_set(raw_fields, '{last_mutation_at}', to_jsonb($3::text))
            WHERE partition_key = $1 AND execution_id = $2
              AND lifecycle_phase NOT IN ('terminal','cancelled')
            "#,
        )
        .bind(partition_key)
        .bind(sib)
        .bind(now)
        .bind(reason)
        .execute(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;
    }

    // Clear flag + members + DELETE the pending row. Single commit.
    sqlx::query(
        r#"
        UPDATE ff_edge_group
        SET cancel_siblings_pending_flag    = false,
            cancel_siblings_pending_members = '{}'::text[]
        WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3
        "#,
    )
    .bind(partition_key)
    .bind(flow_id)
    .bind(downstream_eid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    sqlx::query(
        "DELETE FROM ff_pending_cancel_groups
         WHERE partition_key = $1 AND flow_id = $2 AND downstream_eid = $3",
    )
    .bind(partition_key)
    .bind(flow_id)
    .bind(downstream_eid)
    .execute(&mut *tx)
    .await
    .map_err(map_sqlx_error)?;

    tx.commit().await.map_err(map_sqlx_error)?;

    Ok(GroupOutcome::Dispatched)
}

fn now_ms() -> i64 {
    let d = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    (d.as_millis() as i64).max(0)
}
