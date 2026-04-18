//! Hand-rolled ferriskey baseline — scenario 1.
//!
//! Companion to `benches/comparisons/baseline/` (redis-rs). Same
//! workload shape, same report schema — only the Valkey client crate
//! differs. Reading both numbers alongside FlowFabric's scenario 1
//! gives a 3-way split:
//!
//!   * redis-rs baseline → raw client + Valkey ceiling.
//!   * ferriskey baseline → what a consumer pays by swapping in our
//!     client, without any ff-sdk / ff-server / Lua in the picture.
//!   * FlowFabric scenario 1 → the above + the execution engine.
//!
//! `delta(redis-rs, ferriskey)` = ferriskey's own client overhead.
//! `delta(ferriskey, flowfabric)` = what FCALL + Lua + axum + the SDK
//! layer above ferriskey cost, net of the client.
//!
//! Protocol (IDENTICAL to redis-rs baseline):
//!   * Submit    = `RPUSH bench:q <payload>` in chunks of 1 000
//!   * Claim     = `BLMPOP 1 LEFT bench:q`
//!   * Complete  = `INCR bench:completed`
//!
//! No leases, no retries, no flows, no lanes. If you change the
//! protocol here, change it in both bases — the comparison is only
//! meaningful when the three runners drive the same shape.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use ferriskey::{Client, ClientBuilder, Value};

const SCENARIO: &str = "submit_claim_complete";
const SYSTEM: &str = "ferriskey-baseline";
const QUEUE_KEY: &str = "bench:q";
const COMPLETED_KEY: &str = "bench:completed";
const WORKER_COUNT: usize = 16;
const PAYLOAD_BYTES: usize = 4 * 1024;

#[derive(Parser)]
struct Args {
    #[arg(long, env = "FF_BENCH_VALKEY_HOST", default_value = "localhost")]
    valkey_host: String,

    #[arg(long, env = "FF_BENCH_VALKEY_PORT", default_value_t = 6379)]
    valkey_port: u16,

    /// Number of tasks to submit + drain.
    #[arg(long, default_value_t = 10_000)]
    tasks: usize,

    /// Where to write the JSON report (relative to repo root).
    #[arg(long, default_value = "benches/results")]
    results_dir: String,
}

async fn build_client(host: &str, port: u16) -> Result<Client> {
    // Minimal client — no cluster, no TLS, default timeouts. Matches
    // the shape ff-sdk connects with (see
    // crates/ff-sdk/src/worker.rs), so any overhead we see here is
    // what ff-sdk inherits on its FCALL path.
    ClientBuilder::new()
        .host(host, port)
        .connect_timeout(Duration::from_secs(10))
        .request_timeout(Duration::from_millis(5_000))
        .build()
        .await
        .context("ferriskey ClientBuilder.build()")
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let client = build_client(&args.valkey_host, args.valkey_port).await?;

    // Clean slate — same FLUSH-ish pattern as the redis-rs baseline,
    // but targeted (the full server FLUSHALL would wipe any other
    // bench that happened to be running against the same instance).
    let _: Value = client
        .cmd("DEL")
        .arg(QUEUE_KEY)
        .arg(COMPLETED_KEY)
        .execute()
        .await
        .context("DEL bench keys")?;

    let payload = filler_payload(PAYLOAD_BYTES);
    let submit_start = Instant::now();
    seed_queue(&client, args.tasks, &payload).await?;
    let submit_wall = submit_start.elapsed();
    eprintln!(
        "[ferriskey-baseline] seeded {} tasks in {} ms",
        args.tasks,
        submit_wall.as_millis()
    );

    // Per-worker latency buffers (no Arc<Mutex> on the hot path —
    // same pattern the redis-rs baseline uses post-R1 fix).
    //
    // Each worker gets its OWN ferriskey Client. The redis-rs
    // baseline does the equivalent by calling
    // `client.get_multiplexed_async_connection()` per worker — that
    // hands out a fresh logical connection each time. `Client::clone`
    // on ferriskey shares underlying state, so reusing a single
    // client across 16 workers serialises their BLMPOP commands on
    // one connection and the drain hangs for minutes. Building
    // per-worker puts the two bases on equal footing.
    let drain_start = Instant::now();
    let stop_at = Arc::new(std::sync::atomic::AtomicUsize::new(args.tasks));
    let ok_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let nil_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let err_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut handles = Vec::with_capacity(WORKER_COUNT);
    for wi in 0..WORKER_COUNT {
        let worker_client = build_client(&args.valkey_host, args.valkey_port).await?;
        let stop_at = stop_at.clone();
        let ok_c = ok_count.clone();
        let nil_c = nil_count.clone();
        let err_c = err_count.clone();
        handles.push(tokio::spawn(async move {
            drive_worker(wi, worker_client, stop_at, ok_c, nil_c, err_c).await
        }));
    }
    let mut lat: Vec<u64> = Vec::with_capacity(args.tasks);
    for h in handles {
        let mut per_worker = h.await??;
        lat.append(&mut per_worker);
    }
    let wall = drain_start.elapsed();
    let ok = ok_count.load(std::sync::atomic::Ordering::Relaxed);
    let nil = nil_count.load(std::sync::atomic::Ordering::Relaxed);
    let err = err_count.load(std::sync::atomic::Ordering::Relaxed);

    let throughput = args.tasks as f64 / wall.as_secs_f64();
    let (p50, p95, p99) = percentiles(&lat);

    // Metadata helpers shared with the redis-rs baseline via
    // ff_bench — identical schema + host/valkey probes, only `system`
    // differs so a 3-way compare can be grepped out of `results/`.
    let report = serde_json::json!({
        "scenario": SCENARIO,
        "system": SYSTEM,
        "git_sha": ff_bench::git_sha(),
        "valkey_version": ff_bench::valkey_version(),
        "host": ff_bench::host_info(),
        "cluster": false,
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": { "tasks": args.tasks, "workers": WORKER_COUNT, "payload_bytes": PAYLOAD_BYTES },
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "blmpop_outcomes": { "ok": ok, "nil": nil, "err": err },
        "notes": "ferriskey-baseline = ClientBuilder + RPUSH / BLMPOP / INCR, no ff-sdk no ff-server no Lua. blmpop_outcomes counts per-poll results across all workers: ok=popped-a-value, nil=server returned Nil (queue empty), err=transport/timeout error. All three paths currently sleep 10ms and retry.",
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
        "[ferriskey-baseline] wrote {} — {:.1} ops/sec p99={:.2}ms ok={} nil={} err={}",
        path.display(),
        throughput,
        p99,
        ok,
        nil,
        err
    );
    Ok(())
}

async fn seed_queue(client: &Client, n: usize, payload: &[u8]) -> Result<()> {
    // Chunk RPUSH so one giant command doesn't hit argv limits. Same
    // chunk size the redis-rs baseline uses; keeps the seed phase
    // directly comparable.
    const CHUNK: usize = 1_000;
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(CHUNK);
        let elements: Vec<&[u8]> = (0..take).map(|_| payload).collect();
        let _: i64 = client
            .rpush(QUEUE_KEY, &elements)
            .await
            .context("RPUSH chunk")?;
        remaining -= take;
    }
    Ok(())
}

