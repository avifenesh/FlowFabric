//! apalis comparison — scenario 4 (linear DAG as chained jobs).
//!
//! apalis has no flow/DAG primitive. A consumer who wants a 10-node
//! linear chain in apalis wires it by hand: each stage's handler
//! schedules the next stage via the shared storage. That's the
//! comparison target, not a hypothetical "apalis DAG".
//!
//! # Shape
//!
//! 10 distinct job types `Stage0..Stage9`. Each stage's handler does
//! the stage work, then:
//!   * `Stage9`: INCR `bench:apalis:flow_completed` (terminal; no
//!     further scheduling). A driver on the main task polls this
//!     counter and stops when it equals `--flows`.
//!   * `Stage_{i < 9}`: push the next stage's job via the shared
//!     storage.
//!
//! The measurement: submit `--flows` Stage0 jobs at t=0; drain ends
//! when `bench:apalis:flow_completed == flows`.
//!
//! # What this measures
//!
//! * Wall time per flow = (end of flow_completed increment) -
//!   (Stage0 submit). Per-flow latencies aren't tracked — apalis
//!   doesn't expose "this specific job's completion time" without
//!   middleware wiring that changes the comparison. We report
//!   aggregate wall only.
//! * Throughput = flows / wall.
//!
//! # NOT comparable
//!
//! * No data_passing. Stages carry a dummy `flow_id` token; real
//!   users who want data flow wire JSON blobs into each stage's
//!   payload (and pay the round-trip serialization cost).
//! * No failure semantics. A Stage_i failure strands the flow at
//!   that stage; ff-bench's scenario 4 (when it lands in W3's
//!   flow_dag.rs) has FlowFabric's engine retry/cancel semantics.
//!
//! These deltas are why apalis's numbers on this scenario are a
//! "what it feels like today", not "what a fair DAG engine would
//! give you".
//!
//! # Polling-jitter floor
//!
//! The driver observes completion by polling `GET COMPLETED_KEY`
//! every 50 ms (see the drain loop further down). Up to 50 ms of
//! slack can sit between the Stage9 handler's INCR and the driver's
//! next GET — wall is inflated by that slack. At `flows=10` the
//! bias is ~5%; at `flows ≥ 100` it drops into noise. A tighter
//! measurement would use Redis pub/sub (SUBSCRIBE on a completion
//! channel the Stage9 handler PUBLISHes to) so the driver wakes
//! exactly when the last completion lands. Tracked as a Batch C
//! follow-up rather than fixed here — the current number is honest
//! for the apalis-canonical "poll a counter" shape and consistent
//! with how an apalis user would actually observe drain.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use apalis::prelude::*;
use apalis_redis::RedisStorage;
use clap::Parser;
use redis::AsyncCommands;
use serde::{Deserialize, Serialize};

const SCENARIO: &str = "flow_dag";
const SYSTEM: &str = "apalis-approx";
const STAGES: usize = 10;
const CONCURRENCY: usize = 16;
const COMPLETED_KEY: &str = "bench:apalis:flow_completed";

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FF_BENCH_VALKEY_HOST", default_value = "localhost")]
    valkey_host: String,
    #[arg(long, env = "FF_BENCH_VALKEY_PORT", default_value_t = 6379)]
    valkey_port: u16,
    /// Number of parallel flows to seed. Each flow is one linear
    /// 10-stage chain.
    #[arg(long, default_value_t = 100)]
    flows: usize,
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
    /// Abort if drain takes longer than this. Even a conservative
    /// apalis run shouldn't need more than a minute for 100 flows;
    /// the deadline catches a stuck worker instead of hanging CI.
    #[arg(long, default_value_t = 120)]
    deadline_secs: u64,
}

/// Macro-rolled stage jobs. Each variant carries the flow id so a
/// flow's trajectory can be traced across the chain. The next-stage
/// storage handles are threaded through a shared Arc so each handler
/// can schedule the successor.
macro_rules! stage_job {
    ($name:ident) => {
        #[derive(Debug, Clone, Serialize, Deserialize)]
        struct $name {
            flow_id: u64,
        }
    };
}

