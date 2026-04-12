//! Integration test: real-world workload patterns against a live cluster.
//!
//! Exercises the same Redis operations that apps like rusty-celery,
//! job queues, and caching layers depend on. Validates that ferriskey's
//! optimized direct-dispatch path handles all command types correctly.
//!
//! Requires: VALKEY_HOST and VALKEY_PORT env vars pointing to a cluster.
//! Skip with: cargo test --test test_integration_real_workload -- --ignored

use ferriskey::{
    cmd, ConnectionAddr, ConnectionInfo, Value, ValkeyConnectionInfo,
};
use ferriskey::connection::ConnectionLike;
use ferriskey::cluster::compat::ClusterClientBuilder;
use ferriskey::cluster::ClusterConnection;
use ferriskey::pipeline::Pipeline;

fn connection_info() -> Option<ConnectionInfo> {
    let host = std::env::var("VALKEY_HOST").ok()?;
    let port: u16 = std::env::var("VALKEY_PORT").ok()?.parse().ok()?;
    let tls = std::env::var("VALKEY_TLS").unwrap_or_default() == "true";
    Some(ConnectionInfo {
        addr: if tls {
            ConnectionAddr::TcpTls {
                host: host.clone(),
                port,
                insecure: true,
                tls_params: None,
            }
        } else {
            ConnectionAddr::Tcp(host, port)
        },
        valkey: ValkeyConnectionInfo::default(),
    })
}

async fn get_conn() -> Option<ClusterConnection> {
    let info = connection_info()?;
    let mut builder = ClusterClientBuilder::new(vec![info]);
    if std::env::var("VALKEY_TLS").unwrap_or_default() == "true" {
        builder = builder.tls(ferriskey::cluster::compat::TlsMode::Insecure);
    }
    let client = builder.build().ok()?;
    client.get_async_connection(None, None, None).await.ok()
}

/// Helper: skip test if no cluster available
macro_rules! require_cluster {
    () => {
        match get_conn().await {
            Some(c) => c,
            None => {
                eprintln!("Skipping: VALKEY_HOST not set");
                return;
            }
        }
    };
}

#[tokio::test]
#[ignore] // Requires live cluster
async fn test_basic_get_set() {
    let mut conn = require_cluster!();
    let _ = conn.req_packed_command(&cmd("SET").arg("itest:basic").arg("hello")).await.unwrap();
    let val = conn.req_packed_command(&cmd("GET").arg("itest:basic")).await.unwrap();
    assert_eq!(val, Value::BulkString(bytes::Bytes::from_static(b"hello")));
    let _ = conn.req_packed_command(&cmd("DEL").arg("itest:basic")).await;
}

#[tokio::test]
#[ignore]
async fn test_list_operations_rpop_lpush() {
    // Same pattern as rusty-celery's broker
    let mut conn = require_cluster!();
    let key = "itest:queue";
    let _ = conn.req_packed_command(&cmd("DEL").arg(key)).await;

    // LPUSH 3 items
    let _ = conn.req_packed_command(&cmd("LPUSH").arg(key).arg("task1")).await.unwrap();
    let _ = conn.req_packed_command(&cmd("LPUSH").arg(key).arg("task2")).await.unwrap();
    let _ = conn.req_packed_command(&cmd("LPUSH").arg(key).arg("task3")).await.unwrap();

    // RPOP should return in FIFO order (task1 first)
    let val = conn.req_packed_command(&cmd("RPOP").arg(key)).await.unwrap();
    assert_eq!(val, Value::BulkString(bytes::Bytes::from_static(b"task1")));

    let val = conn.req_packed_command(&cmd("RPOP").arg(key)).await.unwrap();
    assert_eq!(val, Value::BulkString(bytes::Bytes::from_static(b"task2")));

    let _ = conn.req_packed_command(&cmd("DEL").arg(key)).await;
}

#[tokio::test]
#[ignore]
async fn test_hash_operations_hset_hdel() {
    // Same pattern as rusty-celery's task result tracking
    let mut conn = require_cluster!();
    let key = "itest:results";
    let _ = conn.req_packed_command(&cmd("DEL").arg(key)).await;

    // HSET
    let _ = conn.req_packed_command(&cmd("HSET").arg(key).arg("task-123").arg("SUCCESS")).await.unwrap();
    let _ = conn.req_packed_command(&cmd("HSET").arg(key).arg("task-456").arg("FAILURE")).await.unwrap();

    // HGET
    let val = conn.req_packed_command(&cmd("HGET").arg(key).arg("task-123")).await.unwrap();
    assert_eq!(val, Value::BulkString(bytes::Bytes::from_static(b"SUCCESS")));

    // HDEL
    let _ = conn.req_packed_command(&cmd("HDEL").arg(key).arg("task-123")).await.unwrap();
    let val = conn.req_packed_command(&cmd("HGET").arg(key).arg("task-123")).await.unwrap();
    assert_eq!(val, Value::Nil);

    let _ = conn.req_packed_command(&cmd("DEL").arg(key)).await;
}

