//! Dependency-resolution reconciler (RFC-v0.7 Wave 6a).
//!
//! Backstop for the per-hop-tx cascade implemented in
//! [`crate::dispatch::dispatch_completion`] (Wave 5a). The cascade
//! is deliberately NOT wrapped in a single transaction spanning
//! transitive descendants — a mid-cascade failure (crash, lock
//! timeout, SERIALIZABLE retry exhaustion) leaves downstream
//! `ff_completion_event` rows with `dispatched_at_ms IS NULL`. This
//! reconciler sweeps those rows on its interval and re-invokes
//! `dispatch_completion`, which is idempotent via the
//! `UPDATE ... RETURNING` claim on `dispatched_at_ms`.
//!
//! # Transitive-descendant sweep (K-2 round-2)
//!
//! The reconciler query covers the **entire transitive descendant
//! set** of any interrupted cascade, not just 1-hop children of the
//! original trigger. Proof by construction:
//!
//! 1. Every `PolicyDecision::Satisfied` / `::Impossible` that
//!    transitions a downstream to `completed` / `skipped` INSERTs a
//!    new row into `ff_completion_event` inside the same per-hop tx
//!    ([`crate::dispatch::advance_edge_group`] line ~430).
//! 2. If the per-hop tx for hop N commits but hop N+1 crashes, the
//!    hop-N event lands durably with `dispatched_at_ms = NULL`.
//! 3. The reconciler finds that row via the partial index
//!    `ff_completion_event_pending_dispatch_idx` and fires
//!    `dispatch_completion(event_id)`, which kicks the cascade at
//!    hop N+1.
//! 4. Hop N+1's successful execution emits hop N+2's event, and so
//!    on — the reconciler's next tick (or the in-process cascade
//!    inside `dispatch_completion` itself) carries the chain
//!    forward.
//!
//! So a single crashed cascade of depth D eventually drains in at
//! most D reconciler ticks regardless of where the interruption
//! happened in the chain.
//!
//! # Interval + threshold
//!
//! `stale_threshold_ms` (default 1 000 ms) keeps the reconciler out
//! of the hot path: the normal in-process dispatcher flips
//! `dispatched_at_ms` within sub-millisecond of the commit that
//! inserted the row, so rows younger than the threshold are left
//! for the primary dispatcher.

use ff_core::backend::ScannerFilter;
use ff_core::engine_error::EngineError;
use sqlx::{PgPool, Row};

use crate::dispatch::{dispatch_completion, DispatchOutcome};
use crate::error::map_sqlx_error;

/// Per-tick work report. Returned so the driving scanner task can
/// emit metrics + logs at cycle boundary without re-querying.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Rows matched by the scan query before filter application.
    pub scanned: u64,
    /// Events that produced `DispatchOutcome::Advanced(_)`.
    pub reconciled: u64,
    /// Events that produced `DispatchOutcome::NoOp` (concurrent
    /// dispatcher won the claim race between our SELECT and our
    /// re-dispatch — legal, logged at trace).
    pub noop: u64,
    /// Events skipped by the `ScannerFilter`.
    pub filtered: u64,
    /// Re-dispatch invocations that returned `EngineError`. The
    /// event row stays `dispatched_at_ms IS NULL` and the next tick
    /// will retry.
    pub errors: u64,
}

/// Max rows per tick. Bounds the per-tick pool + transaction budget.
const BATCH_LIMIT: i64 = 1_000;

/// Default minimum age of a `ff_completion_event` row before the
/// reconciler will claim it. Keeps this off the hot path — the
/// primary dispatcher has this long to flip `dispatched_at_ms` from
/// NULL before the reconciler takes over.
pub const DEFAULT_STALE_THRESHOLD_MS: i64 = 1_000;

/// Run one reconciler tick. See module docs for semantics.
///
/// The query uses the `ff_completion_event_pending_dispatch_idx`
/// partial index from migration 0003 for partition-local index
/// scans. `ScannerFilter` is applied inline via optional SQL
/// predicates (namespace + instance_tag both live on the event row
/// — issue #122 column additions) so we do not pay an extra RTT per
/// candidate like the Valkey reconciler does.
#[tracing::instrument(name = "pg.dep_reconciler.tick", skip(pool, filter))]
pub async fn reconcile_tick(
    pool: &PgPool,
    filter: &ScannerFilter,
    stale_threshold_ms: i64,
) -> Result<ReconcileReport, EngineError> {
    let now_ms = i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
    )
    .unwrap_or(i64::MAX);
    let cutoff_ms = now_ms.saturating_sub(stale_threshold_ms);

    // We bind both filter dimensions as NULLABLE parameters and let
    // `($N IS NULL OR column = $N)` short-circuit in the planner.
    // That keeps one prepared statement for every filter shape.
    let ns_param: Option<String> = filter.namespace.as_ref().map(|ns| ns.as_str().to_owned());
    let tag_param: Option<String> = filter
        .instance_tag
        .as_ref()
        .map(|(_, v)| v.clone());

    let rows = sqlx::query(
        r#"
        SELECT event_id, namespace, instance_tag
          FROM ff_completion_event
         WHERE dispatched_at_ms IS NULL
           AND committed_at_ms < $1
           AND ($2::text IS NULL OR namespace = $2)
           AND ($3::text IS NULL OR instance_tag = $3)
         ORDER BY event_id ASC
         LIMIT $4
        "#,
    )
    .bind(cutoff_ms)
    .bind(&ns_param)
    .bind(&tag_param)
    .bind(BATCH_LIMIT)
    .fetch_all(pool)
    .await
    .map_err(map_sqlx_error)?;

    let mut report = ReconcileReport {
        scanned: rows.len() as u64,
        ..ReconcileReport::default()
    };

    for row in rows {
        let event_id: i64 = row.get("event_id");
        match dispatch_completion(pool, event_id).await {
            Ok(DispatchOutcome::Advanced(n)) => {
                report.reconciled += 1;
                tracing::debug!(
                    event_id,
                    advanced = n,
                    "dep_reconciler: re-dispatched stale completion event"
                );
            }
            Ok(DispatchOutcome::NoOp) => {
                report.noop += 1;
                tracing::trace!(
                    event_id,
                    "dep_reconciler: concurrent dispatcher won claim race"
                );
            }
            Err(e) => {
                report.errors += 1;
                tracing::warn!(
                    event_id,
                    error = %e,
                    "dep_reconciler: dispatch_completion failed — will retry next tick"
                );
            }
        }
    }

    // `filtered` is always zero here: filter pushdown is SQL-side.
    // The field is retained in the report so a future filter
    // dimension that can't be pushed down stays additive-compatible.
    let _ = &mut report.filtered;

    Ok(report)
}

// ── unit tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_default_is_zero() {
        let r = ReconcileReport::default();
        assert_eq!(r.scanned, 0);
        assert_eq!(r.reconciled, 0);
        assert_eq!(r.noop, 0);
        assert_eq!(r.filtered, 0);
        assert_eq!(r.errors, 0);
    }

    #[test]
    fn default_threshold_is_one_second() {
        assert_eq!(DEFAULT_STALE_THRESHOLD_MS, 1_000);
    }
}
