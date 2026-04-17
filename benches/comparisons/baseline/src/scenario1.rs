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
use tokio::sync::Mutex;

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

    let latencies: Arc<Mutex<Vec<u64>>> = Arc::new(Mutex::new(Vec::with_capacity(args.tasks)));
    let drain_start = Instant::now();
    let stop_at = Arc::new(std::sync::atomic::AtomicUsize::new(args.tasks));
    let mut handles = Vec::with_capacity(WORKER_COUNT);
    for wi in 0..WORKER_COUNT {
        let client = client.clone();
        let latencies = latencies.clone();
        let stop_at = stop_at.clone();
        handles.push(tokio::spawn(async move {
            drive_worker(wi, client, latencies, stop_at).await
        }));
    }
    for h in handles {
        h.await??;
    }
    let wall = drain_start.elapsed();

    let lat = std::mem::take(&mut *latencies.lock().await);
    let throughput = args.tasks as f64 / wall.as_secs_f64();
    let (p50, p95, p99) = percentiles(&lat);

    let report = serde_json::json!({
        "scenario": SCENARIO,
        "system": SYSTEM,
        "git_sha": git_sha(),
        "valkey_version": "unknown",
        "host": host_info(),
        "cluster": false,
        "timestamp_utc": iso8601_utc(),
        "config": { "tasks": args.tasks, "workers": WORKER_COUNT, "payload_bytes": PAYLOAD_BYTES },
        "throughput_ops_per_sec": throughput,
        "latency_ms": { "p50": p50, "p95": p95, "p99": p99 },
        "notes": "baseline = RPUSH / BLMPOP / INCR, no retries no leases no flows",
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
        "[baseline] wrote {} — {:.1} ops/sec p99={:.2}ms",
        path.display(),
        throughput,
        p99
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
    latencies: Arc<Mutex<Vec<u64>>>,
    stop_at: Arc<std::sync::atomic::AtomicUsize>,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    let mut con = client.get_multiplexed_async_connection().await?;
    loop {
        if stop_at.load(Ordering::Relaxed) == 0 {
            return Ok(());
        }
        let t0 = Instant::now();
        let popped: Option<(String, Vec<Vec<u8>>)> =
            redis::cmd("BLMPOP")
                .arg(1_u64) // 1-second block
                .arg(1_u64) // 1 key
                .arg(QUEUE_KEY)
                .arg("LEFT")
                .arg("COUNT")
                .arg(1_u64)
                .query_async(&mut con)
                .await
                .ok()
                .flatten();
        match popped {
            Some((_, values)) if !values.is_empty() => {
                let _: () = con.incr(COMPLETED_KEY, 1).await?;
                latencies
                    .lock()
                    .await
                    .push(t0.elapsed().as_micros() as u64);
                stop_at.fetch_sub(1, Ordering::Relaxed);
            }
            _ => {
                // Queue drained from BLMPOP's perspective; check stop
                // counter and loop. Avoids tight-spinning an empty
                // queue during stragglers.
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

fn git_sha() -> String {
    std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| o.status.success().then(|| o.stdout))
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned())
        .unwrap_or_else(|| "unknown".to_owned())
}

fn host_info() -> serde_json::Value {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|t| {
            t.lines()
                .find(|l| l.starts_with("model name"))
                .and_then(|l| l.splitn(2, ':').nth(1))
                .map(|s| s.trim().to_owned())
        })
        .unwrap_or_else(|| "unknown".to_owned());
    serde_json::json!({ "cpu": cpu, "cores": cores, "mem_gb": 0 })
}

fn iso8601_utc() -> String {
    // Share logic with the harness' report.rs via duplication — the
    // baseline crate is deliberately free of ff-bench deps so its
    // numbers are untainted by any FlowFabric code path.
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let s = (secs % 60) as u32;
    let secs = secs / 60;
    let min = (secs % 60) as u32;
    let secs = secs / 60;
    let hour = (secs % 24) as u32;
    let mut days = (secs / 24) as i64;
    let mut year: i64 = 1970;
    loop {
        let yd = if is_leap(year) { 366 } else { 365 };
        if days < yd {
            break;
        }
        days -= yd;
        year += 1;
    }
    let months = [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31];
    let feb = if is_leap(year) { 29 } else { 28 };
    let mut m = 0;
    while m < 12 {
        let dim = if m == 1 { feb } else { months[m] };
        if days < dim {
            break;
        }
        days -= dim;
        m += 1;
    }
    let day = (days + 1) as u32;
    let month = (m + 1) as u32;
    format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z",
        year, month, day, hour, min, s
    )
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}