async fn drive_worker(
    _wi: usize,
    client: Client,
    stop_at: Arc<std::sync::atomic::AtomicUsize>,
    ok_count: Arc<std::sync::atomic::AtomicU64>,
    nil_count: Arc<std::sync::atomic::AtomicU64>,
    err_count: Arc<std::sync::atomic::AtomicU64>,
) -> Result<Vec<u64>> {
    use std::sync::atomic::Ordering;
    let mut local: Vec<u64> = Vec::new();
    loop {
        if stop_at.load(Ordering::Relaxed) == 0 {
            return Ok(local);
        }
        let t0 = Instant::now();
        // BLMPOP timeout (1s) count (1 key) key direction (LEFT) COUNT 1.
        // Matches the redis-rs baseline's BLMPOP exactly.
        //
        // ferriskey converts the RESP reply for BLMPOP via
        // `ExpectedReturnType::ArrayOfStringAndArrays` (see
        // ferriskey/src/client/value_conversion.rs:373). Success is a
        // map-shaped `Value::Array([Array([key, Array([elements])])])`
        // and drain-this-tick is `Value::Nil`. Typing against
        // `Option<Value>` collapses Nil to None cleanly; we don't
        // need to parse the key/elements — just whether the pop took.
        // 1 s block (matches redis-rs baseline's `.arg(1_u64)`).
        // Drained stragglers exit within ≤ 1 s of the last pop
        // setting stop_at to 0.
        let popped: Result<Option<Value>, _> = client
            .cmd("BLMPOP")
            .arg(1_u64)
            .arg(1_u64)
            .arg(QUEUE_KEY)
            .arg("LEFT")
            .arg("COUNT")
            .arg(1_u64)
            .execute()
            .await;

        match popped {
            Ok(Some(_)) => {
                ok_count.fetch_add(1, Ordering::Relaxed);
                // Record latency BEFORE the INCR so the sample matches
                // the redis-rs baseline's (INCR-included) definition.
                // Need the INCR RTT in the sample for apples-to-apples.
                let _: i64 = client
                    .incr(COMPLETED_KEY)
                    .await
                    .context("INCR completed")?;
                local.push(t0.elapsed().as_micros() as u64);
                stop_at.fetch_sub(1, Ordering::Relaxed);
            }
            Ok(None) => {
                nil_count.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(_) => {
                err_count.fetch_add(1, Ordering::Relaxed);
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }
    }
}

fn percentiles(samples: &[u64]) -> (f64, f64, f64) {
    if samples.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mut s = samples.to_vec();
    s.sort_unstable();
    let at = |q: f64| -> f64 {
        let idx = ((s.len() as f64 - 1.0) * q).round() as usize;
        s[idx.min(s.len() - 1)] as f64 / 1000.0
    };
    (at(0.50), at(0.95), at(0.99))
}

fn filler_payload(size: usize) -> Vec<u8> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let mut x = seed.wrapping_add(0x9E3779B9);
    (0..size)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            ((x >> 16) & 0xFF) as u8
        })
        .collect()
}
