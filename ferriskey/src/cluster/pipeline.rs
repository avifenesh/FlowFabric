use super::ClusterConnInner;
use super::Connect;
use crate::cluster::routing as cluster_routing;
use crate::cluster::routing::Route;
use crate::cluster::routing::RoutingInfo;
use crate::cluster::routing::SlotAddr;
use crate::cluster::routing::{
    MultipleNodeRoutingInfo, ResponsePolicy, SingleNodeRoutingInfo, command_for_multi_slot_indices,
};
use crate::cmd::Cmd;
use crate::connection::ConnectionLike;
use crate::pipeline::Pipeline;
use crate::value::{ErrorKind, ValkeyError, ValkeyResult, Value};
use crate::value::{RetryMethod, ServerError};
use cluster_routing::RoutingInfo::{MultiNode, SingleNode};
use futures::FutureExt;
use logger_core::log_error;
use rand::prelude::IteratorRandom;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use telemetrylib::FerrisKeyOtel;
use tokio::sync::oneshot;
use tokio::sync::oneshot::error::RecvError;

use super::CmdArg;
use super::PendingRequest;
use super::PipelineRetryStrategy;
use super::RedirectNode;
use super::RequestInfo;
use super::boxed_sleep;
use super::connections::RefreshConnectionType;
use super::{Core, InternalSingleNodeRouting, OperationTarget, Response};

/// Represents a pipeline command execution context for a specific node
#[derive(Default)]
pub(crate) struct NodePipelineContext<C> {
    /// The pipeline of commands to be executed
    pub pipeline: Pipeline,
    /// The connection to the node
    pub connection: C,
    /// Vector of (command_index, inner_index) pairs tracking command order
    /// command_index: Position in the original pipeline
    /// inner_index: Optional sub-index for multi-node operations (e.g. MSET)
    /// ignore: Whether to ignore this commands response (e.g. `ASKING` command).
    pub command_indices: Vec<(usize, Option<usize>, bool)>,
}

/// Maps node addresses to their pipeline execution contexts
pub(crate) type NodePipelineMap<C> = HashMap<Arc<str>, NodePipelineContext<C>>;

impl<C> NodePipelineContext<C> {
    fn new(connection: C) -> Self {
        Self {
            pipeline: Pipeline::new(),
            connection,
            command_indices: Vec::new(),
        }
    }

    // Adds a command to the pipeline and records its index, and whether to ignore the response
    fn add_command(
        &mut self,
        cmd: Arc<Cmd>,
        index: usize,
        inner_index: Option<usize>,
        ignore: bool,
    ) {
        self.pipeline.add_command_with_arc(cmd);
        self.command_indices.push((index, inner_index, ignore));
    }
}

/// `NodeResponse` represents a response from a node along with its source node address.
type NodeResponse = (Option<Arc<str>>, Value);
/// `PipelineResponses` represents the responses for each pipeline command.
pub(crate) type PipelineResponses = Vec<Vec<NodeResponse>>;

/// `AddressAndIndices` represents the address of a node and the indices of commands associated with that node.
type AddressAndIndices = Vec<(Arc<str>, Vec<(usize, Option<usize>, bool)>)>;

pub(crate) type ResponsePoliciesMap =
    HashMap<usize, (MultipleNodeRoutingInfo, Option<ResponsePolicy>)>;

#[allow(clippy::too_many_arguments)]
fn add_command_to_node_pipeline_map<C>(
    pipeline_map: &mut NodePipelineMap<C>,
    address: Arc<str>,
    connection: C,
    cmd: Arc<Cmd>,
    index: usize,
    inner_index: Option<usize>,
    add_asking: bool,
    is_retrying: bool,
) where
    C: Clone,
{
    if is_retrying {
        if let Err(e) = FerrisKeyOtel::record_retry_attempt() {
            log_error(
                "OpenTelemetry:retry_error",
                format!("Failed to record retry attempt: {e}"),
            );
        }
    }
    if add_asking {
        let asking_cmd = Arc::new(crate::cmd::cmd("ASKING"));
        pipeline_map
            .entry(address.clone())
            .or_insert_with(|| NodePipelineContext::new(connection.clone()))
            .add_command(asking_cmd, index, inner_index, true);
    }
    pipeline_map
        .entry(address)
        .or_insert_with(|| NodePipelineContext::new(connection))
        .add_command(cmd, index, inner_index, false);
}

