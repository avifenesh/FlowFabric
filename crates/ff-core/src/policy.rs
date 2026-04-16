use crate::types::{BudgetId, TimestampMs};
use serde::{Deserialize, Serialize};

/// Retry configuration for an execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RetryPolicy {
    /// Maximum number of retry attempts (not counting the initial attempt).
    #[serde(default = "default_max_retries")]
    pub max_retries: u32,
    /// Backoff strategy.
    #[serde(default)]
    pub backoff: BackoffStrategy,
    /// Error categories eligible for automatic retry.
    #[serde(default)]
    pub retryable_categories: Vec<String>,
}

fn default_max_retries() -> u32 {
    3
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: default_max_retries(),
            backoff: BackoffStrategy::default(),
            retryable_categories: Vec::new(),
        }
    }
}

/// Backoff strategy for retries.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum BackoffStrategy {
    /// Fixed delay between retries.
    Fixed { delay_ms: u64 },
    /// Exponential backoff with optional jitter.
    Exponential {
        initial_delay_ms: u64,
        max_delay_ms: u64,
        multiplier: f64,
        #[serde(default)]
        jitter: bool,
    },
}

impl Default for BackoffStrategy {
    fn default() -> Self {
        Self::Exponential {
            initial_delay_ms: 1000,
            max_delay_ms: 60_000,
            multiplier: 2.0,
            jitter: false,
        }
    }
}

/// Timeout configuration for an execution.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TimeoutPolicy {
    /// Per-attempt timeout in milliseconds.
    #[serde(default)]
    pub attempt_timeout_ms: Option<u64>,
    /// Total execution deadline (absolute timestamp or duration from creation).
    #[serde(default)]
    pub execution_deadline_ms: Option<u64>,
    /// Maximum number of lease-expiry reclaims before failing with max_reclaims_exceeded.
    /// Default: 100.
    #[serde(default = "default_max_reclaim_count")]
    pub max_reclaim_count: u32,
}

fn default_max_reclaim_count() -> u32 {
    100
}

/// Suspension behavior configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SuspensionPolicy {
    /// Default suspension timeout in milliseconds.
    #[serde(default)]
    pub default_timeout_ms: Option<u64>,
    /// What happens when suspension times out: "fail" or "cancel".
    #[serde(default = "default_timeout_behavior")]
    pub timeout_behavior: String,
}

fn default_timeout_behavior() -> String {
    "fail".to_owned()
}

/// Fallback chain configuration (provider/model progression).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FallbackPolicy {
    /// Ordered list of fallback tiers.
    pub tiers: Vec<FallbackTier>,
}

/// A single tier in the fallback chain.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FallbackTier {
    /// Provider name (e.g., "anthropic", "openai").
    pub provider: String,
    /// Model identifier.
    pub model: String,
    /// Optional per-tier timeout override in ms.
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

/// Routing requirements for worker matching.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct RoutingRequirements {
    /// Required capabilities the worker must have.
    #[serde(default)]
    pub required_capabilities: Vec<String>,
    /// Preferred locality/region.
    #[serde(default)]
    pub preferred_locality: Option<String>,
    /// Isolation level.
    #[serde(default)]
    pub isolation_level: Option<String>,
}

/// Stream durability and retention configuration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct StreamPolicy {
    /// Durability mode: "buffered" (default) or "durable".
    #[serde(default = "default_durability_mode")]
    pub durability_mode: String,
    /// Maximum number of frames to retain per stream.
    #[serde(default = "default_retention_maxlen")]
    pub retention_maxlen: u64,
    /// Stream retention TTL in ms after closure.
    #[serde(default)]
    pub retention_ttl_ms: Option<u64>,
}

fn default_durability_mode() -> String {
    "buffered".to_owned()
}

fn default_retention_maxlen() -> u64 {
    10_000
}

