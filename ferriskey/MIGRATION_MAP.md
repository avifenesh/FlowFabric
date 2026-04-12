# Module Structure Migration Map

Maps every file move from current structure to target (per API_DESIGN.md Module Structure).
For each file: source path, target path, and every file that imports from it.

**Do NOT move files using this document. Research only.**

---

## Summary

| Category | Files Moving | Files Needing Import Updates |
|---|---|---|
| `valkey/` → `protocol/` | 2 | 5 direct + all re-export consumers |
| `valkey/` → `connection/` | 6 | 18 direct |
| `valkey/` → `cluster/` | 9 | 14 direct |
| `valkey/` → `src/` root (flatten) | 6 | 22 direct |
| `valkey/mod.rs` → deleted | 1 | re-exports move to `lib.rs` |
| **Total files moving** | **24** | |
| **Total files needing `use` updates** | **45** | (36 internal + 9 tests/benches) |

---

## Phase 1: Protocol

### src/valkey/parser.rs → src/protocol/parser.rs

RESP parser, `ValueCodec`, `Parser`, `parse_valkey_value`.

Imported by:
- `src/valkey/mod.rs:17,60` — `pub use crate::valkey::parser::{parse_valkey_value, Parser}` + `parse_valkey_value_async`
- `src/valkey/aio/multiplexed_connection.rs:7` — `use crate::valkey::parser::ValueCodec`
- `src/valkey/cluster_routing.rs:1503` (test) — `use crate::valkey::{..., parser::parse_valkey_value, ...}`
- `src/valkey/parser.rs:3` (self) — `use crate::valkey::types::{...}` (needs update to new types location)

Note: API_DESIGN.md splits this into `protocol/parser.rs` + `protocol/codec.rs`. ValueCodec can be extracted in a follow-up.

### src/valkey/aio/mod.rs (codec portion) → src/protocol/codec.rs

The `ValueCodec` actually lives in `parser.rs`. The `aio/mod.rs` contains `ConnectionLike` trait, `setup_connection`, `DisconnectNotifier`, and helper functions — these go to `connection/`, not `protocol/`.

If ValueCodec is extracted from parser.rs into its own file:

Imported by:
- `src/valkey/aio/multiplexed_connection.rs:7` — `use crate::valkey::parser::ValueCodec`

---

## Phase 2: Connection

### src/valkey/aio/mod.rs → src/connection/mod.rs

Contains: `ConnectionLike` trait, `DisconnectNotifier`, `setup_connection`, `get_socket_addrs`, `AsyncStream`.

Imported by:
- `src/valkey/aio/multiplexed_connection.rs:1` — `use super::ConnectionLike`
- `src/valkey/aio/multiplexed_connection.rs:4` — `use crate::valkey::aio::DisconnectNotifier`
- `src/valkey/cluster_async/mod.rs:35` — `use crate::valkey::aio::{get_socket_addrs, ConnectionLike, DisconnectNotifier, MultiplexedConnection}`
- `src/valkey/cluster_async/connections_logic.rs:7` — `use crate::valkey::aio::{ConnectionLike, DisconnectNotifier}`
- `src/valkey/cluster_async/pipeline_routing.rs:1` — `use crate::valkey::aio::ConnectionLike`
- `src/valkey/cluster_scan.rs:43` — `use crate::valkey::aio::ConnectionLike`
- `src/valkey/cmd.rs:44` — `use crate::valkey::aio::ConnectionLike as AsyncConnection`
- `src/client/mod.rs:14` — `use crate::valkey::aio::ConnectionLike`
- `src/client/reconnecting_connection.rs:7` — `use crate::valkey::aio::{DisconnectNotifier, MultiplexedConnection}`
- `src/client/standalone_client.rs:10` — `use crate::valkey::aio::ConnectionLike`

### src/valkey/aio/multiplexed_connection.rs → src/connection/multiplexed.rs

Contains: `MultiplexedConnection`.

Imported by (via re-export through `aio` module):
- `src/valkey/cluster_async/mod.rs:35` — `MultiplexedConnection` via `crate::valkey::aio`
- `src/client/reconnecting_connection.rs:7` — `MultiplexedConnection` via `crate::valkey::aio`

Internal imports (self — need updating):
- `super::ConnectionLike` (line 1)
- `super::runtime` (line 2)
- `crate::valkey::aio::setup_connection` (line 3)
- `crate::valkey::aio::DisconnectNotifier` (line 4)
- `crate::valkey::client::FerrisKeyConnectionOptions` (line 5)
- `crate::valkey::cmd::Cmd` (line 6)
- `crate::valkey::parser::ValueCodec` (line 7)
- `crate::valkey::pipeline::PipelineRetryStrategy` (line 8)
- `crate::valkey::push_manager::PushManager` (line 9)
- `crate::valkey::types::{ValkeyError, ValkeyResult, Value}` (line 10)
- `crate::valkey::{cmd, ConnectionInfo, ProtocolVersion, PushKind}` (line 11)

### src/valkey/aio/runtime.rs → src/connection/runtime.rs

Async runtime abstraction.

