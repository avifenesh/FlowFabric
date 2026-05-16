//! Regenerate the cross-language test corpus consumed by
//! `flowfabric-python/tests/test_cross_lang.py`.
//!
//! The corpus pins the byte-for-byte output of:
//!   * `ff_core::crypto::hmac::hmac_sign` for a fixed set of
//!     (secret, kid, message) triples.
//!   * `ff_core::keys::*` for a fixed set of namespace / lane /
//!     execution-id inputs.
//!
//! Python verifies that `flowfabric.hmac.sign` and
//! `flowfabric.keys.*` reproduce the same outputs. Any future
//! change that breaks the cross-language contract (wire format,
//! key shape, hash domain) trips the Python test next CI run.
//!
//! Run with:
//!   cargo run -p ff-client --example gen_cross_lang_corpus \
//!     > ../flowfabric-python/tests/cross_lang_corpus.json
//!
//! The path-dep on a sibling checkout assumes the layout described
//! in flowfabric-python's README.

use std::collections::BTreeMap;

use ff_core::crypto::hmac::hmac_sign;
use ff_core::keys::{
    ExecKeyContext, global_config_partitions, idempotency_key, lane_config_key,
    lane_counts_key, namespace_executions_key,
};
use ff_core::partition::{Partition, PartitionFamily};
use ff_core::types::{ExecutionId, LaneId, Namespace};

fn hex(bytes: &[u8]) -> String {
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut s, b| {
            s.push_str(&format!("{b:02x}"));
            s
        })
}

fn main() {
    // ── HMAC vectors ──
    // Inputs are stable; outputs are recomputed by ff_core on every
    // regeneration. Hex-encode all bytes so the JSON stays
    // ascii-clean.
    let hmac_cases: Vec<(&[u8], &str, &[u8])> = vec![
        (b"secret-1", "kid-a", b"hello"),
        (b"super-secret-rotating-key-v1", "vllm-demo-2026-05", b"exec=abc:waitpoint=wp-42"),
        // Empty message — verifies the empty-input edge.
        (b"k", "k0", b""),
        // 64-byte secret = HMAC-SHA256 inner block size; specific
        // edge in the algorithm.
        (&[0xaa_u8; 64], "block-sized-secret", b"message"),
        // Non-ASCII bytes in message to confirm we're not stringifying
        // somewhere we shouldn't be.
        (b"k", "binary", &[0x00, 0x01, 0xfe, 0xff]),
    ];

    let mut hmac_out = Vec::new();
    for (secret, kid, message) in hmac_cases {
        let token = hmac_sign(secret, kid, message);
        hmac_out.push(serde_json::json!({
            "secret_hex": hex(secret),
            "kid": kid,
            "message_hex": hex(message),
            "expected_token": token,
        }));
    }

    // ── Free-function keys ──
    let mut free_keys = BTreeMap::new();
    free_keys.insert("global_config_partitions".to_string(), serde_json::json!([
        {"args": {}, "expected": global_config_partitions()},
    ]));

    let mut ns_cases = Vec::new();
    for ns in ["acme", "tenant_1", "ns-with-dashes"] {
        ns_cases.push(serde_json::json!({
            "args": {"namespace": ns},
            "expected": namespace_executions_key(ns),
        }));
    }
    free_keys.insert("namespace_executions_key".to_string(), serde_json::json!(ns_cases));

    let mut idem_cases = Vec::new();
    for (tag, ns, k) in [
        ("exec", "acme", "key-1"),
        ("exec", "acme", "key-with-dash"),
    ] {
        idem_cases.push(serde_json::json!({
            "args": {"tag": tag, "namespace": ns, "idem_key": k},
            "expected": idempotency_key(tag, ns, k),
        }));
    }
    free_keys.insert("idempotency_key".to_string(), serde_json::json!(idem_cases));

    let mut lane_cfg = Vec::new();
    let mut lane_cnt = Vec::new();
    for lane_str in ["default", "hot", "lane-2"] {
        let lane = LaneId::new(lane_str);
        lane_cfg.push(serde_json::json!({
            "args": {"lane": lane_str},
            "expected": lane_config_key(&lane),
        }));
        lane_cnt.push(serde_json::json!({
            "args": {"lane": lane_str},
            "expected": lane_counts_key(&lane),
        }));
    }
    free_keys.insert("lane_config_key".to_string(), serde_json::json!(lane_cfg));
    free_keys.insert("lane_counts_key".to_string(), serde_json::json!(lane_cnt));

    // ── ExecKeys (partition-scoped) ──
    // Deterministic execution_ids — we don't want UUID churn in the
    // corpus. Mint a fixed `{fp:N}:UUID` literal directly via
    // `ExecutionId::parse`.
    let exec_cases: Vec<&str> = vec![
        "{fp:0}:00000000-0000-4000-8000-000000000000",
        "{fp:42}:11111111-2222-4333-8444-555555555555",
        "{fp:255}:ffffffff-ffff-4fff-8fff-ffffffffffff",
    ];
    let mut exec_keys = Vec::new();
    for eid_str in exec_cases {
        let eid = ExecutionId::parse(eid_str).expect("test vector must parse");
        let partition = Partition {
            family: PartitionFamily::Execution,
            index: eid.partition(),
        };
        let ctx = ExecKeyContext::new(&partition, &eid);
        exec_keys.push(serde_json::json!({
            "execution_id": eid_str,
            "keys": {
                "core": ctx.core(),
                "payload": ctx.payload(),
                "result": ctx.result(),
                "tags": ctx.tags(),
                "attempts": ctx.attempts(),
                "exec_signals": ctx.exec_signals(),
                "waitpoints": ctx.waitpoints(),
            },
        }));
    }

    // ── Namespace round-trip (unused in tests today but cheap to
    // pin in case future changes affect Namespace's string shape) ──
    let ns_repr: Vec<serde_json::Value> = ["acme", "tenant_1"]
        .into_iter()
        .map(|n| serde_json::json!({"input": n, "as_str": Namespace::new(n).as_str()}))
        .collect();

    let corpus = serde_json::json!({
        "schema_version": 1,
        "produced_by": "ff-client/examples/gen_cross_lang_corpus.rs (ff_core)",
        "hmac": hmac_out,
        "keys": {
            "free_functions": free_keys,
            "exec_keys": exec_keys,
        },
        "namespace_round_trip": ns_repr,
    });

    println!("{}", serde_json::to_string_pretty(&corpus).expect("serialize corpus"));
}
