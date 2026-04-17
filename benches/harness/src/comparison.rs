//! Shared comparison infrastructure for bench systems.
//!
//! Every system under measurement — FlowFabric, apalis, faktory, the
//! hand-rolled baseline — implements a common runner so the
//! harness-level logic (FLUSHALL between runs, envelope timing, JSON
//! emission) is written exactly once.
//!
//! ## Static dispatch, not dyn
//!
//! The universe of systems is fixed (four today, one more if we ever
//! add celery-rs / sidekiq / etc). An enum-over-trait-impls gives
//! predictable codegen, no `async_trait` macro, and keeps the type
//! surface grep-able. A plugin architecture would be misleading:
//! adding a system is a code change, not a configuration change.
//!
//! ## Support levels
//!
//! A system doesn't have to support every scenario natively. Each
//! runner's [`supports`] return value distinguishes three cases:
//!
//!   * [`SystemSupport::Native`] — the system has a first-class
//!     primitive. Numbers are directly comparable.
//!   * [`SystemSupport::Approximation`] — the system lacks the native
//!     primitive; the runner emulates it with whatever it does have.
//!     Reports land under a distinct `system` name (e.g.
//!     `apalis-approx` rather than `apalis`) so aggregators don't
//!     accidentally graph them alongside native numbers. The emitted
//!     JSON includes an `Approximation: <why>` prefix in `notes`.
//!   * [`SystemSupport::Unsupported`] — the system has no sensible
//!     emulation; the runner skips and emits a zero-throughput report
//!     with `Unsupported: <why>` in notes so COMPARISON.md knows the
//!     gap is structural, not missing data.
//!
//! [`supports`]: ComparisonRunner::supports

use crate::report::Percentiles;
use crate::workload::BenchEnv;
use crate::{LatencyMs, Report};

/// Identifier for a scenario in the bench suite. Extend rather than
/// generify — if a scenario needs new knobs, add fields to
/// [`RunConfig`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Scenario {
    /// Scenario 1: submit → claim → complete throughput on a flat queue.
    SubmitClaimComplete,
    /// Scenario 2: suspend → signal → resume roundtrip latency.
    SuspendSignalResume,
    /// Scenario 4: linear-chain flow DAG end-to-end. Config `nodes`
    /// sets chain length.
    FlowDagLinear,
    /// Scenario 5: capability-routed claim with worker cap sets.
    CapabilityRouted,
}

impl Scenario {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scenario::SubmitClaimComplete => "submit_claim_complete",
            Scenario::SuspendSignalResume => "suspend_signal_resume",
            Scenario::FlowDagLinear => "flow_dag_linear",
            Scenario::CapabilityRouted => "capability_routed",
        }
    }
}

