//! RFC-015 stream-durability-mode integration tests.
//!
//! Covers the eight scenarios enumerated in the implementation plan:
//! 1. Durable happy path (regression).
//! 2. DurableSummary delta-apply (JSON Merge Patch + version monotonicity).
//! 3. Null-sentinel round-trip (`"__ff_null__"` → JSON null on read).
//! 4. DurableSummary + Durable mixed-mode on one stream.
//! 5. BestEffortLive MAXLEN + TTL refresh.
//! 6. Mixed-mode PERSIST-on-flip (best-effort TTL doesn't destroy durable).
//! 7. `ExcludeBestEffort` tail-filter (server-side mode-field filter).
//! 8. Terminal-marker-in-attempt-metadata vs stream-echo (RFC-015 §7).
//!
//! Run with:
//!   cargo test -p ff-test --test engine_backend_stream_modes -- \
//!       --test-threads=1
//!
//! Requires a live Valkey server (see `TestCluster::connect`).

use ferriskey::Value;
use ff_core::keys::ExecKeyContext;
use ff_core::partition::execution_partition;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

// ─────────────────────────────────────────────────────────────────────
// Helpers — lean seed of the exec_core + attempt state so
// `ff_append_frame`'s HMGET lease check passes. Avoids the full
// create/claim dance (~12 FCALLs); the stream-durability tests only
// exercise the append + read paths.
// ─────────────────────────────────────────────────────────────────────

struct AttemptFixture {
    tc: TestCluster,
    eid: ExecutionId,
    attempt_index: u32,
    attempt_id: String,
    lease_id: String,
    lease_epoch: String,
}

impl AttemptFixture {
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

        // Minimal exec_core state that satisfies the HMGET check in
        // ff_append_frame — only the fields it reads are required.
        let core_key = ctx.core();
        let fields: Vec<(&str, String)> = vec![
            ("current_attempt_index", attempt_index.to_string()),
            ("current_lease_id", lease_id.clone()),
            ("current_lease_epoch", lease_epoch.clone()),
            ("lease_expires_at", (now_ms + 30_000).to_string()),
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

    async fn append_frame(
        &self,
        frame_type: &str,
        payload: &str,
        stream_mode: &str,
        patch_kind: &str,
        ttl_ms: u32,
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
            frame_type.to_owned(),
            now.to_string(),
            payload.to_owned(),
            "utf8".to_owned(),
            String::new(), // correlation_id
            "worker".to_owned(),
            "0".to_owned(), // retention_maxlen
            self.attempt_id.clone(),
            "65536".to_owned(),
            stream_mode.to_owned(),
            patch_kind.to_owned(),
            ttl_ms.to_string(),
        ];
        let key_refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let arg_refs: Vec<&str> = args.iter().map(String::as_str).collect();
        self.tc
            .client()
            .fcall("ff_append_frame", &key_refs, &arg_refs)
            .await
            .expect("FCALL ff_append_frame failed")
    }
}

