// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

//! DNS resolution tests.
//! See [DNS Tests](../README.md#dns-tests) for setup instructions.

mod constants;
mod utilities;

#[cfg(test)]
mod dns_tests {
    use crate::constants::{HOSTNAME_NO_TLS, HOSTNAME_TLS};
    use crate::utilities::{
        cluster::{ValkeyCluster, SHORT_CLUSTER_TEST_TIMEOUT},
        *,
    };
    use ferriskey::{
        client::{Client, StandaloneClient},
        client::types::TlsMode,
    };
    use once_cell::sync::Lazy;
    use rstest::rstest;

    // Shared temp directory and TLS paths for all DNS tests
    static TLS_TEMPDIR: Lazy<tempfile::TempDir> =
        Lazy::new(|| tempfile::tempdir().expect("Failed to create temp dir for TLS certs"));
    static TLS_PATHS: Lazy<TlsFilePaths> = Lazy::new(|| build_tls_file_paths(&TLS_TEMPDIR));
    static CA_CERT_BYTES: Lazy<Vec<u8>> = Lazy::new(|| TLS_PATHS.read_ca_cert_as_bytes());

    // ==================== Helper Methods ======================

    /// Returns the port from the given connection address.
    fn extract_port(addr: &ferriskey::ConnectionAddr) -> u16 {
        match addr {
            ferriskey::ConnectionAddr::Tcp(_, p) => *p,
            ferriskey::ConnectionAddr::TcpTls { port, .. } => *port,
            _ => panic!("Unexpected address type"),
        }
    }

    /// Builds and returns a non-TLS cluster client with the specified hostname.
    async fn build_cluster_client(hostname: &str) -> Option<(Client, ValkeyCluster)> {
        let cluster = ValkeyCluster::new(false, &None, None, None);
        let port = extract_port(&cluster.get_server_addresses()[0]);
        let addr = ferriskey::ConnectionAddr::Tcp(hostname.to_string(), port);

        let connection_request = create_connection_request(
            &[addr],
            &TestConfiguration {
                cluster_mode: ClusterMode::Enabled,
                shared_server: false,
                ..Default::default()
            },
        );

        // Wait to ensure server is ready before connecting.
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        let client = Client::new(connection_request, None).await.ok()?;
        Some((client, cluster))
    }

    /// Builds and returns a TLS cluster client with the specified hostname.
    async fn build_tls_cluster_client(hostname: &str) -> Option<(Client, ValkeyCluster)> {
        let cluster = ValkeyCluster::new_with_tls(3, 0, Some(TLS_PATHS.clone()));

        let port = extract_port(&cluster.get_server_addresses()[0]);
        let addr = ferriskey::ConnectionAddr::TcpTls {
            host: hostname.to_string(),
            port,
            insecure: false,
            tls_params: None,
        };

        let mut connection_request = create_connection_request(
            &[addr],
            &TestConfiguration {
                use_tls: true,
                cluster_mode: ClusterMode::Enabled,
                shared_server: false,
                ..Default::default()
            },
        );
        connection_request.tls_mode = Some(TlsMode::SecureTls);
        connection_request.root_certs = vec![CA_CERT_BYTES.clone()];

        // Wait to ensure server is ready before connecting.
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        let client = Client::new(connection_request, None).await.ok()?;
        Some((client, cluster))
    }

    /// Builds and returns a non-TLS standalone client with the specified hostname.
    async fn build_standalone_client(hostname: &str) -> Option<(StandaloneClient, ValkeyServer)> {
        let server = ValkeyServer::new(ServerType::Tcp { tls: false });
        let port = extract_port(&server.get_client_addr());
        let addr = ferriskey::ConnectionAddr::Tcp(hostname.to_string(), port);

        let connection_request = create_connection_request(
            &[addr],
            &TestConfiguration {
                shared_server: false,
                ..Default::default()
            },
        );

        // Wait to ensure server is ready before connecting.
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        let client = create_test_standalone_client(connection_request, None)
            .await
            .ok()?;
        Some((client, server))
    }

