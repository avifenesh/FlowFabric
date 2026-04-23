//! This module provides async functionality for Valkey Cluster.
//!
//! By default, [`ClusterConnection`] makes use of [`MultiplexedConnection`] and maintains a pool
//! of connections to each node in the cluster. While it  generally behaves similarly to
//! the sync cluster module, certain commands do not route identically, due most notably to
//! a current lack of support for routing commands to multiple nodes.
//!
//! Also note that pubsub functionality is not currently provided by this module.
//!
//! # Example
//! ```rust,ignore
//! use ferriskey::cluster::ClusterClient;
//! use ferriskey::AsyncCommands;
//!
//! async fn fetch_an_integer() -> String {
//!     let nodes = vec!["redis://127.0.0.1/"];
//!     let client = ClusterClient::new(nodes).unwrap();
//!     let mut connection = client.get_async_connection(None, None, None).await.unwrap();
//!     let _: () = connection.cmd("SET").arg("test").arg("test_data").await.unwrap();
//!     let rv: String = connection.cmd("GET").arg("test").await.unwrap();
//!     return rv;
//! }
//! ```

pub(crate) mod client;
pub mod compat;
pub mod connections;
pub mod container;
pub(crate) mod pipeline;
pub mod routing;
pub mod scan;
pub mod slotmap;
pub mod topology;
/// Exposed only for testing.
#[cfg(test)]
pub mod testing {
    pub use super::connections::*;
    pub use super::container::ConnectionDetails;
}
use crate::cluster::client::{ClusterParams, RetryParams};
use crate::cluster::compat::slot_cmd;
use crate::cluster::connections::{
    ConnectionFuture, RefreshConnectionType, get_host_and_port_from_addr, get_or_create_conn,
};
use crate::cluster::routing::{
    self as cluster_routing, MultipleNodeRoutingInfo, Redirect, ResponsePolicy, Routable, Route,
    RoutingInfo, ShardUpdateResult, SingleNodeRoutingInfo,
};
use crate::cluster::scan::{ClusterScanArgs, ScanStateRC, cluster_scan};
use crate::cluster::slotmap::SlotMap;
use crate::cluster::topology::{
    DEFAULT_NUMBER_OF_REFRESH_SLOTS_RETRIES, DEFAULT_REFRESH_SLOTS_RETRY_BASE_DURATION_MILLIS,
    DEFAULT_REFRESH_SLOTS_RETRY_BASE_FACTOR, SlotRefreshState, TopologyHash, calculate_topology,
};
use crate::cmd::{Cmd, cmd};
use crate::connection::factory::{FerrisKeyConnectionOptions, IAMTokenProvider};
use crate::connection::info::{ConnectionInfo, IntoConnectionInfo};
use crate::connection::{
    ConnectionLike, DisconnectNotifier, MultiplexedConnection, get_socket_addrs,
};
use crate::pipeline::PipelineRetryStrategy;
use crate::pubsub::push_manager::PushInfo;
use crate::value::{
    ErrorKind, FromValue, InfoDict, ProtocolVersion, RetryMethod, Error,
    Result, Value,
};
use connections::connect_and_check;
use container::{
    ConnectionAndAddress, ConnectionType, ConnectionsMap, RefreshTaskNotifier, RefreshTaskState,
    RefreshTaskStatus,
};
use pipeline::{
    PipelineResponses, ResponsePoliciesMap, collect_and_send_pending_requests,
    map_pipeline_to_nodes, process_and_retry_pipeline_responses, route_for_pipeline,
};

use async_trait::async_trait;
use dashmap::DashMap;
use dispose::{Disposable, Dispose};
use futures::{
    future::{BoxFuture, Shared},
    prelude::*,
    ready,
    stream::{FuturesUnordered, StreamExt},
};
use parking_lot::RwLock as ParkingLotRwLock;
use pin_project_lite::pin_project;
use rand::seq::IteratorRandom;
use std::{
    collections::{HashMap, HashSet},
    fmt, io,
    net::{IpAddr, SocketAddr},
    pin::Pin,
    sync::{
        Arc,
        atomic::{self, AtomicUsize, Ordering},
    },
    task::{self, Poll},
    time::{Duration, SystemTime},
};
use strum_macros::Display;
use tokio::{
    sync::{
        Notify, mpsc,
        oneshot::{self, Receiver},
    },
    task::JoinHandle,
    time::timeout,
};
use tokio_retry2::strategy::{ExponentialFactorBackoff, jitter_range};
use tokio_retry2::{Retry, RetryError};
use tracing::{debug, info, trace, warn};

/// Parses a `"host:port"` address string into its components.
/// Returns `None` if the address has no `:` separator or the port is not a valid integer.
fn parse_node_address(address: &str) -> Option<(&str, i64)> {
    let (host, port_str) = address.rsplit_once(':')?;
    let port = port_str.parse::<i64>().ok()?;
    Some((host, port))
}

/// Attempts to forward a command to a MOVED or ASK redirect target using only
/// already-known cluster connections (never creates new TCP connections).
///
/// Returns:
/// - `Some(Ok(value))` — redirect succeeded and produced a result
/// - `Some(Err(e))`   — redirect target was reached but the command failed
/// - `None`           — redirect address is not in the connection map; caller
///   should fall back to the cluster task for a full retry
///
/// # SAFETY (SSRF)
/// `connection_for_address` performs a map lookup only — it never opens new
/// sockets. An attacker-controlled MOVED/ASK response therefore cannot redirect
/// the client to an arbitrary host; it can only reach nodes already connected.
///
/// # Note on cluster task vs. direct dispatch
/// The cluster task (Request::poll) and pipeline routing (pipeline_routing.rs)
/// keep their own MOVED/ASK handling because they participate in full retry
/// state machines with slot-map refresh, backoff, and response aggregation.
/// This helper is **only for the direct-dispatch fast path** where we want a
/// single opportunistic MOVED redirect without retry infrastructure.
///
/// ASK redirects are NOT handled here — they require an atomic ASKING + command
/// pair which cannot be guaranteed on a shared multiplexed connection. ASK errors
/// fall back to the cluster task's single-threaded state machine instead.
async fn try_redirect_to_known_node<C>(
    conn_lock: &ParkingLotRwLock<ConnectionsContainer<C>>,
    error: &Error,
    packed: bytes::Bytes,
    is_fenced: bool,
) -> Option<Result<Value>>
where
    C: ConnectionLike + Clone + Send + Sync + 'static,
{
    if error.kind() != ErrorKind::Moved {
        return None;
    }

    let (addr, _slot) = error.redirect_node()?;

    // Look up in the existing connection map only — never creates a new connection.
    let conn_future = {
        let container = conn_lock.read();
        container.connection_for_address(addr).map(|(_, c)| c)
    }?;

    let mut conn = conn_future.await;

    Some(conn.send_packed_bytes(packed, is_fenced).await)
}

type SharedConnFuture<C> = futures::future::Shared<BoxFuture<'static, C>>;
type DirectPipelineGroup<C> = (Arc<str>, crate::pipeline::Pipeline, Vec<usize>, SharedConnFuture<C>);
type NodeResponseReceiver = (Option<Arc<str>>, oneshot::Receiver<Result<Response>>);

/// Emits a tracing event identifying the routed node for this
/// command. Replaces the previous OTel-span-attribute path; subscribers
/// that want span-attached attributes can open a `ferriskey.route` span
/// and correlate via the event.
fn emit_routed_node(address: &str) {
    if let Some((host, port)) = parse_node_address(address) {
        tracing::debug!(
            target: "ferriskey",
            event = "routed",
            server.address = %host,
            server.port = port,
            "ferriskey: cluster routing resolved node"
        );
    }
}

/// This represents an async Cluster connection. It stores the
/// underlying connections maintained for each node in the cluster, as well
/// as common parameters for connecting to nodes and executing commands.
#[derive(Clone)]
pub struct ClusterConnection<C = MultiplexedConnection> {
    /// Channel to the cluster task — used for fan-out, MOVED/ASK retries, admin ops.
    cluster_task: mpsc::Sender<Message<C>>,
    /// Direct access to the connections container for fast single-node dispatch.
    /// Bypasses the cluster task entirely for the happy path.
    inner: Core<C>,
}

impl<C> ClusterConnection<C>
where
    C: ConnectionLike + Connect + Clone + Send + Sync + Unpin + 'static,
{
    pub(crate) async fn new(
        initial_nodes: &[ConnectionInfo],
        cluster_params: ClusterParams,
        push_sender: Option<mpsc::UnboundedSender<PushInfo>>,
        pubsub_synchronizer: Option<Arc<dyn crate::pubsub::synchronizer_trait::PubSubSynchronizer>>,
        iam_token_provider: Option<Arc<dyn IAMTokenProvider>>,
    ) -> Result<ClusterConnection<C>> {
        ClusterConnInner::new(
            initial_nodes,
            cluster_params,
            push_sender,
            pubsub_synchronizer,
            iam_token_provider,
        )
        .await
        .map(|inner| {
            let core = inner.inner.clone();
            let (tx, mut rx) = mpsc::channel::<Message<_>>(100);
            let stream = async move {
                let _ = stream::poll_fn(move |cx| rx.poll_recv(cx))
                    .map(Ok)
                    .forward(inner)
                    .await;
            };
            tokio::spawn(stream);
            ClusterConnection {
                cluster_task: tx,
                inner: core,
            }
        })
    }

    /// Special handling for `SCAN` command, using `cluster_scan_with_pattern`.
    /// It is a special case of [`cluster_scan`], with an additional match pattern.
    /// Perform a `SCAN` command on a cluster, using scan state object in order to handle changes in topology
    /// and make sure that all keys that were in the cluster from start to end of the scan are scanned.
    /// In order to make sure all keys in the cluster scanned, topology refresh occurs more frequently and may affect performance.
    ///
    /// # Arguments
    ///
    /// * `scan_state_rc` - A reference to the scan state. For initiating a new scan, send [`ScanStateRC::new()`].
    ///   For each subsequent iteration, use the returned [`ScanStateRC`].
    /// * `cluster_scan_args` - A [`ClusterScanArgs`] struct containing the arguments for the cluster scan command:
    ///   match pattern, count, object type, and the `allow_non_covered_slots` flag.
    ///
    /// # Returns
    ///
    /// A [`ScanStateRC`] for the updated state of the scan and the vector of keys that were found in the scan.
    /// structure of returned value:
    /// `Ok((ScanStateRC, Vec<Value>))`
    ///
    /// When the scan is finished [`ScanStateRC`] will be None, and can be checked by calling `scan_state_wrapper.is_finished()`.
    ///
    /// # Example
    /// ```rust,ignore
    /// use ferriskey::cluster::ClusterClient;
    /// use ferriskey::{ScanStateRC, from_value, Value, ObjectType, ClusterScanArgs};
    ///
    /// async fn scan_all_cluster() -> Vec<String> {
    ///     let nodes = vec!["redis://127.0.0.1/"];
    ///     let client = ClusterClient::new(nodes).unwrap();
    ///     let mut connection = client.get_async_connection(None, None, None).await.unwrap();
    ///     let mut scan_state_rc = ScanStateRC::new();
    ///     let mut keys: Vec<String> = vec![];
    ///     let cluster_scan_args = ClusterScanArgs::builder().with_count(1000).with_object_type(ObjectType::String).build();
    ///     loop {
    ///         let (next_cursor, scan_keys): (ScanStateRC, Vec<Value>) =
    ///         connection.cluster_scan(scan_state_rc, cluster_scan_args.clone()).await.unwrap();
    ///         scan_state_rc = next_cursor;
    ///         let mut scan_keys = scan_keys
    ///             .into_iter()
    ///             .map(|v| from_value(&v).unwrap())
    ///             .collect::<Vec<String>>(); // Change the type of `keys` to `Vec<String>`
    ///         keys.append(&mut scan_keys);
    ///         if scan_state_rc.is_finished() {
    ///             break;
    ///             }
    ///         }
    ///     keys
    ///     }
    /// ```
    pub async fn cluster_scan(
        &mut self,
        scan_state_rc: ScanStateRC,
        mut cluster_scan_args: ClusterScanArgs,
    ) -> Result<(ScanStateRC, Vec<Value>)> {
        cluster_scan_args.set_scan_state_cursor(scan_state_rc);
        self.route_cluster_scan(cluster_scan_args).await
    }

    /// Route cluster scan to be handled by internal cluster_scan command
    async fn route_cluster_scan(
        &mut self,
        cluster_scan_args: ClusterScanArgs,
    ) -> Result<(ScanStateRC, Vec<Value>)> {
        let (sender, receiver) = oneshot::channel();
        self.cluster_task
            .send(Message {
                cmd: CmdArg::ClusterScan { cluster_scan_args },
                sender,
            })
            .await
            .map_err(|e| {
                Error::from(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("Cluster: Error occurred while trying to send SCAN command to internal send task. {e:?}"),
                ))
            })?;
        receiver
            .await
            .unwrap_or_else(|e| {
                Err(Error::from(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("Cluster: Failed to receive SCAN command response from internal send task. {e:?}"),
                )))
            })
            .and_then(|response| match response {
                Response::ClusterScanResult(new_scan_state_ref, key) => Ok((new_scan_state_ref, key)),
                Response::Single(_) | Response::Multiple(_) => Err(Error::from((
                    ErrorKind::ResponseError,
                    "Unexpected response variant for cluster scan",
                ))),
            })
    }

    /// Fast path: resolve route and send directly to the per-node mux connection.
    /// Returns Some(result) on success or non-retryable error.
    /// Returns None if the route couldn't be resolved (falls back to cluster task).
    async fn try_direct_dispatch(
        &self,
        cmd: &Cmd,
        routing: &SingleNodeRoutingInfo,
    ) -> Option<Result<Value>> {
        // Resolve the route to a connection. Clone the connection (cheap — it's
        // just an mpsc sender clone) so we can drop the read lock before async work.
        let conn_future = {
            let container = self.inner.conn_lock.read();
            match routing {
                SingleNodeRoutingInfo::SpecificNode(route) => {
                    container.connection_for_route(route)?.1
                }
                // Random, ByAddress, Redirect, etc. — let cluster task handle
                _ => return None,
            }
        };
        // Read lock is now dropped. Resolve the connection future.
        let mut conn = conn_future.await;

        let packed = cmd.get_packed_command();
        let is_fenced = cmd.is_fenced();
        // Clone packed cheaply (Bytes is Arc-backed) so it's available for redirect.
        let result = conn.send_packed_bytes(packed.clone(), is_fenced).await;

        match result {
            Ok(val) => Some(Ok(val)),
            Err(ref e) if e.kind() == ErrorKind::Ask => {
                // ASK requires ASKING + command as an atomic pair. On a shared
                // multiplexed connection the two writes can be interleaved by
                // concurrent requests. Fall back to the cluster task which
                // handles ASK correctly via its single-threaded state machine.
                None
            }
            Err(ref e) if e.kind() == ErrorKind::Moved => {
                // MOVED is safe to retry on the direct path — it's a single command.
                try_redirect_to_known_node(&self.inner.conn_lock, e, packed, is_fenced).await
            }
            Err(_) => Some(result),
        }
    }

    /// Fast path for non-atomic cross-slot pipelines.
    /// Groups commands by node, sends sub-pipelines in parallel directly to node connections.
    /// Returns None if any sub-pipeline hits MOVED/ASK (falls back to cluster task).
    async fn try_direct_pipeline_dispatch(
        &self,
        pipeline: &crate::pipeline::Pipeline,
        count: usize,
    ) -> Option<Result<Vec<Result<Value>>>> {
        use tokio::task::JoinSet;

        // Step 1: Group commands by node address.
        // Hold conn_lock briefly, only for route resolution.
        let grouped: Vec<DirectPipelineGroup<C>> = {
            let container = self.inner.conn_lock.read();
            if container.is_empty() {
                return None;
            }

            // Route each command to a node
            let mut node_map: std::collections::HashMap<
                Arc<str>,
                (crate::pipeline::Pipeline, Vec<usize>, SharedConnFuture<C>),
            > = std::collections::HashMap::new();

            for (idx, cmd) in pipeline.cmd_iter().enumerate() {
                let route = match cluster_routing::RoutingInfo::for_routable(cmd.as_ref()) {
                    Some(cluster_routing::RoutingInfo::SingleNode(
                        SingleNodeRoutingInfo::SpecificNode(route),
                    )) => route,
                    _ => return None, // Can't resolve inline — fall back
                };

                let (addr, conn) = match container.connection_for_route(&route) {
                    Some(pair) => pair,
                    None => return None, // Node not found — fall back
                };

                let entry = node_map
                    .entry(addr)
                    .or_insert_with(|| (crate::pipeline::Pipeline::new(), Vec::new(), conn));
                entry.0.add_command_with_arc(cmd.clone());
                entry.1.push(idx);
            }

            node_map
                .into_iter()
                .map(|(addr, (pipe, indices, conn))| (addr, pipe, indices, conn))
                .collect()
        };
        // conn_lock dropped here.

        if grouped.is_empty() {
            return None;
        }

        // Step 2: Send sub-pipelines in parallel, directly to node mux connections.
        let mut join_set = JoinSet::new();
        let mut node_info: Vec<(Arc<str>, Vec<usize>)> = Vec::with_capacity(grouped.len());

        for (addr, sub_pipeline, indices, conn_future) in grouped {
            let sub_count = sub_pipeline.len();
            let node_idx = node_info.len();
            node_info.push((addr, indices));

            join_set.spawn(async move {
                let mut conn = conn_future.await;
                let result = conn
                    .req_packed_commands(&sub_pipeline, 0, sub_count, None)
                    .await;
                (node_idx, result)
            });
        }

        // Step 3: Collect results and reassemble in original command order.
        let total_cmds = count;
        let mut final_results: Vec<Option<Result<Value>>> = vec![None; total_cmds];

        while let Some(join_result) = join_set.join_next().await {
            let (node_idx, result) = match join_result {
                Ok(pair) => pair,
                Err(_join_err) => return None, // Task panicked — fall back
            };

            let values = match result {
                Ok(values) => values,
                Err(_) => {
                    // Any sub-pipeline error (MOVED, ASK, connection drop) —
                    // fall back to cluster task which handles retries and reconnection.
                    return None;
                }
            };

            let (_, ref indices) = node_info[node_idx];
            for (sub_idx, &original_idx) in indices.iter().enumerate() {
                if original_idx < total_cmds && sub_idx < values.len() {
                    final_results[original_idx] = Some(values[sub_idx].clone());
                }
            }
        }

        // If any command slot is still None, a node returned fewer replies than
        // expected. Fall back to the cluster task rather than fabricating data.
        if final_results.iter().any(|opt| opt.is_none()) {
            return None;
        }
        let assembled: Vec<Result<Value>> = final_results
            .into_iter()
            .map(|opt| opt.expect("checked for None above"))
            .collect();

        Some(Ok(assembled))
    }

    /// Send a command to the given `routing`. If `routing` is [None], it will be computed from `cmd`.
    pub async fn route_command(
        &mut self,
        cmd: &Cmd,
        routing: cluster_routing::RoutingInfo,
    ) -> Result<Value> {
        trace!("route_command");
        let (sender, receiver) = oneshot::channel();
        let cmd_arg = match &routing {
            cluster_routing::RoutingInfo::MultiNode(_) => CmdArg::MultiCmd {
                cmd: Arc::new(cmd.clone()),
                routing: routing.into(),
            },
            _ => CmdArg::Cmd {
                packed: cmd.get_packed_command(),
                is_fenced: cmd.is_fenced(),
                routing: routing.into(),
            },
        };
        self.cluster_task
            .send(Message {
                cmd: cmd_arg,
                sender,
            })
            .await
            .map_err(|e| {
                Error::from(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!("Cluster: Error occurred while trying to send command to internal sender. {e:?}"),
                ))
            })?;
        receiver
            .await
            .unwrap_or_else(|e| {
                Err(Error::from(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    format!(
                        "Cluster: Failed to receive command response from internal sender. {e:?}"
                    ),
                )))
            })
            .and_then(|response| match response {
                Response::Single(value) => Ok(value),
                Response::ClusterScanResult(..) | Response::Multiple(_) => {
                    Err(Error::from((
                        ErrorKind::ResponseError,
                        "Unexpected response variant for single command",
                    )))
                }
            })
    }

    /// Send commands in `pipeline` to the given `route`. If `route` is [None], it will be computed from `pipeline`.
    /// - `pipeline_retry_strategy`: Configures retry behavior for pipeline commands.
    ///   - `retry_server_error`: If `true`, retries commands on server errors (may cause reordering).
    ///   - `retry_connection_error`: If `true`, retries on connection errors (may lead to duplicate executions).
    ///     TODO: add wiki link.
    pub async fn route_pipeline<'a>(
        &'a mut self,
        pipeline: &'a crate::pipeline::Pipeline,
        offset: usize,
        count: usize,
        route: Option<SingleNodeRoutingInfo>,
        pipeline_retry_strategy: Option<PipelineRetryStrategy>,
    ) -> Result<Vec<Result<Value>>> {
        let (sender, receiver) = oneshot::channel();
        self.cluster_task
            .send(Message {
                cmd: CmdArg::Pipeline {
                    pipeline: Arc::new(pipeline.clone()),
                    offset,
                    count,
                    route: route.map(|r| Some(r).into()),
                    sub_pipeline: false,
                    pipeline_retry_strategy: pipeline_retry_strategy.unwrap_or_default(),
                },
                sender,
            })
            .await
            .map_err(|err| {
                Error::from(io::Error::new(io::ErrorKind::BrokenPipe, err.to_string()))
            })?;

        receiver
            .await
            .unwrap_or_else(|err| {
                Err(Error::from(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    err.to_string(),
                )))
            })
            .and_then(|response| match response {
                Response::Multiple(values) => Ok(values),
                Response::ClusterScanResult(..) | Response::Single(_) => Err(Error::from((
                    ErrorKind::ResponseError,
                    "Unexpected response variant for pipeline",
                ))),
            })
    }
    /// Update the password used to authenticate with all cluster servers
    pub async fn update_connection_password(
        &mut self,
        password: Option<String>,
    ) -> Result<Value> {
        self.route_operation_request(Operation::UpdateConnectionPassword(password))
            .await
    }

    /// Update the database ID used for all cluster connections
    pub async fn update_connection_database(&mut self, database_id: i64) -> Result<Value> {
        self.route_operation_request(Operation::UpdateConnectionDatabase(database_id))
            .await
    }

    /// Update the name used for all cluster connections
    pub async fn update_connection_client_name(
        &mut self,
        client_name: Option<String>,
    ) -> Result<Value> {
        self.route_operation_request(Operation::UpdateConnectionClientName(client_name))
            .await
    }

    /// Update the username used to authenticate with all cluster servers
    ///
    /// This method updates the username for all cluster connections and stores it for future reconnections.
    /// Typically called after a successful AUTH command with a username parameter.
    ///
    /// # Arguments
    ///
    /// * `username` - The username to use for authentication (None to clear)
    ///
    pub async fn update_connection_username(
        &mut self,
        username: Option<String>,
    ) -> Result<Value> {
        self.route_operation_request(Operation::UpdateConnectionUsername(username))
            .await
    }

    /// Update the protocol version used for all cluster connections
    ///
    /// This method updates the protocol version for all cluster connections and stores it for future reconnections.
    /// Typically called after a successful HELLO command that changes the protocol version.
    ///
    /// # Arguments
    ///
    /// * `protocol` - The protocol version to use (RESP2 or RESP3)
    ///
    pub async fn update_connection_protocol(
        &mut self,
        protocol: ProtocolVersion,
    ) -> Result<Value> {
        self.route_operation_request(Operation::UpdateConnectionProtocol(protocol))
            .await
    }

    /// Get the username used to authenticate with all cluster servers
    pub async fn get_username(&mut self) -> Result<Value> {
        self.route_operation_request(Operation::GetUsername).await
    }

    /// Routes an operation request to the appropriate handler.
    async fn route_operation_request(
        &mut self,
        operation_request: Operation,
    ) -> Result<Value> {
        let (sender, receiver) = oneshot::channel();
        self.cluster_task
            .send(Message {
                cmd: CmdArg::OperationRequest(operation_request),
                sender,
            })
            .await
            .map_err(|_| Error::from(io::Error::from(io::ErrorKind::BrokenPipe)))?;

        receiver
            .await
            .unwrap_or_else(|err| {
                Err(Error::from(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    err.to_string(),
                )))
            })
            .and_then(|response| match response {
                Response::Single(values) => Ok(values),
                Response::ClusterScanResult(..) | Response::Multiple(_) => {
                    Err(Error::from((
                        ErrorKind::ResponseError,
                        "Unexpected response variant for operation request",
                    )))
                }
            })
    }
}

