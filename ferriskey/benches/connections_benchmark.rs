// Copyright Valkey GLIDE Project Contributors - SPDX Identifier: Apache-2.0

use criterion::{Criterion, criterion_group, criterion_main};
use futures::future::join_all;
use ferriskey::valkey::{
    ConnectionAddr, ConnectionInfo, GlideConnectionOptions, RedisConnectionInfo,
    RedisResult, Value,
    aio::{ConnectionLike, MultiplexedConnection},
    cluster::ClusterClientBuilder,
    cluster_async::ClusterConnection,
    cmd,
};
use std::env;
use tokio::runtime::{Builder, Runtime};

trait BenchClient: ConnectionLike + Send + Clone {}

impl BenchClient for MultiplexedConnection {}
impl BenchClient for ClusterConnection {}

async fn run_get(mut connection: impl BenchClient) -> RedisResult<Value> {
    connection.req_packed_command(&cmd("GET").arg("foo")).await
}

fn benchmark_single_get(
    c: &mut Criterion,
    connection_id: &str,
    test_group: &str,
    connection: impl BenchClient,
    runtime: &Runtime,
) {
    let mut group = c.benchmark_group(test_group);
    group.significance_level(0.1).sample_size(500);
    group.bench_function(format!("{connection_id}-single get"), move |b| {
        b.to_async(runtime).iter(|| run_get(connection.clone()));
    });
}

fn benchmark_concurrent_gets(
    c: &mut Criterion,
    connection_id: &str,
    test_group: &str,
    connection: impl BenchClient,
    runtime: &Runtime,
) {
    let mut group = c.benchmark_group(test_group);
    group.significance_level(0.1).sample_size(100);

    for concurrency in [10, 100, 1000] {
        group.bench_function(
            format!("{connection_id}-concurrent get x{concurrency}"),
            |b| {
                b.to_async(runtime).iter(|| {
                    let conn = connection.clone();
                    async move {
                        let futs: Vec<_> = (0..concurrency)
                            .map(|_| run_get(conn.clone()))
                            .collect();
                        join_all(futs).await
                    }
                });
            },
        );
    }
}

async fn run_set(mut connection: impl BenchClient, value: &[u8]) -> RedisResult<Value> {
    connection
        .req_packed_command(&cmd("SET").arg("bench_key").arg(value))
        .await
}

fn benchmark_set_sizes(
    c: &mut Criterion,
    connection_id: &str,
    test_group: &str,
    connection: impl BenchClient,
    runtime: &Runtime,
) {
    let mut group = c.benchmark_group(test_group);
    group.significance_level(0.1).sample_size(200);

    for size in [64, 1024, 4096, 65536] {
        let value = vec![b'x'; size];
        let conn = connection.clone();
        group.bench_function(
            format!("{connection_id}-set {size}B"),
            move |b| {
                b.to_async(runtime).iter(|| run_set(conn.clone(), &value));
            },
        );
    }
}

fn get_connection_info(host: &str, port: u16, tls: bool) -> ConnectionInfo {
    ConnectionInfo {
        addr: if tls {
            ConnectionAddr::TcpTls {
                host: host.to_string(),
                port,
                insecure: true,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(host.to_string(), port)
        },
        redis: RedisConnectionInfo::default(),
    }
}

fn create_runtime() -> Runtime {
    Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_cpus::get())
        .build()
        .unwrap()
}

fn multiplexed_benchmarks(c: &mut Criterion) {
    let host = env::var("VALKEY_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port: u16 = env::var("VALKEY_PORT")
        .unwrap_or_else(|_| "6379".to_string())
        .parse()
        .unwrap();
    let tls = env::var("VALKEY_TLS").unwrap_or_else(|_| "false".to_string()) == "true";

    let runtime = create_runtime();
    let connection_info = get_connection_info(&host, port, tls);

    let connection: MultiplexedConnection = runtime.block_on(async {
        let client = ferriskey::valkey::Client::open(connection_info).unwrap();
        client
            .get_multiplexed_async_connection(GlideConnectionOptions::default())
            .await
            .unwrap()
    });

    benchmark_single_get(c, "multiplexed", "standalone", connection.clone(), &runtime);
    benchmark_concurrent_gets(c, "multiplexed", "standalone-concurrent", connection.clone(), &runtime);
    benchmark_set_sizes(c, "multiplexed", "standalone-set", connection, &runtime);
}

fn cluster_benchmarks(c: &mut Criterion) {
    let host = env::var("VALKEY_CLUSTER_HOST").unwrap_or_default();
    let port: u16 = env::var("VALKEY_CLUSTER_PORT")
        .unwrap_or_else(|_| "6379".to_string())
        .parse()
        .unwrap();
    let tls = env::var("VALKEY_TLS").unwrap_or_else(|_| "false".to_string()) == "true";

    if host.is_empty() {
        eprintln!("VALKEY_CLUSTER_HOST not set, skipping cluster benchmarks");
        return;
    }

    let runtime = create_runtime();
    let connection_info = get_connection_info(&host, port, tls);

    let connection: ClusterConnection = runtime.block_on(async {
        let client = ClusterClientBuilder::new(vec![connection_info])
            .tls(if tls {
                ferriskey::valkey::cluster::TlsMode::Insecure
            } else {
                ferriskey::valkey::cluster::TlsMode::Secure
            })
            .build()
            .unwrap();
        client.get_async_connection(None, None, None).await.unwrap()
    });

    benchmark_single_get(c, "cluster", "cluster", connection.clone(), &runtime);
    benchmark_concurrent_gets(c, "cluster", "cluster-concurrent", connection.clone(), &runtime);
    benchmark_set_sizes(c, "cluster", "cluster-set", connection, &runtime);
}

criterion_group!(benches, multiplexed_benchmarks, cluster_benchmarks);
criterion_main!(benches);
