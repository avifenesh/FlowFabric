//! SQLite-backend scanner reconcilers (RFC-023 Phase 3.5).
//!
//! Mirrors `ff-backend-postgres::reconcilers` in shape, but at N=1
//! partition fan-out per RFC-023 §4.1: SQLite is single-writer /
//! single-process, so there is no partition to iterate over. Each
//! reconciler is a single `scan_tick(pool, ...)` function the
//! [`scanner_supervisor`](crate::scanner_supervisor) drives on a
//! fixed cadence.
//!
//! Phase 3.5 ships only `budget_reset` — the scheduled-reset
//! backstop for Wave 9 budgets. `attempt_timeout` / `lease_expiry`
//! / `suspension_timeout` are intentionally NOT ported: SQLite's
//! single-writer invariant + manual admin ops cover that ground
//! under the dev-only envelope (§4.1 A3).

pub mod budget_reset;

/// Result of scanning one tick. Mirrors
/// `ff-backend-postgres::reconcilers::ScanReport` for supervisor-
/// side aggregation parity across backends.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub processed: u32,
    pub errors: u32,
}
