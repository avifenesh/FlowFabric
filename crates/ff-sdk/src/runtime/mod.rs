//! Handler-DI runtime on top of [`FlowFabricWorker`] (issue #331).
//!
//! Closes the DX gap surfaced by the apalis comparison: every consumer
//! service otherwise reinvents the same four pieces of plumbing around
//! `claim_next_via_backend` — idle-backoff, bounded spawn, panic catch,
//! and shared-state threading to handler fns. [`WorkerRuntime`] bundles
//! those once and exposes a typed handler registration surface.
//!
//! ```no_run
//! use ff_sdk::runtime::{Data, Payload, WorkerRuntime};
//! use std::sync::Arc;
//!
//! # async fn example() -> Result<(), ff_sdk::SdkError> {
//! # let worker: ff_sdk::FlowFabricWorker = unimplemented!();
//! #[derive(serde::Deserialize)]
//! struct Job { url: String }
//!
//! #[derive(Clone)]
//! struct HttpClient; // actual client
//!
//! async fn handle(
//!     Payload(job): Payload<Job>,
//!     Data(http): Data<Arc<HttpClient>>,
//! ) -> ff_sdk::runtime::HandlerResult {
//!     let _ = http; let _ = job.url;
//!     Ok(None)
//! }
//!
//! WorkerRuntime::new(worker)
//!     .data(Arc::new(HttpClient))
//!     .on("fetch", handle)
//!     .run()
//!     .await
//! # }
//! ```
//!
//! # Non-goals
//!
//! - No engine, Lua, or backend-trait change.
//! - The imperative `claim_next_via_backend` + [`crate::ClaimedTask`]
//!   loop stays as the lower-level API forever — [`WorkerRuntime`] is
//!   strictly opt-in on top of it.
//! - No return-shape variants beyond [`HandlerResult`] (which is
//!   `Result<Option<Vec<u8>>, Box<dyn std::error::Error + Send + Sync>>`).
//!   A handler returns `Ok(None)` → `complete(None)`;
//!   `Ok(Some(bytes))` → `complete(Some(bytes))`;
//!   `Err(e)` → `fail(e.to_string(), "handler_error")`.
//!
//! # Behaviour decisions (archived RFC §9)
//!
//! - **Missing state:** extractor resolution happens per-claim; a
//!   [`Data<T>`] for an un-registered `T` returns
//!   [`RuntimeError::MissingState`] and the task is failed with
//!   `error_category = "extractor_missing_state"`. Loud failure beats
//!   silent skip.
//! - **Extractor failure:** fails the task with a classified error;
//!   never skips. Lease is released via [`crate::ClaimedTask::fail`].
//! - **Spawn policy:** one `tokio::spawn` per claimed task, bounded by
//!   a semaphore (default `max_concurrent = 64`). No user-injectable
//!   spawn fn in v1.
//! - **Handler return:** [`HandlerResult`] (trait-object error type;
//!   takes `anyhow::Error`, `thiserror` enums, or `Box<dyn Error>`).
//!   `Err(e)` → `fail(e.to_string(), "handler_error")`; panic caught
//!   in-task → `fail("handler panicked", "handler_panic")`.
//! - **Feature flag:** `runtime`. Off by default.

mod ctx;
mod extract;
mod handler;
mod registry;

pub use ctx::RuntimeCtx;
pub use extract::{Data, FromTask, Payload, PayloadBytes, TaskInfo};
pub use handler::{BoxedHandler, Handler, HandlerResult};
pub use registry::StateMap;

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Semaphore;
use tracing::{debug, error, info, warn};

use crate::{ClaimedTask, FlowFabricWorker, SdkError};