/// Parse an `ok(...)` Lua result into the field vec. Fails if the
/// status byte is 0 (error). The Lua `ok(...)` helper returns
/// `{1, "OK", ...fields}`, so the caller-visible fields start at
/// index 2 — drop `status` + the `"OK"` label before returning.
fn parse_ok(raw: &Value) -> Vec<String> {
    let arr = match raw {
        Value::Array(a) => a,
        _ => panic!("expected Array, got {raw:?}"),
    };
    let status = match arr.first() {
        Some(Ok(Value::Int(n))) => *n,
        other => panic!("expected Int status, got {other:?}"),
    };
    if status != 1 {
        panic!("FCALL returned error status: {arr:?}");
    }
    arr.iter()
        .skip(2)
        .map(|v| match v {
            Ok(Value::BulkString(b)) => String::from_utf8_lossy(b).into_owned(),
            Ok(Value::SimpleString(s)) => s.clone(),
            Ok(Value::Int(n)) => n.to_string(),
            _ => String::new(),
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// 1. Durable happy path — regression vs pre-RFC-015 behaviour.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn durable_mode_regression() {
    let f = AttemptFixture::setup().await;

    let raw = f.append_frame("log", r#"{"a":1}"#, "durable", "", 0).await;
    let fields = parse_ok(&raw);
    // ok(entry_id, frame_count, summary_version=""): summary_version is
    // empty for Durable.
    assert!(!fields[0].is_empty(), "entry_id should be present");
    assert_eq!(fields[1], "1");
    assert_eq!(fields.get(2).map(String::as_str), Some(""));

    // Stream entry carries `mode=durable`.
    let ctx = f.ctx();
    let stream_key = ctx.stream(AttemptIndex::new(f.attempt_index));
    let raw_xrange: Value = f
        .tc
        .client()
        .cmd("XRANGE")
        .arg(&stream_key)
        .arg("-")
        .arg("+")
        .execute()
        .await
        .expect("XRANGE");
    let rendered = format!("{raw_xrange:?}");
    // The Debug formatter on RESP3 Value escapes inner quotes; match on
    // the bare substrings "mode" and "durable" which appear in both the
    // RESP2 array and RESP3 map shapes.
    assert!(
        rendered.contains("mode") && rendered.contains("durable"),
        "durable XADD entry must carry mode=durable: {rendered}"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 2. DurableSummary delta-apply — two patches, check version monotonic
//    and merged document shape.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn durable_summary_delta_apply_and_version_monotonic() {
    let f = AttemptFixture::setup().await;

    // First patch: {a:1, b:{c:2}}
    let raw1 = f
        .append_frame(
            "summary_token",
            r#"{"a":1,"b":{"c":2}}"#,
            "summary",
            "json-merge-patch",
            0,
        )
        .await;
    let fields1 = parse_ok(&raw1);
    assert_eq!(fields1[1], "1", "frame_count after first append");
    assert_eq!(fields1[2], "1", "summary_version = 1 on first delta");

    // Second patch: {b:{d:3}, a:null} → a deleted, b.c=2 kept, b.d=3 added
    let raw2 = f
        .append_frame(
            "summary_token",
            r#"{"a":null,"b":{"d":3}}"#,
            "summary",
            "json-merge-patch",
            0,
        )
        .await;
    let fields2 = parse_ok(&raw2);
    assert_eq!(fields2[1], "2");
    assert_eq!(fields2[2], "2", "summary_version monotonic → 2");

    // Read summary document via ff_read_summary.
    let ctx = f.ctx();
    let summary_key = ctx.stream_summary(AttemptIndex::new(f.attempt_index));
    let raw_rs: Value = f
        .tc
        .client()
        .fcall::<Value>("ff_read_summary", &[summary_key.as_str()], &[] as &[&str])
        .await
        .expect("FCALL ff_read_summary");
    let rs_fields = parse_ok(&raw_rs);
    // (document, version, patch_kind, last_updated_ms, first_applied_ms)
    let document = &rs_fields[0];
    assert_eq!(rs_fields[1], "2");
    assert_eq!(rs_fields[2], "json-merge-patch");

    let parsed: serde_json::Value =
        serde_json::from_str(document).expect("summary document must be valid JSON");
    // Per RFC 7396: `a:null` in patch 2 deletes the `a` key.
    assert!(
        parsed.get("a").is_none(),
        "`a` should be deleted by `null` patch; got {parsed}"
    );
    // `b.c` preserved from patch 1, `b.d` added from patch 2.
    let b = parsed.get("b").expect("b object preserved");
    assert_eq!(b.get("c").and_then(|v| v.as_i64()), Some(2));
    assert_eq!(b.get("d").and_then(|v| v.as_i64()), Some(3));
}

// ─────────────────────────────────────────────────────────────────────
// 3. Null-sentinel round-trip — caller encodes explicit JSON null via
//    "__ff_null__"; the read-side document contains JSON null (not the
//    sentinel string).
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn durable_summary_null_sentinel_round_trip() {
    let f = AttemptFixture::setup().await;

    let patch = format!(r#"{{"x":"{}"}}"#, ff_core::backend::SUMMARY_NULL_SENTINEL);
    let _ = f
        .append_frame("summary_token", &patch, "summary", "json-merge-patch", 0)
        .await;

    let ctx = f.ctx();
    let summary_key = ctx.stream_summary(AttemptIndex::new(f.attempt_index));
    let raw_rs: Value = f
        .tc
        .client()
        .fcall::<Value>("ff_read_summary", &[summary_key.as_str()], &[] as &[&str])
        .await
        .expect("FCALL ff_read_summary");
    let rs_fields = parse_ok(&raw_rs);
    let document = &rs_fields[0];
    assert!(
        !document.contains(ff_core::backend::SUMMARY_NULL_SENTINEL),
        "round-trip invariant: summary doc never contains the sentinel; got {document}"
    );
    let parsed: serde_json::Value = serde_json::from_str(document).expect("valid JSON");
    assert!(parsed.get("x").map(|v| v.is_null()).unwrap_or(false),
        "x must be JSON null, got {parsed}");
}

// ─────────────────────────────────────────────────────────────────────
// 4. Mixed-mode: Durable + DurableSummary on the same stream. Both
//    entries present in XLEN / XRANGE; summary Hash only reflects
//    summary-mode appends.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn mixed_durable_and_summary_interleave() {
    let f = AttemptFixture::setup().await;

    let _ = f.append_frame("log", r#"{"msg":"start"}"#, "durable", "", 0).await;
    let _ = f
        .append_frame(
            "summary_token",
            r#"{"tokens":1}"#,
            "summary",
            "json-merge-patch",
            0,
        )
        .await;
    let _ = f.append_frame("log", r#"{"msg":"mid"}"#, "durable", "", 0).await;
    let _ = f
        .append_frame(
            "summary_token",
            r#"{"tokens":2}"#,
            "summary",
            "json-merge-patch",
            0,
        )
        .await;

    let ctx = f.ctx();
    let stream_key = ctx.stream(AttemptIndex::new(f.attempt_index));
    let xlen: i64 = f
        .tc
        .client()
        .cmd("XLEN")
        .arg(&stream_key)
        .execute()
        .await
        .expect("XLEN");
    assert_eq!(xlen, 4, "both Durable and DurableSummary XADD entries are present");

    // Summary Hash reflects only the 2 summary-mode appends.
    let summary_key = ctx.stream_summary(AttemptIndex::new(f.attempt_index));
    let ver: String = f
        .tc
        .client()
        .cmd("HGET")
        .arg(&summary_key)
        .arg("version")
        .execute()
        .await
        .expect("HGET version");
    assert_eq!(ver, "2");
}

// ─────────────────────────────────────────────────────────────────────
// 5. BestEffortLive TTL model — PEXPIRE is set when the stream has
//    never received a durable frame.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn best_effort_live_sets_pexpire_on_pure_stream() {
    let f = AttemptFixture::setup().await;

    let _ = f
        .append_frame("delta", "token-a", "best_effort", "", 5_000)
        .await;
    let _ = f
        .append_frame("delta", "token-b", "best_effort", "", 5_000)
        .await;

    let ctx = f.ctx();
    let stream_key = ctx.stream(AttemptIndex::new(f.attempt_index));
    let pttl: i64 = f
        .tc
        .client()
        .cmd("PTTL")
        .arg(&stream_key)
        .execute()
        .await
        .expect("PTTL");
    // Per RFC-015 §4.1: PEXPIRE = ttl_ms * 2 on best-effort-only streams.
    // Valkey returns -2 if key missing, -1 if no TTL, else ms remaining.
    assert!(
        pttl > 0,
        "best-effort-only stream should carry a PEXPIRE (expected > 0, got {pttl})"
    );
    assert!(
        pttl <= 10_000,
        "PEXPIRE should not exceed ttl_ms*2 = 10_000 (got {pttl})"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 6. Mixed-mode PERSIST-on-flip — a durable append on a previously
//    best-effort-only stream MUST PERSIST the key (RFC-015 §4.1). Best-
//    effort TTL must not destroy durable content.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn best_effort_flip_to_durable_persists_key() {
    let f = AttemptFixture::setup().await;

    let _ = f
        .append_frame("delta", "token-a", "best_effort", "", 5_000)
        .await;
    let ctx = f.ctx();
    let stream_key = ctx.stream(AttemptIndex::new(f.attempt_index));
    let pttl_before: i64 = f
        .tc
        .client()
        .cmd("PTTL")
        .arg(&stream_key)
        .execute()
        .await
        .expect("PTTL before");
    assert!(pttl_before > 0, "best-effort append must have set a TTL");

    // First durable append → PERSIST the key.
    let _ = f.append_frame("log", r#"{"msg":"durable"}"#, "durable", "", 0).await;
    let pttl_after: i64 = f
        .tc
        .client()
        .cmd("PTTL")
        .arg(&stream_key)
        .execute()
        .await
        .expect("PTTL after");
    // -1 signals "key exists, no TTL" (PERSIST'd).
    assert_eq!(
        pttl_after, -1,
        "durable append must PERSIST the stream key (got PTTL = {pttl_after})"
    );

    // Subsequent best-effort appends MUST NOT re-set the TTL.
    let _ = f
        .append_frame("delta", "token-b", "best_effort", "", 5_000)
        .await;
    let pttl_final: i64 = f
        .tc
        .client()
        .cmd("PTTL")
        .arg(&stream_key)
        .execute()
        .await
        .expect("PTTL final");
    assert_eq!(
        pttl_final, -1,
        "best-effort append on a stream with has_durable_frame=1 must \
         leave the key PERSIST'd (got PTTL = {pttl_final})"
    );
}

// ─────────────────────────────────────────────────────────────────────
// 7. ExcludeBestEffort tail filter — server-side `mode` field filter
//    drops best-effort entries from the read.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn exclude_best_effort_tail_filter() {
    let f = AttemptFixture::setup().await;

    let _ = f.append_frame("log", r#"{"m":"d1"}"#, "durable", "", 0).await;
    let _ = f.append_frame("delta", "be-a", "best_effort", "", 5_000).await;
    let _ = f
        .append_frame(
            "summary_token",
            r#"{"tokens":1}"#,
            "summary",
            "json-merge-patch",
            0,
        )
        .await;
    let _ = f.append_frame("delta", "be-b", "best_effort", "", 5_000).await;

    let ctx = f.ctx();
    let stream_key = ctx.stream(AttemptIndex::new(f.attempt_index));
    let stream_meta = ctx.stream_meta(AttemptIndex::new(f.attempt_index));

    // Exercise the Lua-side filter via ff_read_attempt_stream directly.
    // ARGV(4): from_id, to_id, count_limit, visibility.
    let raw: Value = f
        .tc
        .client()
        .fcall(
            "ff_read_attempt_stream",
            &[stream_key.as_str(), stream_meta.as_str()],
            &["-", "+", "100", "exclude_best_effort"],
        )
        .await
        .expect("FCALL ff_read_attempt_stream");
    // parse_ok gives (entries, closed_at, closed_reason). Entries is a
    // nested Array — count via raw-format introspection on the rendered
    // debug string.
    let arr = match &raw {
        Value::Array(a) => a,
        _ => panic!("expected Array"),
    };
    // Status byte + entries slot
    assert!(matches!(arr.first(), Some(Ok(Value::Int(1)))));
    let entries_slot = arr.get(2).expect("entries slot");
    let entries_count = match entries_slot {
        Ok(Value::Array(inner)) => inner.len(),
        _ => panic!("entries should be an Array, got {entries_slot:?}"),
    };
    // Expect 2 entries (durable + summary) — the 2 best-effort entries
    // were filtered out server-side.
    assert_eq!(
        entries_count, 2,
        "exclude_best_effort should drop best-effort entries; kept {entries_count}"
    );

    // Sanity: the unfiltered read returns all 4.
    let raw_all: Value = f
        .tc
        .client()
        .fcall(
            "ff_read_attempt_stream",
            &[stream_key.as_str(), stream_meta.as_str()],
            &["-", "+", "100", ""],
        )
        .await
        .expect("FCALL ff_read_attempt_stream (no filter)");
    let arr_all = match &raw_all {
        Value::Array(a) => a,
        _ => panic!("expected Array"),
    };
    let entries_all = match arr_all.get(2).unwrap() {
        Ok(Value::Array(inner)) => inner.len(),
        _ => panic!("entries must be Array"),
    };
    assert_eq!(entries_all, 4, "unfiltered read returns every XADD entry");
}

// ─────────────────────────────────────────────────────────────────────
// 8. Terminal semantics (RFC-015 §7) — the canonical terminal signal
//    lives on the attempt metadata Hash; the stream-side marker is a
//    convenience echo. Validate that `read_summary` works after a
//    simulated terminal and that the summary Hash survives an
//    aggressive MAXLEN trim on the stream.
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn terminal_summary_survives_maxlen_trim() {
    let f = AttemptFixture::setup().await;

    // Build a summary under DurableSummary mode.
    let _ = f
        .append_frame(
            "summary_token",
            r#"{"final":"answer","tokens":42}"#,
            "summary",
            "json-merge-patch",
            0,
        )
        .await;

    // Simulate the aggressive-trim scenario: XTRIM the stream to 0
    // (every XADD rolled off). The summary Hash must survive.
    let ctx = f.ctx();
    let stream_key = ctx.stream(AttemptIndex::new(f.attempt_index));
    let _: i64 = f
        .tc
        .client()
        .cmd("XTRIM")
        .arg(&stream_key)
        .arg("MAXLEN")
        .arg("0")
        .execute()
        .await
        .expect("XTRIM");
    let xlen: i64 = f
        .tc
        .client()
        .cmd("XLEN")
        .arg(&stream_key)
        .execute()
        .await
        .expect("XLEN");
    assert_eq!(xlen, 0, "stream was aggressively trimmed");

    // Summary read still returns the full document.
    let summary_key = ctx.stream_summary(AttemptIndex::new(f.attempt_index));
    let raw_rs: Value = f
        .tc
        .client()
        .fcall::<Value>("ff_read_summary", &[summary_key.as_str()], &[] as &[&str])
        .await
        .expect("FCALL ff_read_summary");
    let rs_fields = parse_ok(&raw_rs);
    assert_eq!(rs_fields[1], "1", "version preserved through trim");
    let parsed: serde_json::Value =
        serde_json::from_str(&rs_fields[0]).expect("valid summary JSON");
    assert_eq!(
        parsed.get("final").and_then(|v| v.as_str()),
        Some("answer")
    );
    assert_eq!(parsed.get("tokens").and_then(|v| v.as_i64()), Some(42));
}

// ─────────────────────────────────────────────────────────────────────
// read_summary on an attempt with no summary appends returns the
// empty-fields shape (no DurableSummary frame ever appended).
// ─────────────────────────────────────────────────────────────────────

#[tokio::test]
#[serial_test::serial]
async fn read_summary_absent_returns_empty() {
    let f = AttemptFixture::setup().await;
    let ctx = f.ctx();
    let summary_key = ctx.stream_summary(AttemptIndex::new(f.attempt_index));
    let raw_rs: Value = f
        .tc
        .client()
        .fcall::<Value>("ff_read_summary", &[summary_key.as_str()], &[] as &[&str])
        .await
        .expect("FCALL ff_read_summary on absent");
    let rs_fields = parse_ok(&raw_rs);
    // Empty-fields shape per Lua: document="", version="0".
    assert_eq!(rs_fields[0], "");
    assert_eq!(rs_fields[1], "0");
}
