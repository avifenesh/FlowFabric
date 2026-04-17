//! Single source of truth for bench report JSON. `check_release.py`
//! parses this shape; every scenario writes it identically.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

/// System-under-test identifier used in the JSON. Comparison packages
/// write their own constants ("apalis" / "faktory" / "baseline") so we
/// can merge into a single COMPARISON.md.
pub const SYSTEM_FLOWFABRIC: &str = "flowfabric";

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Report {
    pub scenario: String,
    pub system: String,
    pub git_sha: String,
    pub valkey_version: String,
    pub host: HostInfo,
    pub cluster: bool,
    pub timestamp_utc: String,
    pub config: serde_json::Value,
    pub throughput_ops_per_sec: f64,
    pub latency_ms: LatencyMs,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct HostInfo {
    pub cpu: String,
    pub cores: usize,
    pub mem_gb: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct LatencyMs {
    pub p50: f64,
    pub p95: f64,
    pub p99: f64,
}

impl Report {
    /// Resolve everything a scenario doesn't control — git sha, host,
    /// Valkey version, timestamp. The scenario fills config + numbers.
    pub fn fill_env(
        scenario: impl Into<String>,
        system: impl Into<String>,
        cluster: bool,
        config: serde_json::Value,
        throughput_ops_per_sec: f64,
        latency_ms: LatencyMs,
    ) -> Self {
        Self {
            scenario: scenario.into(),
            system: system.into(),
            git_sha: git_sha(),
            valkey_version: valkey_version(),
            host: host_info(),
            cluster,
            timestamp_utc: iso8601_utc(),
            config,
            throughput_ops_per_sec,
            latency_ms,
            notes: None,
        }
    }
}

/// Compute p50 / p95 / p99 from a slice of durations in microseconds.
/// Caller sorts the slice (we sort-copy to keep the API honest).
pub struct Percentiles;

impl Percentiles {
    pub fn from_micros(samples: &[u64]) -> LatencyMs {
        if samples.is_empty() {
            return LatencyMs::default();
        }
        let mut sorted: Vec<u64> = samples.to_vec();
        sorted.sort_unstable();
        LatencyMs {
            p50: pct_us_to_ms(&sorted, 0.50),
            p95: pct_us_to_ms(&sorted, 0.95),
            p99: pct_us_to_ms(&sorted, 0.99),
        }
    }
}

fn pct_us_to_ms(sorted_us: &[u64], q: f64) -> f64 {
    // Nearest-rank; fine for bench-grade stats, not enough samples for
    // the interpolation pedantry to matter.
    let n = sorted_us.len();
    let idx = ((n as f64 - 1.0) * q).round() as usize;
    sorted_us[idx.min(n - 1)] as f64 / 1000.0
}

pub fn write_report(report: &Report, results_dir: &Path) -> anyhow::Result<PathBuf> {
    fs::create_dir_all(results_dir)?;
    let path = results_dir.join(format!("{}-{}.json", report.scenario, report.git_sha));
    let bytes = serde_json::to_vec_pretty(report)?;
    fs::write(&path, bytes)?;
    Ok(path)
}

/// Short git sha of the current HEAD, or `"unknown"` outside a repo.
pub fn git_sha() -> String {
    let out = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| o.status.success().then(|| o.stdout))
        .map(|b| String::from_utf8_lossy(&b).trim().to_owned());
    out.unwrap_or_else(|| "unknown".to_owned())
}

/// Version string of the live Valkey / Redis pointed at by
/// `FF_BENCH_VALKEY_HOST`:`FF_BENCH_VALKEY_PORT`. Falls back to
/// `"unknown"` when neither `valkey-cli` nor `redis-cli` is on PATH.
pub fn valkey_version() -> String {
    // Ask the live server for its version. The harness already honours
    // `FF_BENCH_VALKEY_HOST` / `FF_BENCH_VALKEY_PORT` (see workload.rs);
    // reuse the same env vars so a bench pointed at a non-localhost
    // Valkey doesn't report the wrong version.
    //
    // Try `valkey-cli` first, fall back to `redis-cli`, fall back to
    // `"unknown"`. Both CLIs accept the same `-h/-p/INFO server` args
    // and speak the same RESP — the distinction is only the binary
    // name on disk.
    let host = std::env::var("FF_BENCH_VALKEY_HOST").unwrap_or_else(|_| "localhost".to_owned());
    let port = std::env::var("FF_BENCH_VALKEY_PORT").unwrap_or_else(|_| "6379".to_owned());

    for cli in ["valkey-cli", "redis-cli"] {
        let out = Command::new(cli)
            .args(["-h", &host, "-p", &port, "INFO", "server"])
            .output();
        let text = match out {
            Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
            _ => continue,
        };
        for line in text.lines() {
            if let Some(v) = line
                .strip_prefix("valkey_version:")
                .or_else(|| line.strip_prefix("redis_version:"))
            {
                return v.trim().to_owned();
            }
        }
    }
    "unknown".to_owned()
}

/// Inspect the machine running the bench. Shared across the harness
/// and every comparison package so the report JSON is uniform.
///
/// Portable across the CI matrix: Linux reads `/proc/{cpuinfo,meminfo}`,
/// macOS asks `sysctl`, any other target falls through to `"unknown"`
/// / `0` — a report still serialises, the unknown fields are just less
/// useful.
pub fn host_info() -> HostInfo {
    let cores = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1);
    let cpu = probe_cpu_model().unwrap_or_else(|| "unknown".to_owned());
    let mem_gb = probe_mem_gb().unwrap_or(0);
    HostInfo { cpu, cores, mem_gb }
}