#[tokio::test]
#[ignore]
async fn test_mget_multi_key() {
    let mut conn = require_cluster!();
    // Use hash tags to ensure same slot
    let _ = conn.req_packed_command(&cmd("SET").arg("{itest}:k1").arg("v1")).await.unwrap();
    let _ = conn.req_packed_command(&cmd("SET").arg("{itest}:k2").arg("v2")).await.unwrap();
    let _ = conn.req_packed_command(&cmd("SET").arg("{itest}:k3").arg("v3")).await.unwrap();

    let val = conn.req_packed_command(
        &cmd("MGET").arg("{itest}:k1").arg("{itest}:k2").arg("{itest}:k3")
    ).await.unwrap();

    if let Value::Array(items) = val {
        assert_eq!(items.len(), 3);
        assert_eq!(items[0], Value::BulkString(bytes::Bytes::from_static(b"v1")));
        assert_eq!(items[1], Value::BulkString(bytes::Bytes::from_static(b"v2")));
        assert_eq!(items[2], Value::BulkString(bytes::Bytes::from_static(b"v3")));
    } else {
        panic!("Expected Array, got {:?}", val);
    }

    let _ = conn.req_packed_command(&cmd("DEL").arg("{itest}:k1").arg("{itest}:k2").arg("{itest}:k3")).await;
}

#[tokio::test]
#[ignore]
async fn test_pipeline_atomic() {
    let mut conn = require_cluster!();
    let key = "{itest:pipe}:counter";
    let _ = conn.req_packed_command(&cmd("SET").arg(key).arg("0")).await;

    // Pipeline: INCR 3 times atomically
    let mut pipe = Pipeline::new();
    pipe.atomic();
    let mut c1 = cmd("INCR"); c1.arg(key); pipe.add_command(c1);
    let mut c2 = cmd("INCR"); c2.arg(key); pipe.add_command(c2);
    let mut c3 = cmd("INCR"); c3.arg(key); pipe.add_command(c3);

    let results: Vec<Value> = pipe.query_async(&mut conn).await.unwrap();
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], Value::Int(1));
    assert_eq!(results[1], Value::Int(2));
    assert_eq!(results[2], Value::Int(3));

    let _ = conn.req_packed_command(&cmd("DEL").arg(key)).await;
}

#[tokio::test]
#[ignore]
async fn test_ping() {
    let mut conn = require_cluster!();
    let val = conn.req_packed_command(&cmd("PING")).await.unwrap();
    // PING returns SimpleString "PONG" or Okay depending on protocol version
    match val {
        Value::SimpleString(s) => assert_eq!(s, "PONG"),
        Value::BulkString(b) => assert_eq!(&b[..], b"PONG"),
        Value::Okay => {} // RESP3 may return OK
        other => panic!("Unexpected PING response: {:?}", other),
    }
}

#[tokio::test]
#[ignore]
async fn test_expiry_and_ttl() {
    let mut conn = require_cluster!();
    let key = "itest:expiry";

    let _ = conn.req_packed_command(&cmd("SET").arg(key).arg("temp").arg("EX").arg("10")).await.unwrap();

    let ttl = conn.req_packed_command(&cmd("TTL").arg(key)).await.unwrap();
    if let Value::Int(t) = ttl {
        assert!(t > 0 && t <= 10, "TTL should be between 1-10, got {t}");
    } else {
        panic!("Expected Int, got {:?}", ttl);
    }

    let _ = conn.req_packed_command(&cmd("DEL").arg(key)).await;
}

#[tokio::test]
#[ignore]
async fn test_cross_slot_pipeline_non_atomic() {
    let mut conn = require_cluster!();
    // Keys WITHOUT hash tags → likely different slots
    let _ = conn.req_packed_command(&cmd("SET").arg("itest:xslot:a").arg("1")).await.unwrap();
    let _ = conn.req_packed_command(&cmd("SET").arg("itest:xslot:b").arg("2")).await.unwrap();

    // Non-atomic pipeline across slots — exercises our direct dispatch splitting
    let mut pipe = Pipeline::new();
    let mut c1 = cmd("GET"); c1.arg("itest:xslot:a"); pipe.add_command(c1);
    let mut c2 = cmd("GET"); c2.arg("itest:xslot:b"); pipe.add_command(c2);

    let results: Vec<Value> = pipe.query_async(&mut conn).await.unwrap();
    assert_eq!(results.len(), 2);
    assert_eq!(results[0], Value::BulkString(bytes::Bytes::from_static(b"1")));
    assert_eq!(results[1], Value::BulkString(bytes::Bytes::from_static(b"2")));

    let _ = conn.req_packed_command(&cmd("DEL").arg("itest:xslot:a")).await;
    let _ = conn.req_packed_command(&cmd("DEL").arg("itest:xslot:b")).await;
}
