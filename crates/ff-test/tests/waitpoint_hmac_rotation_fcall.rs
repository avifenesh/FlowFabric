//! Direct FCALL tests for `ff_rotate_waitpoint_hmac_secret` and
//! `ff_list_waitpoint_hmac_kids` (issue #49).
//!
//! These exercise the Lua FCALL surface that cairn-rs and other
//! direct-Valkey consumers invoke. The HTTP-level coverage in
//! `admin_rotate_api.rs` already hits the happy path through the
//! fan-out; this file focuses on behaviors unique to the FCALL:
//! idempotent replay, rotation conflict, orphan GC, list shape,
//! input validation.
//!
//! Run with: cargo test -p ff-test --test waitpoint_hmac_rotation_fcall -- --test-threads=1

use ff_core::contracts::{
    ListWaitpointHmacKidsArgs, RotateWaitpointHmacSecretArgs,
    RotateWaitpointHmacSecretOutcome, WaitpointHmacKids,
};
use ff_core::keys::IndexKeys;
use ff_core::partition::{Partition, PartitionFamily};
use ff_script::error::ScriptError;
use ff_script::functions::suspension::{
    ff_list_waitpoint_hmac_kids, ff_rotate_waitpoint_hmac_secret,
};
use ff_test::fixtures::TestCluster;

// Non-sensitive hex fixtures. Fresh per-test isolation is via a unique
// partition per test (ephemeral index the Rust side never reads), so
// there is no cross-test bleed even when they run serially.
const SECRET_A: &str = "11111111111111111111111111111111";
const SECRET_B: &str = "22222222222222222222222222222222";
const SECRET_C: &str = "33333333333333333333333333333333";

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
        .ok()
        .flatten();
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as i64
}

// Use high partition indices to avoid collision with other tests that
// target low-numbered partitions.
const TEST_PARTITION_BASE: u16 = 240;

#[tokio::test]
#[serial_test::serial]
async fn bootstrap_rotate_sets_current_no_previous() {
    let tc = TestCluster::connect().await;
    let p = partition(TEST_PARTITION_BASE);
    clear_partition(&tc, &p).await;
    let idx = IndexKeys::new(&p);

    let now = now_ms();
    let outcome = ff_rotate_waitpoint_hmac_secret(
        tc.client(),
        &idx,
        &RotateWaitpointHmacSecretArgs {
            new_kid: "kid-boot".to_owned(),
            new_secret_hex: SECRET_A.to_owned(),
            previous_expires_at_ms: now + 60_000,
            now_ms: now,
        },
    )
    .await
    .unwrap();

    match outcome {
        RotateWaitpointHmacSecretOutcome::Rotated {
            previous_kid,
            new_kid,
            gc_count,
        } => {
            assert!(previous_kid.is_none(), "bootstrap has no previous kid");
            assert_eq!(new_kid, "kid-boot");
            assert_eq!(gc_count, 0);
        }
        other => panic!("expected Rotated, got {other:?}"),
    }

    // list: current=kid-boot, 0 verifying.
    let listing = ff_list_waitpoint_hmac_kids(tc.client(), &idx, &ListWaitpointHmacKidsArgs {})
        .await
        .unwrap();
    assert_eq!(
        listing,
        WaitpointHmacKids {
            current_kid: Some("kid-boot".to_owned()),
            verifying: vec![],
        }
    );
}

