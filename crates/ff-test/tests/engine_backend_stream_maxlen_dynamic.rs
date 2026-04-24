//! RFC-015 §4.2 — dynamic MAXLEN sizing for `BestEffortLive`.
//!
//! Three scenarios:
//! 1. Bursty producer → `maxlen_applied_last` climbs past the floor.
//! 2. Idle gap → next append's recomputed EMA drops (rate lull).
//! 3. Injected hi-rate → MAXLEN pegs at the ceiling.
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_stream_maxlen_dynamic \
//!       -- --test-threads=1

use ferriskey::Value;
use ff_core::keys::ExecKeyContext;
use ff_core::partition::execution_partition;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

// ──────────────────────────────────────────────────────────────────────
// Fixture — minimal exec_core seed so lease HMGET in ff_append_frame
// passes. Copy of the pattern used by engine_backend_stream_modes.rs.
// ──────────────────────────────────────────────────────────────────────

struct DynMaxlenFixture {
    tc: TestCluster,
    eid: ExecutionId,
    attempt_index: u32,
    attempt_id: String,
    lease_id: String,
    lease_epoch: String,
}

impl DynMaxlenFixture {
    async fn setup() -> Self {
        let tc = TestCluster::connect().await;
        tc.cleanup().await;
        let eid = tc.new_execution_id();
        let partition = execution_partition(&eid, tc.partition_config());
        let ctx = ExecKeyContext::new(&partition, &eid);

        let now_ms = TimestampMs::now().0;
        let lease_id = format!("lease-{}", uuid::Uuid::new_v4());
        let lease_epoch = "1".to_owned();
        let attempt_index: u32 = 0;
        let attempt_id = format!("attempt-{}", uuid::Uuid::new_v4());

        let core_key = ctx.core();
        let fields: Vec<(&str, String)> = vec![
            ("current_attempt_index", attempt_index.to_string()),
            ("current_lease_id", lease_id.clone()),
            ("current_lease_epoch", lease_epoch.clone()),
            ("lease_expires_at", (now_ms + 300_000).to_string()),
            ("lifecycle_phase", "active".to_owned()),
            ("ownership_state", "owned".to_owned()),
        ];
        for (f, v) in &fields {
            tc.client()
                .hset(&core_key, *f, v.as_str())
                .await
                .unwrap_or_else(|e| panic!("HSET {core_key} {f} failed: {e}"));
        }

        Self {
            tc,
            eid,
            attempt_index,
            attempt_id,
            lease_id,
            lease_epoch,
        }
    }

    fn ctx(&self) -> ExecKeyContext {
        let partition = execution_partition(&self.eid, self.tc.partition_config());
        ExecKeyContext::new(&partition, &self.eid)
    }

    /// Invoke ff_append_frame with the caller-supplied override
    /// `ts_ms`. Overriding `ts` from the caller lets tests simulate
    /// append cadence without actually sleeping — Lua reads ARGV 6 as
    /// the frame timestamp but the EMA estimator uses `redis.call("TIME")`
    /// for `now_ms` (see stream.lua). Tests that exercise cadence
    /// therefore have to issue real waits OR use a fresh XADD per tick
    /// (the latter is what the burst sim does).
    async fn append_best_effort(
        &self,
        payload: &str,
        ttl_ms: u32,
        floor: u32,
        ceiling: u32,
        alpha: f64,
    ) -> Value {
        let ctx = self.ctx();
        let att_idx = AttemptIndex::new(self.attempt_index);
        let keys: Vec<String> = vec![
            ctx.core(),
            ctx.stream(att_idx),
            ctx.stream_meta(att_idx),
            ctx.stream_summary(att_idx),
        ];
        let now = TimestampMs::now();
        let args: Vec<String> = vec![
            self.eid.to_string(),
            self.attempt_index.to_string(),
            self.lease_id.clone(),
            self.lease_epoch.clone(),
            "delta".to_owned(),
            now.to_string(),
            payload.to_owned(),
            "utf8".to_owned(),
            String::new(),
            "worker".to_owned(),
            "0".to_owned(),
            self.attempt_id.clone(),
            "65536".to_owned(),
            "best_effort".to_owned(),
            String::new(),
            ttl_ms.to_string(),
            floor.to_string(),
            ceiling.to_string(),
            format!("{alpha:.6}"),
        ];
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.tc
            .client()
            .fcall("ff_append_frame", &key_refs, &arg_refs)
            .await
            .expect("FCALL ff_append_frame failed")
    }

    async fn read_meta_field(&self, field: &str) -> String {
        let ctx = self.ctx();
        let meta_key = ctx.stream_meta(AttemptIndex::new(self.attempt_index));
        self.tc
            .client()
            .hget(&meta_key, field)
            .await
            .unwrap_or_default()
            .unwrap_or_default()
    }
}

