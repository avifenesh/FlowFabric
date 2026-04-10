// Ferriskey throughput + latency benchmark
//
// 80/20 GET/SET workload mix, fixed-duration runs, HdrHistogram percentiles.
//
// Profiles (BENCH_PROFILE env var):
//   quick  - 5s,  1s warmup, 1KB only,         c=1,100           (~1 min)
//   short  - 30s, 3s warmup, 64B+1KB,          c=10,100          (~8 min)
//   full   - 120s,5s warmup, 64B+1KB+64KB,     c=1,10,100,1000   (~48 min)
//
// Other env vars:
//   VALKEY_HOST          cluster config endpoint (default: 127.0.0.1)
//   VALKEY_PORT          cluster port (default: 6379)
//   VALKEY_TLS           "true" to enable TLS (default: false)
//   BENCH_DURATION       override seconds per permutation
//   BENCH_WARMUP         override warmup seconds

use ferriskey::valkey::{
    ConnectionAddr, ConnectionInfo, RedisConnectionInfo, RedisResult, Value,
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
    value_sizes: Vec<usize>,
    concurrencies: Vec<usize>,
}

impl BenchConfig {
    fn from_env() -> Self {
        let profile = env::var("BENCH_PROFILE").unwrap_or_else(|_| "short".into());

        let (default_duration, default_warmup, value_sizes, concurrencies) = match profile.as_str()
        {
            "quick" => (5, 1, vec![100, 1024], vec![1, 100]),
            "full" => (60, 5, vec![100, 1024, 16384], vec![1, 10, 100]),
            _ => (30, 3, vec![100, 1024], vec![10, 100]), // "short" default
        };

        let duration = env::var("BENCH_DURATION")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_duration);
        let warmup = env::var("BENCH_WARMUP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default_warmup);

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
    connection_type: String,
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
    fn print(&self) {
        println!(
            "\n[{}] 80/20 GET/SET  value={}  concurrency={}  duration={:.0}s",
            self.connection_type,
            format_size(self.value_size),
            self.concurrency,
            self.duration_secs,
        );
        println!(
            "  TPS:    {:<12}  (total: {}{})",
            format_tps(self.tps),
            self.total_ops,
            if self.total_errors > 0 {
                format!(", {} errors", self.total_errors)
            } else {
                String::new()
            },
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
            "connection_type": self.connection_type,
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
    Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_cpus::get())
        .build()
        .unwrap()
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
        connection_type: String::new(),
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
// Bench runner - runs one connection type through the matrix
// ---------------------------------------------------------------------------

fn run_matrix(
    runtime: &Runtime,
    conn_type: &str,
    cfg: &BenchConfig,
    conn: impl ConnectionLike + Clone + Send + 'static,
) -> Vec<BenchResult> {
    let mut results = Vec::new();

    for &size in &cfg.value_sizes {
        for &conc in &cfg.concurrencies {
            print!(
                "  {conn_type} value={} c={} ... ",
                format_size(size),
                conc
            );
            let _ = std::io::Write::flush(&mut std::io::stdout());

            let mut r =
                runtime.block_on(run_workload(conn.clone(), conc, size, cfg.warmup, cfg.duration));
            r.connection_type = conn_type.into();
            r.label = format!("{conn_type}-{}-c{conc}", format_size(size));

            println!("{}", format_tps(r.tps));
            r.print();
            results.push(r);
        }
    }

    results
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

fn main() {
    let cfg = BenchConfig::from_env();
    let runtime = create_runtime();

    let permutations = cfg.value_sizes.len() * cfg.concurrencies.len();
    let est_mins =
        (permutations as u64 * (cfg.duration.as_secs() + cfg.warmup.as_secs()) + 59) / 60;

    println!("=== Ferriskey Benchmark [{}] ===", cfg.profile);
    println!(
        "  {}s measure + {}s warmup | {} permutations | ~{} min",
        cfg.duration.as_secs(),
        cfg.warmup.as_secs(),
        permutations,
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

    println!("--- cluster  {}:{} TLS={} ---", cfg.host, cfg.port, cfg.tls);
    let results = run_matrix(&runtime, "cluster", &cfg, conn);

    // Summary table
    println!("\n\n=== Summary ===");
    println!(
        "{:<28} {:>14} {:>10} {:>10} {:>10} {:>10} {:>10}",
        "Permutation", "TPS", "p50", "p75", "p90", "p99", "max"
    );
    println!("{}", "-".repeat(102));
    for r in &results {
        println!(
            "{:<28} {:>14} {:>9.3}ms {:>9.3}ms {:>9.3}ms {:>9.3}ms {:>9.3}ms",
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
            "workload": "80/20 GET/SET",
            "tls": cfg.tls,
            "value_sizes": cfg.value_sizes,
            "concurrencies": cfg.concurrencies,
        },
        "results": results.iter().map(|r| r.to_json()).collect::<Vec<_>>(),
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
