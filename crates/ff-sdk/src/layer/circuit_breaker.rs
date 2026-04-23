//! `CircuitBreakerLayer` — opens a circuit on consecutive
//! `EngineError::Transport` errors, fails fast with a synthetic
//! Transport error while open, and half-opens a trial call after
//! the cool-down.
//!
//! Triggers on `EngineError::Transport` only (per manager lock
//! 2026-04-22). `Contention` / `Conflict` are typed engine outcomes
//! — tripping the breaker on those would confuse workflow-level
//! retries with transport-degradation.
//!
//! Classification-wise, the synthetic rejection surfaces as
//! `EngineError::Transport { backend: "ff-sdk-layer", source: "circuit open" }`
//! — retryable by `EngineError::is_transport()` so callers with
//! existing retry logic treat it as a transient fault, distinct
//! from a real network error by its `backend` tag.

use std::sync::Arc;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use ff_core::engine_backend::EngineBackend;
use ff_core::engine_error::EngineError;

use super::EngineBackendLayer;
use super::hooks::{Admit, HookOutcome, HookedBackend, LayerHooks};

/// Configurable thresholds. Defaults: open after 5 consecutive
/// transport errors, cool-down 30s, half-open trial admits exactly
/// one in-flight call.
#[derive(Clone, Copy, Debug)]
pub struct CircuitBreakerConfig {
    pub failure_threshold: u32,
    pub cool_down: Duration,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: 5,
            cool_down: Duration::from_secs(30),
        }
    }
}

/// See module-level docs.
pub struct CircuitBreakerLayer {
    config: CircuitBreakerConfig,
}

impl CircuitBreakerLayer {
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self { config }
    }
}

impl Default for CircuitBreakerLayer {
    fn default() -> Self {
        Self::new(CircuitBreakerConfig::default())
    }
}

impl super::sealed::SealedLayer for CircuitBreakerLayer {}

impl EngineBackendLayer for CircuitBreakerLayer {
    fn layer(&self, inner: Arc<dyn EngineBackend>) -> Arc<dyn EngineBackend> {
        Arc::new(HookedBackend::new(
            inner,
            CircuitBreakerHooks {
                config: self.config,
                state: Mutex::new(BreakerState::Closed {
                    consecutive_failures: 0,
                }),
            },
        ))
    }
}

#[derive(Clone, Copy, Debug)]
enum BreakerState {
    Closed { consecutive_failures: u32 },
    Open { opened_at: Instant },
    HalfOpen,
}

pub(crate) struct CircuitBreakerHooks {
    config: CircuitBreakerConfig,
    state: Mutex<BreakerState>,
}

impl LayerHooks for CircuitBreakerHooks {
    fn before(&self, _method_name: &'static str) -> Admit {
        let mut guard = self.state.lock().unwrap();
        match *guard {
            BreakerState::Closed { .. } => Admit::Proceed,
            BreakerState::Open { opened_at } => {
                if opened_at.elapsed() >= self.config.cool_down {
                    // Transition to half-open and admit this one call
                    // as the trial.
                    *guard = BreakerState::HalfOpen;
                    Admit::Proceed
                } else {
                    Admit::reject(synthetic_open_error())
                }
            }
            BreakerState::HalfOpen => {
                // Only the first post-cool-down call becomes the
                // trial; subsequent concurrent calls are rejected
                // until the trial resolves.
                Admit::reject(synthetic_open_error())
            }
        }
    }

    fn after(&self, _method_name: &'static str, _elapsed: Duration, outcome: HookOutcome<'_>) {
        let is_transport_err = matches!(outcome, HookOutcome::Err(e) if is_transport(e));
        let mut guard = self.state.lock().unwrap();
        match *guard {
            BreakerState::Closed {
                consecutive_failures,
            } => {
                if is_transport_err {
                    let next = consecutive_failures.saturating_add(1);
                    if next >= self.config.failure_threshold {
                        *guard = BreakerState::Open {
                            opened_at: Instant::now(),
                        };
                    } else {
                        *guard = BreakerState::Closed {
                            consecutive_failures: next,
                        };
                    }
                } else if !matches!(outcome, HookOutcome::Err(_)) {
                    // Successful call resets the counter. Non-transport
                    // errors leave the counter as-is (they're engine
                    // outcomes, not connectivity degradation).
                    if consecutive_failures > 0 {
                        *guard = BreakerState::Closed {
                            consecutive_failures: 0,
                        };
                    }
                }
            }
            BreakerState::Open { .. } => {
                // Reached only when a rejected `before` fed a synthetic
                // err here; no state change.
            }
            BreakerState::HalfOpen => {
                if is_transport_err {
                    // Trial failed — reopen.
                    *guard = BreakerState::Open {
                        opened_at: Instant::now(),
                    };
                } else {
                    // Trial succeeded (or returned a non-transport
                    // engine error, treated as connectivity OK).
                    *guard = BreakerState::Closed {
                        consecutive_failures: 0,
                    };
                }
            }
        }
    }
}

