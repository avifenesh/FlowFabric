//! `EngineBackend` implementation backed by Valkey FCALL.
//! See RFC-012 §5.1 for the migration plan.
//!
//! **RFC-012 Stage 1a:** this crate lands the [`ValkeyBackend`] struct
//! and the `impl EngineBackend for ValkeyBackend` block. The hot-path
//! methods (`claim`, `renew`, `complete`, `fail`, …) return
//! [`EngineError::Unavailable`] at this stage; hot-path wiring lands
//! across Stages 1b-1d (see issue #89 migration plan). The one
//! method implemented in Stage 1a is [`cancel_flow`], whose thin
//! FCALL wrapper already exists in `ff-script::functions::flow` —
//! this crate wires it up to the trait's `CancelFlowPolicy` /
//! `CancelFlowWait` types.
//!
//! The `EngineBackend` trait stays object-safe; consumers can hold
//! `Arc<dyn EngineBackend>`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use ff_core::backend::{
    AdmissionDecision, BackendConfig, BackendConnection, CancelFlowPolicy, CancelFlowWait,
    CapabilitySet, ClaimPolicy, FailOutcome, FailureClass, FailureReason, Frame, Handle,
    LeaseRenewal, ReclaimToken, ResumeSignal, UsageDimensions, WaitpointSpec,
};
use ff_core::contracts::{CancelFlowArgs, CancelFlowResult, ExecutionSnapshot, FlowSnapshot};
use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;
use ff_core::keys::{FlowIndexKeys, FlowKeyContext};
use ff_core::partition::{flow_partition, PartitionConfig};
use ff_core::types::{BudgetId, ExecutionId, FlowId, LaneId, TimestampMs};
use ff_script::engine_error_ext::transport_script;
use ff_script::error::ScriptError;
use ff_script::functions::flow::{ff_cancel_flow, FlowStructOpKeys};

/// Valkey-FCALL–backed `EngineBackend`.
///
/// Holds a shared [`ferriskey::Client`] + the partition config the
/// Lua functions need to route keys. Construction goes through
/// [`ValkeyBackend::connect`], which dials Valkey (standalone or
/// cluster per [`ValkeyConnection::cluster`]) and loads the
/// deployment's partition counts from `ff:config:partitions` so
/// key routing aligns with ff-server. Consumers interacting with
/// the trait never see `ferriskey::Client` directly (RFC-012 §1.3
/// — trait-ifying the write surface removes the ferriskey leak
/// from the SDK's public API).
///
/// [`ValkeyConnection::cluster`]: ff_core::backend::ValkeyConnection::cluster
pub struct ValkeyBackend {
    client: ferriskey::Client,
    partition_config: PartitionConfig,
}

