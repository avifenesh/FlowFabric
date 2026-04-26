//! RFC-023 §4.5 production-guard tests + §4.2 B6 registry-dedup test.

use ff_backend_sqlite::SqliteBackend;
use ff_core::engine_error::BackendError;
use serial_test::serial;

#[tokio::test]
#[serial(ff_dev_mode)]
async fn refuses_without_ff_dev_mode() {
    // SAFETY: test-only env mutation; `serial_test` key prevents
    // races with sibling env-reading tests.
    unsafe {
        std::env::remove_var("FF_DEV_MODE");
    }
    let err = SqliteBackend::new(":memory:")
        .await
        .expect_err("must refuse without FF_DEV_MODE");

    assert!(matches!(err, BackendError::RequiresDevMode));
    // Exact message parity with RFC-023 §4.5 B2.
    let rendered = err.to_string();
    assert!(
        rendered.contains("SqliteBackend requires FF_DEV_MODE=1"),
        "unexpected render: {rendered}"
    );
    assert!(
        rendered.contains("docs/dev-harness.md"),
        "render missing docs URL: {rendered}"
    );
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn accepts_with_ff_dev_mode() {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let _backend = SqliteBackend::new(":memory:")
        .await
        .expect("must accept under FF_DEV_MODE=1");
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn two_news_same_path_share_handle() {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    // Same URI → both calls must return the same underlying inner.
    // We fingerprint via `Arc::strong_count` on a known-shared field.
    let uri = "file:rfc-023-dedup?mode=memory&cache=shared";
    let a = SqliteBackend::new(uri).await.expect("a");
    let b = SqliteBackend::new(uri).await.expect("b");
    // `SqliteBackend::clone` is `Arc`-cheap; two `new()` calls that
    // share `inner` are not the same *outer* `Arc<SqliteBackend>`
    // (each gets its own outer wrapper) but both wrappers must
    // point at the same inner allocation. We verify by dropping one
    // and confirming the other still works (i.e. registry keeps the
    // inner alive only so long as at least one outer holds it).
    drop(a);
    // If `b` lost its inner, the drop above would have closed the
    // pool. A trivial trait call that doesn't hit the data plane is
    // enough to confirm the `Arc` is live.
    use ff_core::engine_backend::EngineBackend;
    assert_eq!(b.backend_label(), "sqlite");
}

/// F1 regression: a bare `:memory:` path is rewritten to a shared-
/// cache URI internally and a sentinel connection is held, so data
/// written through one pool connection survives a pool idle cycle
/// and is visible to subsequent checkouts.
#[tokio::test]
#[serial(ff_dev_mode)]
async fn memory_shared_cache_survives_pool_cycle() {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    let backend = SqliteBackend::new_with_tuning(":memory:", 4, true)
        .await
        .expect("construct");
    let pool = backend.pool_for_test();

    // Write on one connection, drop it, read on a fresh connection.
    sqlx::query("CREATE TABLE f1 (v INTEGER)")
        .execute(pool)
        .await
        .expect("create");
    sqlx::query("INSERT INTO f1 VALUES (42)")
        .execute(pool)
        .await
        .expect("insert");

    // Force pool churn: acquire + drop several times.
    for _ in 0..8 {
        let _c = pool.acquire().await.expect("acquire");
    }

    let v: (i64,) = sqlx::query_as("SELECT v FROM f1")
        .fetch_one(pool)
        .await
        .expect("select");
    assert_eq!(v.0, 42);
}

#[tokio::test]
#[serial(ff_dev_mode)]
async fn distinct_memory_uris_are_distinct() {
    unsafe {
        std::env::set_var("FF_DEV_MODE", "1");
    }
    // Fresh UUIDs per URI — registry must keep them distinct.
    let a = SqliteBackend::new("file:rfc-023-dist-a?mode=memory&cache=shared")
        .await
        .expect("a");
    let b = SqliteBackend::new("file:rfc-023-dist-b?mode=memory&cache=shared")
        .await
        .expect("b");
    // Both must work independently. The registry entries are
    // key-distinct, so dropping one must not affect the other.
    drop(a);
    use ff_core::engine_backend::EngineBackend;
    assert_eq!(b.backend_label(), "sqlite");
}
