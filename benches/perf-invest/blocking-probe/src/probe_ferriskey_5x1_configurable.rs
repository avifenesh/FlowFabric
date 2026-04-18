//! ferriskey 5×1 BLMPOP probe — configurable blocking-cmd timeout extension.
//!
//! Same workload shape as `probe-ferriskey-5x1`, but reads the server-side
//! BLMPOP timeout and the client-side extension from env vars, then emits
//! structured JSON to a results file named by the parameters.
//!
//! Env:
//!   PROBE_EXT_MS      — blocking_cmd_timeout_extension (default: 500)
//!   PROBE_SRV_TO_S    — server-side BLMPOP timeout seconds (default: 5)
//!   PROBE_RESULTS_DIR — output dir (default: ../results under this crate)
//!
//! Pre-state: redis-cli FLUSHALL && redis-cli LPUSH bench:blocking-list A B
//! Each run: 5 tasks race 5×BLMPOP. 2 should return arrays fast;
//! 3 should block for the server-side timeout then return Nil — UNLESS
//! the client-side deadline cuts in first, in which case they err.

use std::env;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ferriskey::ClientBuilder;

const KEY: &str = "bench:blocking-list";
const N_TASKS: usize = 5;

fn parse_env<T: std::str::FromStr>(name: &str, default: T) -> T {
    env::var(name)
        .ok()
        .and_then(|s| s.parse::<T>().ok())
        .unwrap_or(default)
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let ext_ms: u64 = parse_env("PROBE_EXT_MS", 500);
    let srv_timeout_s: u64 = parse_env("PROBE_SRV_TO_S", 5);
    let results_dir: PathBuf = env::var("PROBE_RESULTS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let manifest = env!("CARGO_MANIFEST_DIR");
            PathBuf::from(manifest).join("results")
        });
    std::fs::create_dir_all(&results_dir)
        .with_context(|| format!("create results dir {}", results_dir.display()))?;

    let ext = Duration::from_millis(ext_ms);
    let client_request_timeout = Duration::from_secs(srv_timeout_s + 2);

    let client = ClientBuilder::new()
        .host("localhost", 6379)
        .request_timeout(client_request_timeout)
        .blocking_cmd_timeout_extension(ext)
        .build()
        .await
        .context("ferriskey connect")?;

    // Warmup
    let _: ferriskey::Value = client
        .cmd("PING")
        .execute()
        .await
        .context("PING warmup")?;

    // Pre-seed the list to 2 entries — matches W1's original probe shape.
    // DEL then LPUSH ensures deterministic pre-state across runs.
    let _: ferriskey::Value = client
        .cmd("DEL")
        .arg(KEY)
        .execute()
        .await
        .context("DEL")?;
    let _: ferriskey::Value = client
        .cmd("LPUSH")
        .arg(KEY)
        .arg("A")
        .arg("B")
        .execute()
        .await
        .context("LPUSH")?;

    let started_at = Instant::now();
    let mut handles = Vec::with_capacity(N_TASKS);
    for i in 0..N_TASKS {
        let c = client.clone();
        let srv = srv_timeout_s;
        handles.push(tokio::spawn(async move {
            let t0 = Instant::now();
            let result: Result<ferriskey::Value, _> = c
                .cmd("BLMPOP")
                .arg(srv.to_string().as_str())
                .arg("1")
                .arg(KEY)
                .arg("LEFT")
                .execute()
                .await;
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            let wall_since_start_ms = started_at.elapsed().as_millis() as u64;
            let (shape, err_msg) = match &result {
                Ok(ferriskey::Value::Nil) => ("nil", None),
                Ok(ferriskey::Value::Array(_)) => ("array", None),
                Ok(_) => ("other-ok", None),
                Err(e) => ("err", Some(format!("{e}"))),
            };
            (i, elapsed_ms, wall_since_start_ms, shape.to_string(), err_msg)
        }));
    }

    let mut results: Vec<(usize, u64, u64, String, Option<String>)> = Vec::with_capacity(N_TASKS);
    for h in handles {
        results.push(h.await.expect("task join"));
    }
    results.sort_by_key(|r| r.0);

    let max_wall = results.iter().map(|r| r.2).max().unwrap_or(0);
    let min_wall = results.iter().map(|r| r.2).min().unwrap_or(0);
    let n_array = results.iter().filter(|r| r.3 == "array").count();
    let n_nil = results.iter().filter(|r| r.3 == "nil").count();
    let n_err = results.iter().filter(|r| r.3 == "err").count();

    println!(
        "ferriskey-5x1 ext={ext_ms}ms srv_timeout={srv_timeout_s}s  array={n_array} nil={n_nil} err={n_err}  spread min={min_wall}ms max={max_wall}ms"
    );
    for (i, elapsed, wall, shape, err) in &results {
        let suffix = err.as_deref().map(|m| format!("  err={m}")).unwrap_or_default();
        println!(
            "  task={i} elapsed_ms={elapsed:>5} wall_from_spawn_ms={wall:>5} result={shape}{suffix}"
        );
    }

    // Emit JSON alongside stdout. Shape matches other perf-invest results
    // (hand-built — keeps us dependency-free).
    let out_path = results_dir.join(format!("probe-5x1-ext{ext_ms}ms-srv{srv_timeout_s}s.json"));
    let mut json = String::new();
    json.push_str("{\n");
    json.push_str(&format!("  \"ext_ms\": {ext_ms},\n"));
    json.push_str(&format!("  \"server_timeout_s\": {srv_timeout_s},\n"));
    json.push_str(&format!("  \"n_tasks\": {N_TASKS},\n"));
    json.push_str(&format!("  \"n_array\": {n_array},\n"));
    json.push_str(&format!("  \"n_nil\": {n_nil},\n"));
    json.push_str(&format!("  \"n_err\": {n_err},\n"));
    json.push_str(&format!("  \"min_wall_ms\": {min_wall},\n"));
    json.push_str(&format!("  \"max_wall_ms\": {max_wall},\n"));
    json.push_str("  \"tasks\": [\n");
    for (idx, (i, elapsed, wall, shape, err)) in results.iter().enumerate() {
        let err_field = match err {
            Some(m) => format!(", \"err\": {}", json_escape(m)),
            None => String::new(),
        };
        let comma = if idx + 1 < results.len() { "," } else { "" };
        json.push_str(&format!(
            "    {{ \"task\": {i}, \"elapsed_ms\": {elapsed}, \"wall_ms\": {wall}, \"result\": \"{shape}\"{err_field} }}{comma}\n"
        ));
    }
    json.push_str("  ]\n}\n");
    std::fs::write(&out_path, json)
        .with_context(|| format!("write {}", out_path.display()))?;
    println!("wrote {}", out_path.display());

    Ok(())
}

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}
