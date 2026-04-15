//! Cross-partition budget check helper.
//!
//! The scheduler uses `BudgetChecker` to check budget status without
//! redundant reads per scheduling cycle. The checker caches budget status
//! per cycle and clears the cache at the start of each new cycle.
//!
//! Reference: RFC-008 §Budget Admission, RFC-010 §3.2

use std::collections::HashMap;

use ff_core::partition::{budget_partition, PartitionConfig};
use ff_core::types::BudgetId;

/// Budget status for admission decisions.
#[derive(Clone, Debug, Default)]
pub struct BudgetStatus {
    /// Hard limit breached — execution must be blocked.
    pub hard_breached: bool,
    /// Soft limit breached — execution may proceed with warning.
    pub soft_breached: bool,
}

/// Cross-partition budget checker with per-cycle caching.
///
/// Created once per scheduler instance. Call `clear_cache()` at the start
/// of each scheduling cycle to invalidate stale status.
pub struct BudgetChecker {
    client: ferriskey::Client,
    partition_config: PartitionConfig,
    cycle_cache: HashMap<BudgetId, BudgetStatus>,
}

impl BudgetChecker {
    pub fn new(client: ferriskey::Client, partition_config: PartitionConfig) -> Self {
        Self {
            client,
            partition_config,
            cycle_cache: HashMap::new(),
        }
    }

    /// Check budget status, returning cached value if available.
    pub async fn check_budget(
        &mut self,
        budget_id: &BudgetId,
    ) -> Result<BudgetStatus, ferriskey::Error> {
        // Return cached status if available
        if let Some(status) = self.cycle_cache.get(budget_id) {
            return Ok(status.clone());
        }

        // Compute partition and read from Valkey
        let partition = budget_partition(budget_id, &self.partition_config);
        let tag = partition.hash_tag();

        let usage_key = format!("ff:budget:{}:{}:usage", tag, budget_id);
        let limits_key = format!("ff:budget:{}:{}:limits", tag, budget_id);

        // Read usage + limits in parallel (both on same {b:M} slot)
        let usage_raw: Vec<String> = self
            .client
            .cmd("HGETALL")
            .arg(&usage_key)
            .execute()
            .await
            .unwrap_or_default();

        let limits_raw: Vec<String> = self
            .client
            .cmd("HGETALL")
            .arg(&limits_key)
            .execute()
            .await
            .unwrap_or_default();

        let status = evaluate_budget_status(&usage_raw, &limits_raw);

        self.cycle_cache.insert(budget_id.clone(), status.clone());
        Ok(status)
    }

    /// Clear the per-cycle cache. Call at the start of each scheduling cycle.
    pub fn clear_cache(&mut self) {
        self.cycle_cache.clear();
    }

    /// Number of cached budget statuses.
    pub fn cache_size(&self) -> usize {
        self.cycle_cache.len()
    }
}

/// Evaluate budget status from raw usage and limits hashes.
fn evaluate_budget_status(usage_raw: &[String], limits_raw: &[String]) -> BudgetStatus {
    if limits_raw.is_empty() {
        return BudgetStatus::default(); // No limits = not breached
    }

    let mut hard_breached = false;
    let mut soft_breached = false;

    // Parse limits: fields are "hard:<dimension>" and "soft:<dimension>"
    let mut i = 0;
    while i + 1 < limits_raw.len() {
        let field = &limits_raw[i];
        let limit_val: i64 = limits_raw[i + 1].parse().unwrap_or(i64::MAX);
        i += 2;

        // Extract dimension name and limit type
        if let Some(dim) = field.strip_prefix("hard:") {
            let current = find_usage(usage_raw, dim);
            if current >= limit_val {
                hard_breached = true;
            }
        } else if let Some(dim) = field.strip_prefix("soft:") {
            let current = find_usage(usage_raw, dim);
            if current >= limit_val {
                soft_breached = true;
            }
        }
    }

    // Also check flat "breached_at" marker from reconciler
    for j in (0..usage_raw.len()).step_by(2) {
        if usage_raw[j] == "breached_at" {
            hard_breached = true;
            break;
        }
    }

    BudgetStatus {
        hard_breached,
        soft_breached,
    }
}

/// Find a dimension's usage value in the flat usage array.
fn find_usage(usage_raw: &[String], dimension: &str) -> i64 {
    let mut i = 0;
    while i + 1 < usage_raw.len() {
        if usage_raw[i] == dimension {
            return usage_raw[i + 1].parse().unwrap_or(0);
        }
        i += 2;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_limits_means_not_breached() {
        let status = evaluate_budget_status(&[], &[]);
        assert!(!status.hard_breached);
        assert!(!status.soft_breached);
    }

    #[test]
    fn hard_breach_detected() {
        let usage = vec!["tokens".into(), "1000".into()];
        let limits = vec!["hard:tokens".into(), "500".into()];
        let status = evaluate_budget_status(&usage, &limits);
        assert!(status.hard_breached);
    }

    #[test]
    fn under_limit_not_breached() {
        let usage = vec!["tokens".into(), "100".into()];
        let limits = vec!["hard:tokens".into(), "500".into()];
        let status = evaluate_budget_status(&usage, &limits);
        assert!(!status.hard_breached);
    }

    #[test]
    fn soft_breach_detected() {
        let usage = vec!["cost_cents".into(), "8000".into()];
        let limits = vec![
            "hard:cost_cents".into(), "10000".into(),
            "soft:cost_cents".into(), "7500".into(),
        ];
        let status = evaluate_budget_status(&usage, &limits);
        assert!(!status.hard_breached);
        assert!(status.soft_breached);
    }
}
