//! Client-local layer surface for [`EngineBackend`].
//!
//! See `rfcs/drafts/RFC-draft-tower-layer-surface.md` for the full
//! design. A layer is a pure function from
//! `Arc<dyn EngineBackend>` to `Arc<dyn EngineBackend>`; the output
//! delegates to the input for every trait method, optionally
//! observing or short-circuiting.
//!
//! Layers are client-local only. They MUST NOT be used to enforce
//! any distributed-correctness invariant (fence triple, lease
//! ownership, waitpoint HMAC, partition routing). See RFC §4 for the
//! hard boundary.
//!
//! # Usage
//!
//! ```ignore
//! use std::sync::Arc;
//! use ff_sdk::layer::{EngineBackendLayerExt, TracingLayer};
//! use ff_core::engine_backend::EngineBackend;
//!
//! let backend: Arc<dyn EngineBackend> = /* ... */;
//! let layered = backend.layer(TracingLayer::default());
//! ```

use std::sync::Arc;

use ff_core::engine_backend::EngineBackend;

#[cfg(any(
    feature = "layer-tracing",
    feature = "layer-ratelimit",
    feature = "layer-metrics",
    feature = "layer-circuit-breaker",
))]
mod hooks;

#[cfg(feature = "layer-circuit-breaker")]
mod circuit_breaker;
#[cfg(feature = "layer-metrics")]
mod metrics;
#[cfg(feature = "layer-ratelimit")]
mod ratelimit;
#[cfg(feature = "layer-tracing")]
mod tracing_layer;

#[cfg(feature = "layer-circuit-breaker")]
pub use circuit_breaker::{CircuitBreakerConfig, CircuitBreakerLayer};
#[cfg(feature = "layer-metrics")]
pub use metrics::{MetricsLayer, MetricsSink, NoopSink, Outcome};
#[cfg(feature = "layer-ratelimit")]
pub use ratelimit::{RateLimitKey, RateLimitKeyFn, RateLimitLayer};
#[cfg(feature = "layer-tracing")]
pub use tracing_layer::TracingLayer;

mod sealed {
    pub trait SealedExt {}
    impl<T: ?Sized> SealedExt for std::sync::Arc<T> where T: ff_core::engine_backend::EngineBackend {}

    /// Seal marker for `EngineBackendLayer`. Only types inside
    /// `ff-sdk` (the bundled built-in layers + the blanket `Fn`
    /// impl) can construct this, which prevents downstream crates
    /// from `impl EngineBackendLayer` directly — they build layers
    /// via the provided `Fn` blanket or by combining built-ins. If
    /// we ever want to open the trait for external implementation,
    /// that's a deliberate future change with a minor bump.
    pub trait SealedLayer {}
}

/// Wrap an [`EngineBackend`] with a client-local cross-cutting concern.
///
/// The returned `Arc<dyn EngineBackend>` MUST delegate every trait
/// method to `inner`, optionally observing or short-circuiting. See
/// the module-level documentation for the contract, and
/// `rfcs/drafts/RFC-draft-tower-layer-surface.md` §4 for the hard
/// boundary on what a layer is forbidden to do.
///
/// This trait is sealed for v1 — consumers implement layers by
/// providing a type that impls this trait's `layer(..)` method via
/// the blanket `Fn` impl below, or by constructing the bundled
/// built-ins. The seal is enforced via the private
/// [`sealed::SealedLayer`] supertrait: only types inside `ff-sdk`
/// can satisfy the bound, so downstream crates cannot add new impls.
/// This keeps the trait free to grow methods in future minor
/// releases without breaking implementors.
pub trait EngineBackendLayer: sealed::SealedLayer + Send + Sync + 'static {
    /// Wrap `inner`, returning a new `Arc<dyn EngineBackend>` that
    /// delegates to `inner`.
    fn layer(&self, inner: Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend>;
}

/// Blanket impl so any `Fn(Arc<dyn EngineBackend>) -> Arc<dyn
/// EngineBackend>` is a layer. Useful for small one-off wrappers
/// without writing a struct. Note this still goes through the
/// sealed supertrait — downstream crates cannot add their own
/// `SealedLayer` impl, so this blanket is the only external-origin
/// path to the trait.
impl<F> sealed::SealedLayer for F where
    F: Fn(Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend> + Send + Sync + 'static
{
}

impl<F> EngineBackendLayer for F
where
    F: Fn(Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend> + Send + Sync + 'static,
{
    fn layer(&self, inner: Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend> {
        (self)(inner)
    }
}

/// Extension trait providing `.layer(L)` on `Arc<dyn EngineBackend>`.
/// Sealed — the set of types that can build layered backends is fixed
/// at `Arc<dyn EngineBackend>` for v1.
pub trait EngineBackendLayerExt: sealed::SealedExt {
    /// Apply a layer, returning the layered backend.
    fn layer<L: EngineBackendLayer>(self, layer: L) -> Arc<dyn EngineBackend>;
}

impl EngineBackendLayerExt for Arc<dyn EngineBackend> {
    fn layer<L: EngineBackendLayer>(self, layer: L) -> Arc<dyn EngineBackend> {
        layer.layer(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::test_support::PassthroughBackend;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn closure_blanket_impl_wraps() {
        let counter = Arc::new(AtomicUsize::new(0));
        let counter_for_layer = counter.clone();
        let layer = move |inner: Arc<dyn EngineBackend>| -> Arc<dyn EngineBackend> {
            counter_for_layer.fetch_add(1, Ordering::SeqCst);
            inner
        };
        let backend: Arc<dyn EngineBackend> = Arc::new(PassthroughBackend::default());
        let _layered = backend.layer(layer);
        assert_eq!(counter.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn zero_layer_case_arc_itself_is_a_backend() {
        let backend: Arc<dyn EngineBackend> = Arc::new(PassthroughBackend::default());
        // Just verify the type compiles; we don't need to call methods.
        let _ = backend;
    }
}

#[cfg(any(
    test,
    all(test, feature = "layer-tracing"),
    all(test, feature = "layer-ratelimit"),
    all(test, feature = "layer-metrics"),
    all(test, feature = "layer-circuit-breaker"),
))]
pub(crate) mod test_support;
