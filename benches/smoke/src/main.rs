//! v0.7 pre-release smoke harness.
//!
//! Wave 7b (pg-wave7b) — runs BEFORE tagging v0.7.0. The v0.6
//! release shipped a broken `read_summary` because smoke ran after
//! publish (`feedback_smoke_before_release`). This harness is the
//! release gate.
//!
//! # Scope
//!
//! Two backends, one scenario loop:
//!
//! * **Valkey backend** — regression sanity. Scenarios drive
//!   `ff-server` over HTTP, which matches the v0.6.1 consumer path.
//!   Proves the Valkey surface did not regress while v0.7 landed.
//! * **Postgres backend** — new v0.7 surface. Scenarios instantiate
//!   [`ff_backend_postgres::PostgresBackend`] via `connect` and
//!   exercise the [`ff_core::EngineBackend`] trait directly. There
//!   is no HTTP frontend for Postgres yet (wave 7 of RFC-v0.7
//!   doesn't wire one), so the trait is the only exposed consumer
//!   entry point.
//!
//! # Scenarios (§ "Scope — what the smoke must cover")
//!
//! 1. `claim_lifecycle`         — create → claim → progress → complete
//! 2. `flow_anyof`              — AnyOf{CancelRemaining} DAG drain
//! 3. `suspend_signal`          — suspend + deliver_signal (RFC-013/014)
//! 4. `stream_durable_summary`  — DurableSummary stream + read_summary
//! 5. `stream_best_effort`      — BestEffortLive dynamic MAXLEN
//! 6. `cancel_cascade`          — cancel cascade through dispatcher
//! 7. `fanout_slo`              — 50-way quorum fanout
//!
//! Each scenario is self-contained: own flow/execution IDs, own
//! cleanup. A scenario that can't run on a given backend (e.g.
//! method returns `EngineError::Unavailable`, or a feature is
//! gated off) returns [`ScenarioStatus::Skip`] with the reason
//! captured. In `--strict` mode, Skip is treated as Fail so a
//! release cannot go out on an un-exercised method.
//!
//! # Cross-backend parity
//!
//! Post-run, the main loop compares the set of scenarios that
//! Pass/Skip/Fail on each backend. A scenario that Passes on
//! Valkey but Fails or Skips on Postgres (or vice versa) is a
//! parity violation and fails the run. This is the "don't ship
//! v0.7 with a quiet regression on one backend" guardrail.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::{Parser, ValueEnum};
use serde::Serialize;

mod scenarios;

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Backend {
    Valkey,
    Postgres,
    Both,
}

#[derive(Parser, Debug)]
#[command(name = "ff-smoke-v0.7")]
#[command(about = "Pre-release v0.7 smoke gate. Exits 0 on all-pass, non-zero on any fail.")]
struct Args {
    /// Backend(s) to exercise.
    #[arg(long, value_enum, default_value_t = Backend::Both)]
    backend: Backend,

    /// Treat `Skip` as `Fail`. Required for release gating.
    #[arg(long)]
    strict: bool,

    /// FlowFabric HTTP server (used by Valkey scenarios). Defaults
    /// to the smoke-shell-script-launched instance on :9090.
    #[arg(long, env = "FF_SMOKE_SERVER", default_value = "http://localhost:9090")]
    server: String,

    /// Valkey host for the regression leg. Matches ff-server config.
    #[arg(long, env = "FF_SMOKE_VALKEY_HOST", default_value = "localhost")]
    valkey_host: String,
    #[arg(long, env = "FF_SMOKE_VALKEY_PORT", default_value_t = 6379)]
    valkey_port: u16,

    /// Postgres URL for the new-surface leg.
    #[arg(
        long,
        env = "FF_SMOKE_POSTGRES_URL",
        default_value = "postgres://postgres:postgres@localhost:5432/ff_smoke"
    )]
    postgres_url: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum ScenarioStatus {
    Pass,
    Skip,
    Fail,
}

#[derive(Debug, Serialize)]
pub struct ScenarioReport {
    pub name: &'static str,
    pub backend: &'static str,
    pub status: ScenarioStatus,
    pub duration_ms: u128,
    pub detail: Option<String>,
}

