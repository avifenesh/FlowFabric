//! `TracingLayer` — wraps every [`EngineBackend`] call in a
//! `tracing::info_span!` with method name + call duration.
//!
//! Distinct from the engine-side `#[instrument]` attributes on Lua
//! dispatch (#170): that span tree lives in the engine/server
//! process, this one lives in the **consumer's** span tree. Useful
//! for consumers that run their own OTEL collector and want backend
//! calls to appear under a consumer-rooted span.

use std::sync::Arc;
use std::time::Duration;

use ff_core::engine_backend::EngineBackend;
use tracing::{Level, event, span};

use super::EngineBackendLayer;
use super::hooks::{Admit, HookOutcome, HookedBackend, LayerHooks};

/// See module-level docs.
#[derive(Debug, Default, Clone, Copy)]
pub struct TracingLayer {
    /// The tracing level at which to emit the span. Defaults to
    /// `Level::INFO` per apalis's default — consumers wanting quieter
    /// output drop to `DEBUG` via [`TracingLayer::at_level`].
    level: TracingLevel,
}

#[derive(Debug, Default, Clone, Copy)]
enum TracingLevel {
    Trace,
    Debug,
    #[default]
    Info,
    Warn,
}

impl TracingLayer {
    /// Construct with the default `INFO` level.
    pub fn new() -> Self {
        Self::default()
    }

    /// Emit spans at `TRACE` level.
    pub fn trace() -> Self {
        Self {
            level: TracingLevel::Trace,
        }
    }

    /// Emit spans at `DEBUG` level.
    pub fn debug() -> Self {
        Self {
            level: TracingLevel::Debug,
        }
    }

    /// Emit spans at `WARN` level (rare — useful when the consumer
    /// wants only problem-signal visibility).
    pub fn warn() -> Self {
        Self {
            level: TracingLevel::Warn,
        }
    }

    fn at_level(level: TracingLevel) -> Self {
        Self { level }
    }
}

impl super::sealed::SealedLayer for TracingLayer {}

impl EngineBackendLayer for TracingLayer {
    fn layer(&self, inner: Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend> {
        Arc::new(HookedBackend::new(
            inner,
            TracingHooks { level: self.level },
        ))
    }
}

pub(crate) struct TracingHooks {
    level: TracingLevel,
}

impl LayerHooks for TracingHooks {
    fn before(&self, _method_name: &'static str) -> Admit {
        Admit::Proceed
    }

    fn after(&self, method_name: &'static str, elapsed: Duration, outcome: HookOutcome<'_>) {
        let elapsed_us = elapsed.as_micros() as u64;
        let ok = matches!(outcome, HookOutcome::Ok);
        // A static dispatch over the level avoids the per-call branch
        // penalty without pulling in the full `tracing` macro machinery
        // behind a dynamic dispatch. Each arm emits a span + a
        // completion event because `tracing` does not expose a
        // non-macro builder for arbitrary Level at runtime.
        match self.level {
            TracingLevel::Trace => {
                let sp = span!(Level::TRACE, "ff_sdk.backend", method = method_name);
                let _g = sp.enter();
                event!(
                    Level::TRACE,
                    method = method_name,
                    elapsed_us,
                    ok,
                    "engine_backend.call"
                );
            }
            TracingLevel::Debug => {
                let sp = span!(Level::DEBUG, "ff_sdk.backend", method = method_name);
                let _g = sp.enter();
                event!(
                    Level::DEBUG,
                    method = method_name,
                    elapsed_us,
                    ok,
                    "engine_backend.call"
                );
            }
            TracingLevel::Info => {
                let sp = span!(Level::INFO, "ff_sdk.backend", method = method_name);
                let _g = sp.enter();
                event!(
                    Level::INFO,
                    method = method_name,
                    elapsed_us,
                    ok,
                    "engine_backend.call"
                );
            }
            TracingLevel::Warn => {
                let sp = span!(Level::WARN, "ff_sdk.backend", method = method_name);
                let _g = sp.enter();
                event!(
                    Level::WARN,
                    method = method_name,
                    elapsed_us,
                    ok,
                    "engine_backend.call"
                );
            }
        }
    }
}

// Quiet dead-code warning on `at_level`: kept for future consumer
// use in case we wire a runtime toggle.
#[allow(dead_code)]
const _: fn(TracingLevel) -> TracingLayer = TracingLayer::at_level;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::{EngineBackendLayerExt, test_support::PassthroughBackend};
    use ff_core::backend::{CapabilitySet, ClaimPolicy};
    use ff_core::types::LaneId;

    #[tokio::test]
    async fn delegates_and_counts_call() {
        let raw = Arc::new(PassthroughBackend::default());
        let raw_clone = raw.clone();
        let inner: Arc<dyn EngineBackend> = raw;
        let layered = inner.layer(TracingLayer::default());

        let lane = LaneId::new("main");
        let caps = CapabilitySet::new::<_, String>(Vec::<String>::new());
        let _ = layered.claim(&lane, &caps, ClaimPolicy::immediate()).await;

        assert_eq!(raw_clone.calls("claim"), 1);
    }
}