    async fn build_tls_standalone_client(
        hostname: &str,
    ) -> Option<(StandaloneClient, ValkeyServer)> {
        let server = ValkeyServer::new_with_tls(true, Some(TLS_PATHS.clone()));

        let port = extract_port(&server.get_client_addr());
        let addr = ferriskey::ConnectionAddr::TcpTls {
            host: hostname.to_string(),
            port,
            insecure: false,
            tls_params: None,
        };

        let mut connection_request = create_connection_request(
            &[addr],
            &TestConfiguration {
                use_tls: true,
                shared_server: false,
                ..Default::default()
            },
        );
        connection_request.tls_mode = Some(TlsMode::SecureTls);
        connection_request.root_certs = vec![CA_CERT_BYTES.clone()];

        // Wait to ensure server is ready before connecting.
        tokio::time::sleep(std::time::Duration::from_millis(1000)).await;

        let client = create_test_standalone_client(connection_request, None)
            .await
            .ok()?;
        Some((client, server))
    }

    // ==================== Standalone Tests ====================

    #[rstest]
    #[serial_test::serial]
    #[timeout(SHORT_STANDALONE_TEST_TIMEOUT)]
    #[cfg_attr(not(dns_tests_enabled), ignore)]
    fn test_standalone_connect_with_valid_hostname_no_tls() {
        block_on_all(async move {
            let (mut client, _server) = build_standalone_client(HOSTNAME_NO_TLS)
                .await
                .expect("Failed to connect");
            assert_connected(&mut client).await;
        });
    }

    #[rstest]
    #[serial_test::serial]
    #[timeout(SHORT_STANDALONE_TEST_TIMEOUT)]
    #[cfg_attr(not(dns_tests_enabled), ignore)]
    fn test_standalone_connect_with_invalid_hostname_no_tls() {
        block_on_all(async move {
            let result = build_standalone_client("nonexistent.invalid").await;
            assert!(result.is_none());
        });
    }

    #[rstest]
    #[serial_test::serial]
    #[timeout(SHORT_STANDALONE_TEST_TIMEOUT)]
    #[cfg_attr(not(dns_tests_enabled), ignore)]
    fn test_standalone_tls_connect_with_hostname_in_cert() {
        block_on_all(async move {
            let (mut client, _server) = build_tls_standalone_client(HOSTNAME_TLS)
                .await
                .expect("Failed to connect");
            assert_connected(&mut client).await;
        });
    }

    #[rstest]
    #[serial_test::serial]
    #[timeout(SHORT_STANDALONE_TEST_TIMEOUT)]
    #[cfg_attr(not(dns_tests_enabled), ignore)]
    fn test_standalone_tls_connect_with_hostname_not_in_cert() {
        block_on_all(async move {
            let result = build_tls_standalone_client(HOSTNAME_NO_TLS).await;
            assert!(result.is_none());
        });
    }

    // ==================== Cluster Tests ====================

    #[rstest]
    #[serial_test::serial]
    #[timeout(SHORT_CLUSTER_TEST_TIMEOUT)]
    #[cfg_attr(not(dns_tests_enabled), ignore)]
    fn test_cluster_connect_with_valid_hostname_no_tls() {
        block_on_all(async move {
            let (mut client, _cluster) = build_cluster_client(HOSTNAME_NO_TLS)
                .await
                .expect("Failed to connect");
            assert_connected(&mut client).await;
        });
    }

    #[rstest]
    #[serial_test::serial]
    #[timeout(SHORT_CLUSTER_TEST_TIMEOUT)]
    #[cfg_attr(not(dns_tests_enabled), ignore)]
    fn test_cluster_connect_with_invalid_hostname_no_tls() {
        block_on_all(async move {
            let result = build_cluster_client("nonexistent.invalid").await;
            assert!(result.is_none());
        });
    }

    #[rstest]
    #[serial_test::serial]
    #[timeout(SHORT_CLUSTER_TEST_TIMEOUT)]
    #[cfg_attr(not(dns_tests_enabled), ignore)]
    fn test_cluster_tls_connect_with_hostname_in_cert() {
        block_on_all(async move {
            let (mut client, _cluster) = build_tls_cluster_client(HOSTNAME_TLS)
                .await
                .expect("Failed to connect");
            assert_connected(&mut client).await;
        });
    }

    #[rstest]
    #[serial_test::serial]
    #[timeout(SHORT_CLUSTER_TEST_TIMEOUT)]
    #[cfg_attr(not(dns_tests_enabled), ignore)]
    fn test_cluster_tls_connect_with_hostname_not_in_cert() {
        block_on_all(async move {
            let result = build_tls_cluster_client(HOSTNAME_NO_TLS).await;
            assert!(result.is_none());
        });
    }
}