/// Errors raised by the runtime itself (distinct from [`SdkError`]).
///
/// These are not returned to the caller of [`WorkerRuntime::run`] —
/// they become `fail(..)` classifications on the claimed task so the
/// engine sees a proper terminal failure, not a dropped lease.
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    /// A [`Data<T>`] extractor asked for a state type that was never
    /// registered via [`WorkerRuntime::data`]. The task fails with
    /// `error_category = "extractor_missing_state"`.
    #[error("extractor: missing state of type {type_name}")]
    MissingState { type_name: &'static str },

    /// A [`Payload<P>`] extractor failed to deserialize the input
    /// bytes. The task fails with
    /// `error_category = "extractor_payload_decode"`.
    #[error("extractor: payload decode failed: {0}")]
    PayloadDecode(serde_json::Error),

    /// No handler registered for the claimed task's `execution_kind`.
    /// The task fails with `error_category = "runtime_no_handler"`.
    #[error("runtime: no handler registered for execution_kind {kind:?}")]
    NoHandler { kind: String },
}

impl RuntimeError {
    /// The error-category string threaded to
    /// [`ClaimedTask::fail`] so Lua sees a stable classifier.
    pub fn error_category(&self) -> &'static str {
        match self {
            Self::MissingState { .. } => "extractor_missing_state",
            Self::PayloadDecode(_) => "extractor_payload_decode",
            Self::NoHandler { .. } => "runtime_no_handler",
        }
    }
}

/// Backoff policy applied between claim polls when no task is
/// available. Matches the idle-loop shape consumers were writing by
/// hand before #331.
#[derive(Clone, Debug)]
pub struct BackoffPolicy {
    /// First sleep on an idle poll. Default: 50ms.
    pub initial: Duration,
    /// Upper bound on the sleep. Default: 1s.
    pub max: Duration,
    /// Multiplier applied on each consecutive idle poll. Default: 2.0.
    pub factor: f64,
}

impl Default for BackoffPolicy {
    fn default() -> Self {
        Self {
            initial: Duration::from_millis(50),
            max: Duration::from_secs(1),
            factor: 2.0,
        }
    }
}

impl BackoffPolicy {
    fn next(&self, current: Duration) -> Duration {
        let scaled =
            Duration::from_secs_f64((current.as_secs_f64() * self.factor).min(self.max.as_secs_f64()));
        scaled.max(self.initial)
    }
}

/// Claim-loop harness on top of [`FlowFabricWorker`]. Owns the
/// handler registry, a type-keyed state map for [`Data<T>`]
/// extractors, and a concurrency semaphore.
pub struct WorkerRuntime {
    worker: Arc<FlowFabricWorker>,
    state: StateMap,
    handlers: std::collections::HashMap<String, BoxedHandler>,
    backoff: BackoffPolicy,
    concurrency: Arc<Semaphore>,
    max_concurrent: usize,
}

impl WorkerRuntime {
    /// Default concurrency cap. 64 matches the apalis `concurrency(N)`
    /// shape and leaves plenty of headroom on the tokio runtime's
    /// default worker-thread pool.
    pub const DEFAULT_MAX_CONCURRENT: usize = 64;

    /// Construct a runtime that wraps an already-connected
    /// [`FlowFabricWorker`].
    pub fn new(worker: FlowFabricWorker) -> Self {
        Self {
            worker: Arc::new(worker),
            state: StateMap::default(),
            handlers: std::collections::HashMap::new(),
            backoff: BackoffPolicy::default(),
            concurrency: Arc::new(Semaphore::new(Self::DEFAULT_MAX_CONCURRENT)),
            max_concurrent: Self::DEFAULT_MAX_CONCURRENT,
        }
    }

