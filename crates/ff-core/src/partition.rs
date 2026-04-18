use crate::types::{BudgetId, ExecutionId, FlowId, LaneId, QuotaPolicyId};
use serde::{Deserialize, Serialize};

/// The partition families in FlowFabric.
///
/// Post-RFC-011: `Execution` is retired as a separate family. Execution
/// keys co-locate with their parent flow's partition (same `{fp:N}`
/// hash-tag), so they route through [`PartitionFamily::Flow`] physically
/// while keeping the logical `ff:exec:*` vs `ff:flow:*` key-name prefix.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionFamily {
    /// Flow-structural + execution partition: `{fp:N}` — flow topology
    /// and all per-execution keys co-located with their parent flow.
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
            Self::Flow => "fp",
            Self::Budget => "b",
            Self::Quota => "q",
        }
    }
}

/// Partition counts for each family. Fixed at deployment time.
///
/// Post-RFC-011: `num_execution_partitions` is retired. All execution
/// keys route via `num_flow_partitions` (exec + flow share a slot under
/// hash-tag co-location). Default bumped 64 → 256 to preserve today's
/// total keyspace fanout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartitionConfig {
    pub num_flow_partitions: u16,
    pub num_budget_partitions: u16,
    pub num_quota_partitions: u16,
}

impl Default for PartitionConfig {
    fn default() -> Self {
        Self {
            num_flow_partitions: 256,
            num_budget_partitions: 32,
            num_quota_partitions: 32,
        }
    }
}