pub(crate) async fn map_pipeline_to_nodes<C>(
    pipeline: &crate::pipeline::Pipeline,
    core: Core<C>,
    route: Option<InternalSingleNodeRouting<C>>,
) -> Result<(NodePipelineMap<C>, ResponsePoliciesMap), (OperationTarget, ValkeyError)>
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    let mut pipelines_per_node = NodePipelineMap::new();
    let mut response_policies = HashMap::new();

    if let Some(route) = route {
        let (addr, conn) = ClusterConnInner::get_connection(route, core, None)
            .await
            .map_err(|err| (OperationTarget::NotFound, err))?;

        let entry = pipelines_per_node
            .entry(Arc::from(&*addr))
            .or_insert_with(|| NodePipelineContext::new(conn));

        for (index, cmd) in pipeline.cmd_iter().enumerate() {
            entry.add_command(cmd.clone(), index, None, false);
        }
    } else {
        for (index, cmd) in pipeline.cmd_iter().enumerate() {
            match RoutingInfo::for_routable(cmd.as_ref())
                .unwrap_or(SingleNode(SingleNodeRoutingInfo::Random))
            {
                SingleNode(route) => {
                    handle_pipeline_single_node_routing(
                        &mut pipelines_per_node,
                        cmd.clone(),
                        route.into(),
                        core.clone(),
                        index,
                    )
                    .await?;
                }
                MultiNode((multi_node_routing, response_policy)) => {
                    response_policies
                        .entry(index)
                        .or_insert((multi_node_routing.clone(), response_policy));
                    match multi_node_routing {
                        MultipleNodeRoutingInfo::AllNodes | MultipleNodeRoutingInfo::AllMasters => {
                            let connections: Vec<_> = {
                                let lock = core.conn_lock.read();
                                if matches!(multi_node_routing, MultipleNodeRoutingInfo::AllNodes) {
                                    lock.all_node_connections().collect()
                                } else {
                                    lock.all_primary_connections().collect()
                                }
                            };

                            if connections.is_empty() {
                                let error_message = if matches!(
                                    multi_node_routing,
                                    MultipleNodeRoutingInfo::AllNodes
                                ) {
                                    "No available connections to any nodes"
                                } else {
                                    "No available connections to primary nodes"
                                };
                                return Err((
                                    OperationTarget::NotFound,
                                    ValkeyError::from((
                                        ErrorKind::AllConnectionsUnavailable,
                                        error_message,
                                    )),
                                ));
                            }
                            for (inner_index, (address, conn)) in
                                connections.into_iter().enumerate()
                            {
                                add_command_to_node_pipeline_map(
                                    &mut pipelines_per_node,
                                    address,
                                    conn.await,
                                    cmd.clone(),
                                    index,
                                    Some(inner_index),
                                    false,
                                    false,
                                );
                            }
                        }
                        MultipleNodeRoutingInfo::MultiSlot((slots, _)) => {
                            handle_pipeline_multi_slot_routing(
                                &mut pipelines_per_node,
                                core.clone(),
                                cmd.clone(),
                                index,
                                slots,
                            )
                            .await?;
                        }
                    }
                }
            }
        }
    }
    Ok((pipelines_per_node, response_policies))
}

