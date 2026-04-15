//! Integration tests for FCALL convenience methods on the high-level Client.
//!
//! Requires: a live Valkey/Redis 7+ standalone server.
//! Run with: cargo test -p ferriskey --test test_fcall

use ferriskey::Value;

/// Trivial Lua library for testing FCALL.
const TEST_LIB: &str = "#!lua name=ferriskey_test\n\
redis.register_function('ft_echo', function(keys, args) return args[1] end)\n\
redis.register_function{function_name='ft_read', callback=function(keys, args) return redis.call('GET', keys[1]) end, flags={'no-writes'}}\n\
";

async fn connect() -> ferriskey::Client {
    ferriskey::connect("redis://127.0.0.1:6379")
        .await
        .expect("failed to connect to local Valkey")
}

#[tokio::test]
#[serial_test::serial]
async fn test_function_load_and_fcall_echo() {
    let client = connect().await;

    // Load the test library (REPLACE so idempotent)
    let name: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("FUNCTION LOAD failed");
    assert_eq!(name, "ferriskey_test");

    // FCALL ft_echo with one key (for routing) and one arg
    let result: String = client
        .fcall("ft_echo", &["{test}:k1"], &["hello_world"])
        .await
        .expect("FCALL failed");
    assert_eq!(result, "hello_world");
}

#[tokio::test]
#[serial_test::serial]
async fn test_function_load_replace_overwrites() {
    let client = connect().await;

    // Load once
    let name1: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("first FUNCTION LOAD failed");
    assert_eq!(name1, "ferriskey_test");

    // Load again with REPLACE — should succeed, not error
    let name2: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("second FUNCTION LOAD (replace) failed");
    assert_eq!(name2, "ferriskey_test");
}

#[tokio::test]
#[serial_test::serial]
async fn test_function_list_finds_library() {
    let client = connect().await;

    // Ensure library is loaded
    let _: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("FUNCTION LOAD failed");

    // FUNCTION LIST LIBRARYNAME ferriskey_test
    let val = client
        .function_list("ferriskey_test")
        .await
        .expect("FUNCTION LIST failed");

    // Response is an Array with one entry per matching library.
    // Each entry is a Map or Array of key-value pairs.
    match val {
        Value::Array(ref entries) => {
            assert!(
                !entries.is_empty(),
                "FUNCTION LIST should return at least one library, got empty array"
            );
        }
        Value::Map(ref entries) => {
            assert!(
                !entries.is_empty(),
                "FUNCTION LIST should return at least one library, got empty map"
            );
        }
        other => panic!(
            "Expected Array or Map from FUNCTION LIST, got: {:?}",
            other
        ),
    }
}

#[tokio::test]
#[serial_test::serial]
async fn test_function_delete_removes_library() {
    let client = connect().await;

    // Ensure library is loaded
    let _: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("FUNCTION LOAD failed");

    // Delete it
    client
        .function_delete("ferriskey_test")
        .await
        .expect("FUNCTION DELETE failed");

    // FCALL should now fail
    let result: ferriskey::Result<String> =
        client.fcall("ft_echo", &["{test}:k1"], &["x"]).await;
    assert!(result.is_err(), "FCALL should fail after FUNCTION DELETE");

    // Reload for subsequent tests
    let _: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("re-load failed");
}

#[tokio::test]
#[serial_test::serial]
async fn test_fcall_readonly() {
    let client = connect().await;

    // Ensure library is loaded
    let _: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("FUNCTION LOAD failed");

    // Set a key to read back
    client
        .set("{test}:readonly_k", "readonly_val")
        .await
        .expect("SET failed");

    // FCALL_RO ft_read — the function is flagged no-writes, reads a key
    let result: String = client
        .fcall_readonly("ft_read", &["{test}:readonly_k"], &[] as &[&str])
        .await
        .expect("FCALL_RO failed");
    assert_eq!(result, "readonly_val");

    // Cleanup
    client.del(&["{test}:readonly_k"]).await.ok();
}

#[tokio::test]
#[serial_test::serial]
async fn test_fcall_no_keys() {
    let client = connect().await;

    // Ensure library is loaded
    let _: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("FUNCTION LOAD failed");

    // FCALL with zero keys — still works (routes to random node on cluster)
    let result: String = client
        .fcall("ft_echo", &[] as &[&str], &["no_key_test"])
        .await
        .expect("FCALL with 0 keys failed");
    assert_eq!(result, "no_key_test");
}

#[tokio::test]
#[serial_test::serial]
async fn test_pipeline_fcall() {
    let client = connect().await;

    // Ensure library is loaded
    let _: String = client
        .function_load_replace(TEST_LIB)
        .await
        .expect("FUNCTION LOAD failed");

    let mut pipe = client.pipeline();
    let slot1 = pipe.fcall::<String>("ft_echo", &["{test}:p1"], &["pipe_a"]);
    let slot2 = pipe.fcall::<String>("ft_echo", &["{test}:p1"], &["pipe_b"]);
    pipe.execute().await.expect("pipeline execute failed");

    assert_eq!(slot1.value().expect("slot1"), "pipe_a");
    assert_eq!(slot2.value().expect("slot2"), "pipe_b");
}
