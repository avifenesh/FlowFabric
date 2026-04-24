//! RFC-015 §4.3 — EMA α selection for BestEffortLive MAXLEN sizing.
//!
//! # What this measures
//!
//! RFC-015 §4.2 specifies that `BestEffortLive { ttl_ms }` derives its
//! per-stream `MAXLEN` from an EMA of append rate:
//!
//!     K = clamp(ceil(ttl_ms/1000 * recent_rate_hz * 2), 64, 16_384)
//!
//! where `recent_rate_hz` is an EMA with decay constant α. RFC §4.3
//! was closed post-Phase-0 with α = 0.2, safety = 2.0, floor = 64,
//! ceiling = 16_384. This harness is the re-validation step: it
//! replays the same LLM-token trace through the final parameters and
//! reports visibility % vs the static-64 shipped baseline.
//!
//! # Workload
//!
//! Synthesizes an LLM-token trace: bursts of 50-2000 frames over
//! 500ms-10s, interleaved with 1-30s idle gaps. Matches the profile
//! called out in RFC §4.2 ("LLM token-by-token delta streams") and
//! the Phase 0 task spec.
//!
//! # Outputs
//!
//! For each candidate α in {0.1, 0.2, 0.3, 0.5}, and for the current
//! static default (MAXLEN = 64), simulates the per-append MAXLEN
//! decision and emits:
//!   - p50 / p99 of the computed K (memory footprint proxy)
//!   - p50 / p99 of the retained stream-size window at sample time
//!   - fraction of frames visible within `ttl_ms` of their append
//!
//! # Gate decision
//!
//! The recommended α is the one that keeps the retained-window p99
//! within the RFC's ttl_ms visibility target while minimizing K p99.
//! Output JSON: `benches/results/rfc-015-ema-alpha-<sha>.json`
//! (gitignored per benches/results/.gitignore).

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

const TTL_MS: u32 = 30_000; // 30s visibility target (RFC §4.2 example)
// Final RFC-015 §4.2 clamp bounds. Floor = 64 matches the pre-dynamic
// static default; ceiling was raised from the draft 2048 to 16_384
// after Phase 0 showed 200–4000 Hz LLM-token bursts saturate any
// lower clamp regardless of α.
const MAXLEN_CLAMP_LO: u32 = 64;
const MAXLEN_CLAMP_HI: u32 = 16_384;
const STATIC_DEFAULT_MAXLEN: u32 = 64; // pre-dynamic static default

/// One synthetic append event in the trace.
#[derive(Clone, Copy, Debug)]
struct Append {
    t_ms: u64,
}

/// Generate a bursty LLM-token-ish trace. Deterministic from `seed`.
fn gen_trace(seed: u64, total_seconds: u64) -> Vec<Append> {
    // Simple LCG; bench-quality randomness is enough.
    let mut state = seed.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut rnd = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        state
    };
    let rnd_range = |r: &mut dyn FnMut() -> u64, lo: u64, hi: u64| -> u64 {
        let v = r();
        lo + (v % (hi - lo + 1))
    };

    let mut trace = Vec::new();
    let mut t: u64 = 0;
    let end = total_seconds * 1000;
    while t < end {
        // Burst: 50-2000 frames emitted over 500ms-10s.
        let frames = rnd_range(&mut rnd, 50, 2000);
        let duration_ms = rnd_range(&mut rnd, 500, 10_000);
        let inter_arrival_us =
            ((duration_ms as f64 * 1000.0) / frames as f64).max(1.0) as u64;
        for i in 0..frames {
            let apt = t + (i * inter_arrival_us) / 1000;
            if apt >= end {
                break;
            }
            trace.push(Append { t_ms: apt });
        }
        t += duration_ms;
        // Idle: 1-30s.
        let idle = rnd_range(&mut rnd, 1_000, 30_000);
        t += idle;
    }
    trace
}

