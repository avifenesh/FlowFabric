//! Shared fixtures for the Postgres bench scenarios.
//!
//! Wave 7c mirrors the Valkey-side benches' direct-backend shape:
//! seed exec rows via raw SQL (matching
//! crates/ff-backend-postgres/tests/pg_engine_backend_attempt.rs), then
//! exercise the EngineBackend trait methods under test. Raw-SQL seeding
//! sidesteps the full `CreateExecutionArgs` plumbing — we only need a
//! claimable row, not the full ingress contract — and keeps the bench
//! code short enough to audit.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use ff_backend_postgres::PostgresBackend;
use ff_core::backend::{CapabilitySet, ClaimPolicy};
use ff_core::engine_backend::EngineBackend;
use ff_core::partition::PartitionConfig;
use ff_core::types::{LaneId, WorkerId, WorkerInstanceId};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use uuid::Uuid;

pub fn now_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis(),
    )
    .unwrap()
}

pub async fn connect_and_migrate(max_conns: u32) -> Result<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL")
        .or_else(|_| std::env::var("DATABASE_URL"))
        .context("FF_PG_TEST_URL or DATABASE_URL must be set")?;
    let pool = PgPoolOptions::new()
        .max_connections(max_conns)
        .connect(&url)
        .await
        .with_context(|| format!("connect {url}"))?;
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .context("apply_migrations")?;
    Ok(pool)
}

pub fn backend(pool: PgPool) -> Arc<dyn EngineBackend> {
    PostgresBackend::from_pool(pool, PartitionConfig::default())
}

pub async fn seed_exec(
    pool: &PgPool,
    lane: &str,
    caps: &[&str],
) -> Result<(i16, Uuid, LaneId)> {
    let part: i16 = 0;
    let exec_uuid = Uuid::new_v4();
    let caps: Vec<String> = caps.iter().map(|s| (*s).to_string()).collect();
    sqlx::query(
        r#"
        INSERT INTO ff_exec_core (
            partition_key, execution_id, flow_id, lane_id,
            required_capabilities, attempt_index,
            lifecycle_phase, ownership_state, eligibility_state,
            public_state, attempt_state,
            priority, created_at_ms
        ) VALUES (
            $1, $2, NULL, $3,
            $4, 0,
            'runnable', 'unowned', 'eligible_now',
            'waiting', 'pending_first_attempt',
            0, $5
        )
        "#,
    )
    .bind(part)
    .bind(exec_uuid)
    .bind(lane)
    .bind(&caps)
    .bind(now_ms())
    .execute(pool)
    .await
    .context("seed exec")?;
    Ok((part, exec_uuid, LaneId::new(lane)))
}

pub async fn bulk_seed(
    pool: &PgPool,
    lane: &str,
    caps: &[&str],
    n: usize,
) -> Result<Vec<Uuid>> {
    const CHUNK: usize = 500;
    let caps_vec: Vec<String> = caps.iter().map(|s| (*s).to_string()).collect();
    let mut out: Vec<Uuid> = Vec::with_capacity(n);
    let created = now_ms();
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(CHUNK);
        let ids: Vec<Uuid> = (0..take).map(|_| Uuid::new_v4()).collect();
        out.extend_from_slice(&ids);
        let mut q = String::from(
            "INSERT INTO ff_exec_core (partition_key, execution_id, flow_id, lane_id, required_capabilities, attempt_index, lifecycle_phase, ownership_state, eligibility_state, public_state, attempt_state, priority, created_at_ms) VALUES ",
        );
        for i in 0..take {
            if i > 0 { q.push(','); }
            let b = i * 3;
            q.push_str(&format!(
                "(0, ${}, NULL, ${}, ${}, 0, 'runnable', 'unowned', 'eligible_now', 'waiting', 'pending_first_attempt', 0, {})",
                b + 1, b + 2, b + 3, created,
            ));
        }
        let mut stmt = sqlx::query(&q);
        for id in &ids {
            stmt = stmt.bind(id).bind(lane).bind(&caps_vec);
        }
        stmt.execute(pool).await.context("bulk seed chunk")?;
        remaining -= take;
    }
    Ok(out)
}

pub fn claim_policy(worker: &str, ttl_ms: u32) -> ClaimPolicy {
    ClaimPolicy::new(
        WorkerId::new(worker),
        WorkerInstanceId::new(format!("{worker}-1")),
        ttl_ms,
        Some(Duration::from_millis(100)),
    )
}

pub fn capset(caps: &[&str]) -> CapabilitySet {
    CapabilitySet::new(caps.iter().map(|s| s.to_string()))
}

pub fn percentiles(samples: &[u64]) -> (f64, f64, f64) {
    if samples.is_empty() { return (0.0, 0.0, 0.0); }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let at = |q: f64| -> f64 {
        let idx = ((s.len() as f64 - 1.0) * q).round() as usize;
        s[idx.min(s.len() - 1)] as f64 / 1000.0
    };
    (at(0.50), at(0.95), at(0.99))
}

pub fn write_report(results_dir: &str, scenario: &str, report: serde_json::Value) -> Result<std::path::PathBuf> {
    let dir = std::path::Path::new(results_dir);
    std::fs::create_dir_all(dir)?;
    let sha = report["git_sha"].as_str().unwrap_or("unknown");
    let path = dir.join(format!("{scenario}-pg-{sha}.json"));
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    Ok(path)
}
