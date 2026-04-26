//! Postgres-backend scanner reconcilers (RFC-v0.7 Wave 6).
//!
//! These modules implement the Postgres twins of the Valkey scanners
//! in `ff-engine::scanner::*`. Each reconciler is a single
//! `reconcile_tick(pool, filter, ...)` function the engine's scanner
//! task drives on a fixed interval.
//!
//! Wave 6a ships the dependency reconciler — the backstop for the
//! per-hop-tx dispatch cascade from Wave 5a.

pub mod attempt_timeout;
pub mod budget_reset;
pub mod dependency;
pub mod edge_cancel_dispatcher;
pub mod edge_cancel_reconciler;
pub mod lease_expiry;
pub mod suspension_timeout;

/// Result of scanning one partition. Mirrors
/// [`ff_engine::scanner::ScanResult`] so engine-side aggregation code
/// stays identical across backends; kept here (not re-exported from
/// ff-engine) so `ff-backend-postgres` does not take a dep on the
/// engine crate. The engine's `scan_tick_pg` wrapper maps one to the
/// other.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScanReport {
    pub processed: u32,
    pub errors: u32,
}
