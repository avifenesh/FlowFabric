use crate::types::{BudgetId, ExecutionId, FlowId, QuotaPolicyId};
use serde::{Deserialize, Serialize};

/// The four partition families in FlowFabric.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionFamily {
    /// Execution partition: `{p:N}` — all per-execution keys.
    Execution,
    /// Flow-structural partition: `{fp:N}` — flow topology.
    Flow,
    /// Budget partition: `{b:M}` — budget definitions and usage.
    Budget,
    /// Quota partition: `{q:K}` — quota policies and sliding windows.
    Quota,
}

impl PartitionFamily {
    /// Hash tag prefix for this family.
    fn prefix(self) -> &'static str {
        match self {
            Self::Execution => "p",
            Self::Flow => "fp",
            Self::Budget => "b",
            Self::Quota => "q",
        }
    }
}

/// Partition counts for each family. Fixed at deployment time.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionConfig {
    pub num_execution_partitions: u16,
    pub num_flow_partitions: u16,
    pub num_budget_partitions: u16,
    pub num_quota_partitions: u16,
}

impl Default for PartitionConfig {
    fn default() -> Self {
        Self {
            num_execution_partitions: 256,
            num_flow_partitions: 64,
            num_budget_partitions: 32,
            num_quota_partitions: 32,
        }
    }
}

impl PartitionConfig {
    /// Get the partition count for a given family.
    pub fn count_for(&self, family: PartitionFamily) -> u16 {
        match family {
            PartitionFamily::Execution => self.num_execution_partitions,
            PartitionFamily::Flow => self.num_flow_partitions,
            PartitionFamily::Budget => self.num_budget_partitions,
            PartitionFamily::Quota => self.num_quota_partitions,
        }
    }
}

/// A resolved partition within a family.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Partition {
    pub family: PartitionFamily,
    pub index: u16,
}

impl Partition {
    /// Returns the Valkey hash tag for this partition, e.g. `{p:42}`, `{fp:7}`.
    pub fn hash_tag(&self) -> String {
        format!("{{{prefix}:{index}}}", prefix = self.family.prefix(), index = self.index)
    }
}

impl std::fmt::Display for Partition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.hash_tag())
    }
}

/// Compute CRC16-CCITT of the given bytes, same algorithm as Valkey Cluster.
fn crc16_ccitt(bytes: &[u8]) -> u16 {
    crc16::State::<crc16::XMODEM>::calculate(bytes)
}

/// Compute the partition index for a UUID-based entity.
/// Panics if `num_partitions` is 0 — this is a configuration error.
fn partition_for_uuid(uuid_bytes: &[u8; 16], num_partitions: u16) -> u16 {
    assert!(num_partitions > 0, "num_partitions must be > 0 (division by zero)");
    crc16_ccitt(uuid_bytes) % num_partitions
}

/// Compute the partition for an execution ID.
pub fn execution_partition(eid: &ExecutionId, config: &PartitionConfig) -> Partition {
    Partition {
        family: PartitionFamily::Execution,
        index: partition_for_uuid(eid.as_bytes(), config.num_execution_partitions),
    }
}

/// Compute the partition for a flow ID.
pub fn flow_partition(fid: &FlowId, config: &PartitionConfig) -> Partition {
    Partition {
        family: PartitionFamily::Flow,
        index: partition_for_uuid(fid.as_bytes(), config.num_flow_partitions),
    }
}

/// Compute the partition for a budget ID.
pub fn budget_partition(bid: &BudgetId, config: &PartitionConfig) -> Partition {
    Partition {
        family: PartitionFamily::Budget,
        index: partition_for_uuid(bid.as_bytes(), config.num_budget_partitions),
    }
}

/// Compute the partition for a quota policy (by scope ID).
pub fn quota_partition(qid: &QuotaPolicyId, config: &PartitionConfig) -> Partition {
    Partition {
        family: PartitionFamily::Quota,
        index: partition_for_uuid(qid.as_bytes(), config.num_quota_partitions),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn partition_determinism() {
        let config = PartitionConfig::default();
        let eid = ExecutionId::parse("550e8400-e29b-41d4-a716-446655440000").unwrap();
        let p1 = execution_partition(&eid, &config);
        let p2 = execution_partition(&eid, &config);
        assert_eq!(p1, p2, "partition must be deterministic for same ID");
    }

    #[test]
    fn partition_hash_tag_format() {
        let p = Partition {
            family: PartitionFamily::Execution,
            index: 42,
        };
        assert_eq!(p.hash_tag(), "{p:42}");

        let p = Partition {
            family: PartitionFamily::Flow,
            index: 7,
        };
        assert_eq!(p.hash_tag(), "{fp:7}");

        let p = Partition {
            family: PartitionFamily::Budget,
            index: 0,
        };
        assert_eq!(p.hash_tag(), "{b:0}");

        let p = Partition {
            family: PartitionFamily::Quota,
            index: 31,
        };
        assert_eq!(p.hash_tag(), "{q:31}");
    }

    #[test]
    fn partition_in_range() {
        let config = PartitionConfig::default();
        // Test many random IDs stay in range
        for _ in 0..1000 {
            let eid = ExecutionId::new();
            let p = execution_partition(&eid, &config);
            assert!(
                p.index < config.num_execution_partitions,
                "partition index {} out of range for {} partitions",
                p.index,
                config.num_execution_partitions
            );
        }
    }

    #[test]
    fn partition_distribution() {
        // Verify we get at least some spread across partitions with 1000 random IDs
        let config = PartitionConfig {
            num_execution_partitions: 16,
            ..Default::default()
        };
        let mut seen = std::collections::HashSet::new();
        for _ in 0..1000 {
            let eid = ExecutionId::new();
            let p = execution_partition(&eid, &config);
            seen.insert(p.index);
        }
        // With 1000 IDs and 16 partitions, we should hit at least 12
        assert!(
            seen.len() >= 12,
            "poor distribution: only {} of 16 partitions hit",
            seen.len()
        );
    }

    #[test]
    fn all_families_produce_distinct_tags() {
        // Same numeric index, different families = different hash tags
        let tags: Vec<String> = [
            PartitionFamily::Execution,
            PartitionFamily::Flow,
            PartitionFamily::Budget,
            PartitionFamily::Quota,
        ]
        .iter()
        .map(|f| Partition { family: *f, index: 0 }.hash_tag())
        .collect();

        // All must be unique
        let unique: std::collections::HashSet<&String> = tags.iter().collect();
        assert_eq!(unique.len(), 4, "all families must produce distinct hash tags");
    }

    #[test]
    fn flow_partition_determinism() {
        let config = PartitionConfig::default();
        let fid = FlowId::new();
        let p1 = flow_partition(&fid, &config);
        let p2 = flow_partition(&fid, &config);
        assert_eq!(p1, p2);
        assert_eq!(p1.family, PartitionFamily::Flow);
        assert!(p1.index < config.num_flow_partitions);
    }

    #[test]
    fn budget_partition_determinism() {
        let config = PartitionConfig::default();
        let bid = BudgetId::new();
        let p1 = budget_partition(&bid, &config);
        let p2 = budget_partition(&bid, &config);
        assert_eq!(p1, p2);
        assert_eq!(p1.family, PartitionFamily::Budget);
        assert!(p1.index < config.num_budget_partitions);
    }

    #[test]
    fn default_config_values() {
        let config = PartitionConfig::default();
        assert_eq!(config.num_execution_partitions, 256);
        assert_eq!(config.num_flow_partitions, 64);
        assert_eq!(config.num_budget_partitions, 32);
        assert_eq!(config.num_quota_partitions, 32);
    }
}