#[tokio::test]
#[serial_test::serial]
async fn rotate_over_existing_promotes_previous() {
    let tc = TestCluster::connect().await;
    let p = partition(TEST_PARTITION_BASE + 1);
    clear_partition(&tc, &p).await;
    let idx = IndexKeys::new(&p);

    let t0 = now_ms();
    ff_rotate_waitpoint_hmac_secret(
        tc.client(),
        &idx,
        &RotateWaitpointHmacSecretArgs {
            new_kid: "kid-a".into(),
            new_secret_hex: SECRET_A.into(),
            previous_expires_at_ms: t0 + 60_000,
            now_ms: t0,
        },
    )
    .await
    .unwrap();

    let t1 = now_ms();
    let prev_exp = t1 + 60_000;
    let outcome = ff_rotate_waitpoint_hmac_secret(
        tc.client(),
        &idx,
        &RotateWaitpointHmacSecretArgs {
            new_kid: "kid-b".into(),
            new_secret_hex: SECRET_B.into(),
            previous_expires_at_ms: prev_exp,
            now_ms: t1,
        },
    )
    .await
    .unwrap();

    match outcome {
        RotateWaitpointHmacSecretOutcome::Rotated {
            previous_kid,
            new_kid,
            gc_count,
        } => {
            assert_eq!(previous_kid.as_deref(), Some("kid-a"));
            assert_eq!(new_kid, "kid-b");
            assert_eq!(gc_count, 0);
        }
        other => panic!("expected Rotated, got {other:?}"),
    }

    let listing = ff_list_waitpoint_hmac_kids(tc.client(), &idx, &ListWaitpointHmacKidsArgs {})
        .await
        .unwrap();
    assert_eq!(listing.current_kid.as_deref(), Some("kid-b"));
    assert_eq!(listing.verifying.len(), 1);
    assert_eq!(listing.verifying[0].kid, "kid-a");
    assert_eq!(listing.verifying[0].expires_at_ms, prev_exp);
}

