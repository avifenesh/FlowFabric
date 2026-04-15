//! Budget reset scanner.
//!
//! Scans `ff:idx:{b:M}:budget_resets` per budget partition, finding budgets
//! whose `next_reset_at` score is <= now. For each, calls
//! `FCALL ff_reset_budget` which zeroes usage counters, records the reset
//! event, computes the next reset time, and re-scores in the index.
//!
//! Reference: RFC-008 §Budget Reset, RFC-010 §6.11

use std::time::Duration;

use ff_core::keys::budget_resets_key;
use ff_core::partition::{Partition, PartitionFamily};

use super::{ScanResult, Scanner};

const BATCH_SIZE: u32 = 20;

pub struct BudgetResetScanner {
    interval: Duration,
}

impl BudgetResetScanner {
    pub fn new(interval: Duration) -> Self {
        Self { interval }
    }
}

impl Scanner for BudgetResetScanner {
    fn name(&self) -> &'static str {
        "budget_reset"
    }

    fn interval(&self) -> Duration {
        self.interval
    }

    async fn scan_partition(
        &self,
        client: &ferriskey::Client,
        partition: u16,
    ) -> ScanResult {
        let p = Partition {
            family: PartitionFamily::Budget,
            index: partition,
        };
        let tag = p.hash_tag();
        let resets_key = budget_resets_key(&tag);

        let now_ms = match crate::scanner::lease_expiry::server_time_ms(client).await {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!(partition, error = %e, "budget_reset: failed to get server time");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        // ZRANGEBYSCORE budget_resets -inf now LIMIT 0 batch_size
        let due: Vec<String> = match client
            .cmd("ZRANGEBYSCORE")
            .arg(&resets_key)
            .arg("-inf")
            .arg(now_ms.to_string().as_str())
            .arg("LIMIT")
            .arg("0")
            .arg(BATCH_SIZE.to_string().as_str())
            .execute()
            .await
        {
            Ok(ids) => ids,
            Err(e) => {
                tracing::warn!(partition, error = %e, "budget_reset: ZRANGEBYSCORE failed");
                return ScanResult { processed: 0, errors: 1 };
            }
        };

        if due.is_empty() {
            return ScanResult { processed: 0, errors: 0 };
        }

        let mut processed: u32 = 0;
        let mut errors: u32 = 0;

        for budget_id_str in &due {
            // FCALL ff_reset_budget on {b:M}
            // KEYS (3): budget_def, budget_usage, budget_resets_zset
            // ARGV (1): budget_id
            let budget_def = format!("ff:budget:{}:{}", tag, budget_id_str);
            let budget_usage = format!("ff:budget:{}:{}:usage", tag, budget_id_str);

            let keys: [&str; 3] = [&budget_def, &budget_usage, &resets_key];
            let now_s = now_ms.to_string();
            let argv: [&str; 2] = [budget_id_str.as_str(), &now_s];

            match client
                .fcall::<ferriskey::Value>("ff_reset_budget", &keys, &argv)
                .await
            {
                Ok(_) => processed += 1,
                Err(e) => {
                    tracing::warn!(
                        partition,
                        budget_id = budget_id_str.as_str(),
                        error = %e,
                        "budget_reset: ff_reset_budget failed"
                    );
                    errors += 1;
                }
            }
        }

        ScanResult { processed, errors }
    }
}