impl ValkeyBackend {
    /// Dial a Valkey node with [`BackendConfig`] and return the
    /// backend as `Arc<dyn EngineBackend>`. The returned handle is
    /// `Send + Sync + 'static` so it can be stored on long-lived
    /// worker structs.
    ///
    /// **Stage 1a scope:** this constructor exists so ff-sdk's new
    /// `FlowFabricWorker::connect_with(backend)` path has something
    /// to hand in. The Valkey dial currently delegates to ferriskey
    /// with host/port from [`BackendConnection::Valkey`]; TLS and
    /// cluster flags flow through unchanged. Full
    /// `BackendTimeouts`/`BackendRetry` wiring is a Stage 1c task.
    pub async fn connect(config: BackendConfig) -> Result<Arc<dyn EngineBackend>, EngineError> {
        // `BackendConnection` is `#[non_exhaustive]` for future
        // backends; the compiler treats the pattern as refutable,
        // hence `let ... else`. Today only `Valkey` exists; a
        // non-Valkey BackendConnection handed to `ValkeyBackend`
        // surfaces as `EngineError::Unavailable` so callers get a
        // typed error rather than a panic.
        let BackendConnection::Valkey(v) = config.connection else {
            return Err(EngineError::Unavailable {
                op: "ValkeyBackend::connect (non-Valkey BackendConnection)",
            });
        };
        let url = if v.tls {
            format!("valkeys://{}:{}", v.host, v.port)
        } else {
            format!("valkey://{}:{}", v.host, v.port)
        };
        // Honour `v.cluster`. `Client::connect` dials standalone;
        // `Client::connect_cluster` discovers the cluster topology
        // and routes by slot — required for any non-default
        // partition deployment the server has published.
        let client = if v.cluster {
            ferriskey::Client::connect_cluster(&[url.as_str()])
                .await
                .map_err(|e| transport_script(ScriptError::Valkey(e)))?
        } else {
            ferriskey::Client::connect(&url)
                .await
                .map_err(|e| transport_script(ScriptError::Valkey(e)))?
        };
        // Load the deployment's partition config from
        // `ff:config:partitions` so `flow_partition` / key routing
        // aligns with what ff-server published. Using
        // `PartitionConfig::default()` (256/32/32) would silently
        // mis-route keys on any non-default deployment; Copilot
        // review comment on PR #114 flagged this as a correctness
        // bug. Mirrors `ff_sdk::worker::read_partition_config`'s
        // warn-and-default behaviour when the hash is missing (e.g.
        // SDK-only tests where ff-server never wrote the hash);
        // transport-level errors propagate so operators notice
        // connectivity issues.
        let partition_config = match load_partition_config(&client).await {
            Ok(cfg) => cfg,
            Err(EngineError::Transport { source, .. })
                if matches!(
                    source.downcast_ref::<ScriptError>(),
                    Some(ScriptError::Parse { .. })
                ) =>
            {
                tracing::warn!(
                    error = %source,
                    "ff:config:partitions not found, using PartitionConfig::default()"
                );
                PartitionConfig::default()
            }
            Err(e) => return Err(e),
        };
        Ok(Arc::new(Self {
            client,
            partition_config,
        }))
    }

    /// Borrow the underlying `ferriskey::Client`. Backend-internal
    /// use; call sites outside this crate should route through the
    /// trait rather than reach in here.
    pub fn client(&self) -> &ferriskey::Client {
        &self.client
    }
}

/// Map [`CancelFlowPolicy`] to the Lua-side policy string.
fn cancel_policy_to_str(p: CancelFlowPolicy) -> &'static str {
    match p {
        CancelFlowPolicy::FlowOnly => "flow_only",
        CancelFlowPolicy::CancelAll => "cancel_all",
        CancelFlowPolicy::CancelPending => "cancel_pending",
        // `CancelFlowPolicy` is `#[non_exhaustive]`. Fall back to
        // the least-destructive recognised policy (`flow_only`) so a
        // newly-added variant does NOT silently widen the cancel
        // scope. Widening defaults lose work; narrowing defaults
        // are safely retryable by the caller via an explicit policy.
        // Follow-up PRs that add variants must still update this
        // match explicitly.
        _ => "flow_only",
    }
}