#[tokio::test]
#[serial_test::serial]
async fn idempotent_replay_returns_noop() {
    let tc = TestCluster::connect().await;
    let p = partition(TEST_PARTITION_BASE + 2);
    clear_partition(&tc, &p).await;
    let idx = IndexKeys::new(&p);

    let t0 = now_ms();
    let args = RotateWaitpointHmacSecretArgs {
        new_kid: "kid-same".into(),
        new_secret_hex: SECRET_A.into(),
        previous_expires_at_ms: t0 + 60_000,
        now_ms: t0,
    };
    let first = ff_rotate_waitpoint_hmac_secret(tc.client(), &idx, &args)
        .await
        .unwrap();
    assert!(matches!(
        first,
        RotateWaitpointHmacSecretOutcome::Rotated { .. }
    ));

    let second = ff_rotate_waitpoint_hmac_secret(tc.client(), &idx, &args)
        .await
        .unwrap();
    match second {
        RotateWaitpointHmacSecretOutcome::Noop { kid } => assert_eq!(kid, "kid-same"),
        other => panic!("expected Noop, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn rotation_conflict_on_same_kid_different_secret() {
    let tc = TestCluster::connect().await;
    let p = partition(TEST_PARTITION_BASE + 3);
    clear_partition(&tc, &p).await;
    let idx = IndexKeys::new(&p);

    let t0 = now_ms();
    ff_rotate_waitpoint_hmac_secret(
        tc.client(),
        &idx,
        &RotateWaitpointHmacSecretArgs {
            new_kid: "kid-x".into(),
            new_secret_hex: SECRET_A.into(),
            previous_expires_at_ms: t0 + 60_000,
            now_ms: t0,
        },
    )
    .await
    .unwrap();

    let err = ff_rotate_waitpoint_hmac_secret(
        tc.client(),
        &idx,
        &RotateWaitpointHmacSecretArgs {
            new_kid: "kid-x".into(),
            new_secret_hex: SECRET_B.into(), // different
            previous_expires_at_ms: t0 + 60_000,
            now_ms: t0,
        },
    )
    .await
    .expect_err("conflict must surface as error");
    match err {
        ScriptError::RotationConflict(kid) => assert_eq!(kid, "kid-x"),
        other => panic!("expected RotationConflict, got {other:?}"),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn orphan_gc_reaps_expired_verifying_kids() {
    let tc = TestCluster::connect().await;
    let p = partition(TEST_PARTITION_BASE + 4);
    clear_partition(&tc, &p).await;
    let idx = IndexKeys::new(&p);

    // Plant kid-stale whose expires_at is already past at call time.
    // Rotate A → B with a generous grace so kid-a stays, and kid-stale
    // gets reaped in the same FCALL.
    let t0 = now_ms();
    ff_rotate_waitpoint_hmac_secret(
        tc.client(),
        &idx,
        &RotateWaitpointHmacSecretArgs {
            new_kid: "kid-a".into(),
            new_secret_hex: SECRET_A.into(),
            previous_expires_at_ms: t0 + 60_000,
            now_ms: t0,
        },
    )
    .await
    .unwrap();

    // Inject a stale kid directly (simulates historical grace window
    // that elapsed while the keystore was quiet).
    let key = idx.waitpoint_hmac_secrets();
    let _: i64 = tc
        .client()
        .cmd("HSET")
        .arg(key.as_str())
        .arg("secret:kid-stale")
        .arg(SECRET_C)
        .arg("expires_at:kid-stale")
        .arg("1") // long expired
        .execute()
        .await
        .unwrap();

    let t1 = now_ms();
    let outcome = ff_rotate_waitpoint_hmac_secret(
        tc.client(),
        &idx,
        &RotateWaitpointHmacSecretArgs {
            new_kid: "kid-b".into(),
            new_secret_hex: SECRET_B.into(),
            previous_expires_at_ms: t1 + 60_000,
            now_ms: t1,
        },
    )
    .await
    .unwrap();

    match outcome {
        RotateWaitpointHmacSecretOutcome::Rotated {
            previous_kid,
            new_kid,
            gc_count,
        } => {
            assert_eq!(previous_kid.as_deref(), Some("kid-a"));
            assert_eq!(new_kid, "kid-b");
            assert_eq!(gc_count, 1, "kid-stale must be reaped");
        }
        other => panic!("expected Rotated, got {other:?}"),
    }

    // Listing must not include kid-stale.
    let listing = ff_list_waitpoint_hmac_kids(tc.client(), &idx, &ListWaitpointHmacKidsArgs {})
        .await
        .unwrap();
    let kids: Vec<_> = listing.verifying.iter().map(|v| v.kid.as_str()).collect();
    assert!(!kids.contains(&"kid-stale"), "kid-stale survived GC: {kids:?}");
}

#[tokio::test]
#[serial_test::serial]
async fn list_uninitialized_is_empty() {
    let tc = TestCluster::connect().await;
    let p = partition(TEST_PARTITION_BASE + 5);
    clear_partition(&tc, &p).await;
    let idx = IndexKeys::new(&p);

    let listing = ff_list_waitpoint_hmac_kids(tc.client(), &idx, &ListWaitpointHmacKidsArgs {})
        .await
        .unwrap();
    assert_eq!(
        listing,
        WaitpointHmacKids {
            current_kid: None,
            verifying: vec![],
        }
    );
}

#[tokio::test]
#[serial_test::serial]
async fn invalid_kid_rejected() {
    let tc = TestCluster::connect().await;
    let p = partition(TEST_PARTITION_BASE + 6);
    clear_partition(&tc, &p).await;
    let idx = IndexKeys::new(&p);

    let t0 = now_ms();
    let err = ff_rotate_waitpoint_hmac_secret(
        tc.client(),
        &idx,
        &RotateWaitpointHmacSecretArgs {
            new_kid: "kid:with:colon".into(),
            new_secret_hex: SECRET_A.into(),
            previous_expires_at_ms: t0 + 60_000,
            now_ms: t0,
        },
    )
    .await
    .expect_err("colon in kid must fail");
    assert!(matches!(err, ScriptError::InvalidKid), "{err:?}");
}

#[tokio::test]
#[serial_test::serial]
async fn invalid_secret_hex_rejected() {
    let tc = TestCluster::connect().await;
    let p = partition(TEST_PARTITION_BASE + 7);
    clear_partition(&tc, &p).await;
    let idx = IndexKeys::new(&p);

    let t0 = now_ms();
    for bad in ["", "abc", "zz11", "1234567"] {
        let err = ff_rotate_waitpoint_hmac_secret(
            tc.client(),
            &idx,
            &RotateWaitpointHmacSecretArgs {
                new_kid: "kid-ok".into(),
                new_secret_hex: bad.into(),
                previous_expires_at_ms: t0 + 60_000,
                now_ms: t0,
            },
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, ScriptError::InvalidSecretHex),
            "bad={bad:?} → {err:?}"
        );
    }
}
