// Ferriskey throughput + latency benchmark
//
// 80/20 GET/SET workload mix, fixed-duration runs, HdrHistogram percentiles.
// Each permutation runs BENCH_RUNS times (default 3), reports median.
//
// Profiles (BENCH_PROFILE env var):
//   quick  - 10s, 2s warmup, 1KB,        c=1,100         (3 runs ~6 min)
//   short  - 120s,5s warmup, 100B+1KB,    c=10,100        (3 runs ~50 min)
//   full   - 120s,5s warmup, 100B+1KB+16KB, c=1,10,100    (3 runs ~1h50m)
//
// Env vars:
//   VALKEY_HOST          cluster config endpoint (default: 127.0.0.1)
//   VALKEY_PORT          cluster port (default: 6379)
//   VALKEY_TLS           "true" to enable TLS (default: false)
//   BENCH_DURATION       override seconds per permutation
//   BENCH_WARMUP         override warmup seconds
//   BENCH_RUNS           runs per permutation for median (default: 3)
//   BENCH_CPUS           CPUs to pin tokio workers to (default: 4-15)

use ferriskey::valkey::{
    ConnectionAddr, ConnectionInfo, Pipeline, RedisConnectionInfo, RedisResult, Value,
    aio::ConnectionLike,
    cluster::ClusterClientBuilder,
    cluster_async::ClusterConnection,
    cmd,
};
use hdrhistogram::Histogram;
use serde_json::json;
use std::env;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::runtime::{Builder, Runtime};
use tokio::sync::Barrier;
use tokio::task::JoinSet;

// ---------------------------------------------------------------------------
// Config
// ---------------------------------------------------------------------------

struct BenchConfig {
    host: String,
    port: u16,
    tls: bool,
    duration: Duration,
    warmup: Duration,
    profile: String,
    runs: usize,
    value_sizes: Vec<usize>,
    concurrencies: Vec<usize>,
}