    /// Register a typed state value. Handlers reach it via
    /// [`Data<T>`]. One value per type; a second `data::<T>(..)` call
    /// overwrites.
    pub fn data<T: Clone + Send + Sync + 'static>(mut self, value: T) -> Self {
        self.state.insert(value);
        self
    }

    /// Register a handler for an `execution_kind`. The kind is the
    /// string threaded through [`ClaimedTask::execution_kind`] (set at
    /// `create_execution` time by the producer). Unknown kinds fail
    /// the task with
    /// [`RuntimeError::NoHandler`].
    pub fn on<Args, H>(mut self, kind: impl Into<String>, handler: H) -> Self
    where
        H: Handler<Args> + 'static,
        Args: 'static,
    {
        let kind = kind.into();
        self.handlers.insert(kind, handler.into_boxed());
        self
    }

    /// Override the default concurrency cap (64).
    pub fn max_concurrent(mut self, n: usize) -> Self {
        let n = n.max(1);
        self.concurrency = Arc::new(Semaphore::new(n));
        self.max_concurrent = n;
        self
    }

    /// Override the default idle-backoff policy.
    pub fn backoff(mut self, policy: BackoffPolicy) -> Self {
        self.backoff = policy;
        self
    }

    /// Drive the claim loop. Never returns on the happy path —
    /// terminates only if [`FlowFabricWorker::claim_next_via_backend`]
    /// returns a transport error after the worker's own retry budget
    /// is exhausted.
    pub async fn run(self) -> Result<(), SdkError> {
        info!(
            max_concurrent = self.max_concurrent,
            handlers = self.handlers.len(),
            "WorkerRuntime starting claim loop"
        );
        let state = Arc::new(self.state);
        let handlers = Arc::new(self.handlers);
        let mut idle_sleep = self.backoff.initial;

        loop {
            match self.worker.claim_next_via_backend().await {
                Ok(Some(task)) => {
                    idle_sleep = self.backoff.initial;
                    let permit = match self.concurrency.clone().acquire_owned().await {
                        Ok(p) => p,
                        Err(_) => {
                            // Semaphore only closes if we drop it —
                            // which we don't. Defensive.
                            warn!("WorkerRuntime semaphore closed unexpectedly; stopping");
                            return Ok(());
                        }
                    };
                    let state = state.clone();
                    let handlers = handlers.clone();
                    // Dispatch the task on a detached tokio task. We
                    // intentionally do not hold the JoinHandle —
                    // lease renewal + panic-catch + terminal-op
                    // dispatch all live inside the task itself, so
                    // there is nothing useful for the runtime to
                    // await. The `permit` captured by the closure is
                    // dropped at task completion, releasing the
                    // concurrency slot.
                    std::mem::drop(tokio::spawn(async move {
                        dispatch(task, state, handlers).await;
                        drop(permit);
                    }));
                }
                Ok(None) => {
                    debug!(sleep_ms = %idle_sleep.as_millis(), "runtime idle");
                    tokio::time::sleep(idle_sleep).await;
                    idle_sleep = self.backoff.next(idle_sleep);
                }
                Err(e) => {
                    error!(error = %e, "claim_next_via_backend returned error; stopping runtime");
                    return Err(e);
                }
            }
        }
    }
}

/// Dispatch a single claimed task through its registered handler.
/// Catches extractor failures, panics, and handler errors — every
/// exit path terminates the task via `complete` / `fail`, never
/// leaving a lease dangling for the worker's renewal loop to burn
/// down on its own.
async fn dispatch(
    task: ClaimedTask,
    state: Arc<StateMap>,
    handlers: Arc<std::collections::HashMap<String, BoxedHandler>>,
) {
    let kind = task.execution_kind().to_owned();
    let handler = match handlers.get(&kind) {
        Some(h) => h.clone_handler(),
        None => {
            let err = RuntimeError::NoHandler { kind: kind.clone() };
            let category = err.error_category().to_owned();
            let reason = err.to_string();
            if let Err(e) = task.fail(&reason, &category).await {
                error!(error = %e, %kind, "failed to mark task failed (no handler)");
            }
            return;
        }
    };

    let outcome = {
        let ctx = RuntimeCtx::new(&task, state.as_ref());
        let fut = handler.call(ctx);
        fut.await
    };

    match outcome {
        HandlerOutcome::Complete(payload) => {
            if let Err(e) = task.complete(payload).await {
                error!(error = %e, %kind, "failed to complete task");
            }
        }
        HandlerOutcome::Fail { reason, category } => {
            if let Err(e) = task.fail(&reason, &category).await {
                error!(error = %e, %kind, "failed to mark task failed");
            }
        }
    }
}