/// How well a system handles a given scenario.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SystemSupport {
    /// First-class primitive. Numbers are directly comparable.
    Native,
    /// Emulation / workaround. Report under a `-approx` system name,
    /// never aggregate with native numbers.
    Approximation { notes: &'static str },
    /// Structurally unsupported. Emit a zero-throughput "gap" entry so
    /// the comparison table shows we tried.
    Unsupported { notes: &'static str },
}

/// Workload knobs shared across scenarios. Extra dimensions for a
/// specific scenario go here; runners ignore fields they don't use.
#[derive(Clone, Debug)]
pub struct RunConfig {
    pub tasks: usize,
    pub workers: usize,
    pub payload_bytes: usize,
    /// Scenario 4 chain length. Ignored by other scenarios.
    pub nodes: usize,
    /// Scenario 4 parallel flow count — how many DAGs to drive at
    /// once. Ignored by other scenarios.
    pub flows: usize,
}

impl RunConfig {
    pub fn small() -> Self {
        Self {
            tasks: 1_000,
            workers: 8,
            payload_bytes: 4 * 1024,
            nodes: 3,
            flows: 10,
        }
    }

    pub fn default_phase_a() -> Self {
        Self {
            tasks: 10_000,
            workers: 16,
            payload_bytes: 4 * 1024,
            nodes: 10,
            flows: 100,
        }
    }
}

/// Raw output of a single run. The harness transforms this into a
/// [`Report`] before writing JSON.
pub struct RunResult {
    pub throughput_ops_per_sec: f64,
    /// Per-operation latency samples in microseconds. The aggregator
    /// computes percentiles; runners just collect.
    pub latencies_us: Vec<u64>,
    pub notes: Option<String>,
}

/// Implementors measure one system. The harness drives them via
/// [`run_scenario`].
pub trait ComparisonRunner {
    fn system_name(&self) -> &'static str;

    fn supports(&self, scenario: Scenario) -> SystemSupport;

    /// Prepare the target system for a clean run — establish
    /// connections, clear residue from prior runs, etc. Called once
    /// per bench invocation before [`run`](Self::run).
    fn setup(
        &mut self,
        env: &BenchEnv,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Release any process-local resources. Called once at the end of
    /// a bench invocation. Hook for disconnecting pools, joining
    /// background tasks, etc.
    fn teardown(
        &mut self,
        env: &BenchEnv,
    ) -> impl std::future::Future<Output = anyhow::Result<()>> + Send;

    /// Run one scenario to completion. The runner owns the workload
    /// guts — seeding, worker orchestration, collection. Returns a
    /// [`RunResult`]; the harness wraps it in a [`Report`].
    fn run(
        &mut self,
        scenario: Scenario,
        config: &RunConfig,
        env: &BenchEnv,
    ) -> impl std::future::Future<Output = anyhow::Result<RunResult>> + Send;
}

/// Drive one runner through one scenario, converting the raw
/// [`RunResult`] into a canonical [`Report`]. The name written to the
/// report's `system` field is the runner's `system_name()` unless the
/// runner flagged the scenario as [`SystemSupport::Approximation`], in
/// which case `-approx` is appended. The two prefixes prevent
/// aggregators from graphing approximations next to native numbers.
pub async fn run_scenario<R: ComparisonRunner>(
    runner: &mut R,
    scenario: Scenario,
    config: &RunConfig,
    env: &BenchEnv,
) -> anyhow::Result<Report> {
    let support = runner.supports(scenario);
    let system = match &support {
        SystemSupport::Approximation { .. } => {
            format!("{}-approx", runner.system_name())
        }
        _ => runner.system_name().to_owned(),
    };

    let (throughput, latency_ms, notes) = match &support {
        SystemSupport::Unsupported { notes } => {
            // Don't even call `run`; the system has no sensible shape
            // for this workload. Emit a zero-throughput entry so the
            // comparison table can render the gap intentionally.
            (
                0.0,
                LatencyMs::default(),
                Some(format!("Unsupported: {notes}")),
            )
        }
        SystemSupport::Approximation { notes } => {
            let result = runner.run(scenario, config, env).await?;
            let latency = Percentiles::from_micros(&result.latencies_us);
            let merged = merge_notes(Some(&format!("Approximation: {notes}")), result.notes);
            (result.throughput_ops_per_sec, latency, merged)
        }
        SystemSupport::Native => {
            let result = runner.run(scenario, config, env).await?;
            let latency = Percentiles::from_micros(&result.latencies_us);
            (result.throughput_ops_per_sec, latency, result.notes)
        }
    };

    let report_config = serde_json::json!({
        "tasks": config.tasks,
        "workers": config.workers,
        "payload_bytes": config.payload_bytes,
        "nodes": config.nodes,
        "flows": config.flows,
    });

    let mut report = Report::fill_env(
        scenario.as_str(),
        system,
        env.cluster,
        report_config,
        throughput,
        latency_ms,
    );
    report.notes = notes;
    Ok(report)
}

fn merge_notes(prefix: Option<&str>, tail: Option<String>) -> Option<String> {
    match (prefix, tail) {
        (None, None) => None,
        (Some(p), None) => Some(p.to_owned()),
        (None, Some(t)) => Some(t),
        (Some(p), Some(t)) => Some(format!("{p} | {t}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FakeRunner {
        support: SystemSupport,
        throughput: f64,
        latencies_us: Vec<u64>,
        notes: Option<String>,
    }

    impl ComparisonRunner for FakeRunner {
        fn system_name(&self) -> &'static str {
            "fake"
        }

        fn supports(&self, _: Scenario) -> SystemSupport {
            self.support.clone()
        }

        async fn setup(&mut self, _env: &BenchEnv) -> anyhow::Result<()> {
            Ok(())
        }

        async fn teardown(&mut self, _env: &BenchEnv) -> anyhow::Result<()> {
            Ok(())
        }

        async fn run(
            &mut self,
            _scenario: Scenario,
            _config: &RunConfig,
            _env: &BenchEnv,
        ) -> anyhow::Result<RunResult> {
            Ok(RunResult {
                throughput_ops_per_sec: self.throughput,
                latencies_us: self.latencies_us.clone(),
                notes: self.notes.clone(),
            })
        }
    }

    #[tokio::test]
    async fn approximation_appends_suffix_and_note() {
        let mut r = FakeRunner {
            support: SystemSupport::Approximation {
                notes: "no native suspend",
            },
            throughput: 42.0,
            latencies_us: vec![100],
            notes: Some("downstream note".to_owned()),
        };
        let env = BenchEnv::from_env();
        let report =
            run_scenario(&mut r, Scenario::SuspendSignalResume, &RunConfig::small(), &env)
                .await
                .unwrap();
        assert_eq!(report.system, "fake-approx");
        assert_eq!(
            report.notes.as_deref(),
            Some("Approximation: no native suspend | downstream note")
        );
    }

    #[tokio::test]
    async fn unsupported_emits_zero_without_calling_run() {
        let mut r = FakeRunner {
            support: SystemSupport::Unsupported { notes: "no DAG" },
            throughput: 999.0, // would be wrong to report — run() isn't called
            latencies_us: vec![],
            notes: None,
        };
        let env = BenchEnv::from_env();
        let report =
            run_scenario(&mut r, Scenario::FlowDagLinear, &RunConfig::small(), &env)
                .await
                .unwrap();
        assert_eq!(report.system, "fake");
        assert_eq!(report.throughput_ops_per_sec, 0.0);
        assert_eq!(report.notes.as_deref(), Some("Unsupported: no DAG"));
    }

    #[tokio::test]
    async fn native_keeps_system_name_and_runner_notes() {
        let mut r = FakeRunner {
            support: SystemSupport::Native,
            throughput: 1234.5,
            latencies_us: vec![10, 20, 30],
            notes: Some("extra=1".to_owned()),
        };
        let env = BenchEnv::from_env();
        let report = run_scenario(
            &mut r,
            Scenario::SubmitClaimComplete,
            &RunConfig::small(),
            &env,
        )
        .await
        .unwrap();
        assert_eq!(report.system, "fake");
        assert_eq!(report.notes.as_deref(), Some("extra=1"));
        assert!(report.latency_ms.p50 > 0.0);
    }
}
