//! Shared helpers used by every wider/ bin. Kept as a plain module
//! (not a library crate) so the bins can `include!` style `mod shared`
//! and stay trivially inspectable side-by-side.
//!
//! WHY a `shared` module and not a lib target: every wider/ bin is
//! <200 LOC around a hot loop. A lib crate would be more formal but
//! the whole point of this workspace is that a reviewer can `diff`
//! ferriskey_80_20.rs against redis_80_20.rs and see the client-API
//! difference directly — introducing a trait-based abstraction to
//! hide the two clients behind one signature would defeat that.

use std::path::Path;
use std::time::Duration;

use hdrhistogram::Histogram;

/// Deterministic 4 KiB filler reused across workloads. SAME bytes
/// across a run — we are measuring client + Valkey round-trip, not
/// payload entropy. Seeded per-run so back-to-back invocations
/// against the same Valkey don't compare identical-key writes and
/// mask cache effects.
pub fn filler_payload(size: usize) -> Vec<u8> {
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

/// Pre-generated key ring — 10 000 keys sharing a `{wider}` hash tag
/// so every command lands on the same Valkey slot. 10 000 is large
/// enough that 16 workers × 30 s × ~10 000 ops/s don't repeatedly
/// hit the same key (avoiding intra-connection contention / hot-key
/// effects) but small enough that the whole ring stays in Valkey's
/// working set.
pub fn key_ring(count: usize) -> Vec<String> {
    (0..count)
        .map(|i| format!("{{wider}}:k{i:08}"))
        .collect()
}

/// HdrHistogram percentile snapshot. us resolution, 3 SIGDIGITS — same
/// shape the native ferriskey bench uses so the two report writers
/// line up on output.
pub struct HdrSnapshot {
    pub p50_us: f64,
    pub p95_us: f64,
    pub p99_us: f64,
    pub max_us: f64,
    pub count: u64,
}

/// Fold a set of per-worker hdr histograms into one snapshot. Values
/// are microseconds. Empty input yields zeros (not NaN) so report
/// JSON stays valid on a run that errored out before any samples.
pub fn hdr_snapshot(histos: &[Histogram<u64>]) -> HdrSnapshot {
    let mut merged = Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap();
    for h in histos {
        merged.add(h).ok();
    }
    if merged.len() == 0 {
        return HdrSnapshot {
            p50_us: 0.0,
            p95_us: 0.0,
            p99_us: 0.0,
            max_us: 0.0,
            count: 0,
        };
    }
    HdrSnapshot {
        p50_us: merged.value_at_quantile(0.50) as f64,
        p95_us: merged.value_at_quantile(0.95) as f64,
        p99_us: merged.value_at_quantile(0.99) as f64,
        max_us: merged.max() as f64,
        count: merged.len(),
    }
}

/// One hdr per worker. Bound = 60 s (well above anything we'd measure
/// here) at 3 SIGDIGITS, same as native ferriskey bench.
pub fn new_worker_hdr() -> Histogram<u64> {
    Histogram::<u64>::new_with_bounds(1, 60_000_000, 3).unwrap()
}

/// Shared report schema — `config` block differs per workload, but
/// the outer shape + metadata fields are identical to baseline/ and
/// ferriskey-baseline/ so `benches/results/` aggregators slurp it
/// with no special case.
#[allow(clippy::too_many_arguments)]
pub fn write_report(
    results_dir: &str,
    scenario: &str,
    system: &str,
    config: serde_json::Value,
    duration: Duration,
    total_ops: u64,
    total_errors: u64,
    lat: &HdrSnapshot,
    notes: &str,
) -> anyhow::Result<std::path::PathBuf> {
    let throughput = if duration.as_secs_f64() > 0.0 {
        total_ops as f64 / duration.as_secs_f64()
    } else {
        0.0
    };
    let report = serde_json::json!({
        "scenario": scenario,
        "system": system,
        "git_sha": ff_bench::git_sha(),
        "valkey_version": ff_bench::valkey_version(),
        "host": ff_bench::host_info(),
        "cluster": false,
        "timestamp_utc": ff_bench::iso8601_utc(),
        "config": config,
        "throughput_ops_per_sec": throughput,
        "total_ops": total_ops,
        "total_errors": total_errors,
        "duration_s": duration.as_secs_f64(),
        "latency_ms": {
            "p50": lat.p50_us / 1000.0,
            "p95": lat.p95_us / 1000.0,
            "p99": lat.p99_us / 1000.0,
            "max": lat.max_us / 1000.0,
        },
        "hdr_samples": lat.count,
        "notes": notes,
    });
    let dir = Path::new(results_dir);
    std::fs::create_dir_all(dir)?;
    let path = dir.join(format!(
        "{}-{}-{}.json",
        scenario,
        system,
        report["git_sha"].as_str().unwrap_or("unknown")
    ));
    std::fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    Ok(path)
}