Imported by:
- `src/valkey/aio/multiplexed_connection.rs:2` — `use super::runtime`

Internal imports (self):
- `crate::valkey::types::ValkeyError` (line 4)

### src/valkey/aio/tokio.rs → src/connection/tokio.rs

Tokio-specific TCP/TLS connect implementations.

Imported by:
- Only via `pub mod tokio;` in `src/valkey/aio/mod.rs:21`

Internal imports (self):
- `super::{AsyncStream, ValkeyResult, RedisRuntime, SocketAddr}` (line 1)
- `super::Path` (line 25)
- `crate::valkey::connection::create_rustls_config` (line 18)
- `crate::valkey::tls::TlsConnParams` (line 22)

### src/valkey/tls.rs → src/connection/tls.rs

TLS certificate loading and configuration.

Imported by:
- `src/valkey/mod.rs:85` — `pub use crate::valkey::tls::{retrieve_tls_certificates, ClientTlsConfig, TlsCertificates, TlsConnParams}`
- `src/valkey/connection.rs:12` — `use crate::valkey::tls::TlsConnParams`
- `src/valkey/connection.rs:348` (test) — `use crate::valkey::tls::ClientTlsParams`
- `src/valkey/aio/mod.rs:18` — `use crate::valkey::tls::TlsConnParams`
- `src/valkey/aio/tokio.rs:22` — `use crate::valkey::tls::TlsConnParams`
- `src/valkey/client.rs:17` — `use crate::valkey::tls::{inner_build_with_tls, TlsCertificates}`
- `src/valkey/cluster_client.rs:14` — `use crate::valkey::tls::TlsConnParams`
- `src/valkey/cluster_client.rs:18` — `use crate::valkey::tls::{retrieve_tls_certificates, TlsCertificates}`
- `src/valkey/cluster.rs:9` — `use crate::valkey::tls::TlsConnParams`
- `src/client/mod.rs:211` — `use crate::valkey::{TlsCertificates, retrieve_tls_certificates}` (via re-export)

Internal imports (self):
- `crate::valkey::{Client, ConnectionAddr, ConnectionInfo, ErrorKind, ValkeyError, ValkeyResult}` (line 7)

### src/valkey/connection.rs → src/connection/info.rs (or merge into connection/mod.rs)

Contains: `ConnectionInfo`, `ConnectionAddr`, `ValkeyConnectionInfo`, `IntoConnectionInfo`, `TlsMode`, `PubSubSubscriptionKind`, `parse_redis_url`, `create_rustls_config`.

Imported by:
- `src/valkey/mod.rs:11-16` — massive pub use block
- `src/valkey/push_manager.rs:1` — `use crate::valkey::connection::PubSubSubscriptionKind`
- `src/valkey/pubsub_synchronizer.rs:4` — `use crate::valkey::connection::{PubSubChannelOrPattern, PubSubSubscriptionKind}`
- `src/valkey/cluster_client.rs:5` — `use crate::valkey::connection::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo}`
- `src/valkey/cluster_client.rs:7` — `use crate::valkey::connection::TlsMode`
- `src/valkey/cluster_topology.rs:7` — `use crate::valkey::{connection::TlsMode, ...}`
- `src/valkey/cluster.rs:8` — `use crate::valkey::connection::{ConnectionAddr, ConnectionInfo, ValkeyConnectionInfo}`
- `src/valkey/aio/mod.rs:3` — `use crate::valkey::connection::{get_resp3_hello_command_error, ValkeyConnectionInfo}`
- `src/valkey/aio/mod.rs:262` — `use crate::valkey::connection::ConnectionAddr`
- `src/valkey/aio/tokio.rs:18` — `use crate::valkey::connection::create_rustls_config`

Internal imports (self):
- `crate::valkey::pipeline::Pipeline` (line 8)
- `crate::valkey::types::{ErrorKind, ValkeyError, ValkeyResult}` (line 9)
- `crate::valkey::ProtocolVersion` (line 10)
- `crate::valkey::tls::TlsConnParams` (line 12)

---

## Phase 3: Cluster

### src/valkey/cluster_async/mod.rs → src/cluster/mod.rs

Contains: `ClusterConnection`, `ClusterConnInner`, `InnerCore`, `Core`, `Connect` trait.

Imported by:
- `src/valkey/cluster_scan.rs:44` — `use crate::valkey::cluster_async::{ClusterConnInner, Connect, InnerCore, RefreshPolicy}`
- `src/valkey/cluster_client.rs:16` — `use crate::valkey::cluster_async`
- `src/client/mod.rs:15` — `use crate::valkey::cluster_async::ClusterConnection`
- `src/valkey/cluster_async/mod.rs:1138` — `pub use crate::valkey::types::InflightRequestTracker`
- Tests: `tests/utilities/cluster.rs`, `tests/test_client.rs`, `benches/connections_benchmark.rs`