async fn handle_pipeline_single_node_routing<C>(
    pipeline_map: &mut NodePipelineMap<C>,
    cmd: Arc<Cmd>,
    routing: InternalSingleNodeRouting<C>,
    core: Core<C>,
    index: usize,
) -> Result<(), (OperationTarget, ValkeyError)>
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    if matches!(routing, InternalSingleNodeRouting::Random) && !pipeline_map.is_empty() {
        let mut rng = rand::rng();
        if let Some(node_context) = pipeline_map.values_mut().choose(&mut rng) {
            node_context.add_command(cmd, index, None, false);
            return Ok(());
        }
    }

    let (address, conn) = ClusterConnInner::get_connection(routing, core, Some(cmd.clone()))
        .await
        .map_err(|err| (OperationTarget::NotFound, err))?;

    add_command_to_node_pipeline_map(pipeline_map, address, conn, cmd, index, None, false, false);
    Ok(())
}

async fn handle_pipeline_multi_slot_routing<C>(
    pipelines_by_connection: &mut NodePipelineMap<C>,
    core: Core<C>,
    cmd: Arc<Cmd>,
    index: usize,
    slots: Vec<(Route, Vec<usize>)>,
) -> Result<(), (OperationTarget, ValkeyError)>
where
    C: Clone,
{
    for (inner_index, (route, indices)) in slots.iter().enumerate() {
        let conn = {
            let lock = core.conn_lock.read();
            lock.connection_for_route(route)
        };
        if let Some((address, conn)) = conn {
            let new_cmd = Arc::new(command_for_multi_slot_indices(cmd.as_ref(), indices.iter()));
            add_command_to_node_pipeline_map(
                pipelines_by_connection,
                address,
                conn.await,
                new_cmd,
                index,
                Some(inner_index),
                false,
                false,
            );
        } else {
            return Err((
                OperationTarget::NotFound,
                ValkeyError::from((
                    ErrorKind::ConnectionNotFoundForRoute,
                    "No available connections for route: ",
                    format!("Slot: {} Slot Address: {}", route.slot(), route.slot_addr()),
                )),
            ));
        }
    }
    Ok(())
}

pub(crate) async fn collect_and_send_pending_requests<C>(
    pipeline_map: NodePipelineMap<C>,
    core: Core<C>,
    retry: u32,
    pipeline_retry_strategy: PipelineRetryStrategy,
) -> (
    Vec<Result<ValkeyResult<Response>, RecvError>>,
    AddressAndIndices,
)
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    let (receivers, pending_requests, addresses_and_indices) =
        collect_pipeline_requests(pipeline_map, retry, pipeline_retry_strategy);

    for request in pending_requests {
        let _ = core.pending_requests_tx.send(request);
    }

    let responses: Vec<_> = futures::future::join_all(receivers.into_iter())
        .await
        .into_iter()
        .collect();

    (responses, addresses_and_indices)
}

#[allow(clippy::type_complexity)]
fn collect_pipeline_requests<C>(
    pipelines_by_connection: NodePipelineMap<C>,
    retry: u32,
    pipeline_retry_strategy: PipelineRetryStrategy,
) -> (
    Vec<oneshot::Receiver<ValkeyResult<Response>>>,
    Vec<PendingRequest<C>>,
    AddressAndIndices,
)
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    let mut receivers = Vec::new();
    let mut pending_requests = Vec::new();
    let mut addresses_and_indices = Vec::new();

    for (address, context) in pipelines_by_connection {
        let (sender, receiver) = oneshot::channel();
        receivers.push(receiver);
        pending_requests.push(PendingRequest {
            retry,
            sender,
            info: RequestInfo {
                cmd: CmdArg::Pipeline {
                    count: context.pipeline.len(),
                    pipeline: context.pipeline.into(),
                    offset: 0,
                    route: Some(InternalSingleNodeRouting::Connection {
                        address: address.clone(),
                        conn: async { context.connection }.boxed().shared(),
                    }),
                    sub_pipeline: true,
                    pipeline_retry_strategy,
                },
            },
        });
        addresses_and_indices.push((address, context.command_indices));
    }

    (receivers, pending_requests, addresses_and_indices)
}