stage_job!(Stage0);
stage_job!(Stage1);
stage_job!(Stage2);
stage_job!(Stage3);
stage_job!(Stage4);
stage_job!(Stage5);
stage_job!(Stage6);
stage_job!(Stage7);
stage_job!(Stage8);
stage_job!(Stage9);

/// Storage handles shared across every stage's handler. Arc'd so the
/// handler closures can cheaply clone per invocation.
struct Storages {
    s1: tokio::sync::Mutex<RedisStorage<Stage1>>,
    s2: tokio::sync::Mutex<RedisStorage<Stage2>>,
    s3: tokio::sync::Mutex<RedisStorage<Stage3>>,
    s4: tokio::sync::Mutex<RedisStorage<Stage4>>,
    s5: tokio::sync::Mutex<RedisStorage<Stage5>>,
    s6: tokio::sync::Mutex<RedisStorage<Stage6>>,
    s7: tokio::sync::Mutex<RedisStorage<Stage7>>,
    s8: tokio::sync::Mutex<RedisStorage<Stage8>>,
    s9: tokio::sync::Mutex<RedisStorage<Stage9>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let url = format!("redis://{}:{}/", args.valkey_host, args.valkey_port);

    // Clean slate on the completion counter so repeated runs don't
    // carry over.
    let raw_client = redis::Client::open(url.clone())?;
    let mut raw = raw_client.get_multiplexed_async_connection().await?;
    let _: () = raw.del(COMPLETED_KEY).await?;

    // Create one RedisStorage per stage — each stage is a distinct
    // typed queue in apalis's model.
    async fn new_storage<T>(url: &str) -> Result<RedisStorage<T>>
    where
        T: 'static + Send + Serialize + for<'de> Deserialize<'de>,
    {
        let conn = apalis_redis::connect(url)
            .await
            .map_err(|e| anyhow::anyhow!("connect: {e}"))?;
        Ok(RedisStorage::new(conn))
    }

    let mut storage_0: RedisStorage<Stage0> = new_storage(&url).await?;
    let storage_1: RedisStorage<Stage1> = new_storage(&url).await?;
    let storage_2: RedisStorage<Stage2> = new_storage(&url).await?;
    let storage_3: RedisStorage<Stage3> = new_storage(&url).await?;
    let storage_4: RedisStorage<Stage4> = new_storage(&url).await?;
    let storage_5: RedisStorage<Stage5> = new_storage(&url).await?;
    let storage_6: RedisStorage<Stage6> = new_storage(&url).await?;
    let storage_7: RedisStorage<Stage7> = new_storage(&url).await?;
    let storage_8: RedisStorage<Stage8> = new_storage(&url).await?;
    let storage_9: RedisStorage<Stage9> = new_storage(&url).await?;

    let storages = Arc::new(Storages {
        s1: tokio::sync::Mutex::new(storage_1.clone()),
        s2: tokio::sync::Mutex::new(storage_2.clone()),
        s3: tokio::sync::Mutex::new(storage_3.clone()),
        s4: tokio::sync::Mutex::new(storage_4.clone()),
        s5: tokio::sync::Mutex::new(storage_5.clone()),
        s6: tokio::sync::Mutex::new(storage_6.clone()),
        s7: tokio::sync::Mutex::new(storage_7.clone()),
        s8: tokio::sync::Mutex::new(storage_8.clone()),
        s9: tokio::sync::Mutex::new(storage_9.clone()),
    });