Internal imports (self — massive block, lines 34-57):
- `crate::valkey::aio::*`, `crate::valkey::client::*`, `crate::valkey::cluster::*`
- `crate::valkey::cluster_client::*`, `crate::valkey::cluster_routing::*`
- `crate::valkey::cluster_scan::*`, `crate::valkey::cluster_slotmap::*`
- `crate::valkey::cluster_topology::*`, `crate::valkey::cmd`, `crate::valkey::push_manager::*`
- `crate::valkey::types::*`, `crate::valkey::Cmd`, `crate::valkey::ConnectionInfo`, etc.

### src/valkey/cluster_routing.rs → src/cluster/routing.rs

Contains: `Route`, `RoutingInfo`, `SlotAddr`, `Routable`, `ResponsePolicy`, `SingleNodeRoutingInfo`, `MultipleNodeRoutingInfo`, `Redirect`, `ShardAddrs`.

Imported by:
- `src/valkey/mod.rs:77` — `pub mod cluster_routing`
- `src/valkey/cluster_slotmap.rs:10` — `use crate::valkey::cluster_routing::{Route, ShardAddrs, Slot, SlotAddr}`
- `src/valkey/cluster_topology.rs:5` — `use crate::valkey::cluster_routing::Slot`
- `src/valkey/cluster_async/mod.rs:42-46` — `use crate::valkey::cluster_routing::{self, MultipleNodeRoutingInfo, Redirect, ResponsePolicy, Route, Routable, RoutingInfo, ShardUpdateResult, SingleNodeRoutingInfo}`
- `src/valkey/cluster_async/pipeline_routing.rs:4-8` — `use crate::valkey::cluster_routing::{RoutingInfo, SlotAddr, ...}`
- `src/valkey/cluster_async/connections_container.rs:2` — `use crate::valkey::cluster_routing::{Route, ShardAddrs, SlotAddr}`
- `src/valkey/cluster_scan.rs:45` — `use crate::valkey::cluster_routing::SlotAddr`
- `src/valkey/types.rs:16` — `use crate::valkey::cluster_routing::Redirect`
- `src/client/mod.rs:16-18` — `use crate::valkey::cluster_routing::{...}`
- `src/client/standalone_client.rs:11` — `use crate::valkey::cluster_routing::{self, ResponsePolicy, Routable, RoutingInfo, is_readonly_cmd}`
- `src/pubsub/mock.rs:8` — `use crate::valkey::cluster_routing::Routable`
- Tests: `tests/test_client.rs:32`

Internal imports (self):
- `crate::valkey::cluster_topology::get_slot` (line 4)
- `crate::valkey::cmd::{Arg, Cmd}` (line 5)
- `crate::valkey::types::Value` (line 6)
- `crate::valkey::{ErrorKind, ValkeyError, ValkeyResult}` (line 7)

### src/valkey/cluster_topology.rs → src/cluster/topology.rs

Contains: `calculate_topology`, `SlotRefreshState`, `TopologyHash`, `get_slot`, `SLOT_SIZE`.

Imported by:
- `src/valkey/mod.rs:78` — `pub mod cluster_topology`
- `src/valkey/cluster_routing.rs:4` — `use crate::valkey::cluster_topology::get_slot`
- `src/valkey/cluster_async/mod.rs:48-52` — `use crate::valkey::cluster_topology::{calculate_topology, SlotRefreshState, TopologyHash, ...}`
- `src/valkey/cluster_async/connections_container.rs:4` — `use crate::valkey::cluster_topology::TopologyHash`
- `src/valkey/cluster_scan.rs:46` — `use crate::valkey::cluster_topology::SLOT_SIZE`
- `src/valkey/cluster_client.rs:2-4` — `use crate::valkey::cluster_topology::{...}`
- Tests: `tests/test_client.rs:27`

Internal imports (self):
- `crate::valkey::cluster::get_connection_addr` (line 3)
- `crate::valkey::cluster_client::SlotsRefreshRateLimit` (line 4)
- `crate::valkey::cluster_routing::Slot` (line 5)
- `crate::valkey::cluster_slotmap::{ReadFromReplicaStrategy, SlotMap}` (line 6)
- `crate::valkey::{connection::TlsMode, ErrorKind, ValkeyError, ValkeyResult, Value}` (line 7)

### src/valkey/cluster_slotmap.rs → src/cluster/slotmap.rs

Contains: `SlotMap`, `ReadFromReplicaStrategy`, `SlotMapValue`.

Imported by:
- `src/valkey/mod.rs:68-69` — `pub mod cluster_slotmap` + `pub use cluster_slotmap::SlotMap`
- `src/valkey/cluster_topology.rs:6` — `use crate::valkey::cluster_slotmap::{ReadFromReplicaStrategy, SlotMap}`
- `src/valkey/cluster_async/mod.rs:47` — `use crate::valkey::cluster_slotmap::SlotMap`
- `src/valkey/cluster_async/connections_container.rs:3` — `use crate::valkey::cluster_slotmap::{ReadFromReplicaStrategy, SlotMap, SlotMapValue}`
- `src/valkey/cluster_async/connections_logic.rs:5` — `use crate::valkey::cluster_slotmap::ReadFromReplicaStrategy`
- `src/valkey/pubsub_synchronizer.rs:3` — `use crate::valkey::cluster_slotmap::SlotMap`
- `src/client/mod.rs:19` — `use crate::valkey::cluster_slotmap::ReadFromReplicaStrategy`