/// Internal outcome after the handler + extractors have resolved.
///
/// Pub-but-hidden: the [`Handler::call`] trait method returns a
/// [`handler::HandlerFuture`] that resolves to this type, so it must
/// be reachable at `pub` for the trait method signature. Consumers
/// never name it — they return [`HandlerResult`]; the mapping to
/// this enum happens inside the tuple-macro blanket impls.
#[doc(hidden)]
#[derive(Debug)]
pub enum HandlerOutcome {
    Complete(Option<Vec<u8>>),
    Fail { reason: String, category: String },
}

impl HandlerOutcome {
    /// Build a Fail outcome from the trait-object error returned by
    /// a handler, classified as `handler_error`. The error chain is
    /// walked via `std::error::Error::source()` so `caused by:` spans
    /// print in the failure reason without pulling a display helper
    /// from anyhow.
    pub(crate) fn handler_error(
        err: Box<dyn std::error::Error + Send + Sync + 'static>,
    ) -> Self {
        let mut reason = err.to_string();
        let mut src = err.source();
        while let Some(s) = src {
            reason.push_str(": ");
            reason.push_str(&s.to_string());
            src = s.source();
        }
        Self::Fail {
            reason,
            category: "handler_error".to_owned(),
        }
    }

    /// Build a Fail outcome from a [`RuntimeError`] (extractor or
    /// registry failure).
    pub(crate) fn runtime_error(err: RuntimeError) -> Self {
        Self::Fail {
            reason: err.to_string(),
            category: err.error_category().to_owned(),
        }
    }

    /// Build a Fail outcome from a caught panic.
    pub(crate) fn panic(panic_msg: String) -> Self {
        Self::Fail {
            reason: format!("handler panicked: {panic_msg}"),
            category: "handler_panic".to_owned(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_policy_ramps_and_clamps() {
        let p = BackoffPolicy::default();
        let t0 = p.initial;
        let t1 = p.next(t0);
        assert!(t1 > t0, "backoff must ramp");
        // Clamp: far above max stays at max.
        let huge = Duration::from_secs(3600);
        let clamped = p.next(huge);
        assert_eq!(clamped, p.max, "backoff must clamp at max");
    }

    #[test]
    fn runtime_error_category_is_stable() {
        assert_eq!(
            RuntimeError::MissingState { type_name: "X" }.error_category(),
            "extractor_missing_state"
        );
        assert_eq!(
            RuntimeError::NoHandler {
                kind: "x".to_owned()
            }
            .error_category(),
            "runtime_no_handler"
        );
    }

    #[test]
    fn handler_error_walks_source_chain() {
        #[derive(Debug)]
        struct Inner(&'static str);
        impl std::fmt::Display for Inner {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str(self.0)
            }
        }
        impl std::error::Error for Inner {}

        #[derive(Debug)]
        struct Outer(Inner);
        impl std::fmt::Display for Outer {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("outer")
            }
        }
        impl std::error::Error for Outer {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }

        let e: Box<dyn std::error::Error + Send + Sync> = Box::new(Outer(Inner("root")));
        let outcome = HandlerOutcome::handler_error(e);
        match outcome {
            HandlerOutcome::Fail { reason, category } => {
                assert_eq!(category, "handler_error");
                assert_eq!(reason, "outer: root", "source chain must be walked");
            }
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn panic_outcome_carries_category() {
        let outcome = HandlerOutcome::panic("boom".to_owned());
        match outcome {
            HandlerOutcome::Fail { reason, category } => {
                assert_eq!(category, "handler_panic");
                assert!(reason.contains("boom"));
            }
            _ => panic!("expected Fail"),
        }
    }

    #[test]
    fn backoff_clamps_idle_to_initial_lower_bound() {
        // Under a tiny `current`, the factor scale should still hit
        // at least `initial` (not drop below). Pinning this so a
        // future refactor of `BackoffPolicy::next` doesn't silently
        // let the loop busy-spin.
        let p = BackoffPolicy {
            initial: Duration::from_millis(50),
            max: Duration::from_secs(1),
            factor: 2.0,
        };
        let tiny = Duration::from_micros(1);
        let result = p.next(tiny);
        assert!(
            result >= p.initial,
            "backoff must not drop below initial; got {result:?}"
        );
    }
}
