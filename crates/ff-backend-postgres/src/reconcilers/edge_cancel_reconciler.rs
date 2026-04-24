//! RFC-016 Stage D — Postgres sibling-cancel reconciler.
//!
//! Safety net that runs at a slower cadence than the Stage-C
//! dispatcher and recovers from a crash mid-cancel. For each row in
//! `ff_pending_cancel_groups` it decides one of three outcomes:
//!
//! - `sremmed_stale`: edge_group missing OR
//!   `cancel_siblings_pending_flag = false`. The pending row is a
//!   leftover; DELETE it.
//! - `completed_drain`: flag still `true` but every listed sibling
//!   is already terminal (cancels landed, crash before flag-clear).
//!   Clear the flag + members array and DELETE the pending row.
//! - `no_op`: flag `true` and at least one sibling still
//!   non-terminal. Dispatcher territory — leave the row alone.
//!
//! The reconciler MUST NOT fight the dispatcher: `no_op` is a hard
//! contract. The 1-tick-later dispatcher run will see the same row
//! and drain it.

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::EngineError;
use sqlx::{PgPool, Row};
use uuid::Uuid;

use crate::error::map_sqlx_error;

/// Per-tick batch size. Matches Valkey + dispatcher.
const BATCH_SIZE: i64 = 50;

/// Counters surfaced by one `reconciler_tick` call. Labels line up
/// with the `ff_sibling_cancel_reconcile_total{action}` metric.
#[derive(Debug, Default, Clone)]
pub struct ReconcileReport {
    pub sremmed_stale: u64,
    pub completed_drain: u64,
    pub no_op: u64,
    pub errors: u64,
}

pub async fn reconciler_tick(
    pool: &PgPool,
    _filter: &ScannerFilter,
) -> Result<ReconcileReport, EngineError> {
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

    let mut report = ReconcileReport::default();

    for (partition_key, flow_id, downstream_eid) in pending {
        match reconcile_one_group(pool, partition_key, flow_id, downstream_eid).await {
            Ok(Action::SremmedStale) => report.sremmed_stale += 1,
            Ok(Action::CompletedDrain) => report.completed_drain += 1,
            Ok(Action::NoOp) => report.no_op += 1,
            Err(e) => {
                tracing::warn!(
                    partition_key,
                    %flow_id,
                    %downstream_eid,
                    error = %e,
                    "pg edge_cancel_reconciler: reconcile failed; retry next tick"
                );
                report.errors += 1;
            }
        }
    }

    Ok(report)
}

enum Action {
    SremmedStale,
    CompletedDrain,
    NoOp,
}

async fn reconcile_one_group(
    pool: &PgPool,
    partition_key: i16,
    flow_id: Uuid,
    downstream_eid: Uuid,
) -> Result<Action, EngineError> {
    let mut tx = pool.begin().await.map_err(map_sqlx_error)?;

    // Non-blocking: if the dispatcher currently holds the lock we'd
    // rather step away entirely than wait (no_op protects that too).
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
        // Two cases: (a) edge_group deleted (retention) → stale; or
        // (b) dispatcher holds the lock → no_op. Probe without a lock.
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
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(Action::NoOp);
        }

        // Retention-deleted group → DELETE the orphan pending row.
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
        return Ok(Action::SremmedStale);
    };

    let flag: bool = row.get("cancel_siblings_pending_flag");
    let members_raw: Vec<String> = row.get("cancel_siblings_pending_members");

    if !flag {
        // Stale pending row — drain it.
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
        return Ok(Action::SremmedStale);
    }

    // flag=true: check if every listed sibling is already terminal.
    let members: Vec<Uuid> = members_raw
        .iter()
        .filter_map(|s| Uuid::parse_str(s).ok())
        .collect();

    if !members.is_empty() {
        let non_terminal: i64 = sqlx::query_scalar(
            r#"
            SELECT count(*)::bigint
            FROM ff_exec_core
            WHERE partition_key = $1
              AND execution_id = ANY($2::uuid[])
              AND lifecycle_phase NOT IN ('terminal','cancelled')
            "#,
        )
        .bind(partition_key)
        .bind(&members)
        .fetch_one(&mut *tx)
        .await
        .map_err(map_sqlx_error)?;

        if non_terminal > 0 {
            // Dispatcher owns this one.
            tx.rollback().await.map_err(map_sqlx_error)?;
            return Ok(Action::NoOp);
        }
    }

    // Either members list is empty + flag stuck on (degenerate crash
    // residue), or every member is terminal. Complete the drain.
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
    Ok(Action::CompletedDrain)
}