Internal imports (self):
- `crate::valkey::cluster_routing::{Route, ShardAddrs, Slot, SlotAddr}` (line 10)
- `crate::valkey::ErrorKind` (line 11)
- `crate::valkey::ValkeyError` (line 12)
- `crate::valkey::ValkeyResult` (line 13)

### src/valkey/cluster_async/connections_container.rs → src/cluster/container.rs

Contains: `ConnectionsContainer`, `ConnectionsMap`, `ClusterNode`, `ConnectionDetails`, `RefreshTaskNotifier`.

Imported by:
- `src/valkey/cluster_async/mod.rs:59-61` — `use connections_container::{ConnectionAndAddress, ConnectionType, ConnectionsMap, RefreshTaskNotifier, ...}`
- `src/valkey/cluster_async/mod.rs:31` (testing) — `pub use super::connections_container::ConnectionDetails`
- `src/valkey/cluster_async/connections_logic.rs:2` — `use super::connections_container::{ClusterNode, ConnectionDetails}`

Internal imports (self):
- `crate::valkey::cluster_async::ConnectionFuture` (line 1)
- `crate::valkey::cluster_routing::{Route, ShardAddrs, SlotAddr}` (line 2)
- `crate::valkey::cluster_slotmap::{ReadFromReplicaStrategy, SlotMap, SlotMapValue}` (line 3)
- `crate::valkey::cluster_topology::TopologyHash` (line 4)

### src/valkey/cluster_async/pipeline_routing.rs → src/cluster/pipeline.rs

Contains: `route_for_pipeline`, `collect_and_send_pending_requests`, `process_and_retry_pipeline_responses`.

Imported by:
- `src/valkey/cluster_async/mod.rs:64-67` — `use pipeline_routing::{...}`
- `src/valkey/cluster_async/mod.rs:4348` (test) — `use super::pipeline_routing::route_for_pipeline`

Internal imports (self):
- `crate::valkey::aio::ConnectionLike` (line 1)
- `crate::valkey::cluster_async::ClusterConnInner` (line 2)
- `crate::valkey::cluster_async::Connect` (line 3)
- `crate::valkey::cluster_routing::RoutingInfo` (line 4)
- `crate::valkey::cluster_routing::SlotAddr` (line 5)
- `crate::valkey::cluster_routing::{...}` (line 6)
- `crate::valkey::types::{RetryMethod, ServerError}` (line 9)
- `crate::valkey::Pipeline` (line 10)
- `crate::valkey::{cluster_routing, ValkeyResult, Value}` (line 11)
- `crate::valkey::{cluster_routing::Route, Cmd, ErrorKind, ValkeyError}` (line 12)
- `super::boxed_sleep`, `super::connections_logic::RefreshConnectionType` (lines 24-25)
- `super::CmdArg`, `super::PendingRequest`, `super::PipelineRetryStrategy` (lines 26-28)
- `super::RedirectNode`, `super::RequestInfo` (lines 29-30)
- `super::{Core, InternalSingleNodeRouting, OperationTarget, Response}` (line 31)

### src/valkey/cluster_async/connections_logic.rs → src/cluster/connections.rs

Contains: `ConnectionFuture`, `AsyncClusterNode`, `RefreshConnectionType`, `connect_and_check`, `get_or_create_conn`.

Imported by:
- `src/valkey/cluster_async/mod.rs:38-40` — `use crate::valkey::cluster_async::connections_logic::{get_host_and_port_from_addr, get_or_create_conn, ConnectionFuture, RefreshConnectionType}`
- `src/valkey/cluster_async/mod.rs:63` — `use connections_logic::connect_and_check`
- `src/valkey/cluster_async/mod.rs:32` (testing) — `pub use super::connections_logic::*`
- `src/valkey/cluster_async/pipeline_routing.rs:25` — `use super::connections_logic::RefreshConnectionType`

Internal imports (self):
- `super::{connections_container::{ClusterNode, ConnectionDetails}, Connect}` (lines 1-3)
- `crate::valkey::cluster_slotmap::ReadFromReplicaStrategy` (line 5)
- `crate::valkey::aio::{ConnectionLike, DisconnectNotifier}` (line 7)
- `crate::valkey::client::FerrisKeyConnectionOptions` (line 8)
- `crate::valkey::cluster::get_connection_info` (line 9)
- `crate::valkey::cluster_client::ClusterParams` (line 10)
- `crate::valkey::{ErrorKind, ValkeyError, ValkeyResult}` (line 11)

### src/valkey/cluster_client.rs → src/cluster/client.rs

Contains: `ClusterClient`, `ClusterClientBuilder`, `ClusterParams`, `RetryParams`, `SlotsRefreshRateLimit`.