fn probe_cpu_model() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        return read_proc_cpu_model();
    }
    #[cfg(target_os = "macos")]
    {
        return read_sysctl("machdep.cpu.brand_string");
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        return None;
    }
}

fn probe_mem_gb() -> Option<u64> {
    #[cfg(target_os = "linux")]
    {
        return read_proc_mem_gb();
    }
    #[cfg(target_os = "macos")]
    {
        // `sysctl -n hw.memsize` returns bytes.
        return read_sysctl("hw.memsize")
            .and_then(|s| s.trim().parse::<u64>().ok())
            .map(|b| b / (1024 * 1024 * 1024));
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        return None;
    }
}

#[cfg(target_os = "linux")]
fn read_proc_cpu_model() -> Option<String> {
    let text = fs::read_to_string("/proc/cpuinfo").ok()?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("model name") {
            // /proc/cpuinfo lines are `key<tab+spaces>: value`; trim the
            // separator tokens, then trim whitespace.
            return Some(
                v.trim()
                    .trim_start_matches(':')
                    .trim()
                    .to_owned(),
            );
        }
    }
    None
}

#[cfg(target_os = "linux")]
fn read_proc_mem_gb() -> Option<u64> {
    let text = fs::read_to_string("/proc/meminfo").ok()?;
    for line in text.lines() {
        if let Some(v) = line.strip_prefix("MemTotal:") {
            let kb: u64 = v.split_whitespace().next()?.parse().ok()?;
            return Some(kb / 1_024 / 1_024);
        }
    }
    None
}

#[cfg(target_os = "macos")]
fn read_sysctl(key: &str) -> Option<String> {
    let out = Command::new("sysctl").args(["-n", key]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    if s.is_empty() { None } else { Some(s) }
}

/// Seconds-precision ISO 8601 UTC timestamp. No external date-time
/// dependency — the bench harness avoids `chrono` / `time` so the
/// comparison packages can copy the field verbatim without inheriting
/// a crate.
pub fn iso8601_utc() -> String {
    // Second-precision is enough for bench reports; avoid pulling chrono
    // into a harness dep graph.
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_secs_as_iso8601(secs)
}

fn format_secs_as_iso8601(mut secs: u64) -> String {
    // Proleptic Gregorian, UTC, no leap seconds — matches every other
    // Rust "just gimme a timestamp" crate.
    let s = (secs % 60) as u32;
    secs /= 60;
    let min = (secs % 60) as u32;
    secs /= 60;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentiles_handle_empty() {
        let p = Percentiles::from_micros(&[]);
        assert_eq!(p.p50, 0.0);
        assert_eq!(p.p95, 0.0);
        assert_eq!(p.p99, 0.0);
    }

    #[test]
    fn percentiles_on_known_distribution() {
        // 100 samples at 1..=100 μs, nearest-rank with (n-1)·q rounded.
        // q=0.50 → idx 50 → 51 μs
        // q=0.95 → idx 94 → 95 μs
        // q=0.99 → idx 98 → 99 μs
        // Reported in ms so divide by 1000.
        let samples: Vec<u64> = (1..=100).collect();
        let p = Percentiles::from_micros(&samples);
        assert!((p.p50 - 0.051).abs() < 1e-6, "p50 was {}", p.p50);
        assert!((p.p95 - 0.095).abs() < 1e-6, "p95 was {}", p.p95);
        assert!((p.p99 - 0.099).abs() < 1e-6, "p99 was {}", p.p99);
    }

    #[test]
    fn iso8601_epoch_is_1970() {
        assert_eq!(format_secs_as_iso8601(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn iso8601_leap_year_handled() {
        // 2024-02-29T00:00:00Z = 1_709_164_800 unix seconds.
        assert_eq!(
            format_secs_as_iso8601(1_709_164_800),
            "2024-02-29T00:00:00Z"
        );
    }
}
