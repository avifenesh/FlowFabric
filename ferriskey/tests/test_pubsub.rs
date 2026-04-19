// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

// These tests require the real synchronizer implementation, not the mock
#![cfg(not(feature = "test-util"))]

mod constants;
mod utilities;

use ferriskey::PubSubSubscriptionKind;
use rstest::rstest;
use std::collections::HashSet;
use std::time::Duration;
use utilities::block_on_all;
use utilities::cluster::{
    ClusterTopology, LONG_CLUSTER_TEST_TIMEOUT, PubSubTestSetup, ValkeyCluster,
    generate_test_subscriptions_different_slots, migrate_channel_to_different_node,
    migrate_channels_to_different_nodes, subscribe_and_wait, trigger_failover,
    verify_subscription_addresses_changed, wait_for_node_to_become_primary, wait_for_pubsub_state,
};


/// Delay between slot migrations to avoid overwhelming the cluster.
const MIGRATION_DELAY: Duration = Duration::from_millis(0);

/// Timeout for waiting for subscriptions to be established.
const SUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(2);

/// Timeout for waiting for subscriptions to be re-established after migration.
const RESUBSCRIPTION_TIMEOUT: Duration = Duration::from_secs(5);

#[rstest]
#[case::one_channel(1)]
#[case::hundred_channels(100)]
#[serial_test::serial]
#[timeout(LONG_CLUSTER_TEST_TIMEOUT)]
fn test_sharded_subscriptions_survive_slot_migrations(#[case] num_channels: usize) {
    block_on_all(async {
        let cluster = ValkeyCluster::new(false, &None, Some(3), Some(0));
        let addresses = cluster.get_server_addresses();
        let mut setup = PubSubTestSetup::new(&addresses).await;

        skip_if_version_below!(setup, "7.0.0");

        let topology = ClusterTopology::from_connection(&mut setup.connection).await;

        let channels_with_slots =
            generate_test_subscriptions_different_slots("sharded", num_channels, false);
        let channels: Vec<Vec<u8>> = channels_with_slots.iter().map(|(c, _)| c.clone()).collect();

        let all_subscribed = subscribe_and_wait(
            &setup.synchronizer,
            &channels,
            PubSubSubscriptionKind::Sharded,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        assert!(
            all_subscribed,
            "All {} sharded subscriptions should be established",
            num_channels
        );

        let subs_before = setup.get_subscriptions_by_address();

        migrate_channels_to_different_nodes(
            &mut setup.connection,
            &topology,
            &channels_with_slots,
            MIGRATION_DELAY,
        )
        .await;

        let all_resubscribed = wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Sharded,
            &channels.iter().cloned().collect(),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        assert!(
            all_resubscribed,
            "All sharded subscriptions should be re-established after migrations"
        );

        let subs_after = setup.get_subscriptions_by_address();
        let (changed, unchanged, not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            &channels,
            PubSubSubscriptionKind::Sharded,
        );

        tracing::debug!(
            "test_pubsub - Subscription address changes: {} changed, {} unchanged, {} not found",
            changed, unchanged, not_found
        );

        assert_eq!(
            not_found, 0,
            "All subscriptions should be found after migration"
        );
        assert_eq!(
            unchanged, 0,
            "All subscriptions should be found after migration"
        );
        assert_eq!(
            changed, num_channels,
            "All {} sharded subscriptions should have moved to different addresses",
            num_channels
        );
    });
}

#[rstest]
#[case::one_channel(1)]
#[case::many_channels(100)]
#[serial_test::serial]
#[timeout(LONG_CLUSTER_TEST_TIMEOUT)]
fn test_exact_subscriptions_survive_slot_migrations(#[case] num_channels: usize) {
    block_on_all(async {
        let cluster = ValkeyCluster::new(false, &None, Some(3), Some(0));
        let addresses = cluster.get_server_addresses();
        let mut setup = PubSubTestSetup::new(&addresses).await;

        let topology = ClusterTopology::from_connection(&mut setup.connection).await;
        let channels_with_slots =
            generate_test_subscriptions_different_slots("exact", num_channels, false);
        let channels: Vec<Vec<u8>> = channels_with_slots.iter().map(|(c, _)| c.clone()).collect();

        let all_subscribed = subscribe_and_wait(
            &setup.synchronizer,
            &channels,
            PubSubSubscriptionKind::Exact,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        assert!(
            all_subscribed,
            "All {} exact subscriptions should be established",
            num_channels
        );

        let subs_before = setup.get_subscriptions_by_address();

        migrate_channels_to_different_nodes(
            &mut setup.connection,
            &topology,
            &channels_with_slots,
            MIGRATION_DELAY,
        )
        .await;

        // small sleep to allow for the synchronizer handle_topology to start and unsubscribe
        // Otherwise we will pass the wait_for_pubsub_state immediately on the same address
        tokio::time::sleep(Duration::from_millis(500)).await;

        let all_resubscribed = wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Exact,
            &channels.iter().cloned().collect(),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        assert!(
            all_resubscribed,
            "All exact subscriptions should be re-established after migrations"
        );

        let subs_after = setup.get_subscriptions_by_address();
        let (changed, unchanged, not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            &channels,
            PubSubSubscriptionKind::Exact,
        );

        tracing::info!(
            "test_pubsub - Subscription address changes: {} changed, {} unchanged, {} not found",
            changed, unchanged, not_found
        );

        assert_eq!(
            not_found, 0,
            "All subscriptions should be found after migration"
        );
        assert_eq!(
            unchanged, 0,
            "All subscriptions should be found after migration"
        );
        assert_eq!(
            changed, num_channels,
            "All {} exact subscriptions should have moved to different addresses",
            num_channels
        );
    });
}

#[rstest]
#[case::one_pattern(1)]
#[case::hundred_patterns(100)]
#[serial_test::serial]
#[timeout(LONG_CLUSTER_TEST_TIMEOUT)]
fn test_pattern_subscriptions_survive_slot_migrations(#[case] num_patterns: usize) {
    block_on_all(async {
        let cluster = ValkeyCluster::new(false, &None, Some(3), Some(0));
        let addresses = cluster.get_server_addresses();
        let mut setup = PubSubTestSetup::new(&addresses).await;

        let topology = ClusterTopology::from_connection(&mut setup.connection).await;
        let patterns_with_slots =
            generate_test_subscriptions_different_slots("pattern", num_patterns, true);
        let patterns: Vec<Vec<u8>> = patterns_with_slots.iter().map(|(p, _)| p.clone()).collect();

        let all_subscribed = subscribe_and_wait(
            &setup.synchronizer,
            &patterns,
            PubSubSubscriptionKind::Pattern,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        assert!(
            all_subscribed,
            "All {} pattern subscriptions should be established",
            num_patterns
        );

        let subs_before = setup.get_subscriptions_by_address();

        migrate_channels_to_different_nodes(
            &mut setup.connection,
            &topology,
            &patterns_with_slots,
            MIGRATION_DELAY,
        )
        .await;

        // small sleep to allow for the synchronizer handle_topology to start and unsubscribe
        // Otherwise we will pass the wait_for_pubsub_state immediately on the same address
        tokio::time::sleep(Duration::from_millis(500)).await;

        let all_resubscribed = wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Pattern,
            &patterns.iter().cloned().collect(),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        assert!(
            all_resubscribed,
            "All pattern subscriptions should be re-established after migrations"
        );

        let subs_after = setup.get_subscriptions_by_address();
        let (changed, unchanged, not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            &patterns,
            PubSubSubscriptionKind::Pattern,
        );

        tracing::info!(
            "test_pubsub - Subscription address changes: {} changed, {} unchanged, {} not found",
            changed, unchanged, not_found
        );

        assert_eq!(
            not_found, 0,
            "All subscriptions should be found after migration"
        );
        assert_eq!(
            unchanged, 0,
            "All subscriptions should be found after migration"
        );
        assert_eq!(
            changed, num_patterns,
            "All {} pattern subscriptions should have moved to different addresses",
            num_patterns
        );
    });
}

#[rstest]
#[serial_test::serial]
#[timeout(LONG_CLUSTER_TEST_TIMEOUT)]
fn test_all_subscription_types_survive_same_slot_migration() {
    block_on_all(async {
        let cluster = ValkeyCluster::new(false, &None, Some(3), Some(0));
        let addresses = cluster.get_server_addresses();
        let mut setup = PubSubTestSetup::new(&addresses).await;

        skip_if_version_below!(setup, "7.0.0");

        let topology = ClusterTopology::from_connection(&mut setup.connection).await;
        let exact_channel = b"{mixed-test}exact-channel".to_vec();
        let pattern = b"{mixed-test}pattern-*".to_vec();
        let sharded_channel = b"{mixed-test}sharded-channel".to_vec();

        let slot = ferriskey::cluster::topology::get_slot(&exact_channel);

        let exact_sub = subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&exact_channel),
            PubSubSubscriptionKind::Exact,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        let pattern_sub = subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&pattern),
            PubSubSubscriptionKind::Pattern,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        let sharded_sub = subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&sharded_channel),
            PubSubSubscriptionKind::Sharded,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;

        assert!(exact_sub, "Exact subscription should be established");
        assert!(pattern_sub, "Pattern subscription should be established");
        assert!(sharded_sub, "Sharded subscription should be established");

        let subs_before = setup.get_subscriptions_by_address();

        let migrated =
            migrate_channel_to_different_node(&mut setup.connection, &topology, slot).await;
        assert!(
            migrated.is_some(),
            "Should have migrated to a different node"
        );

        // small sleep to allow for the synchronizer handle_topology to start and unsubscribe
        // Otherwise we will pass the wait_for_pubsub_state immediately on the same address
        tokio::time::sleep(Duration::from_millis(500)).await;

        let exact_resub = wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Exact,
            &HashSet::from([exact_channel.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        let pattern_resub = wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Pattern,
            &HashSet::from([pattern.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        let sharded_resub = wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Sharded,
            &HashSet::from([sharded_channel.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;

        assert!(exact_resub, "Exact subscription should be re-established");
        assert!(
            pattern_resub,
            "Pattern subscription should be re-established"
        );
        assert!(
            sharded_resub,
            "Sharded subscription should be re-established"
        );

        let subs_after = setup.get_subscriptions_by_address();

        let (exact_changed, _, exact_not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            std::slice::from_ref(&exact_channel),
            PubSubSubscriptionKind::Exact,
        );
        let (pattern_changed, _, pattern_not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            std::slice::from_ref(&pattern),
            PubSubSubscriptionKind::Pattern,
        );
        let (sharded_changed, _, sharded_not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            std::slice::from_ref(&sharded_channel),
            PubSubSubscriptionKind::Sharded,
        );

        assert_eq!(exact_not_found, 0, "Exact subscription should be found");
        assert_eq!(pattern_not_found, 0, "Pattern subscription should be found");
        assert_eq!(sharded_not_found, 0, "Sharded subscription should be found");

        assert_eq!(
            exact_changed, 1,
            "Exact subscription should have moved to different address"
        );
        assert_eq!(
            pattern_changed, 1,
            "Pattern subscription should have moved to different address"
        );
        assert_eq!(
            sharded_changed, 1,
            "Sharded subscription should have moved to different address"
        );
    });
}

#[rstest]
#[serial_test::serial]
#[timeout(LONG_CLUSTER_TEST_TIMEOUT)]
fn test_all_subscription_types_survive_different_slot_migrations() {
    block_on_all(async {
        let cluster = ValkeyCluster::new(false, &None, Some(3), Some(0));
        let addresses = cluster.get_server_addresses();
        let mut setup = PubSubTestSetup::new(&addresses).await;

        skip_if_version_below!(setup, "7.0.0");

        let topology = ClusterTopology::from_connection(&mut setup.connection).await;
        let exact_channel = b"{exact-diff-500}channel".to_vec();
        let pattern = b"{pattern-diff-8000}*".to_vec();
        let sharded_channel = b"{sharded-diff-15000}channel".to_vec();

        let exact_slot = ferriskey::cluster::topology::get_slot(&exact_channel);
        let pattern_slot = ferriskey::cluster::topology::get_slot(&pattern);
        let sharded_slot = ferriskey::cluster::topology::get_slot(&sharded_channel);

        subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&exact_channel),
            PubSubSubscriptionKind::Exact,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&pattern),
            PubSubSubscriptionKind::Pattern,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&sharded_channel),
            PubSubSubscriptionKind::Sharded,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;

        let subs_before = setup.get_subscriptions_by_address();

        for slot in [exact_slot, pattern_slot, sharded_slot] {
            let _ = migrate_channel_to_different_node(&mut setup.connection, &topology, slot).await;
            tokio::time::sleep(MIGRATION_DELAY).await;
        }

        // small sleep to allow for the synchronizer handle_topology to start and unsubscribe
        // Otherwise we will pass the wait_for_pubsub_state immediately on the same address
        tokio::time::sleep(Duration::from_millis(500)).await;

        wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Exact,
            &HashSet::from([exact_channel.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Pattern,
            &HashSet::from([pattern.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Sharded,
            &HashSet::from([sharded_channel.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;

        let subs_after = setup.get_subscriptions_by_address();

        let (exact_changed, _, _) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            std::slice::from_ref(&exact_channel),
            PubSubSubscriptionKind::Exact,
        );
        let (pattern_changed, _, _) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            std::slice::from_ref(&pattern),
            PubSubSubscriptionKind::Pattern,
        );
        let (sharded_changed, _, _) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            std::slice::from_ref(&sharded_channel),
            PubSubSubscriptionKind::Sharded,
        );

        assert_eq!(
            exact_changed, 1,
            "Exact subscription should have moved to different address"
        );
        assert_eq!(
            pattern_changed, 1,
            "Pattern subscription should have moved to different address"
        );
        assert_eq!(
            sharded_changed, 1,
            "Sharded subscription should have moved to different address"
        );
    });
}

#[rstest]
#[serial_test::serial]
#[timeout(LONG_CLUSTER_TEST_TIMEOUT)]
fn test_all_subscription_types_survive_failover() {
    block_on_all(async {
        let cluster = ValkeyCluster::new(false, &None, Some(3), Some(1));
        let addresses = cluster.get_server_addresses();
        let mut setup = PubSubTestSetup::new(&addresses).await;

        skip_if_version_below!(setup, "7.0.0");

        let topology = ClusterTopology::from_connection(&mut setup.connection).await;

        // Create channels with same hash tag so they all go to the same slot
        let exact_channel = b"{failover-all}-exact".to_vec();
        let pattern = b"{failover-all}-pattern-*".to_vec();
        let sharded_channel = b"{failover-all}-sharded".to_vec();

        let slot = ferriskey::cluster::topology::get_slot(&exact_channel);

        let primary = topology
            .find_slot_owner(slot)
            .expect("Should find owner for slot");

        let replicas = topology.find_replicas_of(&primary.node_id);
        assert!(
            !replicas.is_empty(),
            "Primary should have at least one replica"
        );
        let replica = replicas[0];

        tracing::info!(
            "test_pubsub - Channels hash to slot {}. Primary {}:{} with replica {}:{}",
            slot, primary.host, primary.port, replica.host, replica.port
        );

        subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&exact_channel),
            PubSubSubscriptionKind::Exact,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&pattern),
            PubSubSubscriptionKind::Pattern,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;
        subscribe_and_wait(
            &setup.synchronizer,
            std::slice::from_ref(&sharded_channel),
            PubSubSubscriptionKind::Sharded,
            SUBSCRIPTION_TIMEOUT,
        )
        .await;

        let subs_before = setup.get_subscriptions_by_address();

        let failover_initiated = trigger_failover(&mut setup.connection, replica).await;
        assert!(failover_initiated, "Failover should be initiated");

        let became_primary = wait_for_node_to_become_primary(
            &mut setup.connection,
            &replica.node_id,
            Duration::from_secs(30),
        )
        .await;
        assert!(
            became_primary,
            "Replica should become primary after failover"
        );

        wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Exact,
            &HashSet::from([exact_channel.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Pattern,
            &HashSet::from([pattern.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;
        wait_for_pubsub_state(
            &setup.synchronizer,
            PubSubSubscriptionKind::Sharded,
            &HashSet::from([sharded_channel.clone()]),
            true,
            RESUBSCRIPTION_TIMEOUT,
        )
        .await;

        let subs_after = setup.get_subscriptions_by_address();

        let (_exact_changed, _, exact_not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            &[exact_channel],
            PubSubSubscriptionKind::Exact,
        );
        let (_pattern_changed, _, pattern_not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            &[pattern],
            PubSubSubscriptionKind::Pattern,
        );
        let (sharded_changed, _, sharded_not_found) = verify_subscription_addresses_changed(
            &subs_before,
            &subs_after,
            &[sharded_channel],
            PubSubSubscriptionKind::Sharded,
        );

        assert_eq!(exact_not_found, 0, "Exact subscription should be found");
        assert_eq!(pattern_not_found, 0, "Pattern subscription should be found");
        assert_eq!(sharded_not_found, 0, "Sharded subscription should be found");

        // Sharded must follow the primary, so it always moves after failover.
        assert_eq!(sharded_changed, 1, "Sharded subscription should have moved to new primary");
        // Exact/Pattern may or may not change address depending on timing:
        // if the old primary temporarily disappears from topology, they get
        // resubscribed on a new node. The key invariant is not_found == 0.

        tracing::info!(
            "test_pubsub - Test completed: all subscription types survived failover"
        );
    });
}

// ---------------------------------------------------------------------------
// Standalone PubSub tests
// ---------------------------------------------------------------------------
// Requires VALKEY_STANDALONE_HOST env var pointing to a standalone node.
// Run: VALKEY_STANDALONE_HOST=host VALKEY_TLS=true cargo test --test test_pubsub standalone_pubsub

#[cfg(test)]
mod standalone_pubsub_tests {
    use ferriskey::client::types::{ConnectionRetryStrategy, NodeAddress};
    use ferriskey::client::Client;
    use ferriskey::value::Value;
    use ferriskey::PushInfo;
    use std::time::Duration;
    use tokio::sync::mpsc;

    #[allow(dead_code)]
    fn standalone_url() -> Option<String> {
        let host = std::env::var("VALKEY_STANDALONE_HOST").ok()?;
        let port = std::env::var("VALKEY_STANDALONE_PORT").unwrap_or_else(|_| "6379".into());
        let tls = std::env::var("VALKEY_TLS").unwrap_or_default() == "true";
        let scheme = if tls { "rediss" } else { "redis" };
        Some(format!("{scheme}://{host}:{port}/#insecure"))
    }

    fn standalone_request() -> Option<ferriskey::client::types::ConnectionRequest> {
        let host = std::env::var("VALKEY_STANDALONE_HOST").ok()?;
        let port: u16 = std::env::var("VALKEY_STANDALONE_PORT")
            .unwrap_or_else(|_| "6379".into())
            .parse()
            .unwrap_or(6379);
        let tls = std::env::var("VALKEY_TLS").unwrap_or_default() == "true";

        let tls_mode = if tls {
            Some(ferriskey::client::types::TlsMode::InsecureTls)
        } else {
            None
        };

        Some(ferriskey::client::types::ConnectionRequest {
            addresses: vec![NodeAddress {
                host,
                port,
            }],
            cluster_mode_enabled: false,
            tls_mode,
            connection_retry_strategy: Some(ConnectionRetryStrategy {
                number_of_retries: 3,
                factor: 100,
                exponent_base: 2,
                jitter_percent: Some(20),
            }),
            connection_timeout: Some(5000),
            request_timeout: Some(10000),
            ..Default::default()
        })
    }

    /// Create a subscriber client (with push receiver) and a separate publisher client.
    async fn create_sub_pub_clients(
    ) -> Option<(Client, mpsc::UnboundedReceiver<PushInfo>, Client)> {
        let request = standalone_request()?;
        let (push_tx, push_rx) = mpsc::unbounded_channel();
        let sub_client = Client::new(request.clone(), Some(push_tx)).await.ok()?;
        let pub_client = Client::new(request, None).await.ok()?;
        Some((sub_client, push_rx, pub_client))
    }

    /// Build sub+pub via the public [`ferriskey::ClientBuilder::push_sender`]
    /// surface (distinct from the internal `Client::new` path used by
    /// the other tests in this module). Returns `ferriskey::Client`
    /// — the top-level facade type, not `ferriskey::client::Client`.
    #[allow(clippy::type_complexity)]
    async fn create_sub_pub_clients_via_builder() -> Option<(
        ferriskey::Client,
        mpsc::UnboundedReceiver<PushInfo>,
        ferriskey::Client,
    )> {
        use ferriskey::ClientBuilder;
        use ferriskey::value::ProtocolVersion;

        let host = std::env::var("VALKEY_STANDALONE_HOST").ok()?;
        let port: u16 = std::env::var("VALKEY_STANDALONE_PORT")
            .unwrap_or_else(|_| "6379".into())
            .parse()
            .unwrap_or(6379);

        let (push_tx, push_rx) = mpsc::unbounded_channel();
        let sub_client = ClientBuilder::new()
            .host(&host, port)
            .protocol(ProtocolVersion::RESP3)
            .push_sender(push_tx)
            .build()
            .await
            .ok()?;
        let pub_client = ClientBuilder::new()
            .host(&host, port)
            .protocol(ProtocolVersion::RESP3)
            .build()
            .await
            .ok()?;
        Some((sub_client, push_rx, pub_client))
    }

    /// Create a single client (for subscribe/unsubscribe only tests).
    async fn create_single_client() -> Option<Client> {
        let request = standalone_request()?;
        let client = Client::new(request, None).await.ok()?;
        Some(client)
    }

    macro_rules! require_sub_pub {
        () => {
            match create_sub_pub_clients().await {
                Some(c) => c,
                None => {
                    eprintln!("Skipping: VALKEY_STANDALONE_HOST not set or unreachable");
                    return;
                }
            }
        };
    }

    macro_rules! require_client {
        () => {
            match create_single_client().await {
                Some(c) => c,
                None => {
                    eprintln!("Skipping: VALKEY_STANDALONE_HOST not set or unreachable");
                    return;
                }
            }
        };
    }

    async fn drain_push(
        rx: &mut mpsc::UnboundedReceiver<PushInfo>,
        expected_kind: ferriskey::PushKind,
        count: usize,
    ) {
        for i in 0..count {
            let msg = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap_or_else(|_| panic!("Timed out waiting for push {}/{}", i + 1, count))
                .unwrap_or_else(|| panic!("Push channel closed at {}/{}", i + 1, count));
            assert_eq!(
                msg.kind, expected_kind,
                "Expected {:?} at {}/{}, got {:?}",
                expected_kind, i + 1, count, msg.kind
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_standalone_exact_subscribe_and_publish() {
        let (mut sub_client, mut push_rx, mut pub_client) = require_sub_pub!();

        let channels = ["standalone:ch1", "standalone:ch2", "standalone:ch3"];

        // Subscribe to all 3 channels (single SUBSCRIBE with multiple args)
        let mut sub_cmd = ferriskey::cmd("SUBSCRIBE");
        for ch in &channels {
            sub_cmd.arg(*ch);
        }
        sub_client.send_command(&mut sub_cmd, None).await.unwrap();

        // Drain subscription confirmations (one per channel)
        drain_push(&mut push_rx, ferriskey::PushKind::Subscribe, channels.len()).await;

        // Publish from a separate client
        let mut pub_cmd = ferriskey::cmd("PUBLISH");
        pub_cmd.arg("standalone:ch2").arg("hello-standalone");
        pub_client.send_command(&mut pub_cmd, None).await.unwrap();

        // Receive the message
        let msg = tokio::time::timeout(Duration::from_secs(5), push_rx.recv())
            .await
            .expect("Timed out waiting for message")
            .expect("Push channel closed");
        assert_eq!(msg.kind, ferriskey::PushKind::Message);

        // Unsubscribe
        let mut unsub_cmd = ferriskey::cmd("UNSUBSCRIBE");
        for ch in &channels {
            unsub_cmd.arg(*ch);
        }
        sub_client.send_command(&mut unsub_cmd, None).await.unwrap();
    }

    /// Verify the public `ClientBuilder::push_sender` setter actually
    /// delivers RESP3 push frames to the caller-supplied channel
    /// end-to-end against a live Valkey. Mirrors the shape of
    /// `test_standalone_exact_subscribe_and_publish` but builds the
    /// subscriber via the builder instead of the internal
    /// `Client::new(request, Some(tx))` escape hatch.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_standalone_builder_push_sender_delivers() {
        let (sub_client, mut push_rx, pub_client) =
            match create_sub_pub_clients_via_builder().await {
                Some(c) => c,
                None => {
                    eprintln!(
                        "Skipping: VALKEY_STANDALONE_HOST not set or unreachable"
                    );
                    return;
                }
            };

        // SUBSCRIBE via the facade's runtime-intercepted path. The pubsub
        // synchronizer returns as soon as the desired state is recorded;
        // the reconciler then issues the real SUBSCRIBE in the background.
        let _: () = sub_client
            .cmd("SUBSCRIBE")
            .arg("builder:ch1")
            .execute()
            .await
            .unwrap();
        drain_push(&mut push_rx, ferriskey::PushKind::Subscribe, 1).await;

        let _: () = pub_client
            .cmd("PUBLISH")
            .arg("builder:ch1")
            .arg("hello-from-builder")
            .execute()
            .await
            .unwrap();

        let msg = tokio::time::timeout(Duration::from_secs(5), push_rx.recv())
            .await
            .expect("Timed out waiting for message")
            .expect("Push channel closed");
        assert_eq!(msg.kind, ferriskey::PushKind::Message);

        let _: () = sub_client
            .cmd("UNSUBSCRIBE")
            .arg("builder:ch1")
            .execute()
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_standalone_pattern_subscribe_and_publish() {
        let (mut sub_client, mut push_rx, mut pub_client) = require_sub_pub!();

        // Subscribe to a pattern
        let mut sub_cmd = ferriskey::cmd("PSUBSCRIBE");
        sub_cmd.arg("standalone:events:*");
        sub_client.send_command(&mut sub_cmd, None).await.unwrap();

        // Drain pattern subscription confirmation
        drain_push(&mut push_rx, ferriskey::PushKind::PSubscribe, 1).await;

        // Publish from separate client
        let mut pub_cmd = ferriskey::cmd("PUBLISH");
        pub_cmd
            .arg("standalone:events:order123")
            .arg("order-created");
        pub_client.send_command(&mut pub_cmd, None).await.unwrap();

        // Receive the pattern message
        let msg = tokio::time::timeout(Duration::from_secs(5), push_rx.recv())
            .await
            .expect("Timed out waiting for message")
            .expect("Push channel closed");
        assert_eq!(msg.kind, ferriskey::PushKind::PMessage);

        // Unsubscribe
        let mut unsub_cmd = ferriskey::cmd("PUNSUBSCRIBE");
        unsub_cmd.arg("standalone:events:*");
        sub_client
            .send_command(&mut unsub_cmd, None)
            .await
            .unwrap();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_standalone_rapid_subscribe_unsubscribe() {
        let mut client = require_client!();

        // 10 concurrent subscribe → unsubscribe cycles
        let mut handles = Vec::new();
        for i in 0..10 {
            let mut client_clone = client.clone();
            handles.push(tokio::spawn(async move {
                let channel = format!("standalone:rapid:{i}");

                let mut sub = ferriskey::cmd("SUBSCRIBE");
                sub.arg(channel.as_str());
                let _ = client_clone.send_command(&mut sub, None).await;

                tokio::time::sleep(Duration::from_millis(100)).await;

                let mut unsub = ferriskey::cmd("UNSUBSCRIBE");
                unsub.arg(channel.as_str());
                let _ = client_clone.send_command(&mut unsub, None).await;
            }));
        }

        for handle in handles {
            handle.await.unwrap();
        }

        // Allow time for unsubscribe confirmations to propagate
        tokio::time::sleep(Duration::from_millis(500)).await;

        // Verify zero subscriptions remain via GET_SUBSCRIPTIONS
        let mut get_subs = ferriskey::cmd("GET_SUBSCRIPTIONS");
        let result = client.send_command(&mut get_subs, None).await.unwrap();

        if let Value::Array(items) = &result {
            // actual map is items[3], desired map is items[1]
            for idx in [1, 3] {
                if let Some(Ok(Value::Map(map))) = items.get(idx) {
                    for (_, channels_val) in map {
                        if let Value::Array(channels) = channels_val {
                            assert!(
                                channels.is_empty(),
                                "Expected zero subscriptions at index {}, got {:?}",
                                idx,
                                channels
                            );
                        }
                    }
                }
            }
        }
    }
}