Imported by:
- `src/valkey/mod.rs:70,74` — `mod cluster_client` + testing re-export of `ClusterParams`
- `src/valkey/cluster.rs:6` — `pub use crate::valkey::cluster_client::{ClusterClient, ClusterClientBuilder}`
- `src/valkey/cluster_topology.rs:4` — `use crate::valkey::cluster_client::SlotsRefreshRateLimit`
- `src/valkey/cluster_async/mod.rs:41` — `cluster_client::{ClusterParams, RetryParams}`
- `src/valkey/cluster_async/connections_logic.rs:10` — `crate::valkey::cluster_client::ClusterParams`
- `src/valkey/cluster_async/mod.rs:1202` (test) — `use crate::valkey::cluster_client::ClusterParams`

Internal imports (self):
- `crate::valkey::cluster_slotmap::ReadFromReplicaStrategy` (line 1)
- `crate::valkey::cluster_topology::{...}` (lines 2-4)
- `crate::valkey::connection::{ConnectionAddr, ConnectionInfo, IntoConnectionInfo}` (line 5)
- `crate::valkey::types::{ErrorKind, ProtocolVersion, ValkeyError, ValkeyResult}` (line 6)
- `crate::valkey::connection::TlsMode` (line 7)
- `crate::valkey::{PushInfo, RetryStrategy}` (line 8)
- `crate::valkey::tls::TlsConnParams` (line 14)
- `crate::valkey::cluster_async` (line 16)
- `crate::valkey::tls::{retrieve_tls_certificates, TlsCertificates}` (line 18)

### src/valkey/cluster.rs → src/cluster/compat.rs (or merge into cluster/mod.rs)

Contains: `get_connection_addr`, `get_connection_info`, `slot_cmd`, re-exports `ClusterClient`/`ClusterClientBuilder`.

Imported by:
- `src/valkey/mod.rs:67` — `pub mod cluster`
- `src/valkey/cluster_topology.rs:3` — `use crate::valkey::cluster::get_connection_addr`
- `src/valkey/cluster_async/mod.rs:37` — `cluster::slot_cmd`
- `src/valkey/cluster_async/connections_logic.rs:9` — `crate::valkey::cluster::get_connection_info`
- Tests: `benches/connections_benchmark.rs:23` — `cluster::ClusterClientBuilder`
- Tests: `tests/utilities/cluster.rs` (via `ferriskey::valkey::cluster_async`)

Internal imports (self):
- `crate::valkey::cluster_client::{ClusterClient, ClusterClientBuilder}` (line 6)
- `crate::valkey::cmd::Cmd` (line 7)
- `crate::valkey::connection::{ConnectionAddr, ConnectionInfo, ValkeyConnectionInfo}` (line 8)
- `crate::valkey::tls::TlsConnParams` (line 9)
- `crate::valkey::types::{ErrorKind, ValkeyResult}` (line 10)

### src/valkey/cluster_scan.rs → src/cluster/scan.rs

Contains: `cluster_scan`, `ClusterScanArgs`, `ScanStateRC`, `ObjectType`.

Imported by:
- `src/valkey/mod.rs:81-82` — `pub(crate) mod cluster_scan` + `pub use cluster_scan::{ClusterScanArgs, ObjectType, ScanStateRC}`
- `src/valkey/cluster_async/mod.rs:46` — `cluster_scan::{cluster_scan, ClusterScanArgs, ScanStateRC}`
- `src/cluster_scan_container.rs:6` — `use crate::valkey::{ValkeyResult, ScanStateRC}` (via re-export)

Internal imports (self):
- `crate::valkey::aio::ConnectionLike` (line 43)
- `crate::valkey::cluster_async::{ClusterConnInner, Connect, InnerCore, RefreshPolicy}` (line 44)
- `crate::valkey::cluster_routing::SlotAddr` (line 45)
- `crate::valkey::cluster_topology::SLOT_SIZE` (line 46)
- `crate::valkey::{cmd, from_valkey_value, ErrorKind, ValkeyError, ValkeyResult, Value}` (line 47)

---

## Phase 4: Flatten to src/ Root

### src/valkey/types.rs → src/value.rs

Contains: `Value`, `ValkeyError`, `ValkeyResult`, `ErrorKind`, `FromValkeyValue`, `ToValkeyArgs`, `ValkeyWrite`, `ProtocolVersion`, `PushKind`, `InfoDict`, `ServerError`, `RetryMethod`, `InflightRequestTracker`, `from_valkey_value`, `from_owned_valkey_value`.

This is the most heavily imported file in the entire codebase.

Imported by (direct `crate::valkey::types::` references):
- `src/valkey/mod.rs:25-57` — massive pub use block
- `src/valkey/connection.rs:9` — `{ErrorKind, ValkeyError, ValkeyResult}`
- `src/valkey/parser.rs:3` — `{ErrorKind, ValkeyError, ...}` (plus test at line 678)
- `src/valkey/cmd.rs:10` — `{from_owned_valkey_value, FromValkeyValue, ValkeyResult, ValkeyWrite, ToValkeyArgs}`
- `src/valkey/pipeline.rs:6` — `{ErrorKind, FromValkeyValue, ToValkeyArgs, ValkeyError, ValkeyResult, ...}`
- `src/valkey/cluster_client.rs:6` — `{ErrorKind, ProtocolVersion, ValkeyError, ValkeyResult}`
- `src/valkey/cluster_routing.rs:6` — `types::Value`
- `src/valkey/cluster.rs:10` — `{ErrorKind, ValkeyResult}`
- `src/valkey/aio/mod.rs:5-7` — `{ErrorKind, FromValkeyValue, InfoDict, ProtocolVersion, ValkeyError, ValkeyResult, Value}`
- `src/valkey/aio/multiplexed_connection.rs:10` — `{ValkeyError, ValkeyResult, Value}`
- `src/valkey/aio/runtime.rs:4` — `ValkeyError`
- `src/valkey/cluster_async/mod.rs:55` — `{ProtocolVersion, RetryMethod, ServerError}`
- `src/valkey/cluster_async/pipeline_routing.rs:9` — `{RetryMethod, ServerError}`
- `src/valkey/cluster_async/mod.rs:1138` — `pub use crate::valkey::types::InflightRequestTracker`