impl ScenarioReport {
    pub fn pass(name: &'static str, backend: &'static str, started: Instant) -> Self {
        Self {
            name,
            backend,
            status: ScenarioStatus::Pass,
            duration_ms: started.elapsed().as_millis(),
            detail: None,
        }
    }
    pub fn skip(name: &'static str, backend: &'static str, reason: impl Into<String>) -> Self {
        Self {
            name,
            backend,
            status: ScenarioStatus::Skip,
            duration_ms: 0,
            detail: Some(reason.into()),
        }
    }
    pub fn fail(
        name: &'static str,
        backend: &'static str,
        started: Instant,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            name,
            backend,
            status: ScenarioStatus::Fail,
            duration_ms: started.elapsed().as_millis(),
            detail: Some(reason.into()),
        }
    }
}

/// Context handed to every scenario — lets a scenario pick its
/// ingress/egress path per backend without every scenario having to
/// branch on `&str`.
pub struct SmokeCtx {
    pub http_server: String,
    pub http: reqwest::Client,
    pub partition_config: ff_core::partition::PartitionConfig,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info,sqlx=warn")),
        )
        .init();

    let args = Args::parse();
    let http = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    let ctx = Arc::new(SmokeCtx {
        http_server: args.server.clone(),
        http,
        partition_config: ff_core::partition::PartitionConfig::default(),
    });

    let mut all: Vec<ScenarioReport> = Vec::new();

    let run_valkey = matches!(args.backend, Backend::Valkey | Backend::Both);
    let run_postgres = matches!(args.backend, Backend::Postgres | Backend::Both);

    if run_valkey {
        tracing::info!("==== Valkey regression leg (HTTP against ff-server) ====");
        all.extend(scenarios::run_all_valkey(ctx.clone()).await);
    }
    if run_postgres {
        tracing::info!("==== Postgres new-surface leg (direct EngineBackend trait) ====");
        let pg_backend = match scenarios::postgres_connect(&args.postgres_url).await {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::error!(error = %e, "Postgres connect failed — every Postgres scenario will Fail");
                None
            }
        };
        all.extend(scenarios::run_all_postgres(ctx.clone(), pg_backend).await);
    }

    // Print per-scenario report.
    println!("\n==== Scenario report ====");
    for r in &all {
        let tag = match r.status {
            ScenarioStatus::Pass => "PASS",
            ScenarioStatus::Skip => "SKIP",
            ScenarioStatus::Fail => "FAIL",
        };
        let detail = r.detail.as_deref().unwrap_or("");
        println!(
            "[{tag}] {name:<30} {backend:<9} {ms:>5}ms  {detail}",
            tag = tag,
            name = r.name,
            backend = r.backend,
            ms = r.duration_ms,
            detail = detail
        );
    }

    // Cross-backend parity: for scenarios run on both, statuses must
    // match (Pass/Pass or Skip/Skip). Any asymmetry fails the gate.
    let mut parity_fails: Vec<String> = Vec::new();
    if matches!(args.backend, Backend::Both) {
        for name in scenarios::NAMES {
            let v = all
                .iter()
                .find(|r| r.name == *name && r.backend == "valkey")
                .map(|r| r.status);
            let p = all
                .iter()
                .find(|r| r.name == *name && r.backend == "postgres")
                .map(|r| r.status);
            if let (Some(v), Some(p)) = (v, p)
                && v != p
            {
                parity_fails.push(format!(
                    "{name}: valkey={:?} postgres={:?}",
                    v, p
                ));
            }
        }
    }

    let fail_count = all
        .iter()
        .filter(|r| r.status == ScenarioStatus::Fail)
        .count();
    let skip_count = all
        .iter()
        .filter(|r| r.status == ScenarioStatus::Skip)
        .count();
    let pass_count = all
        .iter()
        .filter(|r| r.status == ScenarioStatus::Pass)
        .count();

    println!(
        "\n==== Summary: {} pass, {} skip, {} fail ====",
        pass_count, skip_count, fail_count
    );
    if !parity_fails.is_empty() {
        println!("==== Parity violations ====");
        for pf in &parity_fails {
            println!("  {pf}");
        }
    }

    // Emit JSON summary for CI consumption.
    let json = serde_json::json!({
        "scenarios": all,
        "parity_violations": parity_fails,
        "strict": args.strict,
    });
    println!("\n==== JSON ====\n{}", serde_json::to_string_pretty(&json)?);

    let strict_skip_fail = args.strict && skip_count > 0;
    if fail_count > 0 || !parity_fails.is_empty() || strict_skip_fail {
        std::process::exit(1);
    }
    Ok(())
}
