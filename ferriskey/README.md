# ferriskey

A Rust client for [Valkey](https://valkey.io), built for [FlowFabric](https://github.com/avifenesh/FlowFabric).

Ferriskey descends from [glide-core](https://github.com/valkey-io/valkey-glide) (part of valkey-glide), which itself evolved from redis-rs. It has been refactored for FlowFabric's needs:

- **ClientBuilder API** -- host/port/tls/cluster configuration without URL parsing
- **First-class FCALL** -- `client.fcall()` and `client.fcall_readonly()` for Valkey Functions
- **Function management** -- `function_load_replace()`, `function_list()`, `function_delete()`
- **Typed pipelines** -- `TypedPipeline` with slot-based result extraction
- **RESP3 protocol** -- native support for all Valkey value types
- **Cluster mode** -- automatic slot routing, redirection handling, read replicas
- **TLS** -- rustls-based, platform certificate verification
- **AWS IAM auth** -- ElastiCache/MemoryDB token-based authentication
- **Compression** -- client-side zstd/lz4 support

## Quick start

```rust
use ferriskey::ClientBuilder;

#[tokio::main]
async fn main() -> ferriskey::Result<()> {
    let client = ClientBuilder::new()
        .host("localhost", 6379)
        .build()
        .await?;

    // Basic key/value
    let _: () = client.set("key", "value").await?;
    let val: Option<String> = client.get("key").await?;
    assert_eq!(val.as_deref(), Some("value"));

    // Raw command
    let pong: String = client.cmd("PING").execute().await?;
    assert_eq!(pong, "PONG");

    // FCALL (Valkey Functions)
    let result: ferriskey::Value = client
        .fcall("my_function", &["key1"], &["arg1"])
        .await?;

    Ok(())
}
```

## Cluster mode

```rust
let client = ClientBuilder::new()
    .host("node1", 6379)
    .cluster()
    .tls()
    .build()
    .await?;
```

## Typed pipelines

```rust
let mut pipe = client.pipeline();
let slot_a = pipe.set("a", "1");
let slot_b = pipe.get::<String>("a");
pipe.execute().await?;

let val: Option<String> = slot_b.value()?;
```

## Heritage

This project incorporates code from:

- [valkey-glide](https://github.com/valkey-io/valkey-glide) (Apache-2.0)
- [redis-rs](https://github.com/redis-rs/redis-rs) (BSD-3-Clause)

## License

Apache-2.0