#[derive(Clone)]
struct TokioDisconnectNotifier {
    disconnect_notifier: Arc<Notify>,
}

#[async_trait]
impl DisconnectNotifier for TokioDisconnectNotifier {
    fn notify_disconnect(&mut self) {
        self.disconnect_notifier.notify_one();
    }

    async fn wait_for_disconnect_with_timeout(&self, max_wait: &Duration) {
        let _ = timeout(*max_wait, async {
            self.disconnect_notifier.notified().await;
        })
        .await;
    }

    fn clone_box(&self) -> Box<dyn DisconnectNotifier> {
        Box::new(self.clone())
    }
}

impl TokioDisconnectNotifier {
    fn new() -> TokioDisconnectNotifier {
        TokioDisconnectNotifier {
            disconnect_notifier: Arc::new(Notify::new()),
        }
    }
}

type ConnectionMap<C> = container::ConnectionsMap<ConnectionFuture<C>>;
type ConnectionsContainer<C> = self::container::ConnectionsContainer<ConnectionFuture<C>>;

pub(crate) struct InnerCore<C> {
    pub(crate) conn_lock: ParkingLotRwLock<ConnectionsContainer<C>>,
    cluster_params: ParkingLotRwLock<ClusterParams>,
    pending_requests_tx: mpsc::UnboundedSender<PendingRequest<C>>,
    pending_requests_rx: std::sync::Mutex<mpsc::UnboundedReceiver<PendingRequest<C>>>,
    slot_refresh_state: SlotRefreshState,
    initial_nodes: Vec<ConnectionInfo>,
    ferriskey_connection_options: FerrisKeyConnectionOptions,
    /// Lock to ensure mutual exclusion between topology refresh operations and connection validation.
    ///
    /// This prevents validation from removing connections that were just created
    /// during topology discovery but haven't been assigned slots yet.
    pub(crate) topology_refresh_lock: tokio::sync::Mutex<()>,
}

pub(crate) type Core<C> = Arc<InnerCore<C>>;

impl<C> InnerCore<C>
where
    C: ConnectionLike + Connect + Clone + Send + Sync + 'static,
{
    fn get_cluster_param<T, F>(&self, f: F) -> T
    where
        F: FnOnce(&ClusterParams) -> T,
        T: Clone,
    {
        f(&self.cluster_params.read()).clone()
    }

    fn set_cluster_param<F>(&self, f: F)
    where
        F: FnOnce(&mut ClusterParams),
    {
        f(&mut self.cluster_params.write());
    }

    // return epoch of node
    pub(crate) async fn address_epoch(&self, node_address: &str) -> Result<u64> {
        let command = cmd("CLUSTER").arg("INFO").to_owned();
        let node_conn = {
            let conn_lock = self.conn_lock.read();
            conn_lock.connection_for_address(node_address)
        }
        .ok_or(Error::from((
            ErrorKind::ResponseError,
            "Failed to parse cluster info",
        )))?;

        let cluster_info = node_conn.1.await.req_packed_command(&command).await;
        match cluster_info {
            Ok(value) => {
                let info_dict: Result<InfoDict> =
                    FromValue::from_value(&value);
                if let Ok(info_dict) = info_dict {
                    let epoch = info_dict.get("cluster_my_epoch");
                    if let Some(epoch) = epoch {
                        Ok(epoch)
                    } else {
                        Err(Error::from((
                            ErrorKind::ResponseError,
                            "Failed to get epoch from cluster info",
                        )))
                    }
                } else {
                    Err(Error::from((
                        ErrorKind::ResponseError,
                        "Failed to parse cluster info",
                    )))
                }
            }
            Err(valkey_error) => Err(valkey_error),
        }
    }

    /// return slots of node
    pub(crate) async fn slots_of_address(&self, node_address: Arc<String>) -> Vec<u16> {
        self.conn_lock
            .read()
            .slot_map
            .get_slots_of_node(node_address)
    }

    /// Get connection for address
    pub(crate) async fn connection_for_address(
        &self,
        address: &str,
    ) -> Option<ConnectionFuture<C>> {
        self.conn_lock
            .read()
            .connection_for_address(address)
            .map(|(_, conn)| conn)
    }
}

pub(crate) struct ClusterConnInner<C> {
    pub(crate) inner: Core<C>,
    state: ConnectionState,
    #[allow(clippy::complexity)]
    in_flight_requests: stream::FuturesUnordered<Pin<Box<Request<C>>>>,
    refresh_error: Option<Error>,
    // Handler of the periodic check task.
    periodic_checks_handler: Option<JoinHandle<()>>,
    // Handler of fast connection validation task
    connections_validation_handler: Option<JoinHandle<()>>,
}

impl<C> Dispose for ClusterConnInner<C> {
    fn dispose(self) {
        // Connection count telemetry is handled by ConnectionsContainer::drop(),
        // so we only need to clean up task handles here.

        if let Some(handle) = self.periodic_checks_handler {
            handle.abort()
        }

        if let Some(handle) = self.connections_validation_handler {
            handle.abort()
        }

        tracing::info!(
            target: "ferriskey",
            event = "client_dropped",
            "ferriskey: cluster client dropped"
        );
    }
}

#[derive(Clone)]
pub(crate) enum InternalRoutingInfo<C> {
    SingleNode(InternalSingleNodeRouting<C>),
    MultiNode((MultipleNodeRoutingInfo, Option<ResponsePolicy>)),
}

#[derive(PartialEq, Clone, Debug)]
/// Represents different policies for refreshing the cluster slots.
pub(crate) enum RefreshPolicy {
    /// `Throttable` indicates that the refresh operation can be throttled,
    /// meaning it can be delayed or rate-limited if necessary.
    Throttable,
    /// `NotThrottable` indicates that the refresh operation should not be throttled,
    /// meaning it should be executed immediately without any delay or rate-limiting.
    NotThrottable,
}

/// Indicates what triggered a slot refresh operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SlotRefreshTrigger {
    /// Initial cluster connection setup - reuse existing connections to avoid double DNS lookup
    InitialConnection,
    /// Runtime refresh - periodic checks, topology changes, error recovery
    RuntimeRefresh,
}

impl<C> From<cluster_routing::RoutingInfo> for InternalRoutingInfo<C> {
    fn from(value: cluster_routing::RoutingInfo) -> Self {
        match value {
            cluster_routing::RoutingInfo::SingleNode(route) => {
                InternalRoutingInfo::SingleNode(route.into())
            }
            cluster_routing::RoutingInfo::MultiNode(routes) => {
                InternalRoutingInfo::MultiNode(routes)
            }
        }
    }
}

impl<C> From<InternalSingleNodeRouting<C>> for InternalRoutingInfo<C> {
    fn from(value: InternalSingleNodeRouting<C>) -> Self {
        InternalRoutingInfo::SingleNode(value)
    }
}

#[derive(Clone, Default)]
pub(crate) enum InternalSingleNodeRouting<C> {
    #[default]
    Random,
    SpecificNode(Route),
    ByAddress(String),
    Connection {
        address: Arc<str>,
        conn: ConnectionFuture<C>,
    },
    Redirect {
        redirect: Redirect,
        previous_routing: Box<InternalSingleNodeRouting<C>>,
    },
}

impl<C> From<SingleNodeRoutingInfo> for InternalSingleNodeRouting<C> {
    fn from(value: SingleNodeRoutingInfo) -> Self {
        match value {
            SingleNodeRoutingInfo::Random => InternalSingleNodeRouting::Random,
            SingleNodeRoutingInfo::SpecificNode(route) => {
                InternalSingleNodeRouting::SpecificNode(route)
            }
            SingleNodeRoutingInfo::RandomPrimary => {
                InternalSingleNodeRouting::SpecificNode(Route::new_random_primary())
            }
            SingleNodeRoutingInfo::ByAddress { host, port } => {
                InternalSingleNodeRouting::ByAddress(format!("{host}:{port}"))
            }
        }
    }
}

impl<C> From<Option<SingleNodeRoutingInfo>> for InternalSingleNodeRouting<C> {
    fn from(value: Option<SingleNodeRoutingInfo>) -> Self {
        match value {
            Some(single) => single.into(),
            None => InternalSingleNodeRouting::Random,
        }
    }
}

#[derive(Clone)]
enum CmdArg<C> {
    /// Single-node command: carries pre-packed bytes instead of cloning the Cmd.
    /// This is the hot path — GET, SET, etc. No heap allocation for the Cmd.
    Cmd {
        packed: bytes::Bytes,
        is_fenced: bool,
        routing: InternalRoutingInfo<C>,
    },
    /// Multi-node command: needs the full Cmd for fan-out splitting.
    /// Rare path — MGET across slots, DEL with keys in different slots, etc.
    MultiCmd {
        cmd: Arc<Cmd>,
        routing: InternalRoutingInfo<C>,
    },
    Pipeline {
        pipeline: Arc<crate::pipeline::Pipeline>,
        offset: usize,
        count: usize,
        route: Option<InternalSingleNodeRouting<C>>,
        sub_pipeline: bool,
        /// Configures retry behavior for pipeline commands.
        ///   - `retry_server_error`: If `true`, retries commands on server errors (may cause reordering).
        ///   - `retry_connection_error`: If `true`, retries on connection errors (may lead to duplicate executions).
        pipeline_retry_strategy: PipelineRetryStrategy,
    },
    ClusterScan {
        // struct containing the arguments for the cluster scan command - scan state cursor, match pattern, count and object type.
        cluster_scan_args: ClusterScanArgs,
    },
    // Operational requests which are connected to the internal state of the connection and not send as a command to the server.
    OperationRequest(Operation),
}

// Operation requests which are connected to the internal state of the connection and not send as a command to the server.
#[derive(Clone)]
enum Operation {
    UpdateConnectionPassword(Option<String>),
    UpdateConnectionDatabase(i64),
    UpdateConnectionClientName(Option<String>),
    UpdateConnectionUsername(Option<String>),
    UpdateConnectionProtocol(ProtocolVersion),
    GetUsername,
}

fn boxed_sleep(duration: Duration) -> BoxFuture<'static, ()> {
    Box::pin(tokio::time::sleep(duration))
}

#[derive(Debug, Display)]
pub(crate) enum Response {
    Single(Value),
    ClusterScanResult(ScanStateRC, Vec<Value>),
    Multiple(Vec<Result<Value>>),
}

#[derive(Debug)]
pub(crate) enum OperationTarget {
    Node { address: String },
    FanOut,
    NotFound,
    FatalError,
}
type OperationResult = std::result::Result<Response, (OperationTarget, Error)>;

impl From<String> for OperationTarget {
    fn from(address: String) -> Self {
        OperationTarget::Node { address }
    }
}

/// Represents a node to which a `MOVED` or `ASK` error redirects.
#[derive(Clone, Debug)]
pub(crate) struct RedirectNode {
    /// The address of the redirect node.
    pub address: String,
    /// The slot of the redirect node.
    pub slot: u16,
}

impl RedirectNode {
    /// Constructs a `RedirectNode` from an optional tuple containing an address and a slot number.
    pub(crate) fn from_option_tuple(option: Option<(&str, u16)>) -> Option<Self> {
        option.map(|(address, slot)| RedirectNode {
            address: address.to_string(),
            slot,
        })
    }
}

struct Message<C: Sized> {
    cmd: CmdArg<C>,
    sender: oneshot::Sender<Result<Response>>,
}

enum RecoverFuture {
    RefreshingSlots(JoinHandle<Result<()>>),
    ReconnectToInitialNodes(JoinHandle<()>),
    Reconnect(JoinHandle<()>),
}

enum ConnectionState {
    PollComplete,
    Recover(RecoverFuture),
}

impl fmt::Debug for ConnectionState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}",
            match self {
                ConnectionState::PollComplete => "PollComplete",
                ConnectionState::Recover(_) => "Recover",
            }
        )
    }
}

#[derive(Clone)]
struct RequestInfo<C> {
    cmd: CmdArg<C>,
}

impl<C> RequestInfo<C> {
    fn set_redirect(&mut self, redirect: Option<Redirect>) {
        if let Some(redirect) = redirect {
            match &mut self.cmd {
                CmdArg::Cmd { routing, .. } | CmdArg::MultiCmd { routing, .. } => match routing {
                    InternalRoutingInfo::SingleNode(route) => {
                        let redirect = InternalSingleNodeRouting::Redirect {
                            redirect,
                            previous_routing: Box::new(std::mem::take(route)),
                        }
                        .into();
                        *routing = redirect;
                    }
                    InternalRoutingInfo::MultiNode(_) => {
                        // Multi-node requests cannot be redirected — ignore the redirect.
                    }
                },
                CmdArg::Pipeline { route, .. } => {
                    let redirect = InternalSingleNodeRouting::Redirect {
                        redirect,
                        previous_routing: Box::new(
                            route.take().unwrap_or(InternalSingleNodeRouting::Random),
                        ),
                    };
                    *route = Some(redirect);
                }
                // cluster_scan is sent as a normal command internally so we should not reach this point.
                CmdArg::ClusterScan { .. } => {}
                // Operation requests are not routed — ignore redirects.
                CmdArg::OperationRequest(_) => {}
            }
        }
    }

    fn reset_routing(&mut self) {
        let fix_route = |route: &mut InternalSingleNodeRouting<C>| {
            match route {
                InternalSingleNodeRouting::Redirect {
                    previous_routing, ..
                } => {
                    let previous_routing = std::mem::take(previous_routing.as_mut());
                    *route = previous_routing;
                }
                // If a specific connection is specified, then reconnecting without resetting the routing
                // will mean that the request is still routed to the old connection.
                InternalSingleNodeRouting::Connection { address, .. } => {
                    *route = InternalSingleNodeRouting::ByAddress(address.to_string());
                }
                _ => {}
            }
        };
        match &mut self.cmd {
            CmdArg::Cmd { routing, .. } | CmdArg::MultiCmd { routing, .. } => {
                if let InternalRoutingInfo::SingleNode(route) = routing {
                    fix_route(route);
                }
            }
            CmdArg::Pipeline { route, .. } => {
                if let Some(route) = route.as_mut() {
                    fix_route(route);
                }
            }
            // These variants don't have routing to reset — no-op.
            CmdArg::ClusterScan { .. } | CmdArg::OperationRequest { .. } => {}
        }
    }
}

pin_project! {
    #[project = RequestStateProj]
    enum RequestState<F> {
        None,
        Future {
            #[pin]
            future: F,
        },
        Sleep {
            #[pin]
            sleep: BoxFuture<'static, ()>,
        },
        UpdateMoved {
            #[pin]
            future: BoxFuture<'static, Result<()>>,
        },
    }
}

// InflightSlotGuard and InflightRequestTracker live in crate::value
// to avoid an upward dependency from cmd::Cmd into cluster_async.
pub use crate::value::InflightRequestTracker;