// ──────────────────────────────────────────────────────────────────────
// Test 1 — bursty producer climbs past the floor.
//
// 500 Hz synthetic burst for ~500 ms (256 frames). After the burst the
// stored `ema_rate_hz` should be well above the seed rate and
// `maxlen_applied_last` should be meaningfully above the 64 floor
// given a 30 s TTL target.
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn bursty_producer_climbs_past_floor() {
    let f = DynMaxlenFixture::setup().await;

    // 256 rapid appends. Back-to-back fcalls land well above 200 Hz in
    // wall-clock — typical Valkey local-loop is multi-kHz — which is
    // exactly the in-burst regime the feature targets.
    let n: usize = 256;
    let ttl_ms: u32 = 30_000;
    let floor: u32 = 64;
    let ceiling: u32 = 16_384;
    let alpha: f64 = 0.2;

    for i in 0..n {
        let _ = f
            .append_best_effort(&format!("tok-{i}"), ttl_ms, floor, ceiling, alpha)
            .await;
    }

    let maxlen_str = f.read_meta_field("maxlen_applied_last").await;
    let ema_str = f.read_meta_field("ema_rate_hz").await;
    let maxlen: u64 = maxlen_str.parse().unwrap_or(0);
    let ema_rate: f64 = ema_str.parse().unwrap_or(0.0);

    // A 500 Hz sustained rate with ttl=30s, safety=2 produces
    // K = ceil(500 * 30 * 2) = 30_000 → clamped to 16_384 ceiling. Even
    // allowing for warm-up (EMA started at the floor seed), a 256-append
    // burst at fcall rate must land MUCH higher than the floor.
    assert!(
        maxlen > floor as u64,
        "maxlen_applied_last ({maxlen}) must exceed the floor ({floor}) after a burst"
    );
    assert!(
        ema_rate > 1.0,
        "ema_rate_hz ({ema_rate:.3}) must be well above the seed rate after a burst"
    );
}

// ──────────────────────────────────────────────────────────────────────
// Test 2 — idle gap → EMA drops on next append.
//
// Seed rate high, then sleep > 1 s, then do ONE append. The new
// instantaneous rate is ~1 Hz; with α=0.3 the EMA lands between seed
// and 1 Hz. Assert the next-append EMA is strictly lower than the
// pre-idle EMA.
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn idle_gap_lowers_ema() {
    let f = DynMaxlenFixture::setup().await;

    let ttl_ms: u32 = 30_000;
    let floor: u32 = 64;
    let ceiling: u32 = 16_384;
    let alpha: f64 = 0.3; // stronger decay so the test is decisive

    // Ramp up: 64 back-to-back appends to drive the EMA high.
    for i in 0..64 {
        let _ = f
            .append_best_effort(&format!("fast-{i}"), ttl_ms, floor, ceiling, alpha)
            .await;
    }
    let ema_hot: f64 = f.read_meta_field("ema_rate_hz").await.parse().unwrap_or(0.0);

    // Idle for 2 s so the next instantaneous rate is 0.5 Hz.
    tokio::time::sleep(std::time::Duration::from_millis(2_000)).await;

    let _ = f
        .append_best_effort("post-idle", ttl_ms, floor, ceiling, alpha)
        .await;
    let ema_cold: f64 = f.read_meta_field("ema_rate_hz").await.parse().unwrap_or(0.0);

    assert!(
        ema_cold < ema_hot,
        "EMA must drop after an idle period (hot={ema_hot:.3} Hz, cold={ema_cold:.3} Hz)"
    );
}

// ──────────────────────────────────────────────────────────────────────
// Test 3 — ceiling clamp.
//
// Drive the EMA to a high Hz with a tight ceiling. Assert
// maxlen_applied_last ≤ ceiling, and at a very-high rate it pegs at
// the ceiling.
// ──────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn ceiling_clamp_pegs_maxlen() {
    let f = DynMaxlenFixture::setup().await;

    let ttl_ms: u32 = 30_000;
    let floor: u32 = 64;
    // Tight ceiling — at in-burst fcall rates (~multi-kHz on loopback)
    // the uncapped K would blow past this easily.
    let ceiling: u32 = 256;
    let alpha: f64 = 0.3;

    for i in 0..512 {
        let _ = f
            .append_best_effort(&format!("hot-{i}"), ttl_ms, floor, ceiling, alpha)
            .await;
    }

    let maxlen: u64 = f
        .read_meta_field("maxlen_applied_last")
        .await
        .parse()
        .unwrap_or(0);

    assert!(
        maxlen <= ceiling as u64,
        "maxlen_applied_last ({maxlen}) must never exceed the ceiling ({ceiling})"
    );
    assert_eq!(
        maxlen, ceiling as u64,
        "at sustained high fcall rate the clamp MUST peg the MAXLEN at the ceiling"
    );
}