/// Stage 1a cancel-flow FCALL wrapper. Only
/// [`CancelFlowWait::NoWait`] is supported at Stage 1a — the
/// dispatch+wait loop that [`CancelFlowWait::WaitTimeout`] /
/// [`CancelFlowWait::WaitIndefinite`] require lands in a
/// follow-up stage (today's ff-sdk cancel_flow HTTP path does the
/// wait client-side after the FCALL commits). Rejecting the
/// wait modes explicitly with [`EngineError::Unavailable`] lets
/// callers distinguish "backend won't do this yet" from a silent
/// fallback. See RFC-012 §3.1.1 for the cancel_flow policy matrix.
async fn cancel_flow_fcall(
    client: &ferriskey::Client,
    partition_config: &PartitionConfig,
    flow_id: &FlowId,
    policy: CancelFlowPolicy,
    wait: CancelFlowWait,
) -> Result<CancelFlowResult, EngineError> {
    match wait {
        CancelFlowWait::NoWait => {}
        CancelFlowWait::WaitTimeout(_) => {
            return Err(EngineError::Unavailable {
                op: "cancel_flow(wait=WaitTimeout)",
            });
        }
        CancelFlowWait::WaitIndefinite => {
            return Err(EngineError::Unavailable {
                op: "cancel_flow(wait=WaitIndefinite)",
            });
        }
        // `CancelFlowWait` is `#[non_exhaustive]`. Future wait
        // variants must be reviewed here explicitly; fall closed
        // with Unavailable so callers see a typed error instead of
        // silent fallback to NoWait.
        _ => {
            return Err(EngineError::Unavailable {
                op: "cancel_flow(wait=unknown)",
            });
        }
    }
    let partition = flow_partition(flow_id, partition_config);
    let fctx = FlowKeyContext::new(&partition, flow_id);
    let fidx = FlowIndexKeys::new(&partition);
    let keys = FlowStructOpKeys {
        fctx: &fctx,
        fidx: &fidx,
    };
    let now = now_ms_timestamp();
    let args = CancelFlowArgs {
        flow_id: flow_id.clone(),
        reason: String::new(),
        cancellation_policy: cancel_policy_to_str(policy).to_string(),
        now,
    };
    ff_cancel_flow(client, &keys, &args)
        .await
        .map_err(EngineError::from)
}

/// Read the deployment's partition config from
/// `ff:config:partitions`. Keeps `ValkeyBackend` aligned with
/// ff-server's published `num_flow_partitions` / budget / quota
/// counts. Mirrors the ff-sdk `worker::read_partition_config` helper
/// (Stage 1c will deduplicate once the hot-path migration lands).
async fn load_partition_config(
    client: &ferriskey::Client,
) -> Result<PartitionConfig, EngineError> {
    let key = ff_core::keys::global_config_partitions();
    let fields: HashMap<String, String> = client
        .hgetall(&key)
        .await
        .map_err(|e| transport_script(ScriptError::Valkey(e)))?;
    if fields.is_empty() {
        // Distinct Err so `connect()` can warn-and-default instead
        // of silently routing with the wrong partition counts.
        // Mirrors `ff-sdk::worker::read_partition_config`'s
        // error-on-missing + warn-at-call-site pattern (#111).
        return Err(transport_script(ScriptError::Parse {
            fcall: "load_partition_config".into(),
            execution_id: None,
            message: format!("{key} not found in Valkey"),
        }));
    }
    let parse = |field: &str, default: u16| -> u16 {
        fields
            .get(field)
            .and_then(|v| v.parse().ok())
            .filter(|&n: &u16| n > 0)
            .unwrap_or(default)
    };
    Ok(PartitionConfig {
        num_flow_partitions: parse("num_flow_partitions", 256),
        num_budget_partitions: parse("num_budget_partitions", 32),
        num_quota_partitions: parse("num_quota_partitions", 32),
    })
}

fn now_ms_timestamp() -> TimestampMs {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    TimestampMs::from_millis(now)
}

#[async_trait]
impl EngineBackend for ValkeyBackend {
    async fn claim(
        &self,
        _lane: &LaneId,
        _capabilities: &CapabilitySet,
        _policy: ClaimPolicy,
    ) -> Result<Option<Handle>, EngineError> {
        Err(EngineError::Unavailable { op: "claim" })
    }

    async fn renew(&self, _handle: &Handle) -> Result<LeaseRenewal, EngineError> {
        Err(EngineError::Unavailable { op: "renew" })
    }