/// Complete execution policy snapshot, frozen at creation time.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    /// Higher value = higher priority. Default: 0.
    #[serde(default)]
    pub priority: i32,
    /// Earliest eligible time.
    #[serde(default)]
    pub delay_until: Option<TimestampMs>,
    /// Retry configuration.
    #[serde(default)]
    pub retry_policy: Option<RetryPolicy>,
    /// Timeout configuration.
    #[serde(default)]
    pub timeout_policy: Option<TimeoutPolicy>,
    /// Maximum lease-expiry reclaims. Default: 100.
    #[serde(default = "default_max_reclaim_count")]
    pub max_reclaim_count: u32,
    /// Suspension behavior.
    #[serde(default)]
    pub suspension_policy: Option<SuspensionPolicy>,
    /// Fallback chain.
    #[serde(default)]
    pub fallback_policy: Option<FallbackPolicy>,
    /// Maximum number of replays. Default: 10.
    #[serde(default = "default_max_replay_count")]
    pub max_replay_count: u32,
    /// Attached budget references.
    #[serde(default)]
    pub budget_ids: Vec<BudgetId>,
    /// Routing requirements.
    #[serde(default)]
    pub routing_requirements: Option<RoutingRequirements>,
    /// Idempotency dedup window in ms. V1 default: 24h.
    #[serde(default)]
    pub dedup_window_ms: Option<u64>,
    /// Stream policy.
    #[serde(default)]
    pub stream_policy: Option<StreamPolicy>,
    /// Maximum signal records accepted. Default: 10000.
    #[serde(default = "default_max_signals")]
    pub max_signals_per_execution: u32,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            priority: 0,
            delay_until: None,
            retry_policy: None,
            timeout_policy: None,
            max_reclaim_count: default_max_reclaim_count(),
            suspension_policy: None,
            fallback_policy: None,
            max_replay_count: default_max_replay_count(),
            budget_ids: Vec::new(),
            routing_requirements: None,
            dedup_window_ms: None,
            stream_policy: None,
            max_signals_per_execution: default_max_signals(),
        }
    }
}

fn default_max_replay_count() -> u32 {
    10
}

fn default_max_signals() -> u32 {
    10_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn execution_policy_defaults() {
        let policy = ExecutionPolicy::default();
        assert_eq!(policy.priority, 0);
        assert_eq!(policy.max_reclaim_count, 100);
        assert_eq!(policy.max_replay_count, 10);
        assert_eq!(policy.max_signals_per_execution, 10_000);
        assert!(policy.retry_policy.is_none());
        assert!(policy.timeout_policy.is_none());
    }

    #[test]
    fn retry_policy_serde() {
        let policy = RetryPolicy {
            max_retries: 3,
            backoff: BackoffStrategy::Exponential {
                initial_delay_ms: 100,
                max_delay_ms: 30_000,
                multiplier: 2.0,
                jitter: true,
            },
            retryable_categories: vec!["timeout".into(), "provider_error".into()],
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: RetryPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, parsed);
    }

    #[test]
    fn timeout_policy_defaults() {
        let json = r#"{"attempt_timeout_ms": 30000}"#;
        let policy: TimeoutPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy.attempt_timeout_ms, Some(30_000));
        assert_eq!(policy.max_reclaim_count, 100);
    }

    #[test]
    fn retry_policy_defaults() {
        let policy = RetryPolicy::default();
        assert_eq!(policy.max_retries, 3);
        assert_eq!(
            policy.backoff,
            BackoffStrategy::Exponential {
                initial_delay_ms: 1000,
                max_delay_ms: 60_000,
                multiplier: 2.0,
                jitter: false,
            }
        );
        assert!(policy.retryable_categories.is_empty());
    }

    #[test]
    fn retry_policy_lua_compatible_json() {
        let policy = RetryPolicy::default();
        let json = serde_json::to_value(&policy).unwrap();
        assert_eq!(json["max_retries"], 3);
        let backoff = &json["backoff"];
        assert_eq!(backoff["type"], "exponential");
        assert_eq!(backoff["initial_delay_ms"], 1000);
        assert_eq!(backoff["max_delay_ms"], 60_000);
        assert_eq!(backoff["multiplier"], 2.0);

        let fixed = RetryPolicy {
            max_retries: 1,
            backoff: BackoffStrategy::Fixed { delay_ms: 5000 },
            retryable_categories: vec![],
        };
        let json = serde_json::to_value(&fixed).unwrap();
        assert_eq!(json["backoff"]["type"], "fixed");
        assert_eq!(json["backoff"]["delay_ms"], 5000);
    }

    #[test]
    fn retry_policy_deserialize_minimal() {
        let json = r#"{"max_retries": 5}"#;
        let policy: RetryPolicy = serde_json::from_str(json).unwrap();
        assert_eq!(policy.max_retries, 5);
        assert_eq!(policy.backoff, BackoffStrategy::default());
    }

    #[test]
    fn full_execution_policy_serde() {
        let policy = ExecutionPolicy {
            priority: 10,
            retry_policy: Some(RetryPolicy {
                max_retries: 5,
                backoff: BackoffStrategy::Fixed { delay_ms: 1000 },
                retryable_categories: vec![],
            }),
            ..Default::default()
        };
        let json = serde_json::to_string(&policy).unwrap();
        let parsed: ExecutionPolicy = serde_json::from_str(&json).unwrap();
        assert_eq!(policy, parsed);
    }
}