impl PartitionConfig {
    /// Get the partition count for a given family.
    pub fn count_for(&self, family: PartitionFamily) -> u16 {
        match family {
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
    /// Returns the Valkey hash tag for this partition, e.g. `{fp:7}`, `{b:0}`.
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
///
/// Post-RFC-011: decodes the hash-tag prefix out of the id string; does NOT
/// re-hash the UUID. The partition index is baked into the id at mint time
/// via [`ExecutionId::for_flow`] / [`ExecutionId::solo`]. The `config` arg
/// is retained for API symmetry with the other partition functions, but is
/// unused — the exec id carries its partition intrinsically.
pub fn execution_partition(eid: &ExecutionId, _config: &PartitionConfig) -> Partition {
    Partition {
        family: PartitionFamily::Flow,
        index: eid.partition(),
    }
}

/// Compute the partition for a flow ID.
pub fn flow_partition(fid: &FlowId, config: &PartitionConfig) -> Partition {
    Partition {
        family: PartitionFamily::Flow,
        index: partition_for_uuid(fid.as_bytes(), config.num_flow_partitions),
    }
}

/// Compute the partition for a solo (flow-less) execution's lane shard.
///
/// Solo execs (bare `create_execution`, no parent flow) derive their
/// partition from the lane id instead of a flow id. Ensures per-lane
/// co-location (RFC-011 §4).
pub fn solo_partition(lane: &LaneId, config: &PartitionConfig) -> Partition {
    assert!(
        config.num_flow_partitions > 0,
        "num_flow_partitions must be > 0 (division by zero)"
    );
    Partition {
        family: PartitionFamily::Flow,
        index: crc16_ccitt(lane.as_str().as_bytes()) % config.num_flow_partitions,
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
    fn partition_hash_tag_format() {
        let p = Partition { family: PartitionFamily::Flow, index: 7 };
        assert_eq!(p.hash_tag(), "{fp:7}");

        let p = Partition { family: PartitionFamily::Budget, index: 0 };
        assert_eq!(p.hash_tag(), "{b:0}");

        let p = Partition { family: PartitionFamily::Quota, index: 31 };
        assert_eq!(p.hash_tag(), "{q:31}");
    }

    #[test]
    fn all_families_produce_distinct_tags() {
        let tags: Vec<String> = [
            PartitionFamily::Flow,
            PartitionFamily::Budget,
            PartitionFamily::Quota,
        ]
        .iter()
        .map(|f| Partition { family: *f, index: 0 }.hash_tag())
        .collect();
        let unique: std::collections::HashSet<&String> = tags.iter().collect();
        assert_eq!(unique.len(), 3, "all families must produce distinct hash tags");
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
        assert_eq!(config.num_flow_partitions, 256);
        assert_eq!(config.num_budget_partitions, 32);
        assert_eq!(config.num_quota_partitions, 32);
    }

    // ── RFC-011 phase-1 tests ──

    #[test]
    fn execution_id_for_flow_determinism() {
        let config = PartitionConfig::default();
        let fid = FlowId::new();
        let a = ExecutionId::for_flow(&fid, &config);
        let b = ExecutionId::for_flow(&fid, &config);
        // Same flow → same partition (UUID suffix differs).
        assert_eq!(a.partition(), b.partition());
    }

    #[test]
    fn execution_id_solo_determinism() {
        let config = PartitionConfig::default();
        let lane = LaneId::new("workers-a");
        let a = ExecutionId::solo(&lane, &config);
        let b = ExecutionId::solo(&lane, &config);
        // Same lane → same partition.
        assert_eq!(a.partition(), b.partition());
    }

    #[test]
    fn execution_id_partition_matches_flow_partition() {
        let config = PartitionConfig::default();
        let fid = FlowId::new();
        let eid = ExecutionId::for_flow(&fid, &config);
        let fp = flow_partition(&fid, &config);
        assert_eq!(eid.partition(), fp.index);
        let ep = execution_partition(&eid, &config);
        assert_eq!(ep.index, fp.index);
        assert_eq!(ep.family, PartitionFamily::Flow);
    }

    #[test]
    fn execution_partition_reads_hash_tag_not_uuid() {
        // Construct an ExecutionId with a KNOWN partition (0) and a UUID
        // whose crc16-partition lands somewhere else. execution_partition
        // must pick the hash-tag (0), not re-hash the UUID.
        let known_uuid = "550e8400-e29b-41d4-a716-446655440000";
        let s = format!("{{fp:0}}:{known_uuid}");
        let eid = ExecutionId::parse(&s).unwrap();
        let config = PartitionConfig::default();
        let p = execution_partition(&eid, &config);
        assert_eq!(p.index, 0, "must read hash-tag, not re-hash UUID");
    }

    #[test]
    fn execution_partition_ignores_config_value() {
        // Safety test per manager ask #7 (edge case 3):
        // An id minted in one config must decode to the same partition
        // in any other config — the partition is baked into the id,
        // not computed from the config.
        let small = PartitionConfig { num_flow_partitions: 4, ..Default::default() };
        let fid = FlowId::new();
        let eid = ExecutionId::for_flow(&fid, &small);
        let minted_partition = eid.partition();

        // Decode via a different config — must match.
        let big = PartitionConfig { num_flow_partitions: 1024, ..Default::default() };
        let p = execution_partition(&eid, &big);
        assert_eq!(
            p.index, minted_partition,
            "hash-tag is authoritative; config value must not change decoding"
        );
    }

    #[test]
    fn execution_id_parse_rejects_bare_uuid() {
        let bare = "550e8400-e29b-41d4-a716-446655440000";
        match ExecutionId::parse(bare) {
            Err(crate::types::ExecutionIdParseError::MissingTag(_)) => {}
            other => panic!("expected MissingTag, got {other:?}"),
        }
    }

    #[test]
    fn execution_id_parse_accepts_wellformed_shape() {
        let s = "{fp:42}:550e8400-e29b-41d4-a716-446655440000";
        let eid = ExecutionId::parse(s).unwrap();
        assert_eq!(eid.partition(), 42);
        assert_eq!(eid.as_str(), s);
    }

    #[test]
    fn execution_id_parse_rejects_bad_partition_index() {
        // Non-integer partition
        match ExecutionId::parse("{fp:xx}:550e8400-e29b-41d4-a716-446655440000") {
            Err(crate::types::ExecutionIdParseError::InvalidPartitionIndex(_)) => {}
            other => panic!("expected InvalidPartitionIndex, got {other:?}"),
        }
        // u16-overflow partition (65536)
        match ExecutionId::parse("{fp:65536}:550e8400-e29b-41d4-a716-446655440000") {
            Err(crate::types::ExecutionIdParseError::InvalidPartitionIndex(_)) => {}
            other => panic!("expected InvalidPartitionIndex for u16 overflow, got {other:?}"),
        }
    }

    #[test]
    fn execution_id_parse_rejects_bad_uuid() {
        match ExecutionId::parse("{fp:0}:not-a-uuid") {
            Err(crate::types::ExecutionIdParseError::InvalidUuid(_)) => {}
            other => panic!("expected InvalidUuid, got {other:?}"),
        }
    }

    #[test]
    fn solo_partition_determinism() {
        let config = PartitionConfig::default();
        let lane = LaneId::new("workers-a");
        let p1 = solo_partition(&lane, &config);
        let p2 = solo_partition(&lane, &config);
        assert_eq!(p1, p2);
        assert_eq!(p1.family, PartitionFamily::Flow);
        assert!(p1.index < config.num_flow_partitions);
    }

    #[test]
    fn solo_partition_different_lanes_usually_differ() {
        // Not strict — collisions are possible (and §5.6 acknowledges) —
        // but over 100 distinct lane names we expect most to be distinct.
        let config = PartitionConfig::default();
        let mut seen = std::collections::HashSet::new();
        for i in 0..100 {
            let lane = LaneId::new(format!("lane-{i}"));
            let p = solo_partition(&lane, &config);
            seen.insert(p.index);
        }
        // At 256 partitions with 100 inputs, expect >50 distinct.
        assert!(
            seen.len() > 50,
            "solo_partition distribution too narrow: only {} distinct of 100",
            seen.len()
        );
    }

    #[test]
    fn execution_id_serde_via_deserialize_validates() {
        // Valid shape deserialises.
        let json = r#""{fp:0}:550e8400-e29b-41d4-a716-446655440000""#;
        let eid: ExecutionId = serde_json::from_str(json).unwrap();
        assert_eq!(eid.partition(), 0);

        // Bare UUID fails Deserialize.
        let bare = r#""550e8400-e29b-41d4-a716-446655440000""#;
        assert!(serde_json::from_str::<ExecutionId>(bare).is_err());
    }
}