    async fn progress(
        &self,
        _handle: &Handle,
        _percent: Option<u8>,
        _message: Option<String>,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable { op: "progress" })
    }

    async fn append_frame(&self, _handle: &Handle, _frame: Frame) -> Result<(), EngineError> {
        Err(EngineError::Unavailable { op: "append_frame" })
    }

    async fn complete(
        &self,
        _handle: &Handle,
        _payload: Option<Vec<u8>>,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable { op: "complete" })
    }

    async fn fail(
        &self,
        _handle: &Handle,
        _reason: FailureReason,
        _classification: FailureClass,
    ) -> Result<FailOutcome, EngineError> {
        Err(EngineError::Unavailable { op: "fail" })
    }

    async fn cancel(&self, _handle: &Handle, _reason: &str) -> Result<(), EngineError> {
        Err(EngineError::Unavailable { op: "cancel" })
    }

    async fn suspend(
        &self,
        _handle: &Handle,
        _waitpoints: Vec<WaitpointSpec>,
        _timeout: Option<Duration>,
    ) -> Result<Handle, EngineError> {
        Err(EngineError::Unavailable { op: "suspend" })
    }

    async fn observe_signals(
        &self,
        _handle: &Handle,
    ) -> Result<Vec<ResumeSignal>, EngineError> {
        Err(EngineError::Unavailable {
            op: "observe_signals",
        })
    }

    async fn claim_from_reclaim(
        &self,
        _token: ReclaimToken,
    ) -> Result<Option<Handle>, EngineError> {
        Err(EngineError::Unavailable {
            op: "claim_from_reclaim",
        })
    }

    async fn delay(
        &self,
        _handle: &Handle,
        _delay_until: TimestampMs,
    ) -> Result<(), EngineError> {
        Err(EngineError::Unavailable { op: "delay" })
    }

    async fn wait_children(&self, _handle: &Handle) -> Result<(), EngineError> {
        Err(EngineError::Unavailable {
            op: "wait_children",
        })
    }

    async fn describe_execution(
        &self,
        _id: &ExecutionId,
    ) -> Result<Option<ExecutionSnapshot>, EngineError> {
        Err(EngineError::Unavailable {
            op: "describe_execution",
        })
    }

    async fn describe_flow(
        &self,
        _id: &FlowId,
    ) -> Result<Option<FlowSnapshot>, EngineError> {
        Err(EngineError::Unavailable {
            op: "describe_flow",
        })
    }

    async fn cancel_flow(
        &self,
        id: &FlowId,
        policy: CancelFlowPolicy,
        wait: CancelFlowWait,
    ) -> Result<CancelFlowResult, EngineError> {
        cancel_flow_fcall(&self.client, &self.partition_config, id, policy, wait).await
    }

    async fn report_usage(
        &self,
        _handle: &Handle,
        _budget: &BudgetId,
        _dimensions: UsageDimensions,
    ) -> Result<AdmissionDecision, EngineError> {
        Err(EngineError::Unavailable {
            op: "report_usage",
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_policy_strings() {
        assert_eq!(cancel_policy_to_str(CancelFlowPolicy::FlowOnly), "flow_only");
        assert_eq!(
            cancel_policy_to_str(CancelFlowPolicy::CancelAll),
            "cancel_all"
        );
        assert_eq!(
            cancel_policy_to_str(CancelFlowPolicy::CancelPending),
            "cancel_pending"
        );
    }

    #[test]
    fn backend_config_valkey_shape() {
        let c = BackendConfig::valkey("localhost", 6379);
        // `BackendConnection` is `#[non_exhaustive]`; `if let`
        // matches the Valkey arm without tripping the
        // exhaustive-match / unreachable-pattern pair.
        let BackendConnection::Valkey(v) = &c.connection else {
            panic!("BackendConfig::valkey produced a non-Valkey connection");
        };
        assert_eq!(v.host, "localhost");
        assert_eq!(v.port, 6379);
    }

    // Dyn-safety smoke test: `Arc<dyn EngineBackend>` must hold for
    // ValkeyBackend. If a trait change breaks dyn-safety this fails
    // at compile time.
    #[allow(dead_code)]
    fn _dyn_compatible(b: Arc<ValkeyBackend>) -> Arc<dyn EngineBackend> {
        b
    }
}
