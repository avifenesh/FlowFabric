//! Wave 4 Agent D — suspend + signal integration tests.
//!
//! **Lane note.** The Wave-4 brief pointed this file at
//! `crates/ff-test/tests/pg_engine_backend_suspend_signal.rs`. Adding
//! `ff-backend-postgres` as a dev-dep on `ff-test` would force every
//! parallel agent onto the pg dep graph mid-flight, so the test
//! landed here alongside the pre-existing `schema_0001.rs` — same
//! `FF_PG_TEST_URL` / `#[ignore]` convention, same crate-local
//! import surface. A follow-up can hoist to `ff-test` once the pg
//! dep graph stabilises after Wave 4.
//!
//! Covers:
//!
//! * [`rotate_all_happy_path`] — `rotate_waitpoint_hmac_secret_all`
//!   INSERTs a single global row, marks it `active=true`, and
//!   returns a one-entry result vec with `partition=0`.
//! * [`rotate_all_replay_is_noop`] — same kid + same secret replay
//!   is a `Noop`, not a second INSERT.
//! * [`rotate_all_kid_conflict_rejects`] — same kid + different
//!   secret surfaces `EngineError::Conflict(RotationConflict)`.
//! * [`hmac_sign_verify_round_trip_against_rotated_kid`] — tokens
//!   signed under the current kid verify through [`fetch_kid`] after
//!   a rotation pushes the next kid into active.

use ff_backend_postgres::signal::{
    current_active_kid, fetch_kid, hmac_sign, hmac_verify,
    rotate_waitpoint_hmac_secret_all_impl, HmacVerifyError,
};
use ff_core::contracts::{
    RotateWaitpointHmacSecretAllArgs, RotateWaitpointHmacSecretOutcome,
};
use ff_core::engine_error::{ConflictKind, EngineError};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;

async fn setup_or_skip() -> Option<PgPool> {
    let url = std::env::var("FF_PG_TEST_URL").ok()?;
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .connect(&url)
        .await
        .expect("connect to FF_PG_TEST_URL");
    ff_backend_postgres::apply_migrations(&pool)
        .await
        .expect("apply_migrations clean");
    // Reset the HMAC keystore between runs. Schema-0001 is shared
    // across every suspend+signal test and these tests assert on
    // (prior_kid, active_kid) shape.
    sqlx::query("TRUNCATE TABLE ff_waitpoint_hmac")
        .execute(&pool)
        .await
        .expect("truncate ff_waitpoint_hmac");
    Some(pool)
}

fn args(new_kid: &str, new_secret_hex: &str) -> RotateWaitpointHmacSecretAllArgs {
    RotateWaitpointHmacSecretAllArgs::new(new_kid, new_secret_hex, 0)
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn rotate_all_happy_path() {
    let Some(pool) = setup_or_skip().await else {
        eprintln!("FF_PG_TEST_URL not set — skipping");
        return;
    };

    let res = rotate_waitpoint_hmac_secret_all_impl(
        &pool,
        args("kid-1", &"11".repeat(32)),
        1_000_000,
    )
    .await
    .expect("rotate_all");

    assert_eq!(res.entries.len(), 1, "one global entry per Q4");
    let entry = res.entries.into_iter().next().unwrap();
    assert_eq!(entry.partition, 0, "partition=0 on Postgres");
    match entry.result.expect("rotate_all success") {
        RotateWaitpointHmacSecretOutcome::Rotated {
            previous_kid,
            new_kid,
            gc_count,
        } => {
            assert_eq!(previous_kid, None, "bootstrap has no prior kid");
            assert_eq!(new_kid, "kid-1");
            assert_eq!(gc_count, 0);
        }
        other => panic!("expected Rotated, got {other:?}"),
    }

    // Keystore now carries kid-1 as the active kid.
    let (active_kid, _secret) =
        current_active_kid(&pool).await.unwrap().expect("active kid present");
    assert_eq!(active_kid, "kid-1");
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn rotate_all_replay_is_noop() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let secret = "aa".repeat(32);

    rotate_waitpoint_hmac_secret_all_impl(&pool, args("kid-1", &secret), 1_000_000)
        .await
        .expect("first rotate")
        .entries
        .into_iter()
        .next()
        .unwrap()
        .result
        .expect("first rotate ok");

    // Second call with same kid + same secret ⇒ Noop.
    let res = rotate_waitpoint_hmac_secret_all_impl(&pool, args("kid-1", &secret), 2_000_000)
        .await
        .expect("second rotate");
    let outcome = res.entries.into_iter().next().unwrap().result.unwrap();
    assert!(matches!(outcome, RotateWaitpointHmacSecretOutcome::Noop { .. }));
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn rotate_all_kid_conflict_rejects() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };

    rotate_waitpoint_hmac_secret_all_impl(
        &pool,
        args("kid-1", &"aa".repeat(32)),
        1_000_000,
    )
    .await
    .unwrap();

    // Same kid, different secret ⇒ RotationConflict.
    let res = rotate_waitpoint_hmac_secret_all_impl(
        &pool,
        args("kid-1", &"bb".repeat(32)),
        2_000_000,
    )
    .await
    .unwrap();
    let err = res.entries.into_iter().next().unwrap().result.unwrap_err();
    assert!(matches!(
        err,
        EngineError::Conflict(ConflictKind::RotationConflict(_))
    ));
}

#[tokio::test]
#[ignore = "requires a live Postgres; set FF_PG_TEST_URL"]
async fn hmac_sign_verify_round_trip_against_rotated_kid() {
    let Some(pool) = setup_or_skip().await else {
        return;
    };
    let secret_hex = "cafebabe".repeat(8); // 32 bytes
    let secret_bytes = hex::decode(&secret_hex).unwrap();

    rotate_waitpoint_hmac_secret_all_impl(&pool, args("kid-v1", &secret_hex), 1_000_000)
        .await
        .unwrap();

    // Sign under the fresh kid using the active secret.
    let (kid, resolved_secret) = current_active_kid(&pool).await.unwrap().unwrap();
    assert_eq!(kid, "kid-v1");
    assert_eq!(resolved_secret, secret_bytes);

    let token = hmac_sign(&resolved_secret, &kid, b"exec-42:wp-7");
    hmac_verify(&resolved_secret, &kid, b"exec-42:wp-7", &token)
        .expect("token verifies under current kid");

    // A rotation pushes kid-v2 into active; kid-v1 still lives in
    // the table (active=false) so tokens signed under it continue
    // to verify during the grace window.
    rotate_waitpoint_hmac_secret_all_impl(
        &pool,
        args("kid-v2", &"00".repeat(32)),
        2_000_000,
    )
    .await
    .unwrap();

    let kid_v1_secret = fetch_kid(&pool, "kid-v1").await.unwrap().unwrap();
    assert_eq!(kid_v1_secret, secret_bytes, "prior kid retained for grace");
    hmac_verify(&kid_v1_secret, "kid-v1", b"exec-42:wp-7", &token)
        .expect("token signed under kid-v1 still verifies post-rotation");

    // Tampered message must still fail.
    let err = hmac_verify(&kid_v1_secret, "kid-v1", b"tampered", &token).unwrap_err();
    assert!(matches!(err, HmacVerifyError::SignatureMismatch));
}