#[cfg(test)]
mod iam_token_refresh_tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Mock IAM token provider that returns a configurable token and tracks call count.
    struct MockTokenProvider {
        token: std::sync::Mutex<String>,
        call_count: AtomicUsize,
    }

    impl MockTokenProvider {
        fn new(token: &str) -> Arc<Self> {
            Arc::new(Self {
                token: std::sync::Mutex::new(token.to_string()),
                call_count: AtomicUsize::new(0),
            })
        }

        fn set_token(&self, token: &str) {
            *self.token.lock().unwrap() = token.to_string();
        }

        fn calls(&self) -> usize {
            self.call_count.load(Ordering::Relaxed)
        }
    }

    #[async_trait::async_trait]
    impl IAMTokenProvider for MockTokenProvider {
        async fn get_valid_token(&self) -> Option<String> {
            self.call_count.fetch_add(1, Ordering::Relaxed);
            let token = self.token.lock().unwrap().clone();
            if token.is_empty() { None } else { Some(token) }
        }
    }

    /// Helper to build a minimal FerrisKeyConnectionOptions with an IAM provider.
    fn options_with_provider(
        provider: Option<Arc<dyn IAMTokenProvider>>,
    ) -> FerrisKeyConnectionOptions {
        FerrisKeyConnectionOptions {
            push_sender: None,
            disconnect_notifier: None,
            discover_az: false,
            connection_timeout: None,
            connection_retry_strategy: None,
            tcp_nodelay: false,
            pubsub_synchronizer: None,
            iam_token_provider: provider,
        }
    }

    /// Helper to build a minimal InnerCore with the given password and IAM provider.
    fn build_inner(
        initial_password: Option<String>,
        provider: Option<Arc<dyn IAMTokenProvider>>,
    ) -> Arc<InnerCore<crate::connection::MultiplexedConnection>> {
        use crate::cluster::client::ClusterParams;
        use container::ConnectionsContainer;

        let params = ClusterParams::default_for_test(initial_password);

        let (pending_requests_tx, pending_requests_rx) = tokio::sync::mpsc::unbounded_channel();
        Arc::new(InnerCore {
            conn_lock: ParkingLotRwLock::new(ConnectionsContainer::default()),
            cluster_params: ParkingLotRwLock::new(params),
            pending_requests_tx,
            pending_requests_rx: std::sync::Mutex::new(pending_requests_rx),
            slot_refresh_state: SlotRefreshState::new(
                crate::cluster::client::SlotsRefreshRateLimit::default(),
            ),
            initial_nodes: Vec::new(),
            ferriskey_connection_options: options_with_provider(provider),
            topology_refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn read_password(
        inner: &Arc<InnerCore<crate::connection::MultiplexedConnection>>,
    ) -> Option<String> {
        inner.cluster_params.read().password.clone()
    }

    #[tokio::test]
    async fn refresh_updates_password_when_provider_returns_token() {
        let provider = MockTokenProvider::new("fresh-token-123");
        let inner = build_inner(Some("old-token".into()), Some(provider.clone()));

        ClusterConnInner::refresh_iam_token_in_cluster_params(&inner).await;

        assert_eq!(read_password(&inner), Some("fresh-token-123".into()));
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn refresh_does_not_change_password_when_provider_returns_none() {
        let provider = MockTokenProvider::new(""); // returns None
        let inner = build_inner(Some("old-token".into()), Some(provider.clone()));

        ClusterConnInner::refresh_iam_token_in_cluster_params(&inner).await;

        assert_eq!(read_password(&inner), Some("old-token".into()));
        assert_eq!(provider.calls(), 1);
    }

    #[tokio::test]
    async fn refresh_is_noop_when_no_provider_configured() {
        let inner = build_inner(Some("static-password".into()), None);

        ClusterConnInner::refresh_iam_token_in_cluster_params(&inner).await;

        assert_eq!(read_password(&inner), Some("static-password".into()));
    }

    #[tokio::test]
    async fn refresh_sets_password_when_initially_none() {
        let provider = MockTokenProvider::new("first-token");
        let inner = build_inner(None, Some(provider.clone()));

        ClusterConnInner::refresh_iam_token_in_cluster_params(&inner).await;

        assert_eq!(read_password(&inner), Some("first-token".into()));
    }

    #[tokio::test]
    async fn refresh_picks_up_new_token_on_second_call() {
        let provider = MockTokenProvider::new("token-v1");
        let inner = build_inner(None, Some(provider.clone()));

        ClusterConnInner::refresh_iam_token_in_cluster_params(&inner).await;
        assert_eq!(read_password(&inner), Some("token-v1".into()));

        provider.set_token("token-v2");
        ClusterConnInner::refresh_iam_token_in_cluster_params(&inner).await;
        assert_eq!(read_password(&inner), Some("token-v2".into()));
        assert_eq!(provider.calls(), 2);
    }
}

struct PendingRequest<C> {
    retry: u32,
    sender: oneshot::Sender<Result<Response>>,
    info: RequestInfo<C>,
}

pin_project! {
    struct Request<C> {
        retry_params: RetryParams,
        request: Option<PendingRequest<C>>,
        #[pin]
        future: RequestState<BoxFuture<'static, OperationResult>>,
    }
}

#[must_use]
enum Next<C> {
    Retry {
        request: PendingRequest<C>,
    },
    RetryBusyLoadingError {
        request: PendingRequest<C>,
        address: String,
    },
    Reconnect {
        // if not set, then a reconnect should happen without sending a request afterwards
        request: Option<PendingRequest<C>>,
        target: String,
    },
    RefreshSlots {
        // if not set, then a slot refresh should happen without sending a request afterwards
        request: Option<PendingRequest<C>>,
        sleep_duration: Option<Duration>,
        moved_redirect: Option<RedirectNode>,
    },
    ReconnectToInitialNodes {
        // if not set, then a reconnect should happen without sending a request afterwards
        request: Option<PendingRequest<C>>,
    },
    Done,
}

impl<C> Future for Request<C> {
    type Output = Next<C>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut task::Context) -> Poll<Self::Output> {
        let mut this = self.as_mut().project();
        // If the sender is closed, the caller is no longer waiting for the reply, and it is ambiguous
        // whether they expect the side-effect of the request to happen or not.
        if this.request.is_none() || this.request.as_ref().unwrap().sender.is_closed() {
            return Poll::Ready(Next::Done);
        }
        let future = match this.future.as_mut().project() {
            RequestStateProj::Future { future } => future,
            RequestStateProj::Sleep { sleep } => {
                ready!(sleep.poll(cx));
                return Next::Retry {
                    request: self.project().request.take().unwrap(),
                }
                .into();
            }
            RequestStateProj::UpdateMoved { future } => {
                if let Err(err) = ready!(future.poll(cx)) {
                    // Updating the slot map based on the MOVED error is an optimization.
                    // If it fails, proceed by retrying the request with the redirected node,
                    // and allow the slot refresh task to correct the slot map.
                    info!(
                        "Failed to update the slot map based on the received MOVED error.
                        Error: {err:?}"
                    );
                }
                if let Some(request) = self.project().request.take() {
                    return Next::Retry { request }.into();
                }
                return Next::Done.into();
            }
            _ => return Poll::Ready(Next::Done),
        };

        match ready!(future.poll(cx)) {
            Ok(item) => {
                self.respond(Ok(item));
                Next::Done.into()
            }
            Err((target, err)) => {
                let request = this.request.as_mut().unwrap();
                // TODO - would be nice if we didn't need to repeat this code twice, with & without retries.
                if request.retry >= this.retry_params.number_of_retries {
                    let retry_method = err.retry_method();
                    let next = if err.kind() == ErrorKind::AllConnectionsUnavailable {
                        Next::ReconnectToInitialNodes { request: None }.into()
                    } else if matches!(
                        err.retry_method(),
                        RetryMethod::MovedRedirect | RetryMethod::RefreshSlotsAndRetry
                    ) || matches!(target, OperationTarget::NotFound)
                    {
                        Next::RefreshSlots {
                            request: None,
                            sleep_duration: None,
                            moved_redirect: RedirectNode::from_option_tuple(err.redirect_node()),
                        }
                        .into()
                    } else if matches!(retry_method, RetryMethod::Reconnect)
                        || matches!(retry_method, RetryMethod::ReconnectAndRetry)
                    {
                        if let OperationTarget::Node { address } = target {
                            Next::Reconnect {
                                request: None,
                                target: address,
                            }
                            .into()
                        } else {
                            Next::Done.into()
                        }
                    } else {
                        Next::Done.into()
                    };
                    self.respond(Err(err));
                    return next;
                }
                request.retry = request.retry.saturating_add(1);
                tracing::warn!(
                    target: "ferriskey",
                    event = "retry",
                    attempt = request.retry,
                    "ferriskey: cluster command retry"
                );

                if err.kind() == ErrorKind::AllConnectionsUnavailable {
                    return Next::ReconnectToInitialNodes {
                        request: Some(this.request.take().unwrap()),
                    }
                    .into();
                }

                let sleep_duration = this.retry_params.wait_time_for_retry(request.retry);

                let address = match target {
                    OperationTarget::Node { address } => address,
                    OperationTarget::FanOut => {
                        trace!("Request error `{}` multi-node request", err);

                        // Fanout operation are retried per internal request, and don't need additional retries.
                        self.respond(Err(err));
                        return Next::Done.into();
                    }
                    OperationTarget::NotFound => {
                        // TODO - this is essentially a repeat of the retirable error. probably can remove duplication.
                        let mut request = this.request.take().unwrap();
                        request.info.reset_routing();
                        return Next::RefreshSlots {
                            request: Some(request),
                            sleep_duration: Some(sleep_duration),
                            moved_redirect: None,
                        }
                        .into();
                    }
                    OperationTarget::FatalError => {
                        trace!("Fatal error encountered: {:?}", err);
                        self.respond(Err(err));
                        return Next::Done.into();
                    }
                };

                warn!("Received request error {} on node {:?}.", err, address);

                match err.retry_method() {
                    RetryMethod::AskRedirect => {
                        let mut request = this.request.take().unwrap();
                        request.info.set_redirect(
                            err.redirect_node()
                                .map(|(node, _slot)| Redirect::Ask(node.to_string(), true)),
                        );
                        Next::Retry { request }.into()
                    }
                    RetryMethod::MovedRedirect => {
                        let mut request = this.request.take().unwrap();
                        let redirect_node = err.redirect_node();
                        request.info.set_redirect(
                            err.redirect_node()
                                .map(|(node, _slot)| Redirect::Moved(node.to_string())),
                        );
                        Next::RefreshSlots {
                            request: Some(request),
                            sleep_duration: None,
                            moved_redirect: RedirectNode::from_option_tuple(redirect_node),
                        }
                        .into()
                    }
                    RetryMethod::RefreshSlotsAndRetry => {
                        let mut request = this.request.take().unwrap();
                        request.info.reset_routing();
                        Next::RefreshSlots {
                            request: Some(request),
                            sleep_duration: Some(sleep_duration),
                            moved_redirect: None,
                        }
                        .into()
                    }
                    RetryMethod::WaitAndRetry => {
                        let sleep_duration = this.retry_params.wait_time_for_retry(request.retry);
                        // Sleep and retry.
                        this.future.set(RequestState::Sleep {
                            sleep: boxed_sleep(sleep_duration),
                        });
                        self.poll(cx)
                    }
                    RetryMethod::Reconnect | RetryMethod::ReconnectAndRetry => {
                        let mut request = this.request.take().unwrap();
                        // TODO should we reset the redirect here?
                        request.info.reset_routing();
                        warn!("disconnected from {:?}", address);
                        let should_retry =
                            matches!(err.retry_method(), RetryMethod::ReconnectAndRetry);
                        Next::Reconnect {
                            request: should_retry.then_some(request),
                            target: address,
                        }
                        .into()
                    }
                    RetryMethod::WaitAndRetryOnPrimaryRedirectOnReplica => {
                        Next::RetryBusyLoadingError {
                            request: this.request.take().unwrap(),
                            address,
                        }
                        .into()
                    }
                    RetryMethod::RetryImmediately => Next::Retry {
                        request: this.request.take().unwrap(),
                    }
                    .into(),
                    RetryMethod::NoRetry => {
                        self.respond(Err(err));
                        Next::Done.into()
                    }
                }
            }
        }
    }
}

impl<C> Request<C> {
    fn respond(self: Pin<&mut Self>, msg: Result<Response>) {
        // If `send` errors the receiver has dropped and thus does not care about the message.
        // If the request was already taken (result sent once), this is a no-op.
        if let Some(req) = self.project().request.take() {
            let _ = req.sender.send(msg);
        }
    }
}

enum ConnectionCheck<C> {
    Found((Arc<str>, ConnectionFuture<C>)),
    OnlyAddress(String),
    RandomConnection,
}

impl<C> ClusterConnInner<C>
where
    C: ConnectionLike + Connect + Clone + Send + Sync + 'static,
{
    async fn new(
        initial_nodes: &[ConnectionInfo],
        cluster_params: ClusterParams,
        push_sender: Option<mpsc::UnboundedSender<PushInfo>>,
        pubsub_synchronizer: Option<Arc<dyn crate::pubsub::synchronizer_trait::PubSubSynchronizer>>,
        iam_token_provider: Option<Arc<dyn IAMTokenProvider>>,
    ) -> Result<Disposable<Self>> {
        let disconnect_notifier: Option<Box<dyn DisconnectNotifier>> =
            Some(Box::new(TokioDisconnectNotifier::new()));

        let discover_az = matches!(
            cluster_params.read_from_replicas,
            crate::cluster::slotmap::ReadFromReplicaStrategy::AZAffinity(_)
                | crate::cluster::slotmap::ReadFromReplicaStrategy::AZAffinityReplicasAndPrimary(_)
        );

        let connection_retry_strategy = cluster_params.reconnect_retry_strategy.unwrap_or_default();

        let ferriskey_connection_options = FerrisKeyConnectionOptions {
            push_sender,
            disconnect_notifier,
            discover_az,
            connection_timeout: Some(cluster_params.connection_timeout),
            connection_retry_strategy: Some(connection_retry_strategy),
            tcp_nodelay: cluster_params.tcp_nodelay,
            pubsub_synchronizer,
            iam_token_provider,
        };

        let connections = Self::create_initial_connections(
            initial_nodes,
            &cluster_params,
            ferriskey_connection_options.clone(),
        )
        .await?;

        let topology_checks_interval = cluster_params.topology_checks_interval;
        let slots_refresh_rate_limiter = cluster_params.slots_refresh_rate_limit;
        let (pending_tx, pending_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(InnerCore {
            conn_lock: ParkingLotRwLock::new(ConnectionsContainer::new(
                Default::default(),
                connections,
                cluster_params.read_from_replicas.clone(),
                0,
            )),
            cluster_params: ParkingLotRwLock::new(cluster_params.clone()),
            pending_requests_tx: pending_tx,
            pending_requests_rx: std::sync::Mutex::new(pending_rx),
            slot_refresh_state: SlotRefreshState::new(slots_refresh_rate_limiter),
            initial_nodes: initial_nodes.to_vec(),
            ferriskey_connection_options,
            topology_refresh_lock: tokio::sync::Mutex::new(()),
        });
        let mut connection = ClusterConnInner {
            inner,
            in_flight_requests: Default::default(),
            refresh_error: None,
            state: ConnectionState::PollComplete,
            periodic_checks_handler: None,
            connections_validation_handler: None,
        };
        // Initial slots and subscriptions refresh
        Self::refresh_slots_and_subscriptions_with_retries(
            connection.inner.clone(),
            &RefreshPolicy::NotThrottable,
            SlotRefreshTrigger::InitialConnection,
        )
        .await?;

        if let Some(duration) = topology_checks_interval {
            let periodic_task =
                ClusterConnInner::periodic_topology_check(connection.inner.clone(), duration);
            {
                connection.periodic_checks_handler = Some(tokio::spawn(periodic_task));
            }
        }

        let connections_validation_interval = cluster_params.connections_validation_interval;
        if let Some(duration) = connections_validation_interval {
            let connections_validation_handler =
                ClusterConnInner::connections_validation_task(connection.inner.clone(), duration);
            {
                connection.connections_validation_handler =
                    Some(tokio::spawn(connections_validation_handler));
            }
        }

        tracing::info!(
            target: "ferriskey",
            event = "client_created",
            kind = "cluster",
            "ferriskey: cluster client connected"
        );
        Ok(Disposable::new(connection))
    }

    /// Go through each of the initial nodes and attempt to retrieve all IP entries from them.
    /// If there's a DNS endpoint that directs to several IP addresses, add all addresses to the initial nodes list.
    /// Returns a vector of tuples, each containing a node's address (including the hostname) and its corresponding SocketAddr if retrieved.
    pub(crate) async fn try_to_expand_initial_nodes(
        initial_nodes: &[ConnectionInfo],
    ) -> Vec<(String, Option<SocketAddr>)> {
        stream::iter(initial_nodes)
            .fold(
                Vec::with_capacity(initial_nodes.len()),
                |mut acc, info| async {
                    let (host, port) = match &info.addr {
                        crate::connection::info::ConnectionAddr::Tcp(host, port) => (host, port),
                        crate::connection::info::ConnectionAddr::TcpTls {
                            host,
                            port,
                            insecure: _,
                            tls_params: _,
                        } => (host, port),
                        crate::connection::info::ConnectionAddr::Unix(_) => {
                            // We don't support multiple addresses for a Unix address. Store the initial node address and continue
                            acc.push((info.addr.to_string(), None));
                            return acc;
                        }
                    };
                    match get_socket_addrs(host, *port).await {
                        Ok(socket_addrs) => {
                            for addr in socket_addrs {
                                acc.push((info.addr.to_string(), Some(addr)));
                            }
                        }
                        Err(_) => {
                            // Couldn't find socket addresses, store the initial node address and continue
                            acc.push((info.addr.to_string(), None));
                        }
                    };
                    acc
                },
            )
            .await
    }

    async fn create_initial_connections(
        initial_nodes: &[ConnectionInfo],
        params: &ClusterParams,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> Result<ConnectionMap<C>> {
        let initial_nodes: Vec<(String, Option<SocketAddr>)> =
            Self::try_to_expand_initial_nodes(initial_nodes).await;
        let connections = stream::iter(initial_nodes.iter().cloned())
            .map(|(node_addr, socket_addr)| {
                let params: ClusterParams = params.clone();
                let ferriskey_connection_options = ferriskey_connection_options.clone();
                // set subscriptions to none, they will be applied upon the topology discovery

                async move {
                    let result = connect_and_check::<C>(
                        &node_addr,
                        params,
                        socket_addr,
                        RefreshConnectionType::AllConnections,
                        None,
                        ferriskey_connection_options,
                    )
                    .await
                    .get_node();
                    // The PushManager is initialized with connection_info.addr
                    // (the original hostname, e.g. "localhost:6379"), but the
                    // ConnectionsMap key uses the resolved IP from socket_addr
                    // (e.g. "127.0.0.1:6379"). When these differ, align them so
                    // PubSub synchronization can match subscriptions to nodes.
                    let (node_address, push_manager_needs_update) =
                        if let Some(socket_addr) = socket_addr {
                            let resolved = socket_addr.to_string();
                            let differs = resolved != node_addr;
                            (resolved, differs)
                        } else {
                            (node_addr, false)
                        };
                    if push_manager_needs_update && let Ok(ref node) = result {
                        node.user_connection
                            .conn
                            .clone()
                            .await
                            .update_push_manager_node_address(node_address.clone());
                    }
                    result.map(|node| (node_address, node))
                }
            })
            .buffer_unordered(initial_nodes.len())
            .fold(
                (
                    ConnectionsMap(DashMap::with_capacity(initial_nodes.len())),
                    None,
                ),
                |connections: (ConnectionMap<C>, Option<String>),
                 addr_conn_res: Result<_>| async move {
                    match addr_conn_res {
                        Ok((addr, node)) => {
                            connections.0.0.insert(Arc::from(&*addr), node);
                            (connections.0, None)
                        }
                        Err(e) => (connections.0, Some(e.to_string())),
                    }
                },
            )
            .await;
        if connections.0.0.is_empty() {
            return Err(Error::from((
                ErrorKind::IoError,
                "Failed to create initial connections",
                connections.1.unwrap_or("".to_string()),
            )));
        }
        info!("Connected to initial nodes:\n{}", connections.0);
        Ok(connections.0)
    }

    /// If IAM authentication is configured, refresh the token in `cluster_params` so that
    /// any subsequent connection attempts use a valid credential.
    async fn refresh_iam_token_in_cluster_params(inner: &Arc<InnerCore<C>>) {
        if let Some(ref token_provider) = inner.ferriskey_connection_options.iam_token_provider
            && let Some(valid_token) = token_provider.get_valid_token().await
        {
            inner.set_cluster_param(|params| {
                params.password = Some(valid_token);
            });
        }
    }

    // Reconnect to the initial nodes provided by the user in the creation of the client,
    // and try to refresh the slots based on the initial connections.
    // Being used when all cluster connections are unavailable.
    fn reconnect_to_initial_nodes(inner: Arc<InnerCore<C>>) -> impl Future<Output = ()> {
        let inner = inner.clone();
        Box::pin(async move {
            Self::refresh_iam_token_in_cluster_params(&inner).await;
            let cluster_params = inner.get_cluster_param(|params| params.clone());
            let connection_map = match Self::create_initial_connections(
                &inner.initial_nodes,
                &cluster_params,
                inner.ferriskey_connection_options.clone(),
            )
            .await
            {
                Ok(map) => map,
                Err(err) => {
                    warn!("Can't reconnect to initial nodes: `{err}`");
                    return;
                }
            };
            inner
                .conn_lock
                .write()
                .extend_connection_map(connection_map);
            if let Err(err) = Self::refresh_slots_and_subscriptions_with_retries(
                inner.clone(),
                &RefreshPolicy::NotThrottable,
                SlotRefreshTrigger::InitialConnection,
            )
            .await
            {
                warn!("Can't refresh slots with initial nodes: `{err}`");
            };
        })
    }

    // Validate all existing user connections and try to reconnect if necessary.
    // In addition, as a safety measure, drop nodes that do not have any assigned slots.
    // This function serves as a cheap alternative to slot_refresh() and thus can be used much more frequently.
    // The function does not discover the topology from the cluster and assumes the cached topology is valid.
    // In addition, the validation is done by peeking at the state of the underlying transport w/o overhead of additional commands to server.
    // If we're during slot refresh, we skip the validation to avoid interfering with the slot refresh process.
    async fn validate_all_user_connections(inner: Arc<InnerCore<C>>) {
        // Try to acquire the topology refresh lock - if we can't, it means a slot refresh is in progress.
        let _guard = match inner.topology_refresh_lock.try_lock() {
            Ok(guard) => guard,
            Err(_) => {
                debug!("Skipping connection validation - topology refresh in progress");
                return;
            }
        };

        let mut all_valid_conns = HashMap::new();
        // prep connections and clean out these w/o assigned slots, as we might have established connections to unwanted hosts
        let mut nodes_to_delete = Vec::new();
        let all_nodes_with_slots: HashSet<Arc<String>>;
        {
            let connections_container = inner.conn_lock.read();

            all_nodes_with_slots = connections_container.slot_map.all_node_addresses();

            connections_container
                .all_node_connections()
                .for_each(|(addr, con)| {
                    if all_nodes_with_slots.iter().any(|a| a.as_str() == &*addr) {
                        all_valid_conns.insert(addr.clone(), con.clone());
                    } else {
                        nodes_to_delete.push(addr.clone());
                    }
                });

            for addr in &nodes_to_delete {
                connections_container.remove_node(addr);
            }
        }

        // identify nodes with closed connection
        let mut addrs_to_refresh = HashSet::new();
        for (addr, con_fut) in &all_valid_conns {
            let con = con_fut.clone().await;
            // connection object might be present despite the transport being closed
            if con.is_closed() {
                // transport is closed, need to refresh
                addrs_to_refresh.insert(addr.clone());
            }
        }

        // identify missing nodes
        addrs_to_refresh.extend(
            all_nodes_with_slots
                .iter()
                .filter(|addr| !all_valid_conns.contains_key(addr.as_str()))
                .map(|addr| Arc::from(addr.as_str())),
        );

        if !addrs_to_refresh.is_empty() {
            // don't try existing nodes since we know a. it does not exist. b. exist but its connection is closed
            Self::trigger_refresh_connection_tasks(
                inner.clone(),
                addrs_to_refresh,
                RefreshConnectionType::AllConnections,
                false,
            )
            .await;
        }
    }

    // Creates refresh tasks and await on the tasks' notifier.
    // Awaiting on the notifier guaranties at least one reconnect attempt on each address.
    async fn refresh_and_update_connections(
        inner: Arc<InnerCore<C>>,
        addresses: HashSet<Arc<str>>,
        conn_type: RefreshConnectionType,
        check_existing_conn: bool,
    ) {
        trace!("refresh_and_update_connections: calling trigger_refresh_connection_tasks");
        let refresh_task_notifiers = Self::trigger_refresh_connection_tasks(
            inner.clone(),
            addresses,
            conn_type,
            check_existing_conn,
        )
        .await;

        trace!("refresh_and_update_connections: Await on all tasks' refresh notifier");
        futures::future::join_all(
            refresh_task_notifiers
                .iter()
                .map(|notify| notify.notified()),
        )
        .await;
    }

    // Triggers a reconnection Tokio task for each supplied address.
    // If a refresh task is already running for an address, no new task is created;
    // instead, the notifier from the existing task is returned.
    // Returns a vector of notifiers for the refresh tasks (new or existing) corresponding to the supplied addresses.
    async fn trigger_refresh_connection_tasks(
        inner: Arc<InnerCore<C>>,
        addresses: HashSet<Arc<str>>,
        conn_type: RefreshConnectionType,
        check_existing_conn: bool,
    ) -> Vec<Arc<Notify>> {
        debug!("Triggering refresh connections tasks to {:?} ", addresses);

        let mut notifiers = Vec::<Arc<Notify>>::new();

        for address in addresses {
            // Use a single write lock to atomically check-and-insert, preventing
            // duplicate refresh tasks from racing between the check and insert.
            let mut conn_container = inner.conn_lock.write();
            if let Some(existing_task) = conn_container
                .refresh_conn_state
                .refresh_address_in_progress
                .get(&*address)
            {
                if let RefreshTaskStatus::Reconnecting(ref notifier) = existing_task.status {
                    // Store the notifier
                    notifiers.push(notifier.get_notifier());
                }
                debug!("Skipping refresh for {}: already in progress", address);
                drop(conn_container);
                continue; // Skip creating a new refresh task
            }

            let mut node_option = conn_container.remove_node(&address);

            if !check_existing_conn {
                node_option = None;
            }

            let inner_clone = inner.clone();
            let address_clone_for_task = address.clone();

            let handle = tokio::spawn(async move {
                info!(
                    "refreshing connection task to {:?} started",
                    address_clone_for_task
                );

                // We run infinite retries to reconnect until it succeeds or it's aborted from outside.
                let retry_strategy = inner_clone
                    .ferriskey_connection_options
                    .connection_retry_strategy
                    .unwrap_or_default();
                let infinite_backoff_iter = retry_strategy.get_infinite_backoff_dur_iterator();

                let mut node_result = Err(Error::from((
                    ErrorKind::ClientError,
                    "No attempts performed",
                )));
                let mut first_attempt = true;
                for backoff_duration in infinite_backoff_iter {
                    Self::refresh_iam_token_in_cluster_params(&inner_clone).await;

                    let cluster_params = inner_clone.get_cluster_param(|params| params.clone());

                    node_result = get_or_create_conn(
                        &address_clone_for_task,
                        node_option.clone(),
                        &cluster_params,
                        conn_type,
                        inner_clone.ferriskey_connection_options.clone(),
                    )
                    .await;

                    match node_result {
                        Ok(_) => {
                            break;
                        }
                        Err(ref err) => {
                            if first_attempt {
                                if let Some(ref mut conn_state) = inner_clone
                                    .conn_lock
                                    .write()
                                    .refresh_conn_state
                                    .refresh_address_in_progress
                                    .get_mut(&*address_clone_for_task)
                                {
                                    conn_state.status.flip_status_to_too_long();
                                }

                                first_attempt = false;
                            }
                            debug!(
                                "Failed to refresh connection for node {}. Error: `{:?}`. Retrying in {:?}",
                                address_clone_for_task, err, backoff_duration
                            );
                            tokio::time::sleep(backoff_duration).await;
                        }
                    }
                }

                match node_result {
                    Ok(node) => {
                        info!(
                            "Succeeded to refresh connection for node {}.",
                            address_clone_for_task
                        );
                        inner_clone
                            .conn_lock
                            .read()
                            .replace_or_add_connection_for_address(&*address_clone_for_task, node);
                    }
                    Err(err) => {
                        warn!(
                            "Failed to refresh connection for node {}. Error: `{:?}`",
                            address_clone_for_task, err
                        );
                    }
                }

                inner_clone
                    .conn_lock
                    .write()
                    .refresh_conn_state
                    .refresh_address_in_progress
                    .remove(&*address_clone_for_task);

                debug!(
                    "Refreshing connection task to {:?} is done",
                    address_clone_for_task
                );
            });

            let notifier = RefreshTaskNotifier::new();
            notifiers.push(notifier.get_notifier());

            // Keep the task handle and notifier into the RefreshState of this address
            let refresh_task_state = RefreshTaskState::new(handle, notifier);

            conn_container
                .refresh_conn_state
                .refresh_address_in_progress
                .insert(address, refresh_task_state);
            // Write lock is dropped here at end of loop iteration
        }
        debug!("trigger_refresh_connection_tasks: Done");
        notifiers
    }

    fn spawn_refresh_slots_task(
        inner: Arc<InnerCore<C>>,
        policy: &RefreshPolicy,
    ) -> JoinHandle<Result<()>> {
        // Clone references for task
        let inner_clone = inner.clone();
        let policy_clone = policy.clone();

        // Spawn the background task and return its handle
        tokio::spawn(async move {
            Self::refresh_slots_and_subscriptions_with_retries(
                inner_clone,
                &policy_clone,
                SlotRefreshTrigger::RuntimeRefresh,
            )
            .await
        })
    }

    /// Asynchronously collects and aggregates responses from multiple cluster nodes according to a specified policy.
    ///
    /// This function drives the fan‐out of a multi‐node command, awaiting individual node replies
    /// and combining or selecting results based on `response_policy`. It covers these high‐level steps:
    ///
    /// # Arguments
    ///
    /// * `receivers`: A list of `(node_address, oneshot::Receiver<Result<Response>>)` pairs.
    ///   Each receiver will yield exactly one `Response` (or error) from its node.
    /// * `routing` – The routing information of the command (e.g., multi‐slot, `AllNodes`, `AllPrimaries`).
    /// * `response_policy` – An `Option<ResponsePolicy>` that dictates how to aggregate the results from the different nodes.
    ///
    /// # Returns
    ///
    /// A `Result<Value>` representing the aggregated result.
    pub async fn aggregate_results(
        receivers: Vec<NodeResponseReceiver>,
        routing: &MultipleNodeRoutingInfo,
        response_policy: Option<ResponsePolicy>,
    ) -> Result<Value> {
        // Helper: extract a single Value from a Response::Single.
        // Only Response::Single is expected for multi-node commands.
        let extract_result = |response| match response {
            Response::Single(value) => Ok(value),
            Response::Multiple(_) | Response::ClusterScanResult(_, _) => Err(Error::from((
                ErrorKind::ResponseError,
                "Unexpected response variant in aggregate_results",
            ))),
        };

        // Converts a Result<Result<Response>, _> into Result<Value>
        let convert_result = |res: std::result::Result<Result<Response>, _>| {
            res.map_err(|_| {
                Error::from((
                    ErrorKind::ResponseError,
                    "Internal failure: receiver was dropped before delivering a response",
                ))
            }) // this happens only if the result sender is dropped before usage.
            .and_then(|res| res.and_then(extract_result))
        };

        // Helper: await a (addr, receiver) and return (addr, Result<Value>)
        let get_receiver = |(_, receiver): (_, oneshot::Receiver<Result<Response>>)| async {
            convert_result(receiver.await)
        };

        // Sanity check: if there are no receivers at all, this is a client‐error
        if receivers.is_empty() {
            return Err(Error::from((
                ErrorKind::ClientError,
                "Client internal error",
                "Failed to aggregate results for multi-slot command. Maybe a malformed command?"
                    .to_string(),
            )));
        }

        match response_policy {
            // ────────────────────────────────────────────────────────────────
            // ResponsePolicy::OneSucceeded:
            // Waits for the *first* successful response and returns it.
            // Any other in-flight responses are dropped.
            // ────────────────────────────────────────────────────────────────
            Some(ResponsePolicy::OneSucceeded) => {
                return future::select_ok(receivers.into_iter().map(|tuple| {
                    Box::pin(get_receiver(tuple).map(|res| {
                        res.map(|val| {
                            // Each future in `receivers` represents a single response from a node.
                            // Errors come through as Err(...) from get_receiver, not as values.
                            Ok(val)
                        })
                    }))
                }))
                .await
                .map(|(result, _)| result)?;
            }
            // ────────────────────────────────────────────────────────────────
            // ResponsePolicy::FirstSucceededNonEmptyOrAllEmpty:
            // Waits for each response:
            //   - Returns the first non-Nil success immediately.
            //   - If all are Ok(Nil), returns Value::Nil.
            //   - If any error occurs (and no success is returned), returns the last error.
            // ────────────────────────────────────────────────────────────────
            Some(ResponsePolicy::FirstSucceededNonEmptyOrAllEmpty) => {
                // We want to see each response as it arrives, and:
                //  • If we see `Ok(Value::Nil)`, increment a counter.
                //  • If we see `Ok(other_value)`, return it immediately.
                //  • If we see `Err(e)`, remember it as `last_err`.
                //
                // Once the stream is exhausted:
                //  – if all successes were Nil → return Value::Nil (indicates that all shards are empty).
                //  – else → return the last error we saw (or a generic "all‐unavailable" error).
                //
                // If we received a mix of errors and `Nil`s, we can't determine if all shards are empty, thus we return the last received error instead of `Nil`.
                let num_of_results: usize = receivers.len();
                let mut futures = receivers
                    .into_iter()
                    .map(get_receiver)
                    .collect::<FuturesUnordered<_>>();
                let mut nil_counter = 0;
                let mut last_err = None;
                while let Some(result) = futures.next().await {
                    match result {
                        Ok(Value::Nil) => nil_counter += 1,
                        Ok(val) => return Ok(val),
                        // If we received a Error, it means either the command failed or the receiver returned a RecvError
                        Err(e) => last_err = Some(e),
                    }
                }

                if nil_counter == num_of_results {
                    // All received results are `Nil`
                    Ok(Value::Nil)
                } else {
                    Err(last_err.unwrap_or_else(|| {
                        (
                            ErrorKind::AllConnectionsUnavailable,
                            "Couldn't find any connection",
                        )
                            .into()
                    }))
                }
            }

            // ────────────────────────────────────────────────────────────────
            // All other policies (e.g., AllSucceeded, Aggregate, CombineArrays, etc):
            // Waits for all responses, collects them, and delegates to
            // `aggregate_resolved_results` for interpretation.
            // ────────────────────────────────────────────────────────────────
            Some(ResponsePolicy::AllSucceeded)
            | Some(ResponsePolicy::Aggregate(_))
            | Some(ResponsePolicy::AggregateArray(_))
            | Some(ResponsePolicy::AggregateLogical(_))
            | Some(ResponsePolicy::CombineArrays)
            | Some(ResponsePolicy::CombineMaps)
            | Some(ResponsePolicy::Special)
            | None => {
                let collected = future::try_join_all(receivers.into_iter().map(
                    |(addr, receiver)| async move {
                        let res = convert_result(receiver.await)?;
                        Ok::<(Option<Arc<str>>, Value), Error>((addr, res))
                    },
                ))
                .await?;
                Self::aggregate_resolved_results(collected, routing, response_policy)
            }
        }
    }

    /// Synchronously folds a fully‐collected `Vec<(Option<String>, Value)>` according to `response_policy`.
    ///
    /// This helper is called after all node replies have been received and converted to `(addr, Value)`.
    /// Each policy's logic is applied to that vector of resolved results, returning a single `Value` or error.
    ///
    /// This function is used to handle the results of multi‐node commands, where the replies from multiple nodes are not collected using receivers,
    /// but rather already collected into a vector of `(Option<String>, Value)` pairs (e.g. within a pipeline).
    ///
    /// # Arguments
    ///
    /// * `resolved` – A vector of `(Option<String>, Value)` pairs, where:
    ///     - `Option<String>` is the node address (used for map/policy keys).
    ///     - `Value` is the node's reply.
    /// * `routing` – The routing information of the command (e.g., multi‐slot, `AllNodes`, `AllPrimaries`).
    /// * `response_policy` – An `Option<ResponsePolicy>` that dictates how to aggregate the results from the different nodes.
    ///
    /// # Returns
    ///
    /// A `Result<Value>` representing the aggregated result.
    fn aggregate_resolved_results(
        resolved: Vec<(Option<Arc<str>>, Value)>,
        routing: &MultipleNodeRoutingInfo,
        response_policy: Option<ResponsePolicy>,
    ) -> Result<Value> {
        // Note: errors are now propagated as Err(...) before reaching this function,
        // so there's no need to check for server errors in the resolved values.

        let total = resolved.len();

        match response_policy {
            // ——————————————————————————————————————————
            // AllSucceeded: fail if any Err, otherwise return the last Ok value.
            // ——————————————————————————————————————————
            Some(ResponsePolicy::AllSucceeded) => resolved
                .into_iter()
                .next_back()
                .map(|(_, val)| val)
                .ok_or_else(|| {
                    Error::from((
                        ErrorKind::ResponseError,
                        "No responses to aggregate for AllSucceeded",
                    ))
                }),

            // ——————————————————————————————————————————
            // Aggregate(op): fail on any Err, otherwise call cluster_routing::aggregate
            // ——————————————————————————————————————————
            Some(ResponsePolicy::Aggregate(op)) => {
                let all_vals: Vec<Value> = resolved.into_iter().map(|(_addr, val)| val).collect();
                crate::cluster::routing::aggregate(all_vals, op)
            }

            // ——————————————————————————————————————————
            // AggregateArray(op): fail on any Err, otherwise call cluster_routing::aggregate_array
            // ——————————————————————————————————————————
            Some(ResponsePolicy::AggregateArray(op)) => {
                let all_vals: Vec<Value> = resolved.into_iter().map(|(_addr, val)| val).collect();
                crate::cluster::routing::aggregate_array(all_vals, op)
            }

            // ——————————————————————————————————————————
            // AggregateLogical(op): fail on any Err, otherwise call cluster_routing::logical_aggregate
            // ——————————————————————————————————————————
            Some(ResponsePolicy::AggregateLogical(op)) => {
                let all_vals: Vec<Value> = resolved.into_iter().map(|(_addr, val)| val).collect();
                crate::cluster::routing::logical_aggregate(all_vals, op)
            }

            // ——————————————————————————————————————————
            // CombineArrays: collect all values, then call combine_array_results
            // (or combine_and_sort_array_results if `routing` is MultiSlot)
            // ——————————————————————————————————————————
            Some(ResponsePolicy::CombineArrays) => {
                let all_vals: Vec<Value> = resolved.into_iter().map(|(_addr, val)| val).collect();
                match routing {
                    MultipleNodeRoutingInfo::MultiSlot((keys_vec, args_pattern)) => {
                        crate::cluster::routing::combine_and_sort_array_results(
                            all_vals,
                            keys_vec,
                            args_pattern,
                        )
                    }
                    _ => crate::cluster::routing::combine_array_results(all_vals),
                }
            }

            // ——————————————————————————————————————————
            // CombineMaps: fail on any Err, otherwise call cluster_routing:combine_map_results
            // ——————————————————————————————————————————
            Some(ResponsePolicy::CombineMaps) => {
                let all_vals: Vec<Value> = resolved.into_iter().map(|(_addr, val)| val).collect();
                crate::cluster::routing::combine_map_results(all_vals)
            }

            // ——————————————————————————————————————————
            // Special or None:
            // ——————————————————————————————————————————
            Some(ResponsePolicy::Special) | None => {
                let mut pairs: Vec<(Value, Value)> = Vec::with_capacity(total);
                for (addr_opt, value) in resolved {
                    let key_bytes = match addr_opt {
                        Some(addr) => addr.as_bytes().to_vec(),
                        None => {
                            return Err(Error::from((
                                ErrorKind::ResponseError,
                                "No address provided for response in Special or None response policy",
                            )));
                        }
                    };
                    pairs.push((Value::BulkString(bytes::Bytes::from(key_bytes)), value));
                }
                Ok(Value::Map(pairs))
            }

            // ——————————————————————————————————————————
            // If we reach here, it means that the replies from multiple nodes are not collected using `oneshot::Receiver`,
            // but rather already collected into a vector of `(Option<String>, Value)` pairs (e.g. within a pipeline).
            // ——————————————————————————————————————————

            // ────────────────────────────────────────────────────────────────
            // ResponsePolicy::OneSucceeded:
            // Waits for the *first* successful response and returns it.
            // Any other in-flight responses are dropped.
            // ────────────────────────────────────────────────────────────────
            Some(ResponsePolicy::OneSucceeded) => {
                // Return the first response, or error if none available.
                // Errors are now propagated as Err(...) before reaching this function.
                resolved
                    .into_iter()
                    .next()
                    .map(|(_, val)| val)
                    .ok_or_else(|| {
                        (
                            ErrorKind::AllConnectionsUnavailable,
                            "Couldn't find any connection",
                        )
                            .into()
                    })
            }
            // ────────────────────────────────────────────────────────────────
            // ResponsePolicy::FirstSucceededNonEmptyOrAllEmpty:
            // Returns the first non-Nil value immediately.
            // If all are Nil, returns Value::Nil.
            // ────────────────────────────────────────────────────────────────
            Some(ResponsePolicy::FirstSucceededNonEmptyOrAllEmpty) => {
                let mut nil_counter = 0;
                let num_results = resolved.len();

                for (_addr, val) in resolved {
                    match val {
                        Value::Nil => nil_counter += 1,
                        val => return Ok(val),
                    }
                }

                if nil_counter == num_results {
                    Ok(Value::Nil)
                } else {
                    Err((
                        ErrorKind::AllConnectionsUnavailable,
                        "Couldn't find any connection",
                    )
                        .into())
                }
            }
        }
    }

    // Query a node to discover slot-> master mappings with retries
    async fn refresh_slots_and_subscriptions_with_retries(
        inner: Arc<InnerCore<C>>,
        policy: &RefreshPolicy,
        trigger: SlotRefreshTrigger,
    ) -> Result<()> {
        let _guard = inner.topology_refresh_lock.lock().await;
        Self::refresh_slots_and_subscriptions_with_retries_inner(inner.clone(), policy, trigger)
            .await
    }

    async fn refresh_slots_and_subscriptions_with_retries_inner(
        inner: Arc<InnerCore<C>>,
        policy: &RefreshPolicy,
        trigger: SlotRefreshTrigger,
    ) -> Result<()> {
        let SlotRefreshState {
            in_progress,
            last_run,
            rate_limiter,
        } = &inner.slot_refresh_state;
        // Ensure only a single slot refresh operation occurs at a time
        if in_progress
            .compare_exchange(false, true, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            return Ok(());
        }
        let mut should_refresh_slots = true;
        if *policy == RefreshPolicy::Throttable {
            // Check if the current slot refresh is triggered before the wait duration has passed
            let last_run_rlock = last_run.read().await;
            if let Some(last_run_time) = *last_run_rlock {
                let passed_time = SystemTime::now()
                    .duration_since(last_run_time)
                    .unwrap_or_else(|err| {
                        warn!(
                            "Failed to get the duration since the last slot refresh, received error: {:?}",
                            err
                        );
                        // Setting the passed time to 0 will force the current refresh to continue and reset the stored last_run timestamp with the current one
                        Duration::from_secs(0)
                    });
                let wait_duration = rate_limiter.wait_duration();
                if passed_time <= wait_duration {
                    debug!("Skipping slot refresh as the wait duration hasn't yet passed. Passed time = {:?},
                            Wait duration = {:?}", passed_time, wait_duration);
                    should_refresh_slots = false;
                }
            }
        }

        let mut res = Ok(());
        if should_refresh_slots {
            let retry_strategy = ExponentialFactorBackoff::from_millis(
                DEFAULT_REFRESH_SLOTS_RETRY_BASE_DURATION_MILLIS,
                DEFAULT_REFRESH_SLOTS_RETRY_BASE_FACTOR,
            )
            .map(jitter_range(0.8, 1.2))
            .take(DEFAULT_NUMBER_OF_REFRESH_SLOTS_RETRIES);
            let retries_counter = AtomicUsize::new(0);
            res = Retry::spawn(retry_strategy, || async {
                let curr_retry = retries_counter.fetch_add(1, atomic::Ordering::Relaxed);
                Self::refresh_slots(inner.clone(), curr_retry, trigger)
                    .await
                    .map_err(|err| {
                        if matches!(
                            err.kind(),
                            ErrorKind::AllConnectionsUnavailable
                                | ErrorKind::PermissionDenied
                                | ErrorKind::AuthenticationFailed
                        ) {
                            RetryError::permanent(err)
                        } else {
                            RetryError::transient(err)
                        }
                    })
            })
            .await;
        }
        in_progress.store(false, Ordering::Relaxed);
        res
    }

    /// Determines if the cluster topology has changed and refreshes slots and subscriptions if needed.
    /// Returns `Result` with `true` if changes were detected and slots were refreshed,
    /// or `false` if no changes were found. Raises an error if refreshing the topology fails.
    pub(crate) async fn check_topology_and_refresh_if_diff(
        inner: Arc<InnerCore<C>>,
        policy: &RefreshPolicy,
    ) -> Result<bool> {
        let _guard = inner.topology_refresh_lock.lock().await;

        let topology_changed = Self::check_for_topology_diff(inner.clone()).await;
        if topology_changed {
            Self::refresh_slots_and_subscriptions_with_retries_inner(
                inner.clone(),
                policy,
                SlotRefreshTrigger::RuntimeRefresh,
            )
            .await?;
        }
        Ok(topology_changed)
    }

    async fn periodic_topology_check(inner: Arc<InnerCore<C>>, interval_duration: Duration) {
        loop {
            let _ = boxed_sleep(interval_duration).await;
            // Check and refresh topology if needed
            let _ = match Self::check_topology_and_refresh_if_diff(
                inner.clone(),
                &RefreshPolicy::Throttable,
            )
            .await
            {
                Ok(topology_changed) => !topology_changed,
                Err(err) => {
                    warn!(
                        "Failed to refresh slots during periodic topology checks:\n{:?}",
                        err
                    );
                    true
                }
            };
        }
    }

    async fn connections_validation_task(inner: Arc<InnerCore<C>>, interval_duration: Duration) {
        loop {
            if let Some(disconnect_notifier) = inner
                .ferriskey_connection_options
                .disconnect_notifier
                .clone()
            {
                disconnect_notifier
                    .wait_for_disconnect_with_timeout(&interval_duration)
                    .await;
            } else {
                let _ = boxed_sleep(interval_duration).await;
            }

            Self::validate_all_user_connections(inner.clone()).await;
        }
    }

    /// Queries log2n nodes (where n represents the number of cluster nodes) to determine whether their
    /// topology view differs from the one currently stored in the connection manager.
    /// Returns true if change was detected, otherwise false.
    async fn check_for_topology_diff(inner: Arc<InnerCore<C>>) -> bool {
        let num_of_nodes = inner.conn_lock.read().len();
        let num_of_nodes_to_query =
            std::cmp::max(num_of_nodes.checked_ilog2().unwrap_or(0) as usize, 1);
        let TopologyQueryResult {
            topology_result,
            failed_connections,
        } = calculate_topology_from_random_nodes(
            &inner,
            num_of_nodes_to_query,
            DEFAULT_NUMBER_OF_REFRESH_SLOTS_RETRIES,
            SlotRefreshTrigger::RuntimeRefresh,
        )
        .await;

        if let Ok((_, found_topology_hash)) = topology_result
            && inner.conn_lock.read().get_current_topology_hash() != found_topology_hash
        {
            return true;
        }

        if let Some(failed) = failed_connections
            && !failed.is_empty()
        {
            trace!("check_for_topology_diff: calling trigger_refresh_connection_tasks");
            Self::trigger_refresh_connection_tasks(
                inner,
                failed
                    .into_iter()
                    .map(|s| Arc::<str>::from(s.as_str()))
                    .collect(),
                RefreshConnectionType::OnlyManagementConnection,
                true,
            )
            .await;
        }

        false
    }

    async fn refresh_slots(
        inner: Arc<InnerCore<C>>,
        curr_retry: usize,
        trigger: SlotRefreshTrigger,
    ) -> Result<()> {
        // Update the slot refresh last run timestamp
        let now = SystemTime::now();
        let mut last_run_wlock = inner.slot_refresh_state.last_run.write().await;
        *last_run_wlock = Some(now);
        drop(last_run_wlock);
        Self::refresh_slots_inner(inner, curr_retry, trigger).await
    }

    // Query a node to discover slot-> master mappings
    async fn refresh_slots_inner(
        inner: Arc<InnerCore<C>>,
        curr_retry: usize,
        trigger: SlotRefreshTrigger,
    ) -> Result<()> {
        let num_of_nodes = inner.conn_lock.read().len();
        const MAX_REQUESTED_NODES: usize = 10;
        let num_of_nodes_to_query = num_of_nodes.min(MAX_REQUESTED_NODES);

        let (new_slots, topology_hash) = calculate_topology_from_random_nodes(
            &inner,
            num_of_nodes_to_query,
            curr_retry,
            trigger,
        )
        .await
        .topology_result?;

        // Create a new connection vector of the found nodes
        let nodes = new_slots.all_node_addresses();
        let nodes_len = nodes.len();

        // Ensure cluster_params has a fresh IAM token before creating connections
        Self::refresh_iam_token_in_cluster_params(&inner).await;
        let cluster_params = inner.get_cluster_param(|params| params.clone());
        let ferriskey_connection_options = &inner.ferriskey_connection_options;

        // Find existing connections (by address or DNS resolution) or create new ones
        let connection_futures = nodes.into_iter().map(|addr| {
            let addr = addr.to_string();
            let inner = Arc::clone(&inner);
            let cluster_params = cluster_params.clone();
            let ferriskey_connection_options = ferriskey_connection_options.clone();
            let connection_timeout = cluster_params.connection_timeout;

            async move {
                // TODO: Expose separate `dns_timeout` configuration in advanced settings
                // to allow users to control DNS resolution timeout independently from connection timeout.
                // Issue: https://github.com/valkey-io/valkey-glide/issues/5298
                let result = tokio::time::timeout(connection_timeout, async {
                    // Check for existing connection by direct address
                    let node = inner.conn_lock.read().node_for_address(&addr);

                    let node = match node {
                        Some(n) => Some(n),
                        None => {
                            // If it's a DNS endpoint, it could have been stored in the existing connections vector
                            // using the resolved IP address instead of the DNS endpoint's name.
                            // We shall check if a connection already exists under the resolved IP name.
                            let conn = if let Some((host, port)) =
                                get_host_and_port_from_addr(&addr)
                            {
                                if let Ok(mut socket_addresses) = get_socket_addrs(host, port).await
                                {
                                    let conn_lock = inner.conn_lock.read();
                                    socket_addresses.find_map(|socket_addr| {
                                        conn_lock.node_for_address(&socket_addr.to_string())
                                    })
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            // If we found a connection by IP lookup, update the PushManager. This ensures
                            // the PushManager stores the DNS address (which matches the connection_map key)
                            // instead of the old IP or config endpoint address, which is needed for pubsub tracking.
                            if let Some(ref node) = conn {
                                node.user_connection
                                    .conn
                                    .clone()
                                    .await
                                    .update_push_manager_node_address(addr.clone());
                            }
                            conn
                        }
                    };

                    get_or_create_conn(
                        &addr,
                        node,
                        &cluster_params,
                        RefreshConnectionType::AllConnections,
                        ferriskey_connection_options,
                    )
                    .await
                })
                .await
                .unwrap_or_else(|e| Err(e.into()));

                (addr, result)
            }
        });

        // Await all connection futures, this is bounded by `connection_timeout`.
        let results = futures::future::join_all(connection_futures).await;

        // Collect successful connections. For addresses that failed (e.g. timeout),
        // preserve the existing healthy connection so a partial refresh doesn't
        // discard connections that were working before the refresh.
        let new_connections = ConnectionsMap(DashMap::with_capacity(nodes_len));
        let mut failed_addrs: Vec<String> = Vec::new();
        for (addr, result) in results {
            if let Ok(node) = result {
                new_connections.0.insert(Arc::from(&*addr), node);
            } else {
                failed_addrs.push(addr);
            }
        }

        // Merge: for any address that failed reconnection, keep the old connection
        // if one exists. This prevents a partial timeout from discarding all
        // previously healthy connections.
        if !failed_addrs.is_empty() {
            let read_guard = inner.conn_lock.read();
            for addr in &failed_addrs {
                if let Some(existing_node) = read_guard.node_for_address(addr) {
                    new_connections.0.insert(Arc::from(&**addr), existing_node);
                }
            }
            drop(read_guard);
            info!(
                "refresh_slots: preserved existing connections for {} addresses that failed reconnection",
                failed_addrs.len()
            );
        }

        info!("refresh_slots found nodes:\n{new_connections}");
        // Reset the current slot map and connection vector with the new ones
        {
            let mut write_guard = inner.conn_lock.write();
            // Clear the refresh tasks of the prev instance
            write_guard.refresh_conn_state.clear_refresh_state();
            let read_from_replicas =
                inner.get_cluster_param(|params| params.read_from_replicas.clone());
            *write_guard = ConnectionsContainer::new(
                new_slots,
                new_connections,
                read_from_replicas,
                topology_hash,
            );
        }
        // Write lock released — readers can proceed while we notify the synchronizer.

        // Notify the PubSub synchronizer about the new topology.
        // Acquire a read lock (shared) for the slot_map reference.
        if let Some(sync) = &inner.ferriskey_connection_options.pubsub_synchronizer {
            let read_guard = inner.conn_lock.read();
            sync.handle_topology_refresh(&read_guard.slot_map);
        }

        Ok(())
    }

    /// Handles MOVED errors by updating the client's slot and node mappings based on the new primary's role:
    ///
    /// 1. **No Change**: If the new primary is already the current slot owner, no updates are needed.
    /// 2. **Failover**: If the new primary is a replica within the same shard (indicating a failover),
    ///    the slot ownership is updated by promoting the replica to the primary in the existing shard addresses.
    /// 3. **Slot Migration**: If the new primary is an existing primary in another shard, this indicates a slot migration,
    ///    and the slot mapping is updated to point to the new shard addresses.
    /// 4. **Replica Moved to a Different Shard**: If the new primary is a replica in a different shard, it can be due to:
    ///    - The replica became the primary of its shard after a failover, with new slots migrated to it.
    ///    - The replica has moved to a different shard as the primary.
    ///      Since further information is unknown, the replica is removed from its original shard and added as the primary of a new shard.
    /// 5. **New Node**: If the new primary is unknown, it is added as a new node in a new shard, possibly indicating scale-out.
    ///
    /// # Arguments
    /// * `inner` - Shared reference to InnerCore containing connection and slot state.
    /// * `slot` - The slot number reported as moved.
    /// * `new_primary` - The address of the node now responsible for the slot.
    ///
    /// # Returns
    /// * `Result<()>` indicating success or failure in updating slot mappings.
    async fn update_upon_moved_error(
        inner: Arc<InnerCore<C>>,
        slot: u16,
        new_primary: Arc<String>,
    ) -> Result<()> {
        let curr_shard_addrs = inner.conn_lock.read().slot_map.shard_addrs_for_slot(slot);
        // let curr_shard_addrs = connections_container.slot_map.shard_addrs_for_slot(slot);
        // Check if the new primary is part of the current shard and update if required
        if let Some(curr_shard_addrs) = curr_shard_addrs {
            match curr_shard_addrs.attempt_shard_role_update(new_primary.clone()) {
                // Scenario 1: No changes needed as the new primary is already the current slot owner.
                // Scenario 2: Failover occurred and the new primary was promoted from a replica.
                ShardUpdateResult::AlreadyPrimary | ShardUpdateResult::Promoted => return Ok(()),
                // The node was not found in this shard, proceed with further scenarios.
                ShardUpdateResult::NodeNotFound => {}
            }
        }

        // Scenario 3 & 4: Check if the new primary exists in other shards

        let mut wlock_conn_container = inner.conn_lock.write();
        let mut nodes_iter = wlock_conn_container.slot_map_nodes();
        for (node_addr, (ip_addr, shard_addrs_arc)) in &mut nodes_iter {
            if node_addr == new_primary {
                let is_existing_primary = shard_addrs_arc.primary().eq(&new_primary);
                if is_existing_primary {
                    // Scenario 3: Slot Migration - The new primary is an existing primary in another shard
                    // Update the associated addresses for `slot` to `shard_addrs`.
                    drop(nodes_iter);
                    return wlock_conn_container
                        .slot_map
                        .update_slot_range(slot, shard_addrs_arc.clone());
                } else {
                    // Scenario 4: The MOVED error redirects to `new_primary` which is known as a replica in a shard that doesn't own `slot`.
                    // Remove the replica from its existing shard and treat it as a new node in a new shard.
                    shard_addrs_arc.remove_replica(new_primary.clone())?;
                    drop(nodes_iter);
                    return wlock_conn_container.slot_map.add_new_primary(
                        slot,
                        new_primary,
                        ip_addr,
                    );
                }
            }
        }

        drop(nodes_iter);
        // Scenario 5: New Node - The new primary is not present in the current slots map, add it as a primary of a new shard.
        wlock_conn_container
            .slot_map
            .add_new_primary(slot, new_primary, None)
    }

    async fn execute_on_multiple_nodes<'a>(
        cmd: &'a Arc<Cmd>,
        routing: &'a MultipleNodeRoutingInfo,
        core: Core<C>,
        response_policy: Option<ResponsePolicy>,
    ) -> OperationResult {
        trace!("execute_on_multiple_nodes");

        #[allow(clippy::type_complexity)]
        fn into_channels<C>(
            iterator: impl Iterator<
                Item = Option<(Arc<Cmd>, ConnectionAndAddress<ConnectionFuture<C>>)>,
            >,
        ) -> (
            Vec<(Option<Arc<str>>, Receiver<Result<Response>>)>,
            Vec<Option<PendingRequest<C>>>,
        ) {
            iterator
                .map(|tuple_opt| {
                    let (sender, receiver) = oneshot::channel();
                    if let Some((cmd, conn, address)) =
                        tuple_opt.map(|(cmd, (address, conn))| (cmd, conn, address))
                    {
                        (
                            (Some(address.clone()), receiver),
                            Some(PendingRequest {
                                retry: 0,
                                sender,
                                info: RequestInfo {
                                    cmd: CmdArg::MultiCmd {
                                        cmd,
                                        routing: InternalSingleNodeRouting::Connection {
                                            address,
                                            conn,
                                        }
                                        .into(),
                                    },
                                },
                            }),
                        )
                    } else {
                        let _ = sender.send(Err((
                            ErrorKind::ConnectionNotFoundForRoute,
                            "Connection not found",
                        )
                            .into()));
                        ((None, receiver), None)
                    }
                })
                .unzip()
        }
        let (receivers, requests): (Vec<_>, Vec<_>);
        {
            let connections_container = core.conn_lock.read();
            if connections_container.is_empty() {
                return OperationResult::Err((
                    OperationTarget::FanOut,
                    (
                        ErrorKind::AllConnectionsUnavailable,
                        "No connections found for multi-node operation",
                    )
                        .into(),
                ));
            }

            (receivers, requests) = match routing {
                MultipleNodeRoutingInfo::AllNodes => into_channels(
                    connections_container
                        .all_node_connections()
                        .map(|tuple| Some((cmd.clone(), tuple))),
                ),
                MultipleNodeRoutingInfo::AllMasters => into_channels(
                    connections_container
                        .all_primary_connections()
                        .map(|tuple| Some((cmd.clone(), tuple))),
                ),
                MultipleNodeRoutingInfo::MultiSlot((slots, _)) => {
                    into_channels(slots.iter().map(|(route, indices)| {
                        connections_container
                            .connection_for_route(route)
                            .map(|tuple| {
                                let new_cmd =
                                    crate::cluster::routing::command_for_multi_slot_indices(
                                        cmd.as_ref(),
                                        indices.iter(),
                                    );
                                (Arc::new(new_cmd), tuple)
                            })
                    }))
                }
            };
        }
        for request in requests.into_iter().flatten() {
            let _ = core.pending_requests_tx.send(request);
        }

        Self::aggregate_results(receivers, routing, response_policy)
            .await
            .map(Response::Single)
            .map_err(|err| (OperationTarget::FanOut, err))
    }

    /// Hot path for single-node commands. Uses pre-packed bytes — no Cmd clone.
    async fn try_packed_request(
        packed: bytes::Bytes,
        is_fenced: bool,
        routing: InternalRoutingInfo<C>,
        core: Core<C>,
    ) -> OperationResult {
        let routing = match routing {
            InternalRoutingInfo::SingleNode(routing) => routing,
            InternalRoutingInfo::MultiNode(_) => {
                return Err((
                    OperationTarget::FanOut,
                    Error::from((
                        ErrorKind::ClientError,
                        "MultiNode routing reached single-node dispatch path",
                    )),
                ));
            }
        };
        trace!("route request to single node");

        let (address, mut conn, needs_asking) = Self::get_connection(routing, core, None)
            .await
            .map_err(|err| (OperationTarget::NotFound, err))?;
        emit_routed_node(&address);

        if needs_asking {
            // Send ASKING + real command atomically via a 2-command pipeline
            // to prevent interleaving on the shared multiplexed connection.
            let mut pipeline = crate::pipeline::Pipeline::with_capacity(2);
            pipeline.add_command(crate::cmd::cmd("ASKING"));
            pipeline.add_command(crate::cmd::Cmd::from_packed_command(packed));
            let mut results = conn
                .req_packed_commands(&pipeline, 1, 1, None)
                .await
                .map_err(|err| {
                    (
                        OperationTarget::Node {
                            address: address.to_string(),
                        },
                        err,
                    )
                })?;
            results
                .pop()
                .unwrap_or(Err(Error::from((
                    ErrorKind::ClientError,
                    "Empty pipeline response for ASKING+command",
                ))))
                .map(Response::Single)
                .map_err(|err| {
                    (
                        OperationTarget::Node {
                            address: address.to_string(),
                        },
                        err,
                    )
                })
        } else {
            conn.send_packed_bytes(packed, is_fenced)
                .await
                .map(Response::Single)
                .map_err(|err| {
                    (
                        OperationTarget::Node {
                            address: address.to_string(),
                        },
                        err,
                    )
                })
        }
    }

    /// Multi-node and fan-out path. Keeps full Arc<Cmd> for command splitting.
    pub(crate) async fn try_cmd_request(
        cmd: Arc<Cmd>,
        routing: InternalRoutingInfo<C>,
        core: Core<C>,
    ) -> OperationResult {
        let routing = match routing {
            InternalRoutingInfo::MultiNode((multi_node_routing, response_policy)) => {
                return Self::execute_on_multiple_nodes(
                    &cmd,
                    &multi_node_routing,
                    core,
                    response_policy,
                )
                .await;
            }

            InternalRoutingInfo::SingleNode(routing) => routing,
        };
        trace!("route request to single node");

        let (address, mut conn, needs_asking) =
            Self::get_connection(routing, core, Some(cmd.clone()))
                .await
                .map_err(|err| (OperationTarget::NotFound, err))?;
        emit_routed_node(&address);

        if needs_asking {
            // Send ASKING + real command atomically via a 2-command pipeline.
            let mut pipeline = crate::pipeline::Pipeline::with_capacity(2);
            pipeline.add_command(crate::cmd::cmd("ASKING"));
            pipeline.add_command_with_arc(cmd);
            let mut results = conn
                .req_packed_commands(&pipeline, 1, 1, None)
                .await
                .map_err(|err| {
                    (
                        OperationTarget::Node {
                            address: address.to_string(),
                        },
                        err,
                    )
                })?;
            results
                .pop()
                .unwrap_or(Err(Error::from((
                    ErrorKind::ClientError,
                    "Empty pipeline response for ASKING+command",
                ))))
                .map(Response::Single)
                .map_err(|err| {
                    (
                        OperationTarget::Node {
                            address: address.to_string(),
                        },
                        err,
                    )
                })
        } else {
            conn.req_packed_command(&cmd)
                .await
                .map(Response::Single)
                .map_err(|err| {
                    (
                        OperationTarget::Node {
                            address: address.to_string(),
                        },
                        err,
                    )
                })
        }
    }

    async fn try_pipeline_request(
        pipeline: Arc<crate::pipeline::Pipeline>,
        offset: usize,
        count: usize,
        conn: impl Future<Output = Result<(Arc<str>, C, bool)>>,
    ) -> OperationResult {
        trace!("try_pipeline_request");
        let (address, mut conn, needs_asking) =
            conn.await.map_err(|err| (OperationTarget::NotFound, err))?;
        emit_routed_node(&address);

        // When needs_asking is true, prepend ASKING to the pipeline so
        // it is sent atomically in a single write.  Adjust offset/count
        // so the caller still gets only the responses it expects.
        if needs_asking {
            let mut asking_pipeline =
                crate::pipeline::Pipeline::with_capacity(1 + pipeline.len());
            asking_pipeline.add_command(crate::cmd::cmd("ASKING"));
            for cmd in pipeline.cmd_iter() {
                asking_pipeline.add_command_with_arc(Arc::clone(cmd));
            }
            conn.req_packed_commands(&asking_pipeline, offset + 1, count, None)
                .await
                .map(Response::Multiple)
                .map_err(|err| {
                    (
                        OperationTarget::Node {
                            address: address.to_string(),
                        },
                        err,
                    )
                })
        } else {
            conn.req_packed_commands(&pipeline, offset, count, None)
                .await
                .map(Response::Multiple)
                .map_err(|err| {
                    (
                        OperationTarget::Node {
                            address: address.to_string(),
                        },
                        err,
                    )
                })
        }
    }

    async fn try_request(info: RequestInfo<C>, core: Core<C>) -> OperationResult {
        match info.cmd {
            CmdArg::Cmd {
                packed,
                is_fenced,
                routing,
            } => Self::try_packed_request(packed, is_fenced, routing, core).await,
            CmdArg::MultiCmd { cmd, routing } => Self::try_cmd_request(cmd, routing, core).await,
            CmdArg::Pipeline {
                pipeline,
                offset,
                count,
                route,
                sub_pipeline,
                pipeline_retry_strategy,
            } => {
                if pipeline.is_atomic() || sub_pipeline {
                    Self::try_pipeline_request(
                        pipeline,
                        offset,
                        count,
                        Self::get_connection(
                            route.unwrap_or(InternalSingleNodeRouting::Random),
                            core,
                            None,
                        ),
                    )
                    .await
                } else {
                    // The pipeline is not atomic and not already splitted, we need to split it into sub-pipelines and send them separately.
                    Self::handle_non_atomic_pipeline_request(
                        pipeline,
                        core,
                        0,
                        pipeline_retry_strategy,
                        route,
                    )
                    .await
                }
            }
            CmdArg::ClusterScan {
                cluster_scan_args, ..
            } => {
                let core = core;
                let scan_result = cluster_scan(core, cluster_scan_args).await;
                match scan_result {
                    Ok((scan_state_ref, values)) => {
                        Ok(Response::ClusterScanResult(scan_state_ref, values))
                    }
                    // TODO: After routing issues with sending to random node on not-key based commands are resolved,
                    // this error should be handled in the same way as other errors and not fan-out.
                    Err(err) => Err((OperationTarget::FanOut, err)),
                }
            }
            CmdArg::OperationRequest(operation_request) => match operation_request {
                Operation::UpdateConnectionPassword(password) => {
                    core.set_cluster_param(|params| params.password = password);
                    Ok(Response::Single(Value::Okay))
                }
                Operation::UpdateConnectionDatabase(database_id) => {
                    core.set_cluster_param(|params| params.database_id = database_id);
                    Ok(Response::Single(Value::Okay))
                }
                Operation::UpdateConnectionClientName(client_name) => {
                    core.set_cluster_param(|params| params.client_name = client_name);
                    Ok(Response::Single(Value::Okay))
                }
                Operation::UpdateConnectionUsername(username) => {
                    core.set_cluster_param(|params| params.username = username);
                    Ok(Response::Single(Value::Okay))
                }
                Operation::UpdateConnectionProtocol(protocol) => {
                    core.set_cluster_param(|params| params.protocol = protocol);
                    Ok(Response::Single(Value::Okay))
                }
                Operation::GetUsername => {
                    let username = match core.get_cluster_param(|params| params.username.clone()) {
                        Some(username) => Value::SimpleString(username),
                        None => Value::Nil,
                    };
                    Ok(Response::Single(username))
                }
            },
        }
    }

    /// Handles the execution of a non-atomic pipeline request by splitting it into sub-pipelines and sending them to the appropriate cluster nodes.
    ///
    /// This function distributes the commands in the pipeline across the cluster nodes based on routing information, collects the responses,
    /// and aggregates them if necessary according to the specified response policies.
    async fn handle_non_atomic_pipeline_request(
        pipeline: Arc<crate::pipeline::Pipeline>,
        core: Core<C>,
        retry: u32,
        pipeline_retry_strategy: PipelineRetryStrategy,
        route: Option<InternalSingleNodeRouting<C>>,
    ) -> OperationResult {
        // Distribute pipeline commands across cluster nodes based on routing information.
        // Returns:
        // - pipelines_by_node: Map of node addresses to their pipeline contexts
        // - response_policies: Map of routing info and response aggregation policies for multi-node commands (by command's index).
        let (pipelines_by_node, mut response_policies) =
            map_pipeline_to_nodes(&pipeline, core.clone(), route).await?;

        // Send the requests to each node and collect the responses
        // Returns a tuple containing:
        // - A vector of results for each sub-pipeline execution.
        // - A vector of (address, indices, ignore) tuples indicating where each response should be placed, or if the response should be ignored (e.g. ASKING command).
        let (responses, addresses_and_indices) = collect_and_send_pending_requests(
            pipelines_by_node,
            core.clone(),
            retry,
            pipeline_retry_strategy,
        )
        .await;

        // Process the responses and update the pipeline_responses, retrying the commands if needed.
        let pipeline_responses = process_and_retry_pipeline_responses(
            responses,
            addresses_and_indices,
            &pipeline,
            core,
            &mut response_policies,
            pipeline_retry_strategy,
        )
        .await?;

        // Process response policies after all tasks are complete and aggregate the relevant commands.
        Ok(Response::Multiple(
            Self::aggregate_pipeline_multi_node_commands(pipeline_responses, response_policies)
                .await,
        ))
    }

    /// Aggregates pipeline responses for multi-node commands and produces a final vector of responses.
    ///
    /// Pipeline commands with multi-node routing info, will be splitted into multiple pipelines, therefore, after executing each pipeline and storing the results in `pipeline_responses`,
    /// the multi-node commands will contain more than one response (one for each sub-pipeline that contained the command). This responses must be aggregated into a single response, based on the proper response policy.
    ///
    /// This function processes the provided `response_policies`, which  is a sorted (by command index) vector containing, for each multi-node command:
    /// - The index of the command in the pipeline.
    /// - The routing information (`MultipleNodeRoutingInfo`) for that command.
    /// - An optional `ResponsePolicy` specifying how to aggregate the responses (e.g., sum, all succeeded).
    ///
    /// For each command:
    /// - If a response policy exists for that command, the function aggregates the multiple responses
    ///   (collected from the sub-pipelines) into a single response using the provided routing info and response policy.
    /// - If no response policy is provided, the function simply takes the last response (which is a single response, removing its node address).
    ///
    /// The aggregated result replaces the multiple responses for each command, ensuring that every entry in
    /// the final output vector corresponds to a single, aggregated response for the original pipeline command.
    ///
    /// # Arguments
    ///
    /// * `pipeline_responses` - A vector of vectors, where each inner vector holds tuples of
    ///   `(Value, String)`, representing the responses from the sub-pipelines along with their node addresses.
    /// * `response_policies` - A `ResponsePoliciesMap` containing:
    ///     - An entry of the index of the command in the pipeline with multi-node routing information.
    ///     - The routing information (`MultipleNodeRoutingInfo`) for the command.
    ///     - An optional `ResponsePolicy` that dictates how the responses should be aggregated.
    ///
    /// # Returns
    ///
    /// * `Vec<Value>` - A vector of aggregated responses, one for each command in the original pipeline.
    ///
    /// # Example
    /// ```rust,compile_fail
    /// // Example pipeline responses for multi-node commands:
    /// let mut pipeline_responses = vec![
    ///     vec![(Value::Int(1), "node1".to_string()), (Value::Int(2), "node2".to_string()), (Value::Int(3), "node3".to_string())], // represents `DBSIZE command split across nodes
    ///     vec![(Value::Int(3), "node3".to_string())],
    ///     vec![(Value::SimpleString("PONG".to_string()), "node1".to_string()), (Value::SimpleString("PONG".to_string()), "node2".to_string()), (Value::SimpleString("PONG".to_string()), "node3".to_string())], // represents `PING` command split across nodes
    /// ];
    ///
    /// let response_policies = HashMap::from([
    ///     (0, (MultipleNodeRoutingInfo::AllNodes, Some(ResponsePolicy::Aggregate(AggregateOp::Sum)))),
    ///     (2, (MultipleNodeRoutingInfo::AllNodes, Some(ResponsePolicy::AllSucceeded))),
    /// ]);
    ///
    /// // Aggregation of responses
    /// let final_responses = aggregate_pipeline_multi_node_commands(pipeline_responses, response_policies).await.unwrap();
    ///
    /// // After aggregation, each command has a single aggregated response:
    /// assert_eq!(final_responses[0], Value::Int(6)); // Sum of 1+2+3
    /// assert_eq!(final_responses[1], Value::Int(3));
    /// assert_eq!(final_responses[2], Value::SimpleString("PONG".to_string()));
    /// ```
    async fn aggregate_pipeline_multi_node_commands(
        pipeline_responses: PipelineResponses,
        response_policies: ResponsePoliciesMap,
    ) -> Vec<Result<Value>> {
        let mut final_responses: Vec<Result<Value>> = Vec::with_capacity(pipeline_responses.len());

        for (index, mut responses) in pipeline_responses.into_iter().enumerate() {
            if let Some(&(ref routing_info, response_policy)) = response_policies.get(&index) {
                let mut first_error: Option<Error> = None;
                let resolved: Vec<(Option<Arc<str>>, Value)> = responses
                    .into_iter()
                    .filter_map(|(addr, val)| match val {
                        Ok(v) => Some((addr, v)),
                        Err(err) => {
                            if first_error.is_none() {
                                first_error = Some(err);
                            }
                            None
                        }
                    })
                    .collect();
                let aggregated_response = if let Some(err) = first_error {
                    Err(err)
                } else {
                    Self::aggregate_resolved_results(
                        resolved,
                        routing_info,
                        response_policy,
                    )
                };
                final_responses.push(aggregated_response);
            } else if responses.len() == 1 {
                final_responses.push(responses.pop().unwrap().1);
            } else {
                final_responses.push(Err(crate::value::make_extension_error(
                    "PipelineResponseError".to_string(),
                    Some(format!(
                        "Expected exactly one response for command {}, got {}",
                        index,
                        responses.len(),
                    )),
                )));
            }
        }

        final_responses
    }

    async fn get_connection(
        routing: InternalSingleNodeRouting<C>,
        core: Core<C>,
        cmd: Option<Arc<Cmd>>,
    ) -> Result<(Arc<str>, C, bool)> {
        let mut asking = false;

        let conn_check = match routing {
            InternalSingleNodeRouting::Redirect {
                redirect: Redirect::Moved(moved_addr),
                ..
            } => core
                .conn_lock
                .read()
                .connection_for_address(moved_addr.as_str())
                .map_or(
                    ConnectionCheck::OnlyAddress(moved_addr),
                    ConnectionCheck::Found,
                ),
            InternalSingleNodeRouting::Redirect {
                redirect: Redirect::Ask(ask_addr, should_exec_asking),
                ..
            } => {
                asking = should_exec_asking;
                core.conn_lock
                    .read()
                    .connection_for_address(ask_addr.as_str())
                    .map_or(
                        ConnectionCheck::OnlyAddress(ask_addr),
                        ConnectionCheck::Found,
                    )
            }
            InternalSingleNodeRouting::SpecificNode(route) => {
                // Step 1: Attempt to get the connection directly using the route.
                let conn_check = {
                    let conn_lock = core.conn_lock.read();
                    conn_lock
                        .connection_for_route(&route)
                        .map(ConnectionCheck::Found)
                };

                if let Some(conn_check) = conn_check {
                    conn_check
                } else {
                    // Step 2: Handle cases where no connection is found for the route.
                    // - For key-based commands, attempt redirection to a random node,
                    //   hopefully to be redirected afterwards by a MOVED error.
                    // - For non-key-based commands, avoid attempting redirection to a random node
                    //   as it wouldn't result in MOVED hints and can lead to unwanted results
                    //   (e.g., sending management command to a different node than the user asked for); instead, raise the error.
                    let mut conn_check = ConnectionCheck::RandomConnection;

                    let routable_cmd = cmd.and_then(|cmd| Routable::command(&*cmd));
                    if routable_cmd.is_some()
                        && !RoutingInfo::is_key_routing_command(&routable_cmd.unwrap())
                    {
                        return Err((
                            ErrorKind::ConnectionNotFoundForRoute,
                            "Requested connection not found for route",
                            format!("{route:?}"),
                        )
                            .into());
                    }

                    debug!(
                        "SpecificNode: No connection found for route `{route:?}`.
                        Checking for reconnect tasks before redirecting to a random node."
                    );

                    // Step 3: Obtain the reconnect notifier, ensuring the lock is released immediately after.
                    let reconnect_notifier = {
                        let conn_lock = core.conn_lock.read();
                        conn_lock.notifier_for_route(&route).clone()
                    };

                    // Step 4: If a notifier exists, wait for it to signal completion.
                    if let Some(notifier) = reconnect_notifier {
                        debug!(
                            "SpecificNode: Waiting on reconnect notifier for route `{route:?}`."
                        );

                        notifier.notified().await;

                        debug!(
                            "SpecificNode: Finished waiting on notifier for route `{route:?}`. Retrying connection lookup."
                        );

                        // Step 5: Retry the connection lookup after waiting for the reconnect task.
                        if let Some((conn, address)) =
                            core.conn_lock.read().connection_for_route(&route)
                        {
                            conn_check = ConnectionCheck::Found((conn, address));
                        } else {
                            debug!(
                                "SpecificNode: No connection found for route `{route:?}` after waiting on reconnect notifier. Proceeding to random node."
                            );
                        }
                    } else {
                        debug!(
                            "SpecificNode: No active reconnect task for route `{route:?}`. Proceeding to random node."
                        );
                    }

                    conn_check
                }
            }
            InternalSingleNodeRouting::Random => ConnectionCheck::RandomConnection,
            InternalSingleNodeRouting::Connection { address, conn } => {
                return Ok((address, conn.await, false));
            }
            InternalSingleNodeRouting::ByAddress(address) => {
                let conn_option = core.conn_lock.read().connection_for_address(&address);
                if let Some((address, conn)) = conn_option {
                    let resolved = conn.await;
                    if resolved.is_closed() {
                        return Err((
                            ErrorKind::ConnectionNotFoundForRoute,
                            "Connection is closed",
                            address.to_string(),
                        )
                            .into());
                    }
                    return Ok((address, resolved, false));
                } else {
                    return Err((
                        ErrorKind::ConnectionNotFoundForRoute,
                        "Requested connection not found",
                        address,
                    )
                        .into());
                }
            }
        };

        let (address, conn) = match conn_check {
            ConnectionCheck::Found((address, connection)) => (address, connection.await),
            ConnectionCheck::OnlyAddress(address) => {
                // Validate the address was previously seen in the cluster topology.
                // This prevents SSRF via crafted MOVED/ASK redirects to arbitrary hosts.
                {
                    let container = core.conn_lock.read();
                    let known = container.slot_map.all_node_addresses();
                    if !known.iter().any(|a| a.as_str() == address) {
                        return Err(Error::from((
                            ErrorKind::ConnectionNotFoundForRoute,
                            "Redirect to unknown address rejected",
                            address,
                        )));
                    }
                }
                // Trigger refresh task and get the single notifier
                let mut notifiers = Self::trigger_refresh_connection_tasks(
                    core.clone(),
                    HashSet::from([Arc::<str>::from(address.as_str())]),
                    RefreshConnectionType::AllConnections,
                    false,
                )
                .await;

                // Extract the single notifier (if any)
                if let Some(refresh_notifier) = notifiers.pop() {
                    debug!(
                        "get_connection: Waiting on the refresh notifier for address: {}",
                        address
                    );
                    // Wait for the refresh task to notify that it's done reconnecting (or transitioning).
                    refresh_notifier.notified().await;
                    debug!(
                        "get_connection: After waiting on the refresh notifier for address: {}",
                        address
                    );
                } else {
                    debug!(
                        "get_connection: No notifier to wait on for address: {}",
                        address
                    );
                }

                // Try fetching the connection after the notifier resolves
                let conn_option = core.conn_lock.read().connection_for_address(&address);

                if let Some((address, conn)) = conn_option {
                    debug!("get_connection: Connection found for address: {}", address);
                    (address, conn.await)
                } else {
                    return Err((
                        ErrorKind::ConnectionNotFoundForRoute,
                        "Requested connection not found",
                        address,
                    )
                        .into());
                }
            }
            ConnectionCheck::RandomConnection => {
                let random_conn = core
                    .conn_lock
                    .read()
                    .random_connections(1, ConnectionType::User);
                let (random_address, random_conn_future) =
                    match random_conn.and_then(|conn_iter| conn_iter.into_iter().next()) {
                        Some((address, future)) => (address, future),
                        None => {
                            return Err(Error::from((
                                ErrorKind::AllConnectionsUnavailable,
                                "No random connection found",
                            )));
                        }
                    };

                (random_address, random_conn_future.await)
            }
        };

        if conn.is_closed() {
            return Err((
                ErrorKind::ConnectionNotFoundForRoute,
                "Connection is closed",
                address.to_string(),
            )
                .into());
        }

        Ok((address, conn, asking))
    }

    fn poll_recover(&mut self, _cx: &mut task::Context<'_>) -> Poll<Result<()>> {
        trace!("entered poll_recover");

        let recover_future = match &mut self.state {
            ConnectionState::PollComplete => return Poll::Ready(Ok(())),
            ConnectionState::Recover(future) => future,
        };

        match recover_future {
            RecoverFuture::RefreshingSlots(handle) => {
                // Check if the task has completed
                match handle.now_or_never() {
                    Some(Ok(Ok(()))) => {
                        // Task succeeded
                        trace!("Slot refresh completed successfully!");
                        self.state = ConnectionState::PollComplete;
                        return Poll::Ready(Ok(()));
                    }
                    Some(Ok(Err(e))) => {
                        // Task completed but returned an engine error
                        trace!("Slot refresh failed: {:?}", e);

                        if e.kind() == ErrorKind::AllConnectionsUnavailable {
                            // If all connections unavailable, try reconnect
                            let inner = self.inner.clone();
                            let handle = tokio::spawn(async move {
                                ClusterConnInner::reconnect_to_initial_nodes(inner).await
                            });
                            self.state = ConnectionState::Recover(
                                RecoverFuture::ReconnectToInitialNodes(handle),
                            );
                            return Poll::Ready(Err(e));
                        } else {
                            // Retry refresh
                            let new_handle = Self::spawn_refresh_slots_task(
                                self.inner.clone(),
                                &RefreshPolicy::Throttable,
                            );
                            self.state = ConnectionState::Recover(RecoverFuture::RefreshingSlots(
                                new_handle,
                            ));
                            return Poll::Ready(Ok(()));
                        }
                    }
                    Some(Err(join_err)) => {
                        if join_err.is_cancelled() {
                            // Task was intentionally aborted - don't treat as an error
                            trace!("Slot refresh task was aborted");
                            self.state = ConnectionState::PollComplete;
                            return Poll::Ready(Ok(()));
                        } else {
                            // Task panicked - try reconnecting to initial nodes as a recovery strategy
                            warn!(
                                "Slot refresh task panicked: {:?} - attempting recovery by reconnecting to initial nodes",
                                join_err
                            );

                            // TODO - consider a gracefully closing of the client
                            // Since a panic indicates a bug in the refresh logic,
                            // it might be safer to close the client entirely
                            let inner = self.inner.clone();
                            let handle = tokio::spawn(async move {
                                ClusterConnInner::reconnect_to_initial_nodes(inner).await
                            });
                            self.state = ConnectionState::Recover(
                                RecoverFuture::ReconnectToInitialNodes(handle),
                            );

                            // Report this critical error to clients
                            let err = Error::from((
                                ErrorKind::ClientError,
                                "Slot refresh task panicked",
                                format!("{join_err:?}"),
                            ));
                            return Poll::Ready(Err(err));
                        }
                    }
                    None => {
                        // Task is still running
                        // Just continue and return Ok to not block poll_flush
                    }
                }

                // Always return Ready to not block poll_flush
                Poll::Ready(Ok(()))
            }
            RecoverFuture::ReconnectToInitialNodes(handle) => {
                // Check if the task has completed
                match handle.now_or_never() {
                    Some(Ok(())) => {
                        trace!("Reconnected to initial nodes");
                        self.state = ConnectionState::PollComplete;
                    }
                    Some(Err(join_err)) => {
                        if join_err.is_cancelled() {
                            trace!("Reconnect to initial nodes task was aborted");
                        } else {
                            warn!(
                                "Reconnect to initial nodes task panicked: {:?} - marking recovery as complete",
                                join_err
                            );
                        }
                        self.state = ConnectionState::PollComplete;
                    }
                    None => {
                        // Task is still running
                        // Just continue and return Ok to not block poll_flush
                    }
                }

                // Always return Ready to not block poll_flush
                Poll::Ready(Ok(()))
            }
            RecoverFuture::Reconnect(handle) => {
                // Check if the task has completed
                match handle.now_or_never() {
                    Some(Ok(())) => {
                        trace!("Reconnected connections");
                        self.state = ConnectionState::PollComplete;
                    }
                    Some(Err(join_err)) => {
                        if join_err.is_cancelled() {
                            trace!("Reconnect task was aborted");
                        } else {
                            warn!(
                                "Reconnect task panicked: {:?} - marking recovery as complete",
                                join_err
                            );
                        }
                        self.state = ConnectionState::PollComplete;
                    }
                    None => {
                        // Task is still running
                        // Just continue and return Ok to not block poll_flush
                    }
                }

                // Always return Ready to not block poll_flush
                Poll::Ready(Ok(()))
            }
        }
    }

    async fn handle_loading_error_and_retry(
        core: Core<C>,
        info: RequestInfo<C>,
        address: String,
        retry: u32,
        retry_params: RetryParams,
    ) -> OperationResult {
        Self::handle_loading_error(core.clone(), address, retry, retry_params).await;
        Self::try_request(info, core).await
    }

    async fn handle_loading_error(
        core: Core<C>,
        address: String,
        retry: u32,
        retry_params: RetryParams,
    ) {
        let is_primary = core.conn_lock.read().is_primary(&address);

        if !is_primary {
            // If the connection is a replica, remove the connection and retry.
            // The connection will be established again on the next call to refresh slots once the replica is no longer in loading state.
            core.conn_lock.read().remove_node(&address);
        } else {
            // If the connection is primary, just sleep and retry
            let sleep_duration = retry_params.wait_time_for_retry(retry);
            boxed_sleep(sleep_duration).await;
        }
    }

    fn poll_complete(&mut self, cx: &mut task::Context<'_>) -> Poll<PollFlushAction> {
        let retry_params = self.inner.cluster_params.read().retry_params.clone();
        let mut poll_flush_action = PollFlushAction::None;

        let mut pending_requests = Vec::new();
        let mut rx_guard = self
            .inner
            .pending_requests_rx
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        while let Ok(request) = rx_guard.try_recv() {
            pending_requests.push(request);
        }
        drop(rx_guard);

        for request in pending_requests.drain(..) {
            // Drop the request if none is waiting for a response to free up resources for
            // requests callers care about (load shedding). It will be ambiguous whether the
            // request actually goes through regardless.
            if request.sender.is_closed() {
                continue;
            }

            let future = Self::try_request(request.info.clone(), self.inner.clone()).boxed();
            self.in_flight_requests.push(Box::pin(Request {
                retry_params: retry_params.clone(),
                request: Some(request),
                future: RequestState::Future { future },
            }));
        }

        loop {
            let retry_params = retry_params.clone();
            let result = match Pin::new(&mut self.in_flight_requests).poll_next(cx) {
                Poll::Ready(Some(result)) => result,
                Poll::Ready(None) | Poll::Pending => break,
            };
            match result {
                Next::Done => {}
                Next::Retry { request } => {
                    let future = Self::try_request(request.info.clone(), self.inner.clone());
                    self.in_flight_requests.push(Box::pin(Request {
                        retry_params: retry_params.clone(),
                        request: Some(request),
                        future: RequestState::Future {
                            future: Box::pin(future),
                        },
                    }));
                }
                Next::RetryBusyLoadingError { request, address } => {
                    // TODO - do we also want to try and reconnect to replica if it is loading?
                    let future = Self::handle_loading_error_and_retry(
                        self.inner.clone(),
                        request.info.clone(),
                        address,
                        request.retry,
                        retry_params.clone(),
                    );
                    self.in_flight_requests.push(Box::pin(Request {
                        retry_params: retry_params.clone(),
                        request: Some(request),
                        future: RequestState::Future {
                            future: Box::pin(future),
                        },
                    }));
                }
                Next::RefreshSlots {
                    request,
                    sleep_duration,
                    moved_redirect,
                } => {
                    poll_flush_action =
                        poll_flush_action.change_state(PollFlushAction::RebuildSlots);
                    let future: Option<
                        RequestState<Pin<Box<dyn Future<Output = OperationResult> + Send>>>,
                    > = if let Some(moved_redirect) = moved_redirect {
                        Some(RequestState::UpdateMoved {
                            future: Box::pin(ClusterConnInner::update_upon_moved_error(
                                self.inner.clone(),
                                moved_redirect.slot,
                                moved_redirect.address.into(),
                            )),
                        })
                    } else if let Some(ref request) = request {
                        match sleep_duration {
                            Some(sleep_duration) => Some(RequestState::Sleep {
                                sleep: boxed_sleep(sleep_duration),
                            }),
                            None => Some(RequestState::Future {
                                future: Box::pin(Self::try_request(
                                    request.info.clone(),
                                    self.inner.clone(),
                                )),
                            }),
                        }
                    } else {
                        None
                    };
                    if let Some(future) = future {
                        self.in_flight_requests.push(Box::pin(Request {
                            retry_params,
                            request,
                            future,
                        }));
                    }
                }
                Next::Reconnect { request, target } => {
                    poll_flush_action = poll_flush_action.change_state(PollFlushAction::Reconnect(
                        HashSet::from_iter([Arc::<str>::from(target.as_str())]),
                    ));
                    if let Some(request) = request {
                        let _ = self.inner.pending_requests_tx.send(request);
                    }
                }
                Next::ReconnectToInitialNodes { request } => {
                    poll_flush_action = poll_flush_action
                        .change_state(PollFlushAction::ReconnectFromInitialConnections);
                    if let Some(request) = request {
                        let _ = self.inner.pending_requests_tx.send(request);
                    }
                }
            }
        }

        if matches!(poll_flush_action, PollFlushAction::None) {
            if self.in_flight_requests.is_empty() {
                Poll::Ready(poll_flush_action)
            } else {
                Poll::Pending
            }
        } else {
            Poll::Ready(poll_flush_action)
        }
    }

    fn send_refresh_error(&mut self) {
        if self.refresh_error.is_some() {
            if let Some(mut request) = Pin::new(&mut self.in_flight_requests)
                .iter_pin_mut()
                .find(|request| request.request.is_some())
            {
                (*request)
                    .as_mut()
                    .respond(Err(self.refresh_error.take().unwrap()));
            } else {
                let mut rx_guard = self
                    .inner
                    .pending_requests_rx
                    .lock()
                    .unwrap_or_else(|e| e.into_inner());
                if let Ok(request) = rx_guard.try_recv() {
                    let _ = request.sender.send(Err(self.refresh_error.take().unwrap()));
                }
            }
        }
    }
}

enum PollFlushAction {
    None,
    RebuildSlots,
    Reconnect(HashSet<Arc<str>>),
    ReconnectFromInitialConnections,
}

impl PollFlushAction {
    fn change_state(self, next_state: PollFlushAction) -> PollFlushAction {
        match (self, next_state) {
            (PollFlushAction::None, next_state) => next_state,
            (next_state, PollFlushAction::None) => next_state,
            (PollFlushAction::ReconnectFromInitialConnections, _)
            | (_, PollFlushAction::ReconnectFromInitialConnections) => {
                PollFlushAction::ReconnectFromInitialConnections
            }

            (PollFlushAction::RebuildSlots, _) | (_, PollFlushAction::RebuildSlots) => {
                PollFlushAction::RebuildSlots
            }

            (PollFlushAction::Reconnect(mut addrs), PollFlushAction::Reconnect(new_addrs)) => {
                addrs.extend(new_addrs);
                Self::Reconnect(addrs)
            }
        }
    }
}

impl<C> Sink<Message<C>> for Disposable<ClusterConnInner<C>>
where
    C: ConnectionLike + Connect + Clone + Send + Sync + Unpin + 'static,
{
    type Error = ();

    fn poll_ready(self: Pin<&mut Self>, _cx: &mut task::Context) -> Poll<std::result::Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    fn start_send(self: Pin<&mut Self>, msg: Message<C>) -> std::result::Result<(), Self::Error> {
        let Message { cmd, sender } = msg;

        let info = RequestInfo { cmd };

        let _ = self.inner.pending_requests_tx.send(PendingRequest {
            retry: 0,
            sender,
            info,
        });
        Ok(())
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        trace!("poll_flush: {:?}", self.state);
        loop {
            self.send_refresh_error();

            if let Err(err) = ready!(self.as_mut().poll_recover(cx)) {
                // We failed to reconnect, while we will try again we will report the
                // error if we can to avoid getting trapped in an infinite loop of
                // trying to reconnect
                self.refresh_error = Some(err);

                // Give other tasks a chance to progress before we try to recover
                // again. Since the future may not have registered a wake up we do so
                // now so the task is not forgotten
                cx.waker().wake_by_ref();
                return Poll::Pending;
            }

            match ready!(self.poll_complete(cx)) {
                PollFlushAction::None => return Poll::Ready(Ok(())),
                PollFlushAction::RebuildSlots => {
                    // Spawn refresh task
                    let task_handle = ClusterConnInner::spawn_refresh_slots_task(
                        self.inner.clone(),
                        &RefreshPolicy::Throttable,
                    );

                    // Update state
                    self.state =
                        ConnectionState::Recover(RecoverFuture::RefreshingSlots(task_handle));
                }
                PollFlushAction::ReconnectFromInitialConnections => {
                    let inner = self.inner.clone();
                    let handle = tokio::spawn(async move {
                        ClusterConnInner::reconnect_to_initial_nodes(inner).await
                    });
                    self.state =
                        ConnectionState::Recover(RecoverFuture::ReconnectToInitialNodes(handle));
                }
                PollFlushAction::Reconnect(addresses) => {
                    let inner = self.inner.clone();
                    let handle = tokio::spawn(async move {
                        ClusterConnInner::trigger_refresh_connection_tasks(
                            inner,
                            addresses
                                .into_iter()
                                .map(|s| Arc::<str>::from(&*s))
                                .collect(),
                            RefreshConnectionType::OnlyUserConnection,
                            true,
                        )
                        .await;
                    });
                    self.state = ConnectionState::Recover(RecoverFuture::Reconnect(handle));
                }
            }
        }
    }

    fn poll_close(
        mut self: Pin<&mut Self>,
        cx: &mut task::Context,
    ) -> Poll<std::result::Result<(), Self::Error>> {
        // Try to drive any in flight requests to completion
        match self.poll_complete(cx) {
            Poll::Ready(PollFlushAction::None) => (),
            Poll::Ready(_) => Err(())?,
            Poll::Pending => (),
        };
        // If we no longer have any requests in flight we are done (skips any reconnection
        // attempts)
        if self.in_flight_requests.is_empty() {
            return Poll::Ready(Ok(()));
        }

        self.poll_flush(cx)
    }
}

/// Result of attempting to get connections from initial seed nodes.
struct InitialNodeConnectionsResult<C> {
    /// Successfully found connections with their addresses
    #[allow(clippy::type_complexity)]
    connections: Vec<(Arc<str>, Shared<Pin<Box<dyn Future<Output = C> + Send>>>)>,
    /// Addresses that need connection refresh (exists in slot map but not in connection map)
    addresses_needing_refresh: HashSet<String>,
}

/// Returns connections found for randomly selected initial nodes, along with addresses
/// that needs to be refreshed.
///
/// # Lookup and Retry Logic
///
/// 1. **Resolve initial nodes**: Expand the configured initial nodes to their underlying addresses.
///    A single initial node may resolve to multiple addresses (e.g., DNS hostnames resolving to
///    multiple IP addresses).
/// 2. **Random selection**: Pick `num_of_nodes_to_query` random nodes from the resolved list
/// 3. **Connection lookup**: For each selected node, attempt to find an existing connection,
///    preferring a management connection when available.
///
/// ## Retry Mechanism
///
/// If **no connections** are found in the initial lookup (all selected nodes need refresh),
/// the function performs a single retry:
///
/// 1. Trigger connection refresh for all missing addresses
/// 2. Wait for refresh to complete (with `connection_timeout`)
/// 3. Retry the connection lookup for those addresses
///
/// # Returns
///
/// * `connections` - List of found connections (may be empty if all lookups failed)
/// * `addresses_needing_refresh` - Addresses that needs to be refreshed
async fn get_random_connections_from_initial_nodes<C>(
    inner: &Core<C>,
    num_of_nodes_to_query: usize,
) -> Result<InitialNodeConnectionsResult<C>>
where
    C: ConnectionLike + Connect + Clone + Send + Sync + 'static,
{
    if inner.initial_nodes.is_empty() {
        return Err(Error::from((
            ErrorKind::InvalidClientConfig,
            "Cannot refresh topology from initial nodes: no initial nodes configured",
        )));
    }

    // Resolve initial nodes and select random addresses for topology query.
    let selected_pairs = {
        let resolved =
            ClusterConnInner::<C>::try_to_expand_initial_nodes(&inner.initial_nodes).await;
        let mut rng = rand::rng();
        resolved
            .into_iter()
            .choose_multiple(&mut rng, num_of_nodes_to_query)
    };

    // Find existing connections for selected addresses
    let mut connections = Vec::with_capacity(selected_pairs.len());
    let mut addresses_needing_refresh = HashSet::new();

    for (original_addr, socket_addr) in selected_pairs {
        match lookup_management_connection(inner, &original_addr, socket_addr).await {
            ConnectionLookupResult::Found(conn) => connections.push(conn),
            ConnectionLookupResult::NeedsConnectionRefresh(addr) => {
                addresses_needing_refresh.insert(addr);
            }
        }
    }

    // Only refresh and retry if we have no connections at all
    if connections.is_empty() && !addresses_needing_refresh.is_empty() {
        let connection_timeout = inner.get_cluster_param(|p| p.connection_timeout);

        // Wait for connection refresh to complete (with timeout)
        let _ = tokio::time::timeout(
            connection_timeout,
            ClusterConnInner::refresh_and_update_connections(
                inner.clone(),
                addresses_needing_refresh
                    .iter()
                    .map(|s| Arc::<str>::from(s.as_str()))
                    .collect(),
                RefreshConnectionType::AllConnections,
                true,
            ),
        )
        .await;

        let mut still_need_refresh = HashSet::new();
        for addr in addresses_needing_refresh.drain() {
            if let ConnectionLookupResult::Found(conn) =
                lookup_management_connection(inner, &addr, None).await
            {
                connections.push(conn);
            } else {
                still_need_refresh.insert(addr);
            }
        }
        addresses_needing_refresh = still_need_refresh;
    }

    Ok(InitialNodeConnectionsResult {
        connections,
        addresses_needing_refresh,
    })
}

/// Result of attempting to find a connection for a node
#[allow(clippy::type_complexity)]
enum ConnectionLookupResult<C> {
    /// Connection found - returns the node address as stored in the connection map and the connection future
    Found((Arc<str>, Shared<Pin<Box<dyn Future<Output = C> + Send>>>)),
    /// Connection not found - needs refresh for the given address
    NeedsConnectionRefresh(String),
}

/// Finds a management connection for a node or indicates it needs refresh.
/// Returns the management connection if available, otherwise falls back to user connection.
///
/// **Warning** ⚠️: This function may have O(n) time complexity. Where `n` is the number of nodes in the slot map.
///
/// # Arguments
///
/// * `original_addr` - The address as provided by the user, either as a hostname (DNS)
///   or as a direct IP address (e.g., `"cluster.example.com:6379"` or `"127.0.0.1:6379"`).
/// * `socket_addr` - The resolved socket address if DNS resolution succeeded, or `None`
///   if resolution failed.
///
/// # Lookup Logic
/// Resolves the node's canonical address from the slot map. The canonical address is how
/// the node is stored in the `slot_map`, which is our source of truth (e.g., `"my-cluster-001-001.xyz:6379"`).
/// The caller may provide `original_addr` as an IP, but the slot map may store the same node with its DNS name.
///
/// The address resolution follows these steps:
///
/// 1. **Direct match**: If `original_addr` exists in the slot map, it is treated as
///    the canonical address (O(1)).
/// 2. **IP-based match**: Otherwise, if `socket_addr` is available, attempt to find
///    a canonical address in the slot map that matches its IP (O(n)).
/// 3. **Default address selection**: If no canonical address is found in the slot map,
///    select an address to use for a potential new connection:
///    - Prefer `socket_addr` if available
///    - Otherwise, use `original_addr` as-is
///
/// # Connection Lookup
/// After resolving the canonical address, the function attempts to retrieve a connection
/// for that address, preferring the management connection but falling back to the
/// user connection if the management connection is unavailable.
/// If not found, it indicates that a new connection needs to be established.
///
/// # Returns
/// * `Found` - Connection was found
/// * `NeedsConnectionRefresh` - Connection is missing and needs to be refreshed
async fn lookup_management_connection<C>(
    inner: &Core<C>,
    original_addr: &str,
    socket_addr: Option<SocketAddr>,
) -> ConnectionLookupResult<C>
where
    C: ConnectionLike + Connect + Clone + Send + Sync + 'static,
{
    let original_addr_key = Arc::new(original_addr.to_string());

    let (canonical_addr, conn_opt) = {
        let conn_lock = inner.conn_lock.read();

        // Resolve canonical address using the lookup chain:
        let canonical_addr = if conn_lock
            .slot_map
            .nodes_map()
            .contains_key(&original_addr_key)
        {
            // Step 1: Direct match
            original_addr.to_string()
        } else {
            socket_addr
                // Step 2: IP-based match
                .and_then(|addr| {
                    conn_lock
                        .slot_map
                        .node_address_for_ip(addr.ip())
                        .map(|a| (*a).clone())
                })
                // Step 3: Use socket_addr if available
                .or_else(|| socket_addr.map(|addr| addr.to_string()))
                // Step 4: Last resort - use original_addr
                .unwrap_or_else(|| original_addr.to_string())
        };

        // Look up management connection, fall back to user connection if not available
        let conn_opt = conn_lock.management_connection_for_address(&canonical_addr);
        (canonical_addr, conn_opt)
    };

    match conn_opt {
        Some(conn) => ConnectionLookupResult::Found(conn),
        None => ConnectionLookupResult::NeedsConnectionRefresh(canonical_addr),
    }
}
/// Result of querying random cluster nodes for topology calculation.
struct TopologyQueryResult {
    /// The calculated topology (slot map and hash), or an error if calculation failed
    topology_result: Result<(SlotMap, TopologyHash)>,
    /// Optionally addresses of connections that failed during the query and need refresh
    failed_connections: Option<HashSet<String>>,
}

/// Queries random cluster nodes to calculate the current topology.
///
/// Selects up to `num_of_nodes_to_query` random nodes and queries them for their
/// view of the cluster topology. The results are aggregated to determine the
/// most common topology view.
///
/// When `trigger` is `SlotRefreshTrigger::RuntimeRefresh` and `refresh_topology_from_initial_nodes`
/// is enabled, connections are obtained from initial seed nodes rather than
/// existing cluster connections.
async fn calculate_topology_from_random_nodes<C>(
    inner: &Core<C>,
    num_of_nodes_to_query: usize,
    curr_retry: usize,
    trigger: SlotRefreshTrigger,
) -> TopologyQueryResult
where
    C: ConnectionLike + Connect + Clone + Send + Sync + 'static,
{
    let refresh_topology_from_initial_nodes =
        inner.get_cluster_param(|p| p.refresh_topology_from_initial_nodes);

    // During initial connection, use existing connections to avoid double DNS lookup
    let use_initial_nodes_lookup = refresh_topology_from_initial_nodes
        && !matches!(trigger, SlotRefreshTrigger::InitialConnection);

    // Get connections either from seed nodes or random existing connections.
    let (requested_nodes, mut failed_addresses) = if use_initial_nodes_lookup {
        match get_random_connections_from_initial_nodes(inner, num_of_nodes_to_query).await {
            Ok(InitialNodeConnectionsResult {
                connections,
                addresses_needing_refresh,
            }) => (connections, addresses_needing_refresh),
            Err(err) => {
                return TopologyQueryResult {
                    topology_result: Err(err),
                    failed_connections: None,
                };
            }
        }
    } else if let Some(random_conns) = inner
        .conn_lock
        .read()
        .random_connections(num_of_nodes_to_query, ConnectionType::PreferManagement)
    {
        (random_conns, HashSet::new())
    } else {
        return TopologyQueryResult {
            topology_result: Err(Error::from((
                ErrorKind::AllConnectionsUnavailable,
                "No available connections to refresh slots from",
            ))),
            failed_connections: None,
        };
    };

    let topology_join_results =
        futures::future::join_all(requested_nodes.into_iter().map(|(addr, conn)| async move {
            let mut conn: C = conn.await;
            let res = conn.req_packed_command(&slot_cmd()).await;
            (addr, res)
        }))
        .await;

    // Add topology command failures (with unrecoverable errors) to the existing connection failures set.
    failed_addresses.extend(
        topology_join_results
            .iter()
            .filter_map(|(address, res)| match res {
                Err(err) if err.is_unrecoverable_error() => Some(address.to_string()),
                _ => None,
            }),
    );

    // Check for PermissionDenied errors (NOPERM) and return early if found
    // Note: NOPERM is an ACL error. ACL permissions are expected to be applied cluster wide.
    // If NOPERM is found it should be surfaced first, otherwise we continue.
    if let Some(noperm_err) = topology_join_results.iter().find_map(|(_, res)| {
        res.as_ref()
            .err()
            .filter(|err| err.kind() == ErrorKind::PermissionDenied)
    }) {
        return TopologyQueryResult {
            topology_result: Err(noperm_err.clone_mostly("")),
            failed_connections: Some(failed_addresses),
        };
    }

    let topology_values = topology_join_results.iter().filter_map(|(addr, res)| {
        res.as_ref()
            .ok()
            .and_then(|value| get_host_and_port_from_addr(addr).map(|(host, _)| (host, value)))
    });
    let tls_mode = inner.get_cluster_param(|params| params.tls);

    let read_from_replicas = inner.get_cluster_param(|params| params.read_from_replicas.clone());
    TopologyQueryResult {
        topology_result: calculate_topology(
            topology_values,
            curr_retry,
            tls_mode,
            num_of_nodes_to_query,
            read_from_replicas,
        ),
        failed_connections: Some(failed_addresses),
    }
}

impl<C> ConnectionLike for ClusterConnection<C>
where
    C: ConnectionLike + Send + Clone + Unpin + Sync + Connect + 'static,
{
    async fn req_packed_command(&mut self, cmd: &Cmd) -> Result<Value> {
        let routing = cluster_routing::RoutingInfo::for_routable(cmd).unwrap_or(
            cluster_routing::RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random),
        );

        // Fast path: single-node commands dispatch directly to the mux connection,
        // bypassing the cluster task and FuturesUnordered entirely.
        if let cluster_routing::RoutingInfo::SingleNode(ref single) = routing
            && let Some(result) = self.try_direct_dispatch(cmd, single).await
        {
            return result;
        }

        // Slow path: fan-out, MOVED/ASK retries, or route resolution failed.
        self.route_command(cmd, routing).await
    }

    async fn req_packed_commands(
        &mut self,
        pipeline: &crate::pipeline::Pipeline,
        offset: usize,
        count: usize,
        pipeline_retry_strategy: Option<PipelineRetryStrategy>,
    ) -> Result<Vec<Result<Value>>> {
        let route = route_for_pipeline(pipeline)?;

        // Fast path: same-slot pipeline dispatched directly to the node's mux connection.
        if let Some(ref route) = route {
            let conn_future = {
                let container = match self.inner.conn_lock.try_read() {
                    Some(guard) => guard,
                    None => self.inner.conn_lock.read(),
                };
                container.connection_for_route(route).map(|(_, conn)| conn)
            };
            if let Some(conn_future) = conn_future {
                let mut conn = conn_future.await;
                let result = conn
                    .req_packed_commands(pipeline, offset, count, pipeline_retry_strategy)
                    .await;
                match &result {
                    Ok(_) => return result,
                    Err(e) if e.kind() == ErrorKind::Moved || e.kind() == ErrorKind::Ask => {
                        // Fall through to cluster task for retry
                    }
                    Err(_) => return result,
                }
            }
        }

        // Fast path: cross-slot non-atomic pipeline — split by node and send directly.
        if !pipeline.is_atomic()
            && route.is_none()
            && let Some(result) = self.try_direct_pipeline_dispatch(pipeline, count).await
        {
            return result;
        }

        // Slow path: MOVED/ASK retry, or direct dispatch failed
        self.route_pipeline(
            pipeline,
            offset,
            count,
            route.map(|r| Some(r).into()),
            pipeline_retry_strategy,
        )
        .await
    }

    fn get_db(&self) -> i64 {
        0
    }

    fn is_closed(&self) -> bool {
        false
    }
}

/// Implements the process of connecting to a Valkey server
/// and obtaining a connection handle.
pub trait Connect: Sized {
    /// Connect to a node.
    /// For TCP connections, returning a tuple of handle for command execution and the node's IP address.
    /// For UNIX connections, returning a tuple of handle for command execution and None.
    fn connect<'a, T>(
        info: T,
        response_timeout: Duration,
        connection_timeout: Duration,
        socket_addr: Option<SocketAddr>,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> BoxFuture<'a, Result<(Self, Option<IpAddr>)>>
    where
        T: IntoConnectionInfo + Send + 'a;
}

impl Connect for MultiplexedConnection {
    fn connect<'a, T>(
        info: T,
        response_timeout: Duration,
        connection_timeout: Duration,
        socket_addr: Option<SocketAddr>,
        ferriskey_connection_options: FerrisKeyConnectionOptions,
    ) -> BoxFuture<'a, Result<(MultiplexedConnection, Option<IpAddr>)>>
    where
        T: IntoConnectionInfo + Send + 'a,
    {
        async move {
            let connection_info = info.into_connection_info()?;
            let client = crate::connection::factory::Client::open(connection_info)?;

            crate::connection::runtime::timeout(
                connection_timeout,
                client.get_multiplexed_async_connection_inner::<crate::connection::tokio::Tokio>(
                    response_timeout,
                    socket_addr,
                    ferriskey_connection_options,
                ),
            )
            .await?
        }
        .boxed()
    }
}

#[cfg(test)]
mod pipeline_routing_tests {
    use std::collections::HashMap;
    use std::sync::Arc;

    use futures::executor::block_on;

    use super::ClusterConnInner;
    use super::pipeline::PipelineResponses;
    use super::pipeline::route_for_pipeline;
    use crate::cluster::routing::{
        AggregateOp, MultiSlotArgPattern, MultipleNodeRoutingInfo, ResponsePolicy, Route, SlotAddr,
    };
    use crate::cmd::cmd;
    use crate::connection::MultiplexedConnection;
    use crate::value::{ErrorKind, Error, Value};

    #[test]
    fn test_first_route_is_found() {
        let mut pipeline = crate::pipeline::Pipeline::new();
        pipeline.atomic();

        pipeline
            .add_command(cmd("FLUSHALL")) // route to all masters
            .cmd("GET")
            .arg("foo") // route to slot 12182
            .add_command(cmd("EVAL")); // route randomly

        assert_eq!(
            route_for_pipeline(&pipeline),
            Ok(Some(Route::new(12182, SlotAddr::ReplicaOptional)))
        );
    }

    #[test]
    fn test_numerical_response_aggregation_logic() {
        let pipeline_responses: PipelineResponses = vec![
            vec![
                (Some(Arc::from("node1")), Ok(Value::Int(3))),
                (Some(Arc::from("node2")), Ok(Value::Int(7))),
                (Some(Arc::from("node3")), Ok(Value::Int(0))),
            ],
            vec![(
                Some(Arc::from("node3")),
                Ok(Value::BulkString(b"unchanged".to_vec().into())),
            )],
            vec![
                (Some(Arc::from("node1")), Ok(Value::Int(5))),
                (Some(Arc::from("node2")), Ok(Value::Int(11))),
            ],
        ];
        let response_policies = HashMap::from([
            (
                0,
                (
                    MultipleNodeRoutingInfo::AllNodes,
                    Some(ResponsePolicy::Aggregate(AggregateOp::Sum)),
                ),
            ),
            (
                2,
                (
                    MultipleNodeRoutingInfo::AllMasters,
                    Some(ResponsePolicy::Aggregate(AggregateOp::Min)),
                ),
            ),
        ]);
        let responses = block_on(
            ClusterConnInner::<MultiplexedConnection>::aggregate_pipeline_multi_node_commands(
                pipeline_responses,
                response_policies,
            ),
        );

        // Command 0 should be aggregated to 3 + 7 + 0 = 10.
        // Command 1 should remain unchanged.
        assert_eq!(
            responses,
            vec![
                Ok(Value::Int(10)),
                Ok(Value::BulkString(b"unchanged".to_vec().into())),
                Ok(Value::Int(5))
            ],
            "{responses:?}"
        );
    }

    #[test]
    fn test_combine_arrays_response_aggregation_logic() {
        let pipeline_responses: PipelineResponses = vec![
            vec![
                (Some(Arc::from("node1")), Ok(Value::Array(vec![Ok(Value::Int(1))]))),
                (Some(Arc::from("node2")), Ok(Value::Array(vec![Ok(Value::Int(2))]))),
            ],
            vec![
                (
                    Some(Arc::from("node2")),
                    Ok(Value::Array(vec![
                        Ok(Value::BulkString("key1".into())),
                        Ok(Value::BulkString("key3".into())),
                    ])),
                ),
                (
                    Some(Arc::from("node1")),
                    Ok(Value::Array(vec![
                        Ok(Value::BulkString("key2".into())),
                        Ok(Value::BulkString("key4".into())),
                    ])),
                ),
            ],
        ];
        let response_policies = HashMap::from([
            (
                0,
                (
                    MultipleNodeRoutingInfo::AllNodes,
                    Some(ResponsePolicy::CombineArrays),
                ),
            ),
            (
                1,
                (
                    MultipleNodeRoutingInfo::MultiSlot((
                        vec![
                            (Route::new(1, SlotAddr::Master), vec![0, 2]),
                            (Route::new(2, SlotAddr::Master), vec![1, 3]),
                        ],
                        MultiSlotArgPattern::KeysOnly,
                    )),
                    Some(ResponsePolicy::CombineArrays),
                ),
            ),
        ]);

        let responses = block_on(
            ClusterConnInner::<MultiplexedConnection>::aggregate_pipeline_multi_node_commands(
                pipeline_responses,
                response_policies,
            ),
        );

        let mut expected = Value::Array(vec![Ok(Value::Int(1)), Ok(Value::Int(2))]);
        assert_eq!(
            responses[0], Ok(expected),
            "Expected combined array to include all elements"
        );
        expected = Value::Array(vec![
            Ok(Value::BulkString("key1".into())),
            Ok(Value::BulkString("key2".into())),
            Ok(Value::BulkString("key3".into())),
            Ok(Value::BulkString("key4".into())),
        ]);
        assert_eq!(
            responses[1], Ok(expected),
            "Expected combined array to include all elements"
        );
    }

    #[test]
    fn test_aggregate_pipeline_multi_node_commands_with_error_response() {
        let pipeline_responses: PipelineResponses = vec![
            vec![
                (Some(Arc::from("node1")), Ok(Value::Int(3))),
                (Some(Arc::from("node2")), Ok(Value::Int(7))),
                (
                    Some(Arc::from("node3")),
                    Err(Error::from((ErrorKind::Moved, "MOVED", "127.0.0.1".to_string()))),
                ),
            ],
            vec![(
                Some(Arc::from("node3")),
                Ok(Value::BulkString(b"unchanged".to_vec().into())),
            )],
        ];
        let mut response_policies = HashMap::new();
        response_policies.insert(
            0,
            (
                MultipleNodeRoutingInfo::AllNodes,
                Some(ResponsePolicy::CombineArrays),
            ),
        );

        let responses = block_on(
            ClusterConnInner::<MultiplexedConnection>::aggregate_pipeline_multi_node_commands(
                pipeline_responses,
                response_policies,
            ),
        );

        // First element: error from MOVED is preserved as Err
        assert_eq!(responses.len(), 2);
        assert!(responses[0].is_err(), "Expected Err for MOVED response, got: {:?}", responses[0]);
        assert_eq!(responses[1], Ok(Value::BulkString(b"unchanged".to_vec().into())));
    }

    #[test]
    fn test_aggregate_pipeline_multi_node_commands_with_no_response_for_command() {
        let pipeline_responses: PipelineResponses =
            vec![vec![(Some(Arc::from("node1")), Ok(Value::Int(1)))], vec![]];
        let response_policies = HashMap::new();

        let responses = block_on(
            ClusterConnInner::<MultiplexedConnection>::aggregate_pipeline_multi_node_commands(
                pipeline_responses,
                response_policies,
            ),
        );

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0], Ok(Value::Int(1)));
        assert!(responses[1].is_err(), "Expected Err for missing response, got: {:?}", responses[1]);
    }

    #[test]
    fn test_aggregate_pipeline_responses_with_multiple_responses_for_command() {
        let pipeline_responses: PipelineResponses = vec![
            vec![(Some(Arc::from("node1")), Ok(Value::Int(1)))],
            vec![
                (Some(Arc::from("node2")), Ok(Value::Int(2))),
                (Some(Arc::from("node3")), Ok(Value::Int(3))),
            ],
        ];
        let response_policies = HashMap::new();

        let responses = block_on(
            ClusterConnInner::<MultiplexedConnection>::aggregate_pipeline_multi_node_commands(
                pipeline_responses,
                response_policies,
            ),
        );

        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0], Ok(Value::Int(1)));
        assert!(responses[1].is_err(), "Expected Err for duplicate response, got: {:?}", responses[1]);
    }

    #[test]
    fn test_return_none_if_no_route_is_found() {
        let mut pipeline = crate::pipeline::Pipeline::new();
        pipeline.atomic();
        pipeline
            .add_command(cmd("FLUSHALL")) // route to all masters
            .add_command(cmd("EVAL")); // route randomly

        assert_eq!(route_for_pipeline(&pipeline), Ok(None));
    }

    #[test]
    fn test_prefer_primary_route_over_replica() {
        let mut pipeline = crate::pipeline::Pipeline::new();
        pipeline.atomic();
        pipeline
            .cmd("GET")
            .arg("foo") // route to replica of slot 12182
            .add_command(cmd("FLUSHALL")) // route to all masters
            .add_command(cmd("EVAL")) // route randomly
            .cmd("CONFIG")
            .arg("GET")
            .arg("timeout") // unkeyed command
            .cmd("SET")
            .arg("foo")
            .arg("bar"); // route to primary of slot 12182

        assert_eq!(
            route_for_pipeline(&pipeline),
            Ok(Some(Route::new(12182, SlotAddr::Master)))
        );
    }

    #[test]
    fn test_raise_cross_slot_error_on_conflicting_slots() {
        let mut pipeline = crate::pipeline::Pipeline::new();
        pipeline.atomic();
        pipeline
            .add_command(cmd("FLUSHALL")) // route to all masters
            .cmd("SET")
            .arg("baz")
            .arg("bar") // route to slot 4813
            .cmd("GET")
            .arg("foo"); // route to slot 12182

        assert_eq!(
            route_for_pipeline(&pipeline).unwrap_err().kind(),
            crate::value::ErrorKind::CrossSlot
        );
    }

    #[test]
    fn unkeyed_commands_dont_affect_route() {
        let mut pipeline = crate::pipeline::Pipeline::new();
        pipeline.atomic();
        pipeline
            .cmd("SET")
            .arg("{foo}bar")
            .arg("baz") // route to primary of slot 12182
            .cmd("CONFIG")
            .arg("GET")
            .arg("timeout") // unkeyed command
            .cmd("SET")
            .arg("foo")
            .arg("bar") // route to primary of slot 12182
            .cmd("DEBUG")
            .arg("PAUSE")
            .arg("100") // unkeyed command
            .cmd("ECHO")
            .arg("hello world"); // unkeyed command

        assert_eq!(
            route_for_pipeline(&pipeline),
            Ok(Some(Route::new(12182, SlotAddr::Master)))
        );
    }
}

