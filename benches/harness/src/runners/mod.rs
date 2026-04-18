//! Per-system `ComparisonRunner` implementations.
//!
//! Each module owns one system's workload guts. Add a runner by
//! 1. dropping a module here,
//! 2. re-exporting it below,
//! 3. wiring a match arm in whatever top-level driver invokes the
//!    scenario (e.g. `src/bin/flow_dag.rs`).
//!
//! FlowFabric lives in the harness crate so it can share the SDK
//! without an extra workspace. The apalis / faktory / baseline
//! runners live under `benches/comparisons/<system>/` so their
//! dep graphs stay isolated.

pub mod flowfabric;