Imported via re-export (`crate::valkey::ErrorKind` etc.):
- `src/valkey/cluster_slotmap.rs:11-13` — `ErrorKind`, `ValkeyError`, `ValkeyResult`
- `src/valkey/cluster_routing.rs:7` — `ErrorKind`, `ValkeyError`, `ValkeyResult`
- `src/valkey/push_manager.rs:2,4` — `Value`, `PushKind`, `ValkeyResult`
- `src/valkey/pubsub_synchronizer.rs:5` — `Cmd`, `ValkeyResult`, `Value`
- `src/valkey/cluster_topology.rs:7` — `ErrorKind`, `ValkeyError`, `ValkeyResult`, `Value`
- `src/valkey/tls.rs:7` — `ErrorKind`, `ValkeyError`, `ValkeyResult`
- `src/valkey/cluster_scan.rs:47` — `from_valkey_value`, `ErrorKind`, `ValkeyError`, `ValkeyResult`, `Value`
- `src/valkey/cluster_async/connections_logic.rs:11` — `ErrorKind`, `ValkeyError`, `ValkeyResult`
- `src/valkey/cluster_async/pipeline_routing.rs:11-12` — `ValkeyResult`, `Value`, `ErrorKind`, `ValkeyError`
- `src/valkey/cluster_async/mod.rs:57` — `Cmd`, `ErrorKind`, `FromValkeyValue`, `InfoDict`, `ValkeyError`, etc.
- `src/valkey/client.rs:5-15` — multiple types via `use crate::valkey::{...}`
- `src/valkey/aio/multiplexed_connection.rs:11` — `cmd`, `ConnectionInfo`, `ProtocolVersion`, `PushKind`
- `src/client/mod.rs:20-40` — massive import block
- `src/client/value_conversion.rs:3` — `FromValkeyValue`, `ValkeyError`, `ValkeyResult`, `Value`, etc.
- `src/client/reconnecting_connection.rs:8-9` — multiple types
- `src/client/standalone_client.rs:12` — `PushInfo`, `ValkeyError`, `ValkeyResult`, `RetryStrategy`, `Value`
- `src/pubsub/mod.rs:4-5` — `PushInfo`, `PubSubSubscriptionInfo`, etc.
- `src/pubsub/synchronizer.rs:7` — multiple types
- `src/pubsub/mock.rs:9-10` — multiple types
- `src/errors.rs:3` — `ValkeyError`
- `src/request_type.rs:3` — `Cmd`, `cmd`
- `src/cluster_scan_container.rs:6` — `ValkeyResult`, `ScanStateRC`
- `src/otel_db_semantics.rs:4` — `Arg`, `Cmd`
- `src/compression.rs:779,812,834` (tests) — `Value`
- All test files and benches that use `ferriskey::valkey::*`

### src/valkey/cmd.rs → src/cmd.rs

Contains: `Cmd`, `Arg`, `cmd()`, `fenced_cmd()`, `pack_command()`, `pipe()`, `AsyncIter`.

Imported by (direct):
- `src/valkey/mod.rs:10,60` — pub use block
- `src/valkey/cluster_routing.rs:5` — `use crate::valkey::cmd::{Arg, Cmd}`
- `src/valkey/cluster.rs:7` — `use crate::valkey::cmd::Cmd`
- `src/valkey/pipeline.rs:5` — `use crate::valkey::cmd::{cmd, cmd_len, Cmd}`
- `src/valkey/aio/mod.rs:2` — `use crate::valkey::cmd::{cmd, Cmd}`
- `src/valkey/aio/multiplexed_connection.rs:6` — `use crate::valkey::cmd::Cmd`

Imported via re-export (`crate::valkey::Cmd`, `crate::valkey::cmd`, etc.):
- Virtually every file in the cluster_async/ directory
- `src/request_type.rs:3`, `src/otel_db_semantics.rs:4,271`

Internal imports (self):
- `crate::valkey::pipeline::Pipeline` (line 9)
- `crate::valkey::types::{from_owned_valkey_value, FromValkeyValue, ValkeyResult, ValkeyWrite, ToValkeyArgs}` (line 10)
- `crate::valkey::aio::ConnectionLike as AsyncConnection` (line 44)

### src/valkey/pipeline.rs → src/pipeline.rs