    // One WorkerBuilder per stage. Each handler pushes the next stage.
    // apalis has no "chain" combinator so we wire it by hand — the
    // honest shape a consumer would ship.
    let s0_storages = storages.clone();
    let w0 = WorkerBuilder::new("s0")
        .backend(storage_0.clone())
        .concurrency(CONCURRENCY)
        .build(move |j: Stage0| {
            let st = s0_storages.clone();
            async move {
                st.s1.lock().await.push(Stage1 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s0->s1: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });
    let s1_storages = storages.clone();
    let w1 = WorkerBuilder::new("s1")
        .backend(storage_1)
        .concurrency(CONCURRENCY)
        .build(move |j: Stage1| {
            let st = s1_storages.clone();
            async move {
                st.s2.lock().await.push(Stage2 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s1->s2: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });
    let s2_storages = storages.clone();
    let w2 = WorkerBuilder::new("s2")
        .backend(storage_2)
        .concurrency(CONCURRENCY)
        .build(move |j: Stage2| {
            let st = s2_storages.clone();
            async move {
                st.s3.lock().await.push(Stage3 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s2->s3: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });
    let s3_storages = storages.clone();
    let w3 = WorkerBuilder::new("s3")
        .backend(storage_3)
        .concurrency(CONCURRENCY)
        .build(move |j: Stage3| {
            let st = s3_storages.clone();
            async move {
                st.s4.lock().await.push(Stage4 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s3->s4: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });
    let s4_storages = storages.clone();
    let w4 = WorkerBuilder::new("s4")
        .backend(storage_4)
        .concurrency(CONCURRENCY)
        .build(move |j: Stage4| {
            let st = s4_storages.clone();
            async move {
                st.s5.lock().await.push(Stage5 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s4->s5: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });
    let s5_storages = storages.clone();
    let w5 = WorkerBuilder::new("s5")
        .backend(storage_5)
        .concurrency(CONCURRENCY)
        .build(move |j: Stage5| {
            let st = s5_storages.clone();
            async move {
                st.s6.lock().await.push(Stage6 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s5->s6: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });
    let s6_storages = storages.clone();
    let w6 = WorkerBuilder::new("s6")
        .backend(storage_6)
        .concurrency(CONCURRENCY)
        .build(move |j: Stage6| {
            let st = s6_storages.clone();
            async move {
                st.s7.lock().await.push(Stage7 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s6->s7: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });
    let s7_storages = storages.clone();
    let w7 = WorkerBuilder::new("s7")
        .backend(storage_7)
        .concurrency(CONCURRENCY)
        .build(move |j: Stage7| {
            let st = s7_storages.clone();
            async move {
                st.s8.lock().await.push(Stage8 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s7->s8: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });
    let s8_storages = storages.clone();
    let w8 = WorkerBuilder::new("s8")
        .backend(storage_8)
        .concurrency(CONCURRENCY)
        .build(move |j: Stage8| {
            let st = s8_storages.clone();
            async move {
                st.s9.lock().await.push(Stage9 { flow_id: j.flow_id }).await
                    .map_err(|e| anyhow::anyhow!("s8->s9: {e}"))?;
                Ok::<(), BoxDynError>(())
            }
        });

    // Stage9 handler: INCR the completion counter. MultiplexedConnection
    // is cloneable and shares the underlying TCP socket across calls —
    // clone it once out here, not per-invocation.
    let counter_conn = raw_client.get_multiplexed_async_connection().await?;
    let w9 = WorkerBuilder::new("s9")
        .backend(storage_9)
        .concurrency(CONCURRENCY)
        .build(move |_j: Stage9| {
            let mut c = counter_conn.clone();
            async move {
                let _: () = c.incr(COMPLETED_KEY, 1).await?;
                Ok::<(), BoxDynError>(())
            }
        });

    // Each stage's `run_until` future is polled inline via
    // `futures::future::join_all`, sharing a broadcast shutdown.
    // Wrapping in tokio::spawn would require the worker's Future to
    // be 'static + Send — apalis 1.0-rc.7 workers have an HRTB on
    // their error type that blocks spawn; polling inline sidesteps
    // that. 10 concurrent stages across one task aren't parallel on
    // the runtime worker threads, but each stage's own
    // `.concurrency(16)` tower layer parallelises inside the stage,
    // so the stage-count-vs-cores ratio still covers the hot path.
    use apalis::prelude::WorkerError;
    let (shutdown_tx, _) = tokio::sync::broadcast::channel::<()>(1);

    let mk_signal = || {
        let mut rx = shutdown_tx.subscribe();
        async move {
            let _ = rx.recv().await;
            Ok::<(), WorkerError>(())
        }
    };
    let s0_fut = w0.run_until(mk_signal());
    let s1_fut = w1.run_until(mk_signal());
    let s2_fut = w2.run_until(mk_signal());
    let s3_fut = w3.run_until(mk_signal());
    let s4_fut = w4.run_until(mk_signal());
    let s5_fut = w5.run_until(mk_signal());
    let s6_fut = w6.run_until(mk_signal());
    let s7_fut = w7.run_until(mk_signal());
    let s8_fut = w8.run_until(mk_signal());
    let s9_fut = w9.run_until(mk_signal());

    // Driver side: warm, seed, poll the terminal counter, then fire
    // the shutdown broadcast that releases every run_until future.
    // Ran concurrently with the worker futures via tokio::join!.
    let driver_raw_client = raw_client.clone();
    let flows = args.flows;
    let deadline_secs = args.deadline_secs;
    let driver = async move {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let submit_start = Instant::now();
        for flow_id in 0..flows {
            storage_0
                .push(Stage0 {
                    flow_id: flow_id as u64,
                })
                .await
                .map_err(|e| anyhow::anyhow!("seed Stage0 {flow_id}: {e}"))?;
        }
        let submit_wall = submit_start.elapsed();
        eprintln!(
            "[apalis-s4] seeded {} flows in {} ms",
            flows,
            submit_wall.as_millis()
        );

        let drain_start = Instant::now();
        let deadline = drain_start + Duration::from_secs(deadline_secs);
        let mut c = driver_raw_client
            .get_multiplexed_async_connection()
            .await?;
        loop {
            let done: i64 = c.get(COMPLETED_KEY).await.unwrap_or(0);
            if done as usize >= flows {
                break;
            }
            if Instant::now() >= deadline {
                let _ = shutdown_tx.send(());
                anyhow::bail!(
                    "deadline exceeded: {}/{} flows completed in {}s",
                    done,
                    flows,
                    deadline_secs
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let wall = drain_start.elapsed();
        let _ = shutdown_tx.send(());
        Ok::<Duration, anyhow::Error>(wall)
    };

    let (driver_res, _, _, _, _, _, _, _, _, _, _) = tokio::join!(
        driver, s0_fut, s1_fut, s2_fut, s3_fut, s4_fut, s5_fut, s6_fut, s7_fut, s8_fut, s9_fut
    );
    let wall = driver_res?;

    let throughput = args.flows as f64 / wall.as_secs_f64();

    let report = serde_json::json!({
        "scenario": SCENARIO,
        "system": SYSTEM,
        "git_sha": ff_bench::git_sha(),
        "valkey_version": ff_bench::valkey_version(),
        "host": ff_bench::host_info(),
        "cluster": false,
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": {
            "flows": args.flows,
            "nodes_per_flow": STAGES,
            "concurrency_per_stage": CONCURRENCY,
            "apalis_version": "1.0.0-rc.7",
        },
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": 0.0, "p95": 0.0, "p99": 0.0 },
        "notes": "Approximation: apalis has no DAG primitive. 10-stage chain wired by hand; each stage's handler schedules the next. No data_passing, no flow-level retry, no cancellation propagation. System label is 'apalis-approx' so aggregators don't alias with a hypothetical native apalis DAG. latency_ms not reported — apalis doesn't surface per-flow completion without middleware we'd have to write ourselves, which would change the comparison.",
    });

    let results_dir = std::path::Path::new(&args.results_dir);
    std::fs::create_dir_all(results_dir)?;
    let path = results_dir.join(format!(
        "{}-{}-{}.json",
        SCENARIO,
        SYSTEM,
        report["git_sha"].as_str().unwrap_or("unknown")
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    eprintln!(
        "[apalis-s4] wrote {} — {:.1} flows/sec wall={:.2}s",
        path.display(),
        throughput,
        wall.as_secs_f64()
    );
    Ok(())
}