/// Simulate the MAXLEN policy across the trace. Returns per-append
/// computed K (what the EMA would set MAXLEN to at that XADD) plus
/// the simulated retained-window size AFTER trim.
fn simulate(
    trace: &[Append],
    mode: SizingMode,
) -> SimResult {
    // Window of retained appends (append times) — simulate MAXLEN trim
    // by keeping at most K entries (approximate trim ignored; we take
    // the exact MAXLEN semantic here, which is conservative for K).
    let mut retained: std::collections::VecDeque<u64> =
        std::collections::VecDeque::new();
    let mut k_samples: Vec<u32> = Vec::with_capacity(trace.len());
    let mut retained_samples: Vec<u32> = Vec::with_capacity(trace.len());
    let mut visibility_hits: u64 = 0;
    let mut visibility_total: u64 = 0;

    // EMA state: recent_rate_hz (Hz). Seed at 1 Hz so cold-start K
    // starts at the clamp floor rather than 0.
    let mut rate_hz: f64 = 1.0;
    let mut last_t_ms: u64 = 0;
    let mut first = true;

    // For alpha modes: compute a time-weighted EMA update. Treating α
    // as decay per-append approximates the RFC's "per-sample EMA" —
    // the RFC doesn't specify per-append vs per-tick, we use per-
    // append because that's the cheapest place to update (§4.2 notes
    // "HSET/HGET on metadata Hash per append").
    for (i, a) in trace.iter().enumerate() {
        // Update rate EMA.
        if !first {
            let dt_ms = a.t_ms.saturating_sub(last_t_ms).max(1);
            // Instantaneous rate sample: 1 event / dt seconds.
            let inst_hz = 1000.0 / (dt_ms as f64);
            if let SizingMode::Ema { alpha } = mode {
                rate_hz = alpha * inst_hz + (1.0 - alpha) * rate_hz;
            }
        }
        first = false;
        last_t_ms = a.t_ms;

        // Compute K for this append.
        let k = match mode {
            SizingMode::Static(k) => k,
            SizingMode::Ema { .. } => {
                let raw = ((TTL_MS as f64 / 1000.0) * rate_hz * 2.0).ceil() as u32;
                raw.clamp(MAXLEN_CLAMP_LO, MAXLEN_CLAMP_HI)
            }
        };
        k_samples.push(k);

        // Apply XADD + trim.
        retained.push_back(a.t_ms);
        while retained.len() as u32 > k {
            retained.pop_front();
        }
        retained_samples.push(retained.len() as u32);

        // Visibility check: a frame is "visible" if a tailer arriving
        // ttl_ms after its append would still find it in the retained
        // window. Approximate by checking whether the 10th-most-recent
        // predecessor of `a` is still within retained at time
        // `a.t_ms`. More directly: every append in `retained` that
        // was appended within the last `ttl_ms` counts as "hit".
        let cutoff = a.t_ms.saturating_sub(TTL_MS as u64);
        // Every append within [cutoff, a.t_ms] currently in retained
        // is a visibility hit. Index-based count: walk retained
        // backwards until time < cutoff.
        let mut hits = 0u64;
        for t in retained.iter().rev() {
            if *t < cutoff {
                break;
            }
            hits += 1;
        }
        // The "should be visible" frames are all trace entries in the
        // same time window. For the per-append sample, we measure the
        // ratio of `hits / appends_in_window` -- but only over the
        // last ~1000 entries to cap cost.
        // Simpler: for each append i, visibility_total++; visibility_hits
        // += 1 iff this append itself is still in retained (trivially
        // yes, just pushed) -- so instead we sample deferred: count
        // fraction of trace entries that, at time+ttl_ms, would still
        // be in retained. Approximated below in post-processing.
        let _ = (hits, i);
    }

    // Post-process visibility: for each entry, find the first trace
    // append at time >= (entry.t_ms + ttl_ms). At that future moment,
    // was `entry` still in the retained window? Reconstruct retained
    // state forwards: use the K sequence we already recorded.
    //
    // Efficient approach: maintain a rolling "retained at time T"
    // structure while scanning i forward; for each entry j < i, when
    // i.t_ms >= j.t_ms + ttl_ms, record whether j is still present in
    // the current `retained` deque (sorted by insertion order = by
    // t_ms). Since retained tracks append times, presence test is
    // "j.t_ms >= retained.front()?".
    //
    // Redo the sim cheaply to compute visibility.
    {
        let mut retained2: std::collections::VecDeque<u64> =
            std::collections::VecDeque::new();
        let mut check_idx = 0usize;
        for i in 0..trace.len() {
            let k = k_samples[i];
            retained2.push_back(trace[i].t_ms);
            while retained2.len() as u32 > k {
                retained2.pop_front();
            }
            // Flush any pending visibility checks due at or before now.
            while check_idx < i {
                let target = trace[check_idx].t_ms + TTL_MS as u64;
                if trace[i].t_ms < target {
                    break;
                }
                visibility_total += 1;
                // Present iff retained2.front() <= trace[check_idx].t_ms.
                let present = retained2
                    .front()
                    .map(|f| *f <= trace[check_idx].t_ms)
                    .unwrap_or(false);
                if present {
                    visibility_hits += 1;
                }
                check_idx += 1;
            }
        }
        // Trailing entries whose ttl expires after the trace ends: skip
        // -- they have no tailer-arrival-moment to check.
    }

    SimResult {
        k_samples,
        retained_samples,
        visibility_hits,
        visibility_total,
    }
}

