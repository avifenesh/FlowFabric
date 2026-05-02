//! RFC-025 worker-registry round-trip demo.
//!
//! Exercises the four + one trait methods landed across Phase 2–4:
//! `register_worker`, `heartbeat_worker`, `list_workers`,
//! `mark_worker_dead`, + the PG/SQLite-shaped `list_expired_leases`
//! (not fired here — this demo is Valkey-backed, and Valkey returns
//! leases through the same ZRANGEBYSCORE scanner the real reclaim
//! path uses).
//!
//! Run against a local Valkey on :6379 — the `scripts/run-all-examples.sh`
//! harness brings one up for you.

use std::collections::BTreeSet;
use std::env;

use ff_backend_valkey::ValkeyBackend;
use ff_core::backend::BackendConfig;
use ff_core::contracts::{
    HeartbeatWorkerArgs, HeartbeatWorkerOutcome, ListWorkersArgs, MarkWorkerDeadArgs,
    MarkWorkerDeadOutcome, RegisterWorkerArgs, RegisterWorkerOutcome,
};
use ff_core::types::{LaneId, Namespace, TimestampMs, WorkerId, WorkerInstanceId};

fn now_ms() -> TimestampMs {
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock went backwards")
        .as_millis() as i64;
    TimestampMs::from_millis(ms)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let host = env::var("FF_DEMO_VALKEY_HOST").unwrap_or_else(|_| "127.0.0.1".into());
    let port: u16 = env::var("FF_DEMO_VALKEY_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(6379);

    println!("== RFC-025 worker-registry demo ({host}:{port}) ==");

    let cfg = BackendConfig::valkey(host, port);
    let backend = ValkeyBackend::connect(cfg).await?;

    let namespace = Namespace::new("rfc025-demo");
    let worker_id = WorkerId::new("gpu-pool-1");
    let instance_id = WorkerInstanceId::new(format!(
        "demo-{}",
        std::process::id()
    ));
    let lanes = BTreeSet::from([LaneId::new("default")]);
    let caps = BTreeSet::from(["cuda".to_string(), "fp16".to_string()]);
    let ttl_ms: u64 = 5_000;

    // --- register ------------------------------------------------
    let args = RegisterWorkerArgs::new(
        worker_id.clone(),
        instance_id.clone(),
        lanes.clone(),
        caps.clone(),
        ttl_ms,
        namespace.clone(),
        now_ms(),
    );
    let outcome = backend.register_worker(args).await?;
    match outcome {
        RegisterWorkerOutcome::Registered => println!("register_worker → Registered (fresh)"),
        RegisterWorkerOutcome::Refreshed => println!("register_worker → Refreshed (hot-restart)"),
        _ => unreachable!("non_exhaustive RegisterWorkerOutcome extended"),
    }

    // --- idempotent re-register (caps hot-swap, §9.3) ------------
    let caps_swap = BTreeSet::from(["cuda".to_string(), "fp16".to_string(), "tensor-cores".to_string()]);
    let re_args = RegisterWorkerArgs::new(
        worker_id.clone(),
        instance_id.clone(),
        lanes.clone(),
        caps_swap.clone(),
        ttl_ms,
        namespace.clone(),
        now_ms(),
    );
    match backend.register_worker(re_args).await? {
        RegisterWorkerOutcome::Refreshed => {
            println!("re-register with new caps → Refreshed (caps overwritten in place)")
        }
        RegisterWorkerOutcome::Registered => {
            return Err("expected Refreshed on re-register, got Registered".into());
        }
        _ => unreachable!("non_exhaustive RegisterWorkerOutcome extended"),
    }

    // --- heartbeat -----------------------------------------------
    let hb_args = HeartbeatWorkerArgs::new(instance_id.clone(), namespace.clone(), now_ms());
    match backend.heartbeat_worker(hb_args).await? {
        HeartbeatWorkerOutcome::Refreshed { next_expiry_ms } => {
            println!("heartbeat_worker → Refreshed next_expiry_ms={next_expiry_ms}");
        }
        HeartbeatWorkerOutcome::NotRegistered => {
            return Err("heartbeat returned NotRegistered for a just-registered worker".into());
        }
        _ => unreachable!("non_exhaustive HeartbeatWorkerOutcome extended"),
    }

    // --- list_workers (namespace-scoped; cross-ns returns Unavailable) ---
    let mut list_args = ListWorkersArgs::new();
    list_args.namespace = Some(namespace.clone());
    let page = backend.list_workers(list_args).await?;
    println!(
        "list_workers(ns={namespace:?}) → {} entries",
        page.entries.len()
    );
    for w in &page.entries {
        println!(
            "  - instance={} worker_id={} lanes={:?} caps={:?}",
            w.worker_instance_id, w.worker_id, w.lanes, w.capabilities
        );
    }

    // --- mark_worker_dead ----------------------------------------
    let md = MarkWorkerDeadArgs::new(
        instance_id.clone(),
        namespace.clone(),
        "demo shutdown".into(),
        now_ms(),
    );
    match backend.mark_worker_dead(md).await? {
        MarkWorkerDeadOutcome::Marked => println!("mark_worker_dead → Marked"),
        MarkWorkerDeadOutcome::NotRegistered => {
            return Err("expected Marked, got NotRegistered".into());
        }
        _ => unreachable!("non_exhaustive MarkWorkerDeadOutcome extended"),
    }

    // --- idempotent second mark ----------------------------------
    let md2 = MarkWorkerDeadArgs::new(
        instance_id.clone(),
        namespace.clone(),
        "demo shutdown (replay)".into(),
        now_ms(),
    );
    match backend.mark_worker_dead(md2).await? {
        MarkWorkerDeadOutcome::NotRegistered => {
            println!("mark_worker_dead (replay) → NotRegistered (idempotent)")
        }
        MarkWorkerDeadOutcome::Marked => {
            return Err("expected NotRegistered on replay, got Marked".into());
        }
        _ => unreachable!("non_exhaustive MarkWorkerDeadOutcome extended"),
    }

    // --- heartbeat on a dead worker surfaces NotRegistered -------
    let hb_after = HeartbeatWorkerArgs::new(instance_id.clone(), namespace.clone(), now_ms());
    match backend.heartbeat_worker(hb_after).await? {
        HeartbeatWorkerOutcome::NotRegistered => {
            println!("heartbeat_worker post-mark → NotRegistered (re-register required)")
        }
        HeartbeatWorkerOutcome::Refreshed { .. } => {
            return Err("heartbeat revived a dead worker".into());
        }
        _ => unreachable!("non_exhaustive HeartbeatWorkerOutcome extended"),
    }

    println!("== demo PASS ==");
    Ok(())
}
