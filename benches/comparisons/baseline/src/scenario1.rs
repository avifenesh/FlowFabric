//! Hand-rolled baseline — scenario 1.
//!
//! "What does raw Valkey + tokio give you with no execution engine?"
//! Ground truth for interpreting the FlowFabric + apalis + faktory
//! numbers. The protocol is intentionally the simplest thing that
//! works:
//!
//!   * Submit = `RPUSH bench:q <payload>`
//!   * Claim  = `BLMPOP 1 LEFT bench:q`
//!   * Complete = `INCR bench:completed` (nothing more to do; the
//!     baseline has no result storage, retries, leases, or flows)
//!
//! This is an intentionally thin target: any real framework should
//! beat it on features but will trail on raw throughput because
//! features cost. The delta between FlowFabric and this baseline is
//! the overhead budget the execution engine occupies.

use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::Result;
use clap::Parser;
use redis::AsyncCommands;

const SCENARIO: &str = "submit_claim_complete";
const SYSTEM: &str = "baseline";
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let url = format!("redis://{}:{}/", args.valkey_host, args.valkey_port);
    let client = redis::Client::open(url)?;

    // Clean slate so consecutive runs don't accumulate residue.
    let mut con = client.get_multiplexed_async_connection().await?;
    let _: () = con.del(QUEUE_KEY).await?;
    let _: () = con.del(COMPLETED_KEY).await?;
    drop(con);

    let payload = filler_payload(PAYLOAD_BYTES);
    let submit_start = Instant::now();
    seed_queue(&client, args.tasks, &payload).await?;
    let submit_wall = submit_start.elapsed();
    eprintln!(
        "[baseline] seeded {} tasks in {} ms",
        args.tasks,
        submit_wall.as_millis()
    );

    // Per-worker latency buffers — each task owns its Vec<u64> and
    // returns it via the JoinHandle. No Arc<Mutex> on the hot path, so
    // the measurement isn't biased by lock contention under 16-worker
    // concurrency (PR#10 bot review).
    let drain_start = Instant::now();
    let stop_at = Arc::new(std::sync::atomic::AtomicUsize::new(args.tasks));
    let ok_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let nil_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let err_count = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let mut handles = Vec::with_capacity(WORKER_COUNT);
    for wi in 0..WORKER_COUNT {
        let client = client.clone();
        let stop_at = stop_at.clone();
        let ok_c = ok_count.clone();
        let nil_c = nil_count.clone();
        let err_c = err_count.clone();
        handles.push(tokio::spawn(async move {
            drive_worker(wi, client, stop_at, ok_c, nil_c, err_c).await
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

    // Metadata helpers live in the ff-bench harness crate so every
    // bench — harness + baseline + future apalis/faktory — emits the
    // same report shape with the same platform-gated probes. The
    // measured workload above is untouched.
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
        "notes": "baseline = RPUSH / BLMPOP / INCR, no retries no leases no flows. blmpop_outcomes counts per-poll results across all workers: ok=popped-a-value, nil=server returned Nil (queue empty), err=transport/timeout error. All three paths currently sleep 10ms and retry.",
    });

    // Write using the same path convention the harness uses so
    // check_release.py can discover both.
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
        "[baseline] wrote {} — {:.1} ops/sec p99={:.2}ms ok={} nil={} err={}",
        path.display(),
        throughput,
        p99,
        ok,
        nil,
        err
    );
    Ok(())
}

async fn seed_queue(client: &redis::Client, n: usize, payload: &[u8]) -> Result<()> {
    let mut con = client.get_multiplexed_async_connection().await?;
    // Chunk RPUSH so one giant command doesn't hit argv limits. 1000/
    // call is a standard Redis best-practice.
    const CHUNK: usize = 1_000;
    let mut remaining = n;
    while remaining > 0 {
        let take = remaining.min(CHUNK);
        let vals: Vec<&[u8]> = (0..take).map(|_| payload).collect();
        let _: () = con.rpush(QUEUE_KEY, vals).await?;
        remaining -= take;
    }
    Ok(())
}

async fn drive_worker(
    _wi: usize,
    client: redis::Client,
    stop_at: Arc<std::sync::atomic::AtomicUsize>,
    ok_count: Arc<std::sync::atomic::AtomicU64>,
    nil_count: Arc<std::sync::atomic::AtomicU64>,
    err_count: Arc<std::sync::atomic::AtomicU64>,
) -> Result<Vec<u64>> {
    use std::sync::atomic::Ordering;
    let mut con = client.get_multiplexed_async_connection().await?;
    let mut local: Vec<u64> = Vec::new();
    loop {
        if stop_at.load(Ordering::Relaxed) == 0 {
            return Ok(local);
        }
        let t0 = Instant::now();
        let popped: Result<Option<(String, Vec<Vec<u8>>)>, _> =
            redis::cmd("BLMPOP")
                .arg(1_u64) // 1-second block
                .arg(1_u64) // 1 key
                .arg(QUEUE_KEY)
                .arg("LEFT")
                .arg("COUNT")
                .arg(1_u64)
                .query_async(&mut con)
                .await;
        let popped_something = match popped {
            Ok(Some((_, values))) if !values.is_empty() => {
                ok_count.fetch_add(1, Ordering::Relaxed);
                let _: () = con.incr(COMPLETED_KEY, 1).await?;
                local.push(t0.elapsed().as_micros() as u64);
                stop_at.fetch_sub(1, Ordering::Relaxed);
                true
            }
            Ok(_) => {
                nil_count.fetch_add(1, Ordering::Relaxed);
                false
            }
            Err(_) => {
                err_count.fetch_add(1, Ordering::Relaxed);
                false
            }
        };
        if !popped_something {
            // Queue drained from BLMPOP's perspective; check stop
            // counter and loop. Avoids tight-spinning an empty
            // queue during stragglers.
            tokio::time::sleep(Duration::from_millis(10)).await;
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

