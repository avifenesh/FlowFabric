# FerrisKey API Design Plan

No migration needed — no existing users. Clean break from inherited redis-rs/glide API.

## Entry Points

```rust
// Simple
let client = ferriskey::connect("valkey://localhost:6379").await?;
let client = ferriskey::connect_cluster(&["valkey://node1:6379", "valkey://node2:6379"]).await?;

// Advanced
let client = ferriskey::ClientBuilder::new()
    .host("localhost", 6379)
    .tls()
    .password("secret")
    .read_from(ReadFrom::PreferReplica)
    .build()
    .await?;
```

## Client — Single Type, Clone is O(1)

```rust
pub struct Client(Arc<ClientInner>);

// String
let val: String = client.get("key").await?;
client.set("key", "value").await?;
client.set_ex("key", "value", Duration::from_secs(60)).await?;
let old: Option<String> = client.get_set("key", "new_val").await?;

// Hash
client.hset("user:1", "name", "alice").await?;
let name: String = client.hget("user:1", "name").await?;
let all: HashMap<String, String> = client.hgetall("user:1").await?;

// List (queue pattern)
client.lpush("queue", &["task1", "task2"]).await?;
let task: Option<String> = client.rpop("queue", None).await?;

// Low-level escape hatch — first class, not afterthought
let n: i64 = client.cmd("INCRBY").arg("hits").arg(5).execute().await?;
```

## Pipeline — Typed Slots

```rust
let mut pipe = client.pipeline();
let name  = pipe.get::<String>("user:1:name");
let score = pipe.get::<f64>("user:1:score");
let _     = pipe.set("user:1:name", "alice");
pipe.execute().await?;

let name: Option<String> = name.value()?;
let score: Option<f64> = score.value()?;

// Transaction (MULTI/EXEC) — separate type, prevents confusion
let results = client.transaction()
    .incr("counter")
    .set("last_updated", timestamp)
    .execute().await?;
```

## PubSub — Owned Subscriber, Stream Trait

```rust
let mut sub = client.subscriber()
    .channel("events:orders")
    .pattern("alerts:*")
    .subscribe()
    .await?;

while let Some(msg) = sub.next().await {
    match msg? {
        Message::Channel { channel, payload } => { ... }
        Message::Pattern { pattern, channel, payload } => { ... }
        Message::Reconnected => { /* may have missed messages */ }
    }
}
```

## Error — One Type, MOVED/ASK Never Exposed

```rust
pub struct Error { kind: ErrorKind, source: Option<Box<dyn std::error::Error + Send + Sync>> }

pub enum ErrorKind {
    Connection,
    Timeout,
    Authentication,
    Permission,
    WrongType,
    ServerError(String),
    Configuration,
    Cluster,
    Io(std::io::ErrorKind),
}
```

## ClientBuilder — Replaces ConnectionRequest

```rust
pub struct ClientBuilder { ... }

impl ClientBuilder {
    pub fn new() -> Self;
    pub fn host(self, host: &str, port: u16) -> Self;
    pub fn url(self, url: &str) -> Result<Self>;
    pub fn cluster(self) -> Self;
    pub fn tls(self) -> Self;
    pub fn tls_insecure(self) -> Self;
    pub fn password(self, pw: &str) -> Self;
    pub fn username(self, u: impl Into<String>) -> Self;
    pub fn iam(self, config: IamConfig) -> Self;
    pub fn database(self, db: i64) -> Self;
    pub fn read_from(self, strategy: ReadFrom) -> Self;
    pub fn connect_timeout(self, t: Duration) -> Self;
    pub fn request_timeout(self, t: Duration) -> Self;
    pub fn max_inflight(self, n: u32) -> Self;
    pub async fn build(self) -> Result<Client>;
}
```

## Module Structure (Internal)

```
src/
  lib.rs              — pub: connect(), Client, Error, Value, Pipeline, Subscriber
  client.rs           — Client impl, ClientBuilder, ClientInner enum
  error.rs            — Error, ErrorKind
  value.rs            — Value, FromValue, ToArgs (renamed from types.rs subset)
  cmd.rs              — Cmd, CommandBuilder (internal cmd building)
  pipeline.rs         — Pipeline, PipeSlot, Transaction
  pubsub.rs           — Subscriber, SubscriberBuilder, Message
  protocol/
    parser.rs         — RESP parser (internal)
    codec.rs          — ValueCodec (internal)
  connection/
    multiplexed.rs    — MultiplexedConnection (internal)
    reconnecting.rs   — ReconnectingConnection (internal)
    tls.rs            — TLS setup (internal)
  cluster/
    mod.rs            — ClusterClient (internal)
    routing.rs        — slot hash, routing info
    topology.rs       — slot map, topology refresh
    container.rs      — connections container
    pipeline.rs       — cross-slot pipeline splitting
  iam/                — IAM token management
  compression/        — request/response compression
  telemetry/          — OTel integration
```

## What's Public vs Internal

| Public | Internal |
|---|---|
| `Client`, `ClientBuilder` | `MultiplexedConnection`, `ClusterConnection` |
| `Pipeline`, `Transaction`, `PipeSlot` | `ValueCodec`, `PipelineSink` |
| `Subscriber`, `Message` | `PubSubSynchronizer`, `PushManager` |
| `Error`, `ErrorKind` | `ServerError`, `ServerErrorKind` |
| `Value`, `FromValue`, `ToArgs` | `ValkeyConnectionInfo`, `ConnectionInfo` |
| `cmd()`, `CommandBuilder` | `get_packed_command()`, `write_command()` |
| `connect()`, `connect_cluster()` | `ConnectionLike`, `Connect` trait |

## Implementation Plan

No phased migration. Direct replacement:

1. Create new `src/client.rs` with `Client` struct wrapping existing `ClientInner`
2. Add convenience methods as thin wrappers over `cmd().execute()`
3. Move `valkey/` contents into `src/` subdirectories (protocol/, connection/, cluster/)
4. Delete `valkey/mod.rs`, update `lib.rs` to export from new locations
5. Delete old `client/mod.rs` `Client` and `ConnectionRequest`
6. Update tests to use new API
