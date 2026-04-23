//! `RateLimitLayer` — in-process token-bucket rate cap on configured
//! methods.
//!
//! Per-worker-process, in-memory, best-effort. Distinct from RFC-008
//! flow budgets (engine-enforced, global, per-flow). Use this when
//! the consumer wants to protect a downstream sink from traffic this
//! worker emits — e.g. "don't append more than 500 frames per second
//! into my log pipe".
//!
//! Backed by `governor` (token-bucket implementation). Feature
//! `layer-ratelimit` pulls `governor` + `nonzero_ext`.

use std::num::NonZeroU32;
use std::sync::Arc;
use std::time::Duration;

use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::{EngineError, ValidationKind};
use governor::Quota;
use governor::RateLimiter;
use governor::clock::DefaultClock;
use governor::state::{InMemoryState, NotKeyed};
use std::borrow::Cow;
use std::collections::HashMap;

use super::EngineBackendLayer;
use super::hooks::{Admit, HookOutcome, HookedBackend, LayerHooks};

/// Key identifying which token bucket a method call draws from.
/// `'static` static strs for all 17 method names, `Owned` for the
/// dynamic fallback — matches the "allocation-optional" contract.
pub type RateLimitKey = Cow<'static, str>;

/// Pluggable classifier `Fn(method_name) -> RateLimitKey`. The
/// default classifier sends every call to the same bucket; consumers
/// building per-method buckets return `Cow::Borrowed(method_name)`
/// (zero-alloc) or a grouping like `Cow::Borrowed("writes")` for
/// method-name-independent sharing.
pub type RateLimitKeyFn = Arc<dyn Fn(&'static str) -> RateLimitKey + Send + Sync + 'static>;

type Limiter = RateLimiter<NotKeyed, InMemoryState, DefaultClock>;

/// See module-level docs.
pub struct RateLimitLayer {
    /// Per-key limiters, built at construction time; reads are
    /// lock-free (`HashMap` is read-only after `build`).
    buckets: HashMap<RateLimitKey, Arc<Limiter>>,
    classifier: RateLimitKeyFn,
    /// Default bucket for keys the consumer didn't pre-configure.
    default_bucket: Option<Arc<Limiter>>,
}

impl RateLimitLayer {
    /// Construct a builder with a single default bucket (`per_second`
    /// permits, shared across all methods).
    pub fn single_bucket(per_second: u32) -> Self {
        let limiter = build_limiter(per_second);
        Self {
            buckets: HashMap::new(),
            classifier: Arc::new(|_method| Cow::Borrowed("__default__")),
            default_bucket: Some(limiter),
        }
    }

    /// Construct a rate-limiter with an explicit classifier. Call
    /// [`RateLimitLayer::bucket`] to register a bucket per key. Keys
    /// returned by the classifier that are not registered fall back
    /// to the `default_per_second` bucket if set, otherwise the call
    /// is admitted unconditionally (no limit).
    pub fn with_classifier<F>(classifier: F) -> Self
    where
        F: Fn(&'static str) -> RateLimitKey + Send + Sync + 'static,
    {
        Self {
            buckets: HashMap::new(),
            classifier: Arc::new(classifier),
            default_bucket: None,
        }
    }

    /// Register a bucket under `key` with `per_second` permits.
    pub fn bucket(mut self, key: impl Into<RateLimitKey>, per_second: u32) -> Self {
        self.buckets.insert(key.into(), build_limiter(per_second));
        self
    }

    /// Register a catch-all default bucket (used for keys the
    /// classifier returns that are not in [`RateLimitLayer::bucket`]).
    pub fn default_bucket(mut self, per_second: u32) -> Self {
        self.default_bucket = Some(build_limiter(per_second));
        self
    }
}

fn build_limiter(per_second: u32) -> Arc<Limiter> {
    let rate = NonZeroU32::new(per_second.max(1)).expect("per_second is clamped to >=1");
    let quota = Quota::per_second(rate);
    Arc::new(RateLimiter::direct(quota))
}

impl EngineBackendLayer for RateLimitLayer {
    fn layer(&self, inner: Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend> {
        // Clone the bucket map so the HookedBackend owns it (cheap:
        // each bucket is an Arc).
        let buckets = self.buckets.clone();
        let classifier = self.classifier.clone();
        let default_bucket = self.default_bucket.clone();
        Arc::new(HookedBackend::new(
            inner,
            RateLimitHooks {
                buckets,
                classifier,
                default_bucket,
            },
        ))
    }
}

pub(crate) struct RateLimitHooks {
    buckets: HashMap<RateLimitKey, Arc<Limiter>>,
    classifier: RateLimitKeyFn,
    default_bucket: Option<Arc<Limiter>>,
}

impl LayerHooks for RateLimitHooks {
    fn before(&self, method_name: &'static str) -> Admit {
        let key = (self.classifier)(method_name);
        let limiter = self.buckets.get(&key).or(self.default_bucket.as_ref());
        match limiter {
            Some(l) => match l.check() {
                Ok(_) => Admit::Proceed,
                Err(_) => Admit::reject(EngineError::Validation {
                    kind: ValidationKind::InvalidInput,
                    detail: format!("rate-limit: method {method_name} bucket {key} exhausted"),
                }),
            },
            None => Admit::Proceed,
        }
    }

    fn after(&self, _method_name: &'static str, _elapsed: Duration, _outcome: HookOutcome<'_>) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::{EngineBackendLayerExt, test_support::PassthroughBackend};

    fn test_exec_id() -> ff_core::types::ExecutionId {
        ff_core::types::ExecutionId::parse("{fp:0}:00000000-0000-0000-0000-000000000000").unwrap()
    }

    #[tokio::test]
    async fn rejects_after_exceeding_quota() {
        let raw = Arc::new(PassthroughBackend::default());
        let raw_clone = raw.clone();
        let inner: Arc<dyn EngineBackend> = raw;
        // 1 permit/s with burst 1 — second call in the same tick
        // should be rejected.
        let layered = inner.layer(RateLimitLayer::single_bucket(1));
        let id = test_exec_id();

        let r1 = layered.describe_execution(&id).await;
        let r2 = layered.describe_execution(&id).await;

        assert!(r1.is_ok(), "first call admitted");
        assert!(matches!(
            r2,
            Err(EngineError::Validation {
                kind: ValidationKind::InvalidInput,
                ..
            })
        ));
        // Only the admitted call reached the inner backend.
        assert_eq!(raw_clone.calls("describe_execution"), 1);
    }

    #[tokio::test]
    async fn unconfigured_key_with_no_default_admits() {
        let raw = Arc::new(PassthroughBackend::default());
        let raw_clone = raw.clone();
        let inner: Arc<dyn EngineBackend> = raw;
        let layered =
            inner.layer(RateLimitLayer::with_classifier(|m| Cow::Borrowed(m)).bucket("claim", 100));
        let id = test_exec_id();
        let _ = layered.describe_execution(&id).await;
        assert_eq!(raw_clone.calls("describe_execution"), 1);
    }
}