Contains: `Pipeline`, `PipelineRetryStrategy`.

Imported by (direct):
- `src/valkey/mod.rs:18` — `pub use crate::valkey::pipeline::{Pipeline, PipelineRetryStrategy}`
- `src/valkey/connection.rs:8` — `use crate::valkey::pipeline::Pipeline`
- `src/valkey/cmd.rs:9` — `use crate::valkey::pipeline::Pipeline`
- `src/valkey/aio/mod.rs:4` — `use crate::valkey::pipeline::PipelineRetryStrategy`
- `src/valkey/aio/multiplexed_connection.rs:8` — `use crate::valkey::pipeline::PipelineRetryStrategy`
- `src/valkey/cluster_async/pipeline_routing.rs:10` — `use crate::valkey::Pipeline` (via re-export)

Internal imports (self):
- `crate::valkey::cmd::{cmd, cmd_len, Cmd}` (line 5)
- `crate::valkey::types::{...}` (line 6)

### src/valkey/client.rs → merge into src/client/ (or src/valkey_client.rs)

Contains: `Client` (the low-level valkey client), `FerrisKeyConnectionOptions`, `IAMTokenProvider`.

**Note:** There is already `src/client/mod.rs` with the high-level `Client`. These need to be reconciled — the valkey `Client` becomes internal, high-level `Client` stays public.

Imported by:
- `src/valkey/mod.rs:7-9` — `pub use crate::valkey::client::{Client, FerrisKeyConnectionOptions, IAMTokenProvider}`
- `src/valkey/aio/multiplexed_connection.rs:5` — `use crate::valkey::client::FerrisKeyConnectionOptions`
- `src/valkey/cluster_async/mod.rs:36` — `client::FerrisKeyConnectionOptions`
- `src/valkey/cluster_async/connections_logic.rs:8` — `crate::valkey::client::FerrisKeyConnectionOptions`
- `src/valkey/tls.rs:7` — `crate::valkey::{Client, ...}` (via re-export)

Internal imports (self):
- `crate::valkey::aio::DisconnectNotifier` (line 3)
- `crate::valkey::{ConnectionAddr, ConnectionInfo, ...}` (lines 5-15)
- `crate::valkey::pubsub_synchronizer::PubSubSynchronizer` (line 16)
- `crate::valkey::tls::{inner_build_with_tls, TlsCertificates}` (line 17)

### src/valkey/push_manager.rs → src/pubsub/push_manager.rs

Contains: `PushManager`, `PushInfo`.

Imported by:
- `src/valkey/mod.rs:20` — `pub use push_manager::{PushInfo, PushManager}`
- `src/valkey/aio/multiplexed_connection.rs:9` — `use crate::valkey::push_manager::PushManager`
- `src/valkey/cluster_async/mod.rs:54` — `push_manager::PushInfo`

Internal imports (self):
- `crate::valkey::connection::PubSubSubscriptionKind` (line 1)
- `crate::valkey::{PubSubSynchronizer, PushKind, Value}` (line 2)
- `crate::valkey::ValkeyResult` (line 4)

### src/valkey/pubsub_synchronizer.rs → src/pubsub/valkey_synchronizer.rs (or merge into synchronizer.rs)

Contains: `PubSubSynchronizer` trait definition.

**Note:** `src/pubsub/synchronizer.rs` already exists with `ValkeyPubSubSynchronizer` impl.

Imported by:
- `src/valkey/mod.rs:19` — `pub use crate::valkey::pubsub_synchronizer::PubSubSynchronizer`
- `src/valkey/client.rs:16` — `use crate::valkey::pubsub_synchronizer::PubSubSynchronizer`
- `src/valkey/push_manager.rs:2` — via `crate::valkey::PubSubSynchronizer` re-export
- `src/pubsub/mod.rs` — `PubSubSynchronizer` re-export
- `src/client/mod.rs:39` — `use crate::pubsub::{PubSubSynchronizer, create_pubsub_synchronizer}`

Internal imports (self):
- `crate::valkey::cluster_slotmap::SlotMap` (line 3)
- `crate::valkey::connection::{PubSubChannelOrPattern, PubSubSubscriptionKind}` (line 4)
- `crate::valkey::{Cmd, ValkeyResult, Value}` (line 5)

### src/valkey/retry_strategies.rs → src/retry_strategies.rs

Contains: `RetryStrategy`.

Imported by:
- `src/valkey/mod.rs:21` — `pub use retry_strategies::RetryStrategy`
- `src/valkey/cluster_client.rs:8` — `use crate::valkey::{..., RetryStrategy}` (via re-export)
- `src/client/standalone_client.rs:12` — `use crate::valkey::{..., RetryStrategy, ...}` (via re-export)

### src/valkey/macros.rs → src/macros.rs

Contains: Helper macros (used via `#[macro_use]`).

Imported by:
- `src/valkey/mod.rs:63` — `mod macros` (loaded via `#[macro_use]` on `pub mod valkey` in lib.rs)
- Used implicitly anywhere macros are invoked

### src/valkey/mod.rs → DELETED

All re-exports move to `src/lib.rs`. Module declarations replaced by direct `mod` statements in lib.rs for the new subdirectories.