fn add_pipeline_result(
    pipeline_responses: &mut PipelineResponses,
    index: usize,
    inner_index: Option<usize>,
    value: Value,
    address: Arc<str>,
) -> Result<(), (OperationTarget, ValkeyError)> {
    if let Some(responses) = pipeline_responses.get_mut(index) {
        match inner_index {
            Some(inner_index) => {
                if responses.len() <= inner_index {
                    responses.resize(
                        inner_index + 1,
                        (
                            None,
                            Value::ServerError(ServerError::ExtensionError {
                                code: "PipelineNoResponse".to_string(),
                                detail: (Some("no response from node".to_string())),
                            }),
                        ),
                    );
                }
                responses[inner_index] = (Some(address), value);
            }
            None => {
                if responses.is_empty() {
                    responses.push((Some(address), value));
                } else if let Value::ServerError(_) = responses[0].1 {
                    responses[0] = (Some(address), value);
                } else {
                    return Err((
                        OperationTarget::FatalError,
                        ValkeyError::from((
                            ErrorKind::ClientError,
                            "Existing response is not a ServerError; cannot override.",
                        )),
                    ));
                }
            }
        }
        Ok(())
    } else {
        Err((
            OperationTarget::FatalError,
            ValkeyError::from((
                ErrorKind::ClientError,
                "Index not found in pipeline responses",
            )),
        ))
    }
}

type RetryEntry = ((usize, Option<usize>), Arc<str>, ServerError);
type RetryMap = HashMap<RetryMethod, Vec<RetryEntry>>;

fn process_pipeline_responses(
    pipeline_responses: &mut PipelineResponses,
    responses: Vec<Result<ValkeyResult<Response>, RecvError>>,
    addresses_and_indices: AddressAndIndices,
    pipeline_retry_strategy: PipelineRetryStrategy,
) -> Result<RetryMap, (OperationTarget, ValkeyError)> {
    let mut retry_map: RetryMap = HashMap::new();
    for ((address, command_indices), response_result) in
        addresses_and_indices.into_iter().zip(responses)
    {
        let (server_error, retry_method) = match response_result {
            Ok(Ok(Response::Multiple(values))) => {
                for ((index, inner_index, ignore), value) in command_indices.into_iter().zip(values)
                {
                    if let Value::ServerError(error) = &value {
                        let retry_method = ValkeyError::from(error.clone()).retry_method();
                        update_retry_map(
                            &mut retry_map,
                            retry_method,
                            (index, inner_index),
                            address.clone(),
                            error.clone(),
                            pipeline_retry_strategy,
                        );
                    }
                    if !ignore {
                        add_pipeline_result(
                            pipeline_responses,
                            index,
                            inner_index,
                            value,
                            address.clone(),
                        )?;
                    }
                }
                continue;
            }
            Ok(Ok(Response::Single(_))) => (
                ServerError::ExtensionError {
                    code: "SingleResponseError".to_string(),
                    detail: Some(
                        "Received a single response for a pipeline with multiple commands."
                            .to_string(),
                    ),
                },
                RetryMethod::NoRetry,
            ),
            Ok(Ok(Response::ClusterScanResult(_, _))) => (
                ServerError::ExtensionError {
                    code: "ClusterScanError".to_string(),
                    detail: Some("Received a cluster scan result inside a pipeline.".to_string()),
                },
                RetryMethod::NoRetry,
            ),
            Ok(Err(err)) => {
                let retry_method = err.retry_method();
                (err.into(), retry_method)
            }
            Err(err) => (
                ServerError::ExtensionError {
                    code: "BrokenPipe".to_string(),
                    detail: Some(format!(
                        "Cluster: Failed to receive command response from internal sender. {err:?}"
                    )),
                },
                if pipeline_retry_strategy.retry_connection_error {
                    RetryMethod::ReconnectAndRetry
                } else {
                    RetryMethod::Reconnect
                },
            ),
        };

        for (index, inner_index, ignore) in command_indices {
            update_retry_map(
                &mut retry_map,
                retry_method,
                (index, inner_index),
                address.clone(),
                server_error.clone(),
                pipeline_retry_strategy,
            );
            if !ignore {
                add_pipeline_result(
                    pipeline_responses,
                    index,
                    inner_index,
                    Value::ServerError(server_error.clone()),
                    address.clone(),
                )?;
            }
        }
    }
    Ok(retry_map)
}