impl BenchConfig {
    fn from_env() -> Self {
        let profile = env::var("BENCH_PROFILE").unwrap_or_else(|_| "short".into());

        let (default_duration, default_warmup, value_sizes, concurrencies) = match profile.as_str()
        {
            "quick" => (10, 2, vec![1024], vec![1, 100]),
            "full" => (60, 5, vec![100, 1024, 16384], vec![1, 10, 100]),
            _ => (60, 5, vec![100, 1024], vec![10, 100]), // "short" default
        };

        let duration = env::var("BENCH_DURATION")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_duration);
        let warmup = env::var("BENCH_WARMUP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_warmup);
        let runs = env::var("BENCH_RUNS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(3);

        Self {
            host: env::var("VALKEY_HOST").unwrap_or_else(|_| "127.0.0.1".into()),
            port: env::var("VALKEY_PORT")
                .unwrap_or_else(|_| "6379".into())
                .parse()
                .unwrap(),
            tls: env::var("VALKEY_TLS").unwrap_or_default() == "true",
            duration: Duration::from_secs(duration),
            warmup: Duration::from_secs(warmup),
            profile,
            runs,
            value_sizes,
            concurrencies,
        }
    }
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct BenchResult {
    label: String,
    value_size: usize,
    concurrency: usize,
    duration_secs: f64,
    total_ops: u64,
    total_errors: u64,
    tps: f64,
    p50_us: f64,
    p75_us: f64,
    p90_us: f64,
    p99_us: f64,
    max_us: f64,
}

impl BenchResult {
    fn print_short(&self) {
        println!(
            "    {:>12}  p50={:.3}ms  p99={:.3}ms  max={:.3}ms",
            format_tps(self.tps),
            self.p50_us / 1000.0,
            self.p99_us / 1000.0,
            self.max_us / 1000.0,
        );
    }

    fn print_full(&self) {
        let err_str = if self.total_errors > 0 {
            format!("  ({} errors)", self.total_errors)
        } else {
            String::new()
        };
        println!(
            "  TPS:    {:<14}  total: {}{}",
            format_tps(self.tps),
            self.total_ops,
            err_str,
        );
        println!("  p50:    {:.3}ms", self.p50_us / 1000.0);
        println!("  p75:    {:.3}ms", self.p75_us / 1000.0);
        println!("  p90:    {:.3}ms", self.p90_us / 1000.0);
        println!("  p99:    {:.3}ms", self.p99_us / 1000.0);
        println!("  max:    {:.3}ms", self.max_us / 1000.0);
    }

    fn to_json(&self) -> serde_json::Value {
        json!({
            "label": self.label,
            "value_size": self.value_size,
            "concurrency": self.concurrency,
            "duration_secs": self.duration_secs,
            "total_ops": self.total_ops,
            "total_errors": self.total_errors,
            "tps": self.tps,
            "latency_us": {
                "p50": self.p50_us,
                "p75": self.p75_us,
                "p90": self.p90_us,
                "p99": self.p99_us,
                "max": self.max_us,
            }
        })
    }
}

fn format_size(bytes: usize) -> String {
    if bytes >= 1024 {
        format!("{}KB", bytes / 1024)
    } else {
        format!("{}B", bytes)
    }
}

fn format_tps(tps: f64) -> String {
    if tps >= 1_000_000.0 {
        format!("{:.2}M ops/sec", tps / 1_000_000.0)
    } else if tps >= 1_000.0 {
        format!("{:.0}K ops/sec", tps / 1_000.0)
    } else {
        format!("{:.0} ops/sec", tps)
    }
}

/// Pick the result with the median TPS from a vec of runs.
fn median_by_tps(mut runs: Vec<BenchResult>) -> BenchResult {
    runs.sort_by(|a, b| a.tps.partial_cmp(&b.tps).unwrap());
    runs.swap_remove(runs.len() / 2)
}

// ---------------------------------------------------------------------------
// Connection helpers
// ---------------------------------------------------------------------------

fn connection_info(host: &str, port: u16, tls: bool) -> ConnectionInfo {
    ConnectionInfo {
        addr: if tls {
            ConnectionAddr::TcpTls {
                host: host.to_string(),
                port,
                insecure: true,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(host.to_string(), port)
        },
        redis: RedisConnectionInfo::default(),
    }
}

fn create_runtime() -> Runtime {
    // Pin to CPUs 4-15 by default (leave 0-3 for OS/network).
    // Override with BENCH_CPUS=0-15 for all cores.
    let cpu_str = env::var("BENCH_CPUS").unwrap_or_else(|_| "4-15".into());
    let worker_threads = parse_cpu_count(&cpu_str);

    // Set CPU affinity before building the runtime so workers inherit it.
    if let Err(e) = set_cpu_affinity(&cpu_str) {
        eprintln!("Warning: could not set CPU affinity: {e}");
    }

    Builder::new_multi_thread()
        .enable_all()
        .worker_threads(worker_threads)
        .build()
        .unwrap()
}

fn parse_cpu_count(cpu_str: &str) -> usize {
    // "4-15" -> 12, "0-7" -> 8, "0-15" -> 16
    if let Some((start, end)) = cpu_str.split_once('-') {
        if let (Ok(s), Ok(e)) = (start.parse::<usize>(), end.parse::<usize>()) {
            return e - s + 1;
        }
    }
    num_cpus::get()
}

fn set_cpu_affinity(cpu_str: &str) -> Result<(), String> {
    // Use libc sched_setaffinity to pin this process
    if let Some((start, end)) = cpu_str.split_once('-') {
        let start: usize = start.parse().map_err(|e| format!("{e}"))?;
        let end: usize = end.parse().map_err(|e| format!("{e}"))?;

        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            for cpu in start..=end {
                libc::CPU_SET(cpu, &mut set);
            }
            let ret = libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &set);
            if ret != 0 {
                return Err(format!("sched_setaffinity returned {ret}"));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Workload runner
// ---------------------------------------------------------------------------

struct FastRng(u64);

impl FastRng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    fn is_get(&mut self) -> bool {
        (self.next_u64() % 5) != 0
    }
}

async fn run_workload<C: ConnectionLike + Clone + Send + 'static>(
    conn: C,
    concurrency: usize,
    value_size: usize,
    warmup: Duration,
    measure: Duration,
) -> BenchResult {
    let value: Arc<Vec<u8>> = Arc::new(vec![b'x'; value_size]);
    let barrier = Arc::new(Barrier::new(concurrency));
    let mut tasks = JoinSet::new();

    for task_id in 0..concurrency {
        let mut conn = conn.clone();
        let value = Arc::clone(&value);
        let barrier = Arc::clone(&barrier);

        tasks.spawn(async move {
            barrier.wait().await;

            let mut rng = FastRng::new(task_id as u64 ^ 0xdeadbeef);
            let key = format!("bench:{task_id}");

            let _ = conn
                .req_packed_command(&cmd("SET").arg(&key).arg(value.as_slice()))
                .await;

            // Warmup
            let warmup_deadline = Instant::now() + warmup;
            while Instant::now() < warmup_deadline {
                if rng.is_get() {
                    let _ = conn.req_packed_command(&cmd("GET").arg(&key)).await;
                } else {
                    let _ = conn
                        .req_packed_command(&cmd("SET").arg(&key).arg(value.as_slice()))
                        .await;
                }
            }

            // Measure
            let mut latencies: Vec<u64> = Vec::with_capacity(1_000_000);
            let mut errors: u64 = 0;
            let deadline = Instant::now() + measure;

            loop {
                let start = Instant::now();
                let result: RedisResult<Value> = if rng.is_get() {
                    conn.req_packed_command(&cmd("GET").arg(&key)).await
                } else {
                    conn.req_packed_command(&cmd("SET").arg(&key).arg(value.as_slice()))
                        .await
                };
                let elapsed_us = start.elapsed().as_micros() as u64;

                if result.is_ok() {
                    latencies.push(elapsed_us);
                } else {
                    errors += 1;
                }

                if Instant::now() >= deadline {
                    break;
                }
            }

            (latencies, errors)
        });
    }

    let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    let mut total_ops: u64 = 0;
    let mut total_errors: u64 = 0;

    while let Some(result) = tasks.join_next().await {
        let (latencies, errors) = result.unwrap();
        total_ops += latencies.len() as u64;
        total_errors += errors;
        for &lat in &latencies {
            let _ = hist.record(lat);
        }
    }

    let duration_secs = measure.as_secs_f64();
    let tps = total_ops as f64 / duration_secs;

    BenchResult {
        label: String::new(),
        value_size,
        concurrency,
        duration_secs,
        total_ops,
        total_errors,
        tps,
        p50_us: hist.value_at_quantile(0.50) as f64,
        p75_us: hist.value_at_quantile(0.75) as f64,
        p90_us: hist.value_at_quantile(0.90) as f64,
        p99_us: hist.value_at_quantile(0.99) as f64,
        max_us: hist.max() as f64,
    }
}

// ---------------------------------------------------------------------------
// Pipeline workload: N GET commands in a single pipeline, same key (same slot)
// ---------------------------------------------------------------------------

async fn run_pipeline_workload<C: ConnectionLike + Clone + Send + 'static>(
    conn: C,
    concurrency: usize,
    pipeline_size: usize,
    warmup: Duration,
    measure: Duration,
) -> BenchResult {
    let barrier = Arc::new(Barrier::new(concurrency));
    let mut tasks = JoinSet::new();

    for task_id in 0..concurrency {
        let mut conn = conn.clone();
        let barrier = Arc::clone(&barrier);

        tasks.spawn(async move {
            barrier.wait().await;

            let key = format!("bench:{task_id}");

            // Seed key
            let _ = conn
                .req_packed_command(&cmd("SET").arg(&key).arg("val"))
                .await;

            // Build pipeline once — same key, same slot
            let mut pipe = Pipeline::new();
            for _ in 0..pipeline_size {
                let mut c = cmd("GET");
                c.arg(&key);
                pipe.add_command(c);
            }

            // Warmup
            let warmup_deadline = Instant::now() + warmup;
            while Instant::now() < warmup_deadline {
                let _: RedisResult<Vec<Value>> = pipe.query_async(&mut conn).await;
            }

            // Measure
            let mut latencies: Vec<u64> = Vec::with_capacity(500_000);
            let mut errors: u64 = 0;
            let deadline = Instant::now() + measure;

            loop {
                let start = Instant::now();
                let result: RedisResult<Vec<Value>> = pipe.query_async(&mut conn).await;
                let elapsed_us = start.elapsed().as_micros() as u64;

                if result.is_ok() {
                    latencies.push(elapsed_us);
                } else {
                    errors += 1;
                }

                if Instant::now() >= deadline {
                    break;
                }
            }

            (latencies, errors)
        });
    }

    let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    let mut total_ops: u64 = 0;
    let mut total_errors: u64 = 0;

    while let Some(result) = tasks.join_next().await {
        let (latencies, errors) = result.unwrap();
        total_ops += latencies.len() as u64;
        total_errors += errors;
        for &lat in &latencies {
            let _ = hist.record(lat);
        }
    }

    let duration_secs = measure.as_secs_f64();
    let tps = total_ops as f64 / duration_secs;

    BenchResult {
        label: String::new(),
        value_size: 0,
        concurrency,
        duration_secs,
        total_ops,
        total_errors,
        tps,
        p50_us: hist.value_at_quantile(0.50) as f64,
        p75_us: hist.value_at_quantile(0.75) as f64,
        p90_us: hist.value_at_quantile(0.90) as f64,
        p99_us: hist.value_at_quantile(0.99) as f64,
        max_us: hist.max() as f64,
    }
}

// ---------------------------------------------------------------------------
// MGET workload: single MGET of N keys, same hash tag (same slot)
// ---------------------------------------------------------------------------

async fn run_mget_workload<C: ConnectionLike + Clone + Send + 'static>(
    conn: C,
    concurrency: usize,
    num_keys: usize,
    warmup: Duration,
    measure: Duration,
) -> BenchResult {
    let barrier = Arc::new(Barrier::new(concurrency));
    let mut tasks = JoinSet::new();

    for task_id in 0..concurrency {
        let mut conn = conn.clone();
        let barrier = Arc::clone(&barrier);

        tasks.spawn(async move {
            barrier.wait().await;

            // All keys in same hash tag → same slot
            let keys: Vec<String> = (0..num_keys)
                .map(|i| format!("{{bench:{task_id}}}:k{i}"))
                .collect();

            // Seed keys
            for k in &keys {
                let _ = conn
                    .req_packed_command(&cmd("SET").arg(k).arg("v"))
                    .await;
            }

            // Build MGET command
            let mut mget = cmd("MGET");
            for k in &keys {
                mget.arg(k.as_str());
            }

            // Warmup
            let warmup_deadline = Instant::now() + warmup;
            while Instant::now() < warmup_deadline {
                let _ = conn.req_packed_command(&mget).await;
            }

            // Measure
            let mut latencies: Vec<u64> = Vec::with_capacity(500_000);
            let mut errors: u64 = 0;
            let deadline = Instant::now() + measure;

            loop {
                let start = Instant::now();
                let result = conn.req_packed_command(&mget).await;
                let elapsed_us = start.elapsed().as_micros() as u64;

                if result.is_ok() {
                    latencies.push(elapsed_us);
                } else {
                    errors += 1;
                }

                if Instant::now() >= deadline {
                    break;
                }
            }

            (latencies, errors)
        });
    }

    let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    let mut total_ops: u64 = 0;
    let mut total_errors: u64 = 0;

    while let Some(result) = tasks.join_next().await {
        let (latencies, errors) = result.unwrap();
        total_ops += latencies.len() as u64;
        total_errors += errors;
        for &lat in &latencies {
            let _ = hist.record(lat);
        }
    }

    let duration_secs = measure.as_secs_f64();
    let tps = total_ops as f64 / duration_secs;

    BenchResult {
        label: String::new(),
        value_size: 0,
        concurrency,
        duration_secs,
        total_ops,
        total_errors,
        tps,
        p50_us: hist.value_at_quantile(0.50) as f64,
        p75_us: hist.value_at_quantile(0.75) as f64,
        p90_us: hist.value_at_quantile(0.90) as f64,
        p99_us: hist.value_at_quantile(0.99) as f64,
        max_us: hist.max() as f64,
    }
}

// ---------------------------------------------------------------------------
// Cross-slot batch: non-atomic pipeline with commands to different slots
// ---------------------------------------------------------------------------

async fn run_batch_cross_slot_workload<C: ConnectionLike + Clone + Send + 'static>(
    conn: C,
    concurrency: usize,
    num_keys: usize,
    warmup: Duration,
    measure: Duration,
) -> BenchResult {
    let barrier = Arc::new(Barrier::new(concurrency));
    let mut tasks = JoinSet::new();

    for task_id in 0..concurrency {
        let mut conn = conn.clone();
        let barrier = Arc::clone(&barrier);

        tasks.spawn(async move {
            barrier.wait().await;

            // Keys WITHOUT hash tags → different slots → cross-slot pipeline
            let keys: Vec<String> = (0..num_keys)
                .map(|i| format!("xslot:{task_id}:{i}"))
                .collect();

            // Seed keys
            for k in &keys {
                let _ = conn
                    .req_packed_command(&cmd("SET").arg(k).arg("v"))
                    .await;
            }

            // Build non-atomic pipeline — different slots
            let build_pipe = || {
                let mut pipe = Pipeline::new();
                for k in &keys {
                    let mut c = cmd("GET");
                    c.arg(k.as_str());
                    pipe.add_command(c);
                }
                pipe
            };

            // Warmup
            let warmup_deadline = Instant::now() + warmup;
            while Instant::now() < warmup_deadline {
                let pipe = build_pipe();
                let _: RedisResult<Vec<Value>> = pipe.query_async(&mut conn).await;
            }

            // Measure
            let mut latencies: Vec<u64> = Vec::with_capacity(200_000);
            let mut errors: u64 = 0;
            let deadline = Instant::now() + measure;

            loop {
                let pipe = build_pipe();
                let start = Instant::now();
                let result: RedisResult<Vec<Value>> = pipe.query_async(&mut conn).await;
                let elapsed_us = start.elapsed().as_micros() as u64;

                if result.is_ok() {
                    latencies.push(elapsed_us);
                } else {
                    errors += 1;
                }

                if Instant::now() >= deadline {
                    break;
                }
            }

            (latencies, errors)
        });
    }

    let mut hist = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    let mut total_ops: u64 = 0;
    let mut total_errors: u64 = 0;

    while let Some(result) = tasks.join_next().await {
        let (latencies, errors) = result.unwrap();
        total_ops += latencies.len() as u64;
        total_errors += errors;
        for &lat in &latencies {
            let _ = hist.record(lat);
        }
    }

    let duration_secs = measure.as_secs_f64();
    let tps = total_ops as f64 / duration_secs;

    BenchResult {
        label: String::new(),
        value_size: 0,
        concurrency,
        duration_secs,
        total_ops,
        total_errors,
        tps,
        p50_us: hist.value_at_quantile(0.50) as f64,
        p75_us: hist.value_at_quantile(0.75) as f64,
        p90_us: hist.value_at_quantile(0.90) as f64,
        p99_us: hist.value_at_quantile(0.99) as f64,
        max_us: hist.max() as f64,
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cfg = BenchConfig::from_env();
    let runtime = create_runtime();

    let permutations = cfg.value_sizes.len() * cfg.concurrencies.len();
    let total_runs = permutations * cfg.runs;
    let est_mins =
        (total_runs as u64 * (cfg.duration.as_secs() + cfg.warmup.as_secs()) + 59) / 60;

    println!("=== Ferriskey Benchmark [{}] ===", cfg.profile);
    println!(
        "  {}s measure + {}s warmup | {} permutations x {} runs | ~{} min",
        cfg.duration.as_secs(),
        cfg.warmup.as_secs(),
        permutations,
        cfg.runs,
        est_mins,
    );
    println!("  Workload: 80% GET / 20% SET");
    println!(
        "  Sizes: {:?}  Concurrencies: {:?}",
        cfg.value_sizes.iter().map(|s| format_size(*s)).collect::<Vec<_>>(),
        cfg.concurrencies,
    );
    println!();

    let info = connection_info(&cfg.host, cfg.port, cfg.tls);
    let conn: ClusterConnection = runtime.block_on(async {
        let client = ClusterClientBuilder::new(vec![info])
            .tls(if cfg.tls {
                ferriskey::valkey::cluster::TlsMode::Insecure
            } else {
                ferriskey::valkey::cluster::TlsMode::Secure
            })
            .build()
            .unwrap();
        client.get_async_connection(None, None, None).await.unwrap()
    });

    println!("--- cluster  {}:{} TLS={} ---\n", cfg.host, cfg.port, cfg.tls);

    let mut medians: Vec<BenchResult> = Vec::new();

    for &size in &cfg.value_sizes {
        for &conc in &cfg.concurrencies {
            let label = format!("cluster-{}-c{conc}", format_size(size));
            println!("[{}]  {} runs of {}s:", label, cfg.runs, cfg.duration.as_secs());

            let mut runs: Vec<BenchResult> = Vec::with_capacity(cfg.runs);

            for run in 0..cfg.runs {
                // Flush between runs
                {
                    let mut c = conn.clone();
                    runtime.block_on(async {
                        let _ = c.req_packed_command(&cmd("FLUSHALL")).await;
                    });
                }

                print!("  run {}/{} ... ", run + 1, cfg.runs);
                let _ = std::io::Write::flush(&mut std::io::stdout());

                let mut r = runtime.block_on(run_workload(
                    conn.clone(),
                    conc,
                    size,
                    cfg.warmup,
                    cfg.duration,
                ));
                r.label = label.clone();

                println!("{}", format_tps(r.tps));
                r.print_short();
                runs.push(r);
            }

            let median = median_by_tps(runs);
            println!("  MEDIAN:");
            median.print_full();
            println!();
            medians.push(median);
        }
    }

    // Pipeline benchmark (10-cmd pipeline, same slot) — only on short/full
    if cfg.profile != "quick" {
        for &conc in &cfg.concurrencies {
            let label = format!("pipeline-10cmd-c{conc}");
            println!("[{}]  {} runs of {}s:", label, cfg.runs, cfg.duration.as_secs());

            let mut runs: Vec<BenchResult> = Vec::with_capacity(cfg.runs);
            for run in 0..cfg.runs {
                {
                    let mut c = conn.clone();
                    runtime.block_on(async { let _ = c.req_packed_command(&cmd("FLUSHALL")).await; });
                }
                print!("  run {}/{} ... ", run + 1, cfg.runs);
                let _ = std::io::Write::flush(&mut std::io::stdout());

                let mut r = runtime.block_on(run_pipeline_workload(
                    conn.clone(), conc, 10, cfg.warmup, cfg.duration,
                ));
                r.label = label.clone();
                println!("{}", format_tps(r.tps));
                r.print_short();
                runs.push(r);
            }
            let median = median_by_tps(runs);
            println!("  MEDIAN:");
            median.print_full();
            println!();
            medians.push(median);
        }
    }

    // MGET benchmark (10 keys, same hash tag → same slot) — only on short/full
    if cfg.profile != "quick" {
        for &conc in &cfg.concurrencies {
            let label = format!("mget-10keys-c{conc}");
            println!("[{}]  {} runs of {}s:", label, cfg.runs, cfg.duration.as_secs());

            let mut runs: Vec<BenchResult> = Vec::with_capacity(cfg.runs);
            for run in 0..cfg.runs {
                {
                    let mut c = conn.clone();
                    runtime.block_on(async { let _ = c.req_packed_command(&cmd("FLUSHALL")).await; });
                }
                print!("  run {}/{} ... ", run + 1, cfg.runs);
                let _ = std::io::Write::flush(&mut std::io::stdout());

                let mut r = runtime.block_on(run_mget_workload(
                    conn.clone(), conc, 10, cfg.warmup, cfg.duration,
                ));
                r.label = label.clone();
                println!("{}", format_tps(r.tps));
                r.print_short();
                runs.push(r);
            }
            let median = median_by_tps(runs);
            println!("  MEDIAN:");
            median.print_full();
            println!();
            medians.push(median);
        }
    }

    // Cross-slot batch (10 keys, different slots, non-atomic) — only on full
    if cfg.profile == "full" {
        for &conc in &cfg.concurrencies {
            let label = format!("batch-xslot-10cmd-c{conc}");
            println!("[{}]  {} runs of {}s:", label, cfg.runs, cfg.duration.as_secs());

            let mut runs: Vec<BenchResult> = Vec::with_capacity(cfg.runs);
            for run in 0..cfg.runs {
                {
                    let mut c = conn.clone();
                    runtime.block_on(async { let _ = c.req_packed_command(&cmd("FLUSHALL")).await; });
                }
                print!("  run {}/{} ... ", run + 1, cfg.runs);
                let _ = std::io::Write::flush(&mut std::io::stdout());

                let mut r = runtime.block_on(run_batch_cross_slot_workload(
                    conn.clone(), conc, 10, cfg.warmup, cfg.duration,
                ));
                r.label = label.clone();
                println!("{}", format_tps(r.tps));
                r.print_short();
                runs.push(r);
            }
            let median = median_by_tps(runs);
            println!("  MEDIAN:");
            median.print_full();
            println!();
            medians.push(median);
        }
    }

    // Summary table
    println!("\n=== Summary (median of {} runs) ===", cfg.runs);
    println!(
        "{:<24} {:>14} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Permutation", "TPS", "p50", "p75", "p90", "p99", "max"
    );
    println!("{}", "-".repeat(98));
    for r in &medians {
        println!(
            "{:<24} {:>14} {:>9.3}ms {:>9.3}ms {:>9.3}ms {:>9.3}ms {:>9.3}ms",
            r.label,
            format_tps(r.tps),
            r.p50_us / 1000.0,
            r.p75_us / 1000.0,
            r.p90_us / 1000.0,
            r.p99_us / 1000.0,
            r.max_us / 1000.0,
        );
    }

    // JSON output
    let timestamp = chrono_timestamp();
    let json_path = format!("bench-results-{}-{}.json", cfg.profile, timestamp);
    let json_output = json!({
        "timestamp": timestamp,
        "profile": cfg.profile,
        "config": {
            "duration_secs": cfg.duration.as_secs(),
            "warmup_secs": cfg.warmup.as_secs(),
            "runs_per_permutation": cfg.runs,
            "workload": "80/20 GET/SET",
            "tls": cfg.tls,
            "value_sizes": cfg.value_sizes,
            "concurrencies": cfg.concurrencies,
        },
        "medians": medians.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
    });

    if let Ok(f) = std::fs::File::create(&json_path) {
        let _ = serde_json::to_writer_pretty(f, &json_output);
        println!("\nResults written to {json_path}");
    }
}

fn chrono_timestamp() -> String {
    use std::time::SystemTime;
    let d = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap();
    format!("{}", d.as_secs())
}