fn is_transport(err: &EngineError) -> bool {
    match err {
        EngineError::Transport { .. } => true,
        EngineError::Contextual { source, .. } => is_transport(source),
        _ => false,
    }
}

fn synthetic_open_error() -> EngineError {
    EngineError::Transport {
        backend: "ff-sdk-layer",
        source: "circuit open".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layer::{EngineBackendLayerExt, test_support::PassthroughBackend};

    fn test_exec_id() -> ff_core::types::ExecutionId {
        ff_core::types::ExecutionId::parse("{fp:0}:00000000-0000-0000-0000-000000000000").unwrap()
    }

    #[tokio::test]
    async fn opens_after_failure_threshold_and_rejects_fast() {
        let raw = Arc::new(PassthroughBackend::default());
        let raw_clone = raw.clone();
        let inner: Arc<dyn EngineBackend> = raw;
        let cfg = CircuitBreakerConfig {
            failure_threshold: 3,
            cool_down: Duration::from_secs(60),
        };
        let layered = inner.layer(CircuitBreakerLayer::new(cfg));
        raw_clone.set_fail_transport(true);

        let id = test_exec_id();
        for _ in 0..3 {
            let _ = layered.describe_execution(&id).await;
        }
        // 4th call: circuit is open, should not reach inner.
        let before = raw_clone.calls("describe_execution");
        let r = layered.describe_execution(&id).await;
        assert!(matches!(
            r,
            Err(EngineError::Transport {
                backend: "ff-sdk-layer",
                ..
            })
        ));
        assert_eq!(raw_clone.calls("describe_execution"), before);
    }

    #[tokio::test]
    async fn successful_call_resets_counter() {
        let raw = Arc::new(PassthroughBackend::default());
        let raw_clone = raw.clone();
        let inner: Arc<dyn EngineBackend> = raw;
        let cfg = CircuitBreakerConfig {
            failure_threshold: 3,
            cool_down: Duration::from_secs(60),
        };
        let layered = inner.layer(CircuitBreakerLayer::new(cfg));
        let id = test_exec_id();

        raw_clone.set_fail_transport(true);
        let _ = layered.describe_execution(&id).await;
        let _ = layered.describe_execution(&id).await;
        raw_clone.set_fail_transport(false);
        let _ok = layered.describe_execution(&id).await;
        raw_clone.set_fail_transport(true);
        let _ = layered.describe_execution(&id).await;
        // Counter was reset by the ok call; one subsequent transport
        // err isn't enough to open.
        let id2 = test_exec_id();
        raw_clone.set_fail_transport(false);
        let r = layered.describe_execution(&id2).await;
        assert!(r.is_ok());
    }

    #[tokio::test]
    async fn half_open_trial_closes_on_success() {
        let raw = Arc::new(PassthroughBackend::default());
        let raw_clone = raw.clone();
        let inner: Arc<dyn EngineBackend> = raw;
        let cfg = CircuitBreakerConfig {
            failure_threshold: 2,
            cool_down: Duration::from_millis(10),
        };
        let layered = inner.layer(CircuitBreakerLayer::new(cfg));

        raw_clone.set_fail_transport(true);
        let id = test_exec_id();
        let _ = layered.describe_execution(&id).await;
        let _ = layered.describe_execution(&id).await;
        // Circuit is open.
        tokio::time::sleep(Duration::from_millis(20)).await;
        raw_clone.set_fail_transport(false);
        // Cool-down elapsed — this call is the trial; succeeds.
        let r = layered.describe_execution(&id).await;
        assert!(r.is_ok());
        // Next calls should flow through normally.
        let r2 = layered.describe_execution(&id).await;
        assert!(r2.is_ok());
    }
}
