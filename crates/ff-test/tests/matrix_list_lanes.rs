//! Backend-matrix variant of `engine_backend_list_lanes.rs` —
//! RFC-v0.7 Wave 7a.
//!
//! The same bodies exercise `ValkeyBackend` and `PostgresBackend` via
//! the [`run_matrix`] harness. Driven by `FF_TEST_BACKEND`:
//!
//! ```bash
//! # default — Valkey only
//! cargo test -p ff-test --test matrix_list_lanes -- --test-threads=1
//!
//! # Postgres only (requires FF_PG_TEST_URL)
//! FF_TEST_BACKEND=postgres FF_PG_TEST_URL=postgres://… \
//!     cargo test -p ff-test --test matrix_list_lanes -- --test-threads=1
//!
//! # Both — proves trait parity
//! FF_TEST_BACKEND=both FF_PG_TEST_URL=postgres://… \
//!     cargo test -p ff-test --test matrix_list_lanes -- --test-threads=1
//! ```

use ff_core::types::LaneId;
use ff_test::backend_matrix::run_matrix;

#[tokio::test(flavor = "multi_thread")]
async fn list_lanes_paginates_sorted_registry() {
    run_matrix(|fx| async move {
        // Seed 5 lanes in non-sorted order. Expected sort:
        //   ["alpha", "bravo", "charlie", "delta", "echo"]
        fx.seed_lanes(&["charlie", "alpha", "echo", "bravo", "delta"])
            .await;

        let backend = fx.backend();

        let p1 = backend.list_lanes(None, 2).await.expect("list_lanes p1");
        assert_eq!(
            p1.lanes,
            vec![LaneId::new("alpha"), LaneId::new("bravo")],
            "[{}] page 1 sort order wrong",
            fx.kind().label()
        );
        assert_eq!(p1.next_cursor, Some(LaneId::new("bravo")));

        let p2 = backend
            .list_lanes(p1.next_cursor.clone(), 2)
            .await
            .expect("list_lanes p2");
        assert_eq!(
            p2.lanes,
            vec![LaneId::new("charlie"), LaneId::new("delta")]
        );
        assert_eq!(p2.next_cursor, Some(LaneId::new("delta")));

        let p3 = backend
            .list_lanes(p2.next_cursor.clone(), 2)
            .await
            .expect("list_lanes p3");
        assert_eq!(p3.lanes, vec![LaneId::new("echo")]);
        assert_eq!(
            p3.next_cursor,
            None,
            "[{}] last page must have no next_cursor",
            fx.kind().label()
        );

        // Over-limit returns the full sorted set with no next_cursor.
        let full = backend
            .list_lanes(None, 100)
            .await
            .expect("list_lanes full");
        assert_eq!(full.lanes.len(), 5);
        assert_eq!(full.next_cursor, None);
        let mut expected = full.lanes.clone();
        expected.sort();
        assert_eq!(full.lanes, expected);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_lanes_empty_registry_returns_empty_page() {
    run_matrix(|fx| async move {
        let page = fx
            .backend()
            .list_lanes(None, 10)
            .await
            .expect("list_lanes empty");
        assert!(page.lanes.is_empty());
        assert_eq!(page.next_cursor, None);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn list_lanes_limit_zero_short_circuits() {
    run_matrix(|fx| async move {
        fx.seed_lanes(&["alpha", "bravo"]).await;
        let page = fx
            .backend()
            .list_lanes(None, 0)
            .await
            .expect("list_lanes zero");
        assert!(page.lanes.is_empty());
        assert_eq!(page.next_cursor, None);
    })
    .await;
}
