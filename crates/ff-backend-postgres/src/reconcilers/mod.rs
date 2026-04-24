//! Postgres-backend scanner reconcilers (RFC-v0.7 Wave 6).
//!
//! These modules implement the Postgres twins of the Valkey scanners
//! in `ff-engine::scanner::*`. Each reconciler is a single
//! `reconcile_tick(pool, filter, ...)` function the engine's scanner
//! task drives on a fixed interval.
//!
//! Wave 6a ships the dependency reconciler — the backstop for the
//! per-hop-tx dispatch cascade from Wave 5a.

pub mod dependency;