fn update_retry_map(
    retry_map: &mut RetryMap,
    retry_method: RetryMethod,
    indices: (usize, Option<usize>),
    address: Arc<str>,
    error: ServerError,
    pipeline_retry_strategy: PipelineRetryStrategy,
) {
    let (index, inner_index) = indices;
    match retry_method {
        RetryMethod::NoRetry => {}
        RetryMethod::Reconnect | RetryMethod::ReconnectAndRetry => {
            let effective = if pipeline_retry_strategy.retry_connection_error {
                RetryMethod::ReconnectAndRetry
            } else {
                retry_method
            };
            retry_map
                .entry(effective)
                .or_default()
                .push(((index, inner_index), address, error));
        }
        RetryMethod::AskRedirect | RetryMethod::MovedRedirect => {
            retry_map
                .entry(retry_method)
                .or_default()
                .push(((index, inner_index), address, error));
        }
        RetryMethod::RefreshSlotsAndRetry => {}
        RetryMethod::RetryImmediately
        | RetryMethod::WaitAndRetry
        | RetryMethod::WaitAndRetryOnPrimaryRedirectOnReplica => {
            if pipeline_retry_strategy.retry_server_error {
                retry_map.entry(retry_method).or_default().push((
                    (index, inner_index),
                    address,
                    error,
                ));
            }
        }
    }
}

