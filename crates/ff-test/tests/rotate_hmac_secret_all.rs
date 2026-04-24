//! Integration test for
//! [`ff_core::engine_backend::EngineBackend::rotate_waitpoint_hmac_secret_all`]
//! on the Valkey-backed impl (v0.7 migration-master Q4).
//!
//! Proves the new cluster-wide rotation method installs the fresh kid
//! across EVERY execution partition by:
//!
//! 1. Bootstrapping each of the 4 test partitions with `kid-a`.
//! 2. Invoking `rotate_waitpoint_hmac_secret_all` with `kid-b`.
//! 3. Reading back each partition's keystore via
//!    `ff_list_waitpoint_hmac_kids` and asserting `current_kid ==
//!    "kid-b"` with `kid-a` surfaced as the (grace-windowed)
//!    verifying kid.
//!
//! Run with:
//!   cargo test -p ff-test --test rotate_hmac_secret_all -- --test-threads=1

use ff_core::contracts::{
    ListWaitpointHmacKidsArgs, RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretArgs,
    RotateWaitpointHmacSecretOutcome,
};
use ff_core::engine_backend::EngineBackend;
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_script::functions::suspension::{
    ff_list_waitpoint_hmac_kids, ff_rotate_waitpoint_hmac_secret,
};
use ff_test::fixtures::TestCluster;

// Fixture secrets — public, test-only values. See
// waitpoint_hmac_rotation_fcall.rs for the CodeQL rationale note.
// codeql[rust/cleartext-logging-sensitive-data]
const SECRET_A: &str = "11111111111111111111111111111111";
// codeql[rust/cleartext-logging-sensitive-data]
const SECRET_B: &str = "22222222222222222222222222222222";
const GRACE_MS: u64 = 60_000;

fn partition(index: u16) -> Partition {
    Partition {
        family: PartitionFamily::Execution,
        index,
    }
}

async fn clear_partition(tc: &TestCluster, p: &Partition) {
    let idx = IndexKeys::new(p);
    let key = idx.waitpoint_hmac_secrets();
    let _: Option<i64> = tc
        .client()
        .cmd("DEL")
        .arg(key.as_str())
        .execute()
        .await
        .expect("DEL during test cleanup must succeed");
}

#[tokio::test]
#[serial_test::serial]
async fn rotate_all_fans_out_across_every_partition() {
    let tc = TestCluster::connect().await;
    let cfg = *tc.partition_config();
    let num = cfg.num_flow_partitions;
    assert!(
        num >= 2,
        "test needs >=2 partitions to prove fan-out (got {num})"
    );

    // 1. Cleanup + bootstrap every partition with kid-a so the rotation
    //    surfaces kid-a as the verifying kid on every partition.
    for i in 0..num {
        let p = partition(i);
        clear_partition(&tc, &p).await;
        let idx = IndexKeys::new(&p);
        ff_rotate_waitpoint_hmac_secret(
            tc.client(),
            &idx,
            &RotateWaitpointHmacSecretArgs {
                new_kid: "kid-a".to_owned(),
                new_secret_hex: SECRET_A.to_owned(),
                grace_ms: GRACE_MS,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("bootstrap partition {i}: {e}"));
    }

    // 2. Invoke the new trait method via ValkeyBackend (the Arc<dyn
    //    EngineBackend> is the same shape ff-sdk FlowFabricWorker
    //    forwards through).
    let backend: std::sync::Arc<dyn EngineBackend> =
        ff_backend_valkey::ValkeyBackend::from_client_and_partitions(tc.client().clone(), cfg);
    let result = backend
        .rotate_waitpoint_hmac_secret_all(RotateWaitpointHmacSecretAllArgs::new(
            "kid-b", SECRET_B, GRACE_MS,
        ))
        .await
        .expect("rotate_waitpoint_hmac_secret_all must succeed");

    // 3a. One entry per partition, all Ok, all Rotated with previous =
    //     Some("kid-a").
    assert_eq!(result.entries.len(), num as usize);
    for (i, entry) in result.entries.iter().enumerate() {
        assert_eq!(entry.partition, i as u16);
        let outcome = entry
            .result
            .as_ref()
            .unwrap_or_else(|e| panic!("partition {i} rotation failed: {e}"));
        match outcome {
            RotateWaitpointHmacSecretOutcome::Rotated {
                previous_kid,
                new_kid,
                gc_count: _,
            } => {
                assert_eq!(previous_kid.as_deref(), Some("kid-a"));
                assert_eq!(new_kid, "kid-b");
            }
            other => panic!("partition {i}: expected Rotated, got {other:?}"),
        }
    }

    // 3b. Read-back: every partition's keystore now has kid-b as
    //     current_kid + kid-a in verifying.
    for i in 0..num {
        let p = partition(i);
        let idx = IndexKeys::new(&p);
        let listing = ff_list_waitpoint_hmac_kids(tc.client(), &idx, &ListWaitpointHmacKidsArgs {})
            .await
            .unwrap_or_else(|e| panic!("list partition {i}: {e}"));
        assert_eq!(
            listing.current_kid.as_deref(),
            Some("kid-b"),
            "partition {i}: current_kid"
        );
        assert_eq!(
            listing.verifying.len(),
            1,
            "partition {i}: exactly one verifying kid"
        );
        assert_eq!(
            listing.verifying[0].kid, "kid-a",
            "partition {i}: verifying kid name"
        );
    }

    // 4. Cleanup — other tests share the keyspace so leave no state.
    for i in 0..num {
        let p = partition(i);
        clear_partition(&tc, &p).await;
    }
}
