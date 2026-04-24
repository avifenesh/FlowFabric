//! Regression: `EngineBackend::read_summary` must decode the RESP3
//! `Value::Map` shape returned by `HGETALL` on Valkey 7.2.
//!
//! v0.6.0 (`ff-backend-valkey::read_summary_impl`) matched only
//! `Value::Array`, so every call returned `Ok(None)` on RESP3 — the
//! ferriskey-default protocol. This test writes a `DurableSummary`
//! delta via the same `ff_append_frame` FCALL the production path
//! uses, then reads it back through the backend trait. It must return
//! the written document, not `None`.
//!
//! Run with:
//!   cargo test -p ff-test --test read_summary_resp3 -- --test-threads=1

use std::sync::Arc;

use ferriskey::Value;
use ff_backend_valkey::ValkeyBackend;
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::ExecKeyContext;
use ff_core::partition::execution_partition;
use ff_core::types::*;
use ff_test::fixtures::TestCluster;

fn build_backend(tc: &TestCluster) -> Arc<dyn EngineBackend> {
    ValkeyBackend::from_client_and_partitions(
        tc.client().clone(),
        ff_test::fixtures::TEST_PARTITION_CONFIG,
    )
}

/// Lean attempt seed — mirrors the fixture used by
/// `engine_backend_stream_modes.rs` but kept local so this test can
/// compile standalone.
struct SeededAttempt {
    tc: TestCluster,
    eid: ExecutionId,
    attempt_index: u32,
    attempt_id: String,
    lease_id: String,
    lease_epoch: String,
}

impl SeededAttempt {
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

    async fn append_summary_delta(&self, payload: &str) -> Value {
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
            "summary_token".to_owned(),
            now.to_string(),
            payload.to_owned(),
            "utf8".to_owned(),
            String::new(),
            "worker".to_owned(),
            "0".to_owned(),
            self.attempt_id.clone(),
            "65536".to_owned(),
            "summary".to_owned(),
            "json-merge-patch".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
            "0".to_owned(),
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

/// Regression for v0.6.0 read_summary RESP3 Map decode bug.
///
/// Pre-fix: ferriskey's RESP3 client made `HGETALL` return
/// `Value::Map`, but `read_summary_impl` matched only `Value::Array`,
/// so this test would see `Ok(None)` after a successful delta write.
/// Post-fix: the decoder accepts both shapes and this returns the
/// written document.
#[tokio::test]
#[serial_test::serial]
async fn read_summary_decodes_resp3_map() {
    let f = SeededAttempt::setup().await;

    // Write one summary delta via the production FCALL path.
    let _ = f.append_summary_delta(r#"{"hello":"resp3","n":7}"#).await;

    // Read back via the trait — this is the path that broke on v0.6.0.
    let backend = build_backend(&f.tc);
    let attempt_index = AttemptIndex::new(f.attempt_index);
    let doc = backend
        .read_summary(&f.eid, attempt_index)
        .await
        .expect("read_summary call succeeded");

    let doc = doc.expect(
        "read_summary must return Some(..) after a delta write; \
         Ok(None) here means the RESP3 Map regression is back",
    );

    assert_eq!(doc.version, 1, "version == 1 after first delta");

    let parsed: serde_json::Value = serde_json::from_slice(&doc.document_json)
        .expect("summary document is valid JSON");
    assert_eq!(parsed.get("hello").and_then(|v| v.as_str()), Some("resp3"));
    assert_eq!(parsed.get("n").and_then(|v| v.as_i64()), Some(7));
}
