//! Built-in extractors.
//!
//! Each implements [`FromTask`] and plugs into the handler-tuple
//! macro in `handler.rs`. Kept minimal — more extractors ship when
//! concrete consumer asks surface (see issue #331 scope reductions).

use std::collections::HashMap;

use ff_core::types::{AttemptIndex, ExecutionId, LaneId};
use serde::de::DeserializeOwned;

use super::ctx::RuntimeCtx;
use super::RuntimeError;

/// Extractor trait. Implemented by types that can be pulled out of a
/// [`RuntimeCtx`] before the handler body runs.
///
/// Failure semantics: `Err(RuntimeError)` aborts the handler call and
/// fails the task with the variant's classified category (see
/// [`RuntimeError::error_category`]). There is no "skip" arm — that
/// would hide contract violations.
pub trait FromTask: Sized + Send + 'static {
    fn from_task(ctx: &RuntimeCtx<'_>) -> Result<Self, RuntimeError>;
}

/// Input payload decoded as `P` via `serde_json`.
///
/// Producers set the payload at `create_execution` time; the bytes
/// are whatever the producer wrote. `Payload<P>` assumes JSON. If the
/// payload isn't JSON, use [`TaskInfo`] + `PayloadBytes` or drop to
/// the lower-level [`crate::ClaimedTask::input_payload`] API.
pub struct Payload<P: DeserializeOwned + Send + 'static>(pub P);

impl<P: DeserializeOwned + Send + 'static> FromTask for Payload<P> {
    fn from_task(ctx: &RuntimeCtx<'_>) -> Result<Self, RuntimeError> {
        let bytes = ctx.task().input_payload();
        let p: P = serde_json::from_slice(bytes).map_err(RuntimeError::PayloadDecode)?;
        Ok(Payload(p))
    }
}

/// Raw input payload as bytes. Use when the payload isn't JSON, or
/// when you want to decode it yourself with a different codec.
pub struct PayloadBytes(pub Vec<u8>);

impl FromTask for PayloadBytes {
    fn from_task(ctx: &RuntimeCtx<'_>) -> Result<Self, RuntimeError> {
        Ok(PayloadBytes(ctx.task().input_payload().to_vec()))
    }
}

/// Clone a typed state value registered via
/// [`super::WorkerRuntime::data`]. Returns
/// [`RuntimeError::MissingState`] if no value of this type has been
/// registered — the task fails with
/// `error_category = "extractor_missing_state"`.
///
/// Wrap shared handles you do not want to clone (e.g. a reqwest
/// client, a DB pool) in `Arc<_>` before handing them to
/// `.data(Arc::new(client))`; `Data<Arc<T>>` then clones the Arc
/// cheaply per task.
pub struct Data<T: Clone + Send + Sync + 'static>(pub T);

impl<T: Clone + Send + Sync + 'static> FromTask for Data<T> {
    fn from_task(ctx: &RuntimeCtx<'_>) -> Result<Self, RuntimeError> {
        ctx.state()
            .get::<T>()
            .map(Data)
            .ok_or(RuntimeError::MissingState {
                type_name: std::any::type_name::<T>(),
            })
    }
}

/// Owned snapshot of the task's immutable metadata. Cheap to clone
/// out of the ctx; keeps the extractor lifetime-free (unlike a
/// borrowed `&ClaimedTask`, which would force GAT or HRTB plumbing
/// through the handler-tuple macro for a marginal benefit).
///
/// If a handler needs the richer [`crate::ClaimedTask`] surface
/// (stream helpers, progress updates, lease-health introspection), it
/// should use the lower-level
/// [`crate::FlowFabricWorker::claim_next_via_backend`] loop instead of
/// [`super::WorkerRuntime`].
#[derive(Clone, Debug)]
pub struct TaskInfo {
    pub execution_id: ExecutionId,
    pub attempt_index: AttemptIndex,
    pub execution_kind: String,
    pub lane_id: LaneId,
    pub tags: HashMap<String, String>,
}

impl FromTask for TaskInfo {
    fn from_task(ctx: &RuntimeCtx<'_>) -> Result<Self, RuntimeError> {
        let t = ctx.task();
        Ok(TaskInfo {
            execution_id: t.execution_id().clone(),
            attempt_index: t.attempt_index(),
            execution_kind: t.execution_kind().to_owned(),
            lane_id: t.lane_id().clone(),
            tags: t.tags().clone(),
        })
    }
}