---

## Files NOT Moving (but needing import updates)

These files stay in place but their `use crate::valkey::` imports must change.

### src/lib.rs
- Remove `#[macro_use] pub mod valkey;`
- Add: `mod protocol; mod connection; mod cluster; mod cmd; mod pipeline; mod value; mod macros; mod retry_strategies;`
- Move all re-exports from `valkey/mod.rs` into lib.rs pointing at new locations

### src/client/mod.rs
- 10+ `use crate::valkey::` statements → `use crate::`

### src/client/standalone_client.rs
- 3 `use crate::valkey::` statements → `use crate::`

### src/client/reconnecting_connection.rs
- 2 `use crate::valkey::` statements → `use crate::`

### src/client/value_conversion.rs
- 1 `use crate::valkey::` statement → `use crate::`

### src/pubsub/mod.rs
- 2 `use crate::valkey::` statements → `use crate::`

### src/pubsub/synchronizer.rs
- 1 `use crate::valkey::` statement → `use crate::`

### src/pubsub/mock.rs
- 3 `use crate::valkey::` statements → `use crate::`

### src/errors.rs
- 1 `use crate::valkey::ValkeyError` → `use crate::ValkeyError`

### src/request_type.rs
- 1 `use crate::valkey::{Cmd, cmd}` → `use crate::{Cmd, cmd}`

### src/cluster_scan_container.rs
- 1 `use crate::valkey::{ValkeyResult, ScanStateRC}` → `use crate::{ValkeyResult, ScanStateRC}`

### src/otel_db_semantics.rs
- 1 `use crate::valkey::{Arg, Cmd}` → `use crate::{Arg, Cmd}`
- 1 inline `crate::valkey::cmd(name)` → `crate::cmd(name)`

### src/compression.rs
- 3 test-only `use crate::valkey::Value` → `use crate::Value`

---

## External Files Needing Import Updates

### tests/utilities/mod.rs
- `use ferriskey::valkey::{...}` → `use ferriskey::{...}`

### tests/utilities/mocks.rs
- `use ferriskey::valkey::{Cmd, ConnectionAddr, Value}` → `use ferriskey::{Cmd, ConnectionAddr, Value}`

### tests/utilities/cluster.rs
- 3 `use ferriskey::valkey::` statements → `use ferriskey::`

### tests/test_client.rs
- 4 `use ferriskey::valkey::` statements → `use ferriskey::`

### tests/test_cluster_client.rs
- 1 `use ferriskey::valkey::` statement → `use ferriskey::`

### tests/test_standalone_client.rs
- 1 `use ferriskey::valkey::` statement → `use ferriskey::`

### tests/test_pubsub.rs
- 1 `use ferriskey::valkey::PubSubSubscriptionKind` → `use ferriskey::PubSubSubscriptionKind`

### tests/test_integration_real_workload.rs
- 1 `use ferriskey::valkey::{...}` → `use ferriskey::{...}`

### benches/connections_benchmark.rs
- 1 `use ferriskey::valkey::{...}` → `use ferriskey::{...}`

---

## Total Impact

| Metric | Count |
|---|---|
| Files moving | 24 |
| Files deleted | 1 (`valkey/mod.rs`) |
| New directories created | 3 (`protocol/`, `connection/`, `cluster/`) |
| Internal files needing `use` updates | 36 |
| Test/bench files needing `use` updates | 9 |
| **Total files touched** | **45** |
| Estimated `use crate::valkey::` replacements | ~150 statements |
| Estimated `ferriskey::valkey::` replacements | ~20 statements |
| Estimated `super::` adjustments (within moved files) | ~40 statements |
| **Total import statement changes** | **~210** |

## Recommended Execution Order

1. **Create target directories**: `src/protocol/`, `src/connection/`, `src/cluster/`
2. **Move + update cluster/ files** (most self-contained, fewest external consumers)
3. **Move + update connection/ files** (depends on protocol/, but protocol has few files)
4. **Move + update protocol/ files** (parser.rs only initially)
5. **Flatten remaining valkey/ files to src/ root** (types.rs, cmd.rs, pipeline.rs, macros.rs, retry_strategies.rs)
6. **Reconcile client.rs** (valkey/client.rs → merge into client/mod.rs)
7. **Reconcile pubsub** (pubsub_synchronizer.rs + push_manager.rs → into pubsub/)
8. **Delete valkey/mod.rs**, rewrite lib.rs exports
9. **Update all external imports** (tests/, benches/)
10. **Compile, fix, repeat** — `cargo check` after each phase

## Risk Notes

- `#[macro_use] pub mod valkey` in lib.rs means macros from `valkey/macros.rs` are available crate-wide. Moving macros.rs requires either keeping `#[macro_use]` on the new location or adding explicit `use` at each call site.
- `valkey/mod.rs` re-exports ~60 symbols. Missing even one breaks downstream code.
- `super::` imports within `cluster_async/` submodules are relative — these change when the parent module moves.
- The `testing` feature-gated modules in `valkey/mod.rs` and `cluster_async/mod.rs` need equivalent gating in the new structure.