#[cfg(test)]
mod parse_node_address_tests {
    use super::parse_node_address;

    #[test]
    fn host_and_port() {
        assert_eq!(
            parse_node_address("127.0.0.1:6379"),
            Some(("127.0.0.1", 6379))
        );
    }

    #[test]
    fn hostname_and_port() {
        assert_eq!(
            parse_node_address("redis.example.com:6380"),
            Some(("redis.example.com", 6380))
        );
    }

    #[test]
    fn ipv6_and_port() {
        // rsplit_once splits on the last colon, giving the IPv6 prefix as host
        assert_eq!(parse_node_address("::1:6379"), Some(("::1", 6379)));
    }

    #[test]
    fn no_colon_returns_none() {
        assert_eq!(parse_node_address("localhost"), None);
    }

    #[test]
    fn invalid_port_returns_none() {
        assert_eq!(parse_node_address("host:notaport"), None);
    }

    #[test]
    fn empty_string_returns_none() {
        assert_eq!(parse_node_address(""), None);
    }

    #[test]
    fn port_only() {
        // ":6379" → host="", port=6379
        assert_eq!(parse_node_address(":6379"), Some(("", 6379)));
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Direct-dispatch integration tests
//
// These tests exercise try_direct_dispatch and try_direct_pipeline_dispatch
// in isolation by injecting a lightweight MockConn that records every bytes
// payload it receives and returns pre-queued Result<Value> responses.
// No real TCP connections are used.
//
// Slot facts used throughout (CRC-16/CCITT of the key name):
//   "foo"  → slot 12182  (well-known from routing unit tests)
//   "baz"  → slot 4813
// ─────────────────────────────────────────────────────────────────────────────
#[cfg(test)]
mod direct_dispatch_tests {
    use super::*;
    use crate::cluster::{
        client::ClusterParams,
        routing::{Route, SingleNodeRoutingInfo, Slot, SlotAddr},
        slotmap::{ReadFromReplicaStrategy, SlotMap},
    };
    use crate::pipeline::PipelineRetryStrategy;
    use container::{ClusterNode, ConnectionDetails, ConnectionsMap};
    use dashmap::DashMap;

    use std::{
        collections::VecDeque,
        net::IpAddr,
        sync::{Arc, Mutex},
    };

    // ── MockConn ────────────────────────────────────────────────────────────
    // A zero-TCP fake connection. Stores pre-queued responses and records every
    // raw bytes payload it receives (from send_packed_bytes / req_packed_command).

    #[derive(Clone)]
    struct MockConn {
        responses: Arc<Mutex<VecDeque<Result<Value>>>>,
        /// Each entry is the raw bytes payload received by send_packed_bytes or
        /// req_packed_command (packed command bytes).
        received: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    impl MockConn {
        fn new() -> Self {
            MockConn {
                responses: Arc::new(Mutex::new(VecDeque::new())),
                received: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn push_ok(&self, v: Value) {
            self.responses.lock().unwrap().push_back(Ok(v));
        }

        fn push_err(&self, e: Error) {
            self.responses.lock().unwrap().push_back(Err(e));
        }

        fn received_count(&self) -> usize {
            self.received.lock().unwrap().len()
        }

        /// Returns a copy of the nth received payload, panicking if absent.
        #[allow(dead_code)]
        fn received_nth(&self, n: usize) -> Vec<u8> {
            self.received.lock().unwrap()[n].clone()
        }

        fn pop_response(&self) -> Result<Value> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Ok(Value::Nil))
        }
    }

    impl ConnectionLike for MockConn {
        async fn req_packed_command(&mut self, cmd: &Cmd) -> Result<Value> {
            self.received
                .lock()
                .unwrap()
                .push(cmd.get_packed_command().to_vec());
            self.pop_response()
        }

        async fn req_packed_commands(
            &mut self,
            _pipeline: &crate::pipeline::Pipeline,
            _offset: usize,
            _count: usize,
            _retry: Option<PipelineRetryStrategy>,
        ) -> Result<Vec<Result<Value>>> {
            self.received.lock().unwrap().push(b"<pipeline>".to_vec());
            match self.pop_response() {
                Ok(Value::Array(vals)) => Ok(vals),
                Ok(v) => Ok(vec![Ok(v)]),
                Err(e) => Err(e),
            }
        }

        async fn send_packed_bytes(
            &mut self,
            packed: bytes::Bytes,
            _is_fenced: bool,
        ) -> Result<Value> {
            self.received.lock().unwrap().push(packed.to_vec());
            self.pop_response()
        }

        fn get_db(&self) -> i64 {
            0
        }
        fn is_closed(&self) -> bool {
            false
        }
    }

    impl Connect for MockConn {
        fn connect<'a, T>(
            _info: T,
            _response_timeout: Duration,
            _connection_timeout: Duration,
            _socket_addr: Option<SocketAddr>,
            _opts: FerrisKeyConnectionOptions,
        ) -> BoxFuture<'a, Result<(Self, Option<IpAddr>)>>
        where
            T: IntoConnectionInfo + Send + 'a,
        {
            async { Ok((MockConn::new(), None)) }.boxed()
        }
    }

    // ── Builder helpers ─────────────────────────────────────────────────────

    /// Wrap a MockConn in the ConnectionFuture type expected by ClusterNode.
    fn into_future(conn: MockConn) -> ConnectionFuture<MockConn> {
        async move { conn }.boxed().shared()
    }

    /// Build a ClusterConnection<MockConn> from a list of (addr, slot_start,
    /// slot_end, conn) tuples.  Each entry becomes a primary node.
    fn build_cluster(nodes: Vec<(&str, u16, u16, MockConn)>) -> ClusterConnection<MockConn> {
        let slots: Vec<Slot> = nodes
            .iter()
            .map(|(addr, start, end, _)| Slot::new(*start, *end, addr.to_string(), vec![]))
            .collect();

        let slot_map = SlotMap::new(
            slots,
            HashMap::new(),
            ReadFromReplicaStrategy::AlwaysFromPrimary,
        );

        let dm: DashMap<Arc<str>, ClusterNode<ConnectionFuture<MockConn>>> = DashMap::new();
        for (addr, _, _, conn) in nodes {
            let details = ConnectionDetails {
                conn: into_future(conn),
                ip: None,
                az: None,
            };
            dm.insert(Arc::from(addr), ClusterNode::new(details, None));
        }

        let container = container::ConnectionsContainer::new(
            slot_map,
            ConnectionsMap(dm),
            ReadFromReplicaStrategy::AlwaysFromPrimary,
            0,
        );

        let params = ClusterParams::default_for_test(None);
        let (pending_tx, pending_rx) = mpsc::unbounded_channel();
        let inner = Arc::new(InnerCore {
            conn_lock: ParkingLotRwLock::new(container),
            cluster_params: ParkingLotRwLock::new(params),
            pending_requests_tx: pending_tx,
            pending_requests_rx: std::sync::Mutex::new(pending_rx),
            slot_refresh_state: SlotRefreshState::new(
                crate::cluster::client::SlotsRefreshRateLimit::default(),
            ),
            initial_nodes: vec![],
            ferriskey_connection_options: FerrisKeyConnectionOptions {
                push_sender: None,
                disconnect_notifier: None,
                discover_az: false,
                connection_timeout: None,
                connection_retry_strategy: None,
                tcp_nodelay: false,
                pubsub_synchronizer: None,
                iam_token_provider: None,
            },
            topology_refresh_lock: tokio::sync::Mutex::new(()),
        });

        let (cluster_tx, _cluster_rx) = mpsc::channel(1);
        ClusterConnection {
            cluster_task: cluster_tx,
            inner,
        }
    }

    /// Create a MOVED error that redirect_node() will parse as (addr, slot).
    fn moved_err(slot: u16, addr: &str) -> Error {
        Error::from((ErrorKind::Moved, "key moved", format!("{slot} {addr}")))
    }

    /// Create an ASK error that redirect_node() will parse as (addr, slot).
    fn ask_err(slot: u16, addr: &str) -> Error {
        Error::from((ErrorKind::Ask, "key moved (ask)", format!("{slot} {addr}")))
    }

    // ── Test 1 ──────────────────────────────────────────────────────────────
    // Happy-path routing: GET "foo" (slot 12182) goes directly to the node
    // that owns the slot, without touching the other node.
    #[tokio::test]
    async fn test_try_direct_dispatch_routes_to_correct_node() {
        let node_a = MockConn::new(); // owns slots 0–8191 (not slot 12182)
        let node_b = MockConn::new(); // owns slots 8192–16383 (includes slot 12182)
        node_b.push_ok(Value::BulkString(b"bar".to_vec().into()));

        let conn = build_cluster(vec![
            ("127.0.0.1:7001", 0, 8191, node_a.clone()),
            ("127.0.0.1:7002", 8192, 16383, node_b.clone()),
        ]);

        // GET "foo" → slot 12182 → node_b
        let get_foo = cmd("GET").arg("foo").to_owned();
        let routing = SingleNodeRoutingInfo::SpecificNode(Route::new(12182, SlotAddr::Master));
        let result = conn.try_direct_dispatch(&get_foo, &routing).await;

        assert!(result.is_some(), "expected Some from direct dispatch");
        assert_eq!(
            result.unwrap(),
            Ok(Value::BulkString(b"bar".to_vec().into()))
        );

        // Command must have gone to node_b only.
        assert_eq!(
            node_b.received_count(),
            1,
            "node_b should have received 1 command"
        );
        assert_eq!(
            node_a.received_count(),
            0,
            "node_a must not receive any command"
        );
    }

    // ── Test 2 ──────────────────────────────────────────────────────────────
    // MOVED to a known node: the first node returns MOVED pointing to the
    // second node.  The command must be retried on the redirect target.
    #[tokio::test]
    async fn test_try_direct_dispatch_moved_to_known_node() {
        let node_a = MockConn::new(); // initially owns "baz" (slot 4813)
        let node_b = MockConn::new(); // will receive the redirected command

        // node_a returns MOVED → node_b
        node_a.push_err(moved_err(4813, "127.0.0.1:7002"));
        // node_b returns success
        node_b.push_ok(Value::BulkString(b"value".to_vec().into()));

        let conn = build_cluster(vec![
            ("127.0.0.1:7001", 0, 8191, node_a.clone()),
            ("127.0.0.1:7002", 8192, 16383, node_b.clone()),
        ]);

        let get_baz = cmd("GET").arg("baz").to_owned();
        let routing = SingleNodeRoutingInfo::SpecificNode(Route::new(4813, SlotAddr::Master));
        let result = conn.try_direct_dispatch(&get_baz, &routing).await;

        assert!(result.is_some());
        assert_eq!(
            result.unwrap(),
            Ok(Value::BulkString(b"value".to_vec().into()))
        );
        // node_a got the original command; node_b got the redirected retry.
        assert_eq!(
            node_a.received_count(),
            1,
            "node_a should receive the original command"
        );
        assert_eq!(
            node_b.received_count(),
            1,
            "node_b should receive the redirected command"
        );
    }

    // ── Test 3 ──────────────────────────────────────────────────────────────
    // MOVED to an address that is NOT in the connection map.
    // try_direct_dispatch must return None so the caller falls back to the
    // cluster task.  No new connection must be established.
    #[tokio::test]
    async fn test_try_direct_dispatch_moved_to_unknown_address() {
        let node_a = MockConn::new();
        // Returns MOVED to a node that is not in our topology.
        node_a.push_err(moved_err(4813, "10.0.0.99:7999"));

        let conn = build_cluster(vec![("127.0.0.1:7001", 0, 16383, node_a.clone())]);

        let get_baz = cmd("GET").arg("baz").to_owned();
        let routing = SingleNodeRoutingInfo::SpecificNode(Route::new(4813, SlotAddr::Master));
        let result = conn.try_direct_dispatch(&get_baz, &routing).await;

        // Must fall back: unknown redirect target → None.
        assert!(
            result.is_none(),
            "expected None when redirect target is not in the topology"
        );
        // node_a still received the original (failed) attempt.
        assert_eq!(node_a.received_count(), 1);
    }

    // ── Test 4 ──────────────────────────────────────────────────────────────
    // ASK redirect: node_a returns ASK pointing to node_b.
    // The direct dispatch path must NOT handle ASK (ASKING + command cannot
    // be sent atomically on a shared multiplexed connection), so it returns
    // None to fall back to the cluster task's single-threaded state machine.
    #[tokio::test]
    async fn test_try_direct_dispatch_ask_redirect() {
        let node_a = MockConn::new();
        let node_b = MockConn::new();

        node_a.push_err(ask_err(4813, "127.0.0.1:7002"));

        let conn = build_cluster(vec![
            ("127.0.0.1:7001", 0, 8191, node_a.clone()),
            ("127.0.0.1:7002", 8192, 16383, node_b.clone()),
        ]);

        let get_baz = cmd("GET").arg("baz").to_owned();
        let routing = SingleNodeRoutingInfo::SpecificNode(Route::new(4813, SlotAddr::Master));
        let result = conn.try_direct_dispatch(&get_baz, &routing).await;

        // ASK must fall back to cluster task — direct path cannot guarantee
        // atomic ASKING + command on a shared multiplexed connection.
        assert!(
            result.is_none(),
            "expected None for ASK redirect so cluster task handles it correctly"
        );
        // node_b must NOT have received anything — the direct path did not attempt ASK.
        assert_eq!(
            node_b.received_count(),
            0,
            "node_b should not receive any requests in the direct path for ASK"
        );
    }

    // ── Test 5 ──────────────────────────────────────────────────────────────
    // Cross-slot pipeline ordering: a two-command pipeline targeting different
    // nodes.  Results must be assembled in the original command order regardless
    // of which sub-pipeline completes first.
    //
    //   cmd[0] = GET "foo"  → slot 12182 → node_b  returns "val_foo"
    //   cmd[1] = GET "baz"  → slot 4813  → node_a  returns "val_baz"
    //
    // Final result must be ["val_foo", "val_baz"].
    #[tokio::test]
    async fn test_direct_pipeline_dispatch_ordering() {
        let node_a = MockConn::new(); // owns slots 0–8191
        let node_b = MockConn::new(); // owns slots 8192–16383

        // Each node returns its sub-pipeline results as a Value::Array.
        node_b.push_ok(Value::Array(vec![Ok(Value::BulkString(
            b"val_foo".to_vec().into(),
        ))]));
        node_a.push_ok(Value::Array(vec![Ok(Value::BulkString(
            b"val_baz".to_vec().into(),
        ))]));

        let conn = build_cluster(vec![
            ("127.0.0.1:7001", 0, 8191, node_a.clone()),
            ("127.0.0.1:7002", 8192, 16383, node_b.clone()),
        ]);

        let mut pipeline = crate::pipeline::Pipeline::new();
        pipeline.cmd("GET").arg("foo"); // slot 12182 → node_b
        pipeline.cmd("GET").arg("baz"); // slot 4813  → node_a

        let result = conn.try_direct_pipeline_dispatch(&pipeline, 2).await;

        assert!(
            result.is_some(),
            "expected Some from pipeline direct dispatch"
        );
        let values = result.unwrap().expect("pipeline should succeed");
        assert_eq!(values.len(), 2);
        assert_eq!(
            values[0],
            Ok(Value::BulkString(b"val_foo".to_vec().into())),
            "index 0 must be the result of GET foo"
        );
        assert_eq!(
            values[1],
            Ok(Value::BulkString(b"val_baz".to_vec().into())),
            "index 1 must be the result of GET baz"
        );
    }

    // ── Test 6 ──────────────────────────────────────────────────────────────
    // Partial sub-pipeline failure: if any node returns an error the whole
    // direct-dispatch path must return None so the cluster task handles retry.
    #[tokio::test]
    async fn test_direct_pipeline_dispatch_partial_failure_falls_back() {
        let node_a = MockConn::new(); // will fail
        let node_b = MockConn::new(); // will succeed

        node_b.push_ok(Value::Array(vec![Ok(Value::BulkString(b"ok".to_vec().into()))]));
        node_a.push_err(Error::from(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "simulated connection drop",
        )));

        let conn = build_cluster(vec![
            ("127.0.0.1:7001", 0, 8191, node_a.clone()),
            ("127.0.0.1:7002", 8192, 16383, node_b.clone()),
        ]);

        let mut pipeline = crate::pipeline::Pipeline::new();
        pipeline.cmd("GET").arg("foo"); // slot 12182 → node_b (succeeds)
        pipeline.cmd("GET").arg("baz"); // slot 4813  → node_a (fails)

        let result = conn.try_direct_pipeline_dispatch(&pipeline, 2).await;

        assert!(
            result.is_none(),
            "any sub-pipeline error must cause direct dispatch to return None"
        );
    }
}
