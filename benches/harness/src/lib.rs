//! Shared fixtures and utilities for the FlowFabric benchmark harness.
//!
//! Five scenarios live under `benches/` (criterion) and `src/bin/` (custom
//! long-running harnesses). All five emit the same JSON report shape, so
//! `benches/scripts/check_release.py` can compare them against a prior
//! release's snapshot without per-scenario glue.
//!
//! Phase A ships scenario 1 (submit → claim → complete throughput) only;
//! scenarios 2–5 land in Batch C follow-up issues.

pub mod report;
pub mod workload;

pub use report::{write_report, HostInfo, LatencyMs, Percentiles, Report, SYSTEM_FLOWFABRIC};