pub(crate) async fn process_and_retry_pipeline_responses<C>(
    mut responses: Vec<Result<ValkeyResult<Response>, RecvError>>,
    mut addresses_and_indices: AddressAndIndices,
    pipeline: &crate::pipeline::Pipeline,
    core: Core<C>,
    response_policies: &mut ResponsePoliciesMap,
    pipeline_retry_strategy: PipelineRetryStrategy,
) -> Result<PipelineResponses, (OperationTarget, ValkeyError)>
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    let retry_params = core.get_cluster_param(|params| params.retry_params.clone());
    let mut retry = 0;
    let mut pipeline_responses: PipelineResponses = vec![Vec::new(); pipeline.len()];
    loop {
        match process_pipeline_responses(
            &mut pipeline_responses,
            responses,
            addresses_and_indices,
            pipeline_retry_strategy,
        ) {
            Ok(retry_map) => {
                if retry_map.is_empty() || retry >= retry_params.number_of_retries {
                    return Ok(pipeline_responses);
                }
                retry = retry.saturating_add(1);
                match handle_retry_map(
                    retry_map,
                    core.clone(),
                    pipeline,
                    retry,
                    &mut pipeline_responses,
                    response_policies,
                    pipeline_retry_strategy,
                )
                .await
                {
                    Ok((r, a)) => {
                        responses = r;
                        addresses_and_indices = a;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }
}

async fn handle_retry_map<C>(
    retry_map: RetryMap,
    core: Core<C>,
    pipeline: &crate::pipeline::Pipeline,
    retry: u32,
    pipeline_responses: &mut PipelineResponses,
    response_policies: &mut ResponsePoliciesMap,
    pipeline_retry_strategy: PipelineRetryStrategy,
) -> Result<
    (
        Vec<Result<ValkeyResult<Response>, RecvError>>,
        AddressAndIndices,
    ),
    (OperationTarget, ValkeyError),
>
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    let mut pipeline_map = NodePipelineMap::new();
    for (retry_method, entries) in retry_map {
        match retry_method {
            RetryMethod::NoRetry | RetryMethod::RefreshSlotsAndRetry => {}
            RetryMethod::Reconnect | RetryMethod::ReconnectAndRetry => {
                handle_reconnect_logic(
                    entries,
                    core.clone(),
                    pipeline,
                    pipeline_responses,
                    matches!(retry_method, RetryMethod::ReconnectAndRetry),
                    &mut pipeline_map,
                    response_policies,
                )
                .await?;
            }
            RetryMethod::RetryImmediately
            | RetryMethod::WaitAndRetry
            | RetryMethod::WaitAndRetryOnPrimaryRedirectOnReplica => {
                handle_retry_logic(
                    retry_method,
                    retry,
                    core.clone(),
                    entries,
                    pipeline,
                    pipeline_responses,
                    &mut pipeline_map,
                    response_policies,
                )
                .await?;
            }
            RetryMethod::MovedRedirect | RetryMethod::AskRedirect => {
                handle_redirect_logic(
                    retry_method,
                    core.clone(),
                    pipeline,
                    entries,
                    pipeline_responses,
                    &mut pipeline_map,
                    response_policies,
                )
                .await?;
            }
        }
    }
    Ok(collect_and_send_pending_requests(pipeline_map, core, retry, pipeline_retry_strategy).await)
}

async fn handle_reconnect_logic<C>(
    entries: Vec<RetryEntry>,
    core: Core<C>,
    pipeline: &Pipeline,
    pipeline_responses: &mut PipelineResponses,
    should_retry: bool,
    pipeline_map: &mut NodePipelineMap<C>,
    response_policies: &mut ResponsePoliciesMap,
) -> Result<(), (OperationTarget, ValkeyError)>
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    let addresses: HashSet<Arc<str>> = entries.iter().map(|(_, a, _)| a.clone()).collect();
    if should_retry {
        ClusterConnInner::refresh_and_update_connections(
            core.clone(),
            addresses,
            RefreshConnectionType::OnlyUserConnection,
            true,
        )
        .await;
        append_commands_to_retry(
            pipeline_map,
            pipeline,
            core.clone(),
            entries,
            pipeline_responses,
            response_policies,
        )
        .await?;
    } else {
        ClusterConnInner::trigger_refresh_connection_tasks(
            core.clone(),
            addresses.clone(),
            RefreshConnectionType::OnlyUserConnection,
            true,
        )
        .await;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn handle_retry_logic<C>(
    retry_method: RetryMethod,
    retry: u32,
    core: Core<C>,
    entries: Vec<RetryEntry>,
    pipeline: &Pipeline,
    pipeline_responses: &mut PipelineResponses,
    pipeline_map: &mut NodePipelineMap<C>,
    response_policies: &mut ResponsePoliciesMap,
) -> Result<(), (OperationTarget, ValkeyError)>
where
    C: Clone + Sync + ConnectionLike + Send + Connect + 'static,
{
    let retry_params = core.get_cluster_param(|params| params.retry_params.clone());
    if matches!(retry_method, RetryMethod::WaitAndRetry) {
        boxed_sleep(retry_params.wait_time_for_retry(retry)).await;
    } else if matches!(
        retry_method,
        RetryMethod::WaitAndRetryOnPrimaryRedirectOnReplica
    ) {
        let futures = entries
            .iter()
            .fold(HashSet::new(), |mut set, (_, a, _)| {
                set.insert(a.to_string());
                set
            })
            .into_iter()
            .map(|address| {
                ClusterConnInner::handle_loading_error(
                    core.clone(),
                    address,
                    retry,
                    retry_params.clone(),
                )
            });
        futures::future::join_all(futures).await;
    }
    append_commands_to_retry(
        pipeline_map,
        pipeline,
        core,
        entries,
        pipeline_responses,
        response_policies,
    )
    .await?;
    Ok(())
}

async fn handle_redirect_logic<C>(
    retry_method: RetryMethod,
    core: Core<C>,
    pipeline: &Pipeline,
    entries: Vec<RetryEntry>,
    pipeline_responses: &mut PipelineResponses,
    pipeline_map: &mut NodePipelineMap<C>,
    response_policies: &mut ResponsePoliciesMap,
) -> Result<(), (OperationTarget, ValkeyError)>
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    for (indices, address, mut error) in entries {
        let valkey_error: ValkeyError = error.clone().into();
        let (index, inner_index) = indices;

        if matches!(retry_method, RetryMethod::MovedRedirect)
            && let Err(server_error) =
                pipeline_handle_moved_redirect(core.clone(), &valkey_error).await
        {
            error.append_detail(&server_error);
            add_pipeline_result(
                pipeline_responses,
                index,
                inner_index,
                Value::ServerError(error),
                address,
            )?;
            continue;
        }

        if let Some(redirect_info) = valkey_error.redirect(false) {
            let routing = InternalSingleNodeRouting::Redirect {
                redirect: redirect_info,
                previous_routing: Box::new(InternalSingleNodeRouting::ByAddress(
                    address.to_string(),
                )),
            };
            match get_original_cmd(pipeline, index, inner_index, Some(response_policies)) {
                Ok(cmd) => {
                    match ClusterConnInner::get_connection(routing, core.clone(), Some(cmd.clone()))
                        .await
                    {
                        Ok((addr, conn)) => {
                            add_command_to_node_pipeline_map(
                                pipeline_map,
                                addr,
                                conn,
                                cmd,
                                index,
                                inner_index,
                                matches!(retry_method, RetryMethod::AskRedirect),
                                true,
                            );
                            continue;
                        }
                        Err(err) => error.append_detail(&err.into()),
                    }
                }
                Err(cmd_err) => error.append_detail(&cmd_err),
            }
        } else {
            error.append_detail(&ServerError::ExtensionError {
                code: "RedirectError".to_string(),
                detail: Some("Failed to find redirect info".to_string()),
            });
        }

        add_pipeline_result(
            pipeline_responses,
            index,
            inner_index,
            Value::ServerError(error),
            address,
        )?;
    }
    Ok(())
}

async fn pipeline_handle_moved_redirect<C>(
    core: Core<C>,
    valkey_error: &ValkeyError,
) -> Result<(), ServerError>
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    let redirect_node =
        RedirectNode::from_option_tuple(valkey_error.redirect_node()).ok_or_else(|| {
            ServerError::ExtensionError {
                code: "ParsingError".to_string(),
                detail: Some("Failed to parse MOVED error".to_string()),
            }
        })?;
    ClusterConnInner::update_upon_moved_error(
        core.clone(),
        redirect_node.slot,
        redirect_node.address.into(),
    )
    .await
    .map_err(Into::into)
}

async fn append_commands_to_retry<C>(
    pipeline_map: &mut NodePipelineMap<C>,
    pipeline: &crate::pipeline::Pipeline,
    core: Core<C>,
    entries: Vec<RetryEntry>,
    pipeline_responses: &mut PipelineResponses,
    response_policies: &mut ResponsePoliciesMap,
) -> Result<(), (OperationTarget, ValkeyError)>
where
    C: Clone + ConnectionLike + Connect + Send + Sync + 'static,
{
    for ((index, inner_index), address, mut error) in entries {
        let cmd = match get_original_cmd(pipeline, index, inner_index, Some(response_policies)) {
            Ok(cmd) => cmd,
            Err(server_error) => {
                error.append_detail(&server_error);
                add_pipeline_result(
                    pipeline_responses,
                    index,
                    inner_index,
                    Value::ServerError(error),
                    address.clone(),
                )?;
                continue;
            }
        };
        let routing = InternalSingleNodeRouting::ByAddress(address.to_string());
        match ClusterConnInner::get_connection(routing, core.clone(), None).await {
            Ok((addr, conn)) => {
                add_command_to_node_pipeline_map(
                    pipeline_map,
                    addr,
                    conn,
                    cmd,
                    index,
                    inner_index,
                    false,
                    true,
                );
            }
            Err(valkey_error) => {
                error.append_detail(&valkey_error.into());
                add_pipeline_result(
                    pipeline_responses,
                    index,
                    inner_index,
                    Value::ServerError(error),
                    address,
                )?;
            }
        }
    }
    Ok(())
}

fn get_original_cmd(
    pipeline: &crate::pipeline::Pipeline,
    index: usize,
    inner_index: Option<usize>,
    response_policies: Option<&ResponsePoliciesMap>,
) -> Result<Arc<Cmd>, ServerError> {
    let cmd = pipeline
        .get_command(index)
        .ok_or_else(|| ServerError::ExtensionError {
            code: "IndexNotFoundInPipelineResponses".to_string(),
            detail: Some(format!("Index {index} was not found in pipeline")),
        })?;
    if inner_index.is_some() {
        let routing_info = response_policies.and_then(|map| map.get(&index).map(|t| t.0.clone()));
        if let Some(MultipleNodeRoutingInfo::MultiSlot((slots, _))) = routing_info {
            let inner_index = inner_index.ok_or_else(|| ServerError::ExtensionError {
                code: "IndexNotFoundInPipelineResponses".to_string(),
                detail: Some(format!(
                    "Inner index is required for a multi-slot command: {cmd:?}"
                )),
            })?;
            let indices = slots.get(inner_index).ok_or_else(|| ServerError::ExtensionError {
                code: "IndexNotFoundInPipelineResponses".to_string(),
                detail: Some(format!("Inner index {inner_index} for multi-slot command {cmd:?} was not found in command slots {slots:?}")),
            })?;
            return Ok(command_for_multi_slot_indices(cmd.as_ref(), indices.1.iter()).into());
        }
    }
    Ok(cmd)
}

pub(crate) fn route_for_pipeline(
    pipeline: &crate::pipeline::Pipeline,
) -> ValkeyResult<Option<Route>> {
    fn route_for_command(cmd: &Cmd) -> Option<Route> {
        match cluster_routing::RoutingInfo::for_routable(cmd) {
            Some(cluster_routing::RoutingInfo::SingleNode(SingleNodeRoutingInfo::Random)) => None,
            Some(cluster_routing::RoutingInfo::SingleNode(
                SingleNodeRoutingInfo::SpecificNode(route),
            )) => Some(route),
            Some(cluster_routing::RoutingInfo::SingleNode(
                SingleNodeRoutingInfo::RandomPrimary,
            )) => Some(Route::new_random_primary()),
            Some(cluster_routing::RoutingInfo::MultiNode(_)) => None,
            Some(cluster_routing::RoutingInfo::SingleNode(SingleNodeRoutingInfo::ByAddress {
                ..
            })) => None,
            None => None,
        }
    }
    if pipeline.is_atomic() {
        pipeline
            .cmd_iter()
            .map(|cmd| route_for_command(cmd.as_ref()))
            .try_fold(None, |chosen_route, next_cmd_route| {
                match (chosen_route, next_cmd_route) {
                    (None, _) => Ok(next_cmd_route),
                    (_, None) => Ok(chosen_route),
                    (Some(chosen_route), Some(next_cmd_route)) => {
                        if chosen_route.slot() != next_cmd_route.slot() {
                            Err((ErrorKind::CrossSlot, "Received crossed slots in pipeline").into())
                        } else if chosen_route.slot_addr() == SlotAddr::ReplicaOptional {
                            Ok(Some(next_cmd_route))
                        } else {
                            Ok(Some(chosen_route))
                        }
                    }
                }
            })
    } else {
        Ok(None)
    }
}