#[derive(Clone, Copy, Debug)]
enum SizingMode {
    Static(u32),
    Ema { alpha: f64 },
}

struct SimResult {
    k_samples: Vec<u32>,
    retained_samples: Vec<u32>,
    visibility_hits: u64,
    visibility_total: u64,
}

fn percentile(vals: &mut Vec<u32>, p: f64) -> u32 {
    if vals.is_empty() {
        return 0;
    }
    vals.sort_unstable();
    let idx = ((vals.len() as f64 - 1.0) * p / 100.0).round() as usize;
    vals[idx]
}

#[derive(Serialize, Deserialize)]
struct ModeReport {
    label: String,
    k_p50: u32,
    k_p99: u32,
    retained_p50: u32,
    retained_p99: u32,
    visibility_ratio: f64,
    total_appends: usize,
}

#[derive(Serialize, Deserialize)]
struct Report {
    scenario: &'static str,
    rfc: &'static str,
    trace_seconds: u64,
    ttl_ms: u32,
    total_appends: usize,
    modes: Vec<ModeReport>,
    recommendation: String,
    timestamp: String,
}

fn main() -> anyhow::Result<()> {
    let trace_seconds = std::env::var("FF_EMA_TRACE_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600u64); // 10 minutes
    let seed: u64 = std::env::var("FF_EMA_SEED")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0xFEED_FACE_CAFE_BEEF);

    let trace = gen_trace(seed, trace_seconds);
    eprintln!(
        "trace: {} appends over {} s (p.avg rate {:.1} Hz)",
        trace.len(),
        trace_seconds,
        trace.len() as f64 / trace_seconds as f64
    );

    let modes: Vec<(String, SizingMode)> = vec![
        (
            format!("static_maxlen_{STATIC_DEFAULT_MAXLEN}"),
            SizingMode::Static(STATIC_DEFAULT_MAXLEN),
        ),
        ("ema_alpha_0.1".to_string(), SizingMode::Ema { alpha: 0.1 }),
        ("ema_alpha_0.2".to_string(), SizingMode::Ema { alpha: 0.2 }),
        ("ema_alpha_0.3".to_string(), SizingMode::Ema { alpha: 0.3 }),
        ("ema_alpha_0.5".to_string(), SizingMode::Ema { alpha: 0.5 }),
    ];

    let mut mode_reports = Vec::new();
    for (label, mode) in &modes {
        let sim = simulate(&trace, *mode);
        let mut ks = sim.k_samples.clone();
        let mut rs = sim.retained_samples.clone();
        let k_p50 = percentile(&mut ks.clone(), 50.0);
        let k_p99 = percentile(&mut ks, 99.0);
        let r_p50 = percentile(&mut rs.clone(), 50.0);
        let r_p99 = percentile(&mut rs, 99.0);
        let vis = if sim.visibility_total > 0 {
            sim.visibility_hits as f64 / sim.visibility_total as f64
        } else {
            0.0
        };
        eprintln!(
            "{label:20}  K p50={k_p50:5}  K p99={k_p99:5}  retained p50={r_p50:5}  retained p99={r_p99:5}  vis={vis:.4}"
        );
        mode_reports.push(ModeReport {
            label: label.clone(),
            k_p50,
            k_p99,
            retained_p50: r_p50,
            retained_p99: r_p99,
            visibility_ratio: vis,
            total_appends: trace.len(),
        });
    }

    // Recommendation logic:
    // 1. Static-64 baseline visibility ratio is the "what do we ship
    //    today if EMA isn't implemented?" number.
    // 2. Find the α with the HIGHEST visibility ratio such that K p99
    //    stays <= 2*MAXLEN_CLAMP_HI (i.e., doesn't saturate the clamp).
    //    Tie-break by lower K p99 (cheaper memory).
    let static_baseline = &mode_reports[0];
    let mut best: Option<&ModeReport> = None;
    for mr in mode_reports.iter().skip(1) {
        if let Some(cur) = best {
            if mr.visibility_ratio > cur.visibility_ratio + 1e-6
                || (mr.visibility_ratio >= cur.visibility_ratio - 1e-6
                    && mr.k_p99 < cur.k_p99)
            {
                best = Some(mr);
            }
        } else {
            best = Some(mr);
        }
    }
    let recommendation = match best {
        Some(b) if b.visibility_ratio > static_baseline.visibility_ratio + 0.01 => {
            format!(
                "Adopt {}: +{:.2}pp visibility vs static MAXLEN={} baseline (K p99 {} vs {}).",
                b.label,
                100.0 * (b.visibility_ratio - static_baseline.visibility_ratio),
                STATIC_DEFAULT_MAXLEN,
                b.k_p99,
                static_baseline.k_p99
            )
        }
        _ => format!(
            "KEEP static MAXLEN={STATIC_DEFAULT_MAXLEN}: EMA variants do not meaningfully \
             improve visibility over the static default on this workload \
             (baseline vis={:.4}). Ship v0.6 with MAXLEN={STATIC_DEFAULT_MAXLEN}; \
             defer EMA implementation.",
            static_baseline.visibility_ratio
        ),
    };

    let sha = std::env::var("FF_BENCH_SHA").unwrap_or_else(|_| short_git_sha());
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let report = Report {
        scenario: "rfc-015-ema-alpha",
        rfc: "RFC-015 §4.3",
        trace_seconds,
        ttl_ms: TTL_MS,
        total_appends: trace.len(),
        modes: mode_reports,
        recommendation,
        timestamp: format!("{now}"),
    };

    let out_dir = PathBuf::from("benches/results");
    fs::create_dir_all(&out_dir)?;
    let out = out_dir.join(format!("rfc-015-ema-alpha-{sha}.json"));
    let mut f = fs::File::create(&out)?;
    f.write_all(serde_json::to_string_pretty(&report)?.as_bytes())?;
    eprintln!("wrote {}", out.display());
    println!("{}", report.recommendation);

    Ok(())
}

fn short_git_sha() -> String {
    use std::process::Command;
    Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                Some(String::from_utf8_lossy(&o.stdout).trim().to_string())
            } else {
                None
            }
        })
        .unwrap_or_else(|| "unknown".to_string())
}
