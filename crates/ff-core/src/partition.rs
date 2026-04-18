use crate::types::{BudgetId, ExecutionId, FlowId, LaneId, QuotaPolicyId};
use serde::{Deserialize, Serialize};

/// The partition families in FlowFabric.
///
/// Post-RFC-011: `Execution` and `Flow` are now routing **aliases** —
/// both produce the same `{fp:N}` hash-tag, because execution keys
/// co-locate with their parent flow's partition under hash-tag
/// co-location. The `Execution` variant is **deliberately retained**
/// per RFC-011 §11 Non-goals ("Deleting `PartitionFamily::Execution`
/// from the public API. The variant stays for API compatibility; only
/// its routing behaviour changes."), so downstream crates like
/// cairn-fabric that construct `Partition { family: Execution, .. }`
/// continue to compile and route correctly without source changes.
///
/// New FF-internal code should prefer `PartitionFamily::Flow` for
/// clarity — the `Execution` alias exists solely to preserve the
/// public-API contract promised by RFC-011. The logical distinction
/// between exec-scoped and flow-scoped keys continues to live in the
/// key-name prefix (`ff:exec:*` vs `ff:flow:*`), not in the
/// `PartitionFamily` discriminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PartitionFamily {
    /// Flow-structural + execution partition: `{fp:N}` — flow topology
    /// and all per-execution keys co-located with their parent flow.
    Flow,
    /// Execution partition — **routing alias for [`PartitionFamily::Flow`]**
    /// under RFC-011 co-location. Produces `{fp:N}` hash-tags identical to
    /// `Flow` and indexes into `num_flow_partitions`. Kept as a distinct
    /// variant per RFC-011 §11 for downstream-API compatibility (cairn-
    /// fabric and other consumers that construct this variant directly).
    Execution,
    /// Budget partition: `{b:M}` — budget definitions and usage.
    Budget,
    /// Quota partition: `{q:K}` — quota policies and sliding windows.
    Quota,
}

impl PartitionFamily {
    /// Hash tag prefix for this family.
    ///
    /// `Flow` and `Execution` are aliases and both return `"fp"` — see
    /// the enum-level rustdoc for the RFC-011 §11 compatibility rationale.
    fn prefix(self) -> &'static str {
        match self {
            Self::Flow | Self::Execution => "fp",
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
    ///
    /// `Flow` and `Execution` return the same value (`num_flow_partitions`)
    /// — they are routing aliases under RFC-011 co-location.
    pub fn count_for(&self, family: PartitionFamily) -> u16 {
        match family {
            PartitionFamily::Flow | PartitionFamily::Execution => self.num_flow_partitions,
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
    // `Execution` family preserves exec-scoped semantics at the metadata
    // layer (logs, tracing spans, metric labels) even though routing is
    // aliased to `Flow`'s hash-tag prefix under RFC-011 co-location.
    // See PartitionFamily enum rustdoc for the alias contract.
    Partition {
        family: PartitionFamily::Execution,
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

/// Strategy for picking a solo execution's partition from its lane id.
///
/// RFC-011 §5.6 defines the birthday-paradox traffic-amplification
/// mitigation: solo execs hash their lane id to a flow partition with
/// crc16, which can collide with a flow's own partition. Operators
/// that hit a persistent collision install a custom strategy at boot
/// time via [`solo_partition_with`] instead of rebuilding `ff-server`.
///
/// The default impl [`Crc16SoloPartitioner`] matches the algorithm
/// used by every other partition family. Replace only when a real
/// collision surfaces via `ff-server admin partition-collisions`.
pub trait SoloPartitioner: Send + Sync {
    /// Return the partition index for a solo execution on the given lane.
    ///
    /// Must return a value in `0..config.num_flow_partitions`.
    /// Must be deterministic — the same `(lane, config)` always produces
    /// the same index (violated determinism → exec keys would route
    /// differently on each mint, breaking all cross-lookup invariants).
    fn partition_for_lane(&self, lane: &LaneId, config: &PartitionConfig) -> u16;
}

/// Default [`SoloPartitioner`]: `crc16_ccitt(lane_utf8) % num_flow_partitions`.
///
/// Matches the hashing used by [`flow_partition`], [`budget_partition`],
/// [`quota_partition`] — same crc16-CCITT algorithm Valkey Cluster uses
/// for slot assignment.
#[derive(Clone, Copy, Debug, Default)]
pub struct Crc16SoloPartitioner;

impl SoloPartitioner for Crc16SoloPartitioner {
    fn partition_for_lane(&self, lane: &LaneId, config: &PartitionConfig) -> u16 {
        assert!(
            config.num_flow_partitions > 0,
            "num_flow_partitions must be > 0 (division by zero)"
        );
        crc16_ccitt(lane.as_str().as_bytes()) % config.num_flow_partitions
    }
}

/// Compute the partition for a solo (flow-less) execution's lane shard.
///
/// Uses the default [`Crc16SoloPartitioner`]. Deployments that hit a
/// traffic-amplification hotspot (per RFC-011 §5.6) should call
/// [`solo_partition_with`] with a custom partitioner instead.
pub fn solo_partition(lane: &LaneId, config: &PartitionConfig) -> Partition {
    solo_partition_with(lane, config, &Crc16SoloPartitioner)
}

/// Compute the partition for a solo execution using a custom
/// [`SoloPartitioner`] strategy.
///
/// The operator-facing escape hatch for RFC-011 §5.6 traffic-amplification
/// collisions. A deployment that observes a collision via
/// `ff-server admin partition-collisions` instantiates an alternate
/// [`SoloPartitioner`] impl and routes solo mint paths through this
/// function. The default [`Crc16SoloPartitioner`] is used by
/// [`solo_partition`] and [`ExecutionId::solo`] — neither signature
/// changes under this extension point.
pub fn solo_partition_with(
    lane: &LaneId,
    config: &PartitionConfig,
    partitioner: &dyn SoloPartitioner,
) -> Partition {
    // Solo execs are execution-scoped — preserve `Execution` family at
    // the metadata layer. Routing is aliased to `Flow`'s `{fp:N}` tag
    // under RFC-011 co-location; see PartitionFamily rustdoc for the
    // alias contract.
    Partition {
        family: PartitionFamily::Execution,
        index: partitioner.partition_for_lane(lane, config),
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

        let p = Partition { family: PartitionFamily::Execution, index: 7 };
        assert_eq!(p.hash_tag(), "{fp:7}", "Execution must alias Flow (RFC-011 §11)");

        let p = Partition { family: PartitionFamily::Budget, index: 0 };
        assert_eq!(p.hash_tag(), "{b:0}");

        let p = Partition { family: PartitionFamily::Quota, index: 31 };
        assert_eq!(p.hash_tag(), "{q:31}");
    }

    /// Execution and Flow are deliberate routing aliases post-RFC-011 §11.
    /// This test pins the alias contract so a future edit that diverges the
    /// two routes fails loudly rather than silently breaking co-location.
    #[test]
    fn execution_family_aliases_flow() {
        // Same hash-tag at every index.
        for index in [0u16, 1, 7, 42, 255, 65535] {
            let flow = Partition { family: PartitionFamily::Flow, index };
            let exec = Partition { family: PartitionFamily::Execution, index };
            assert_eq!(
                flow.hash_tag(),
                exec.hash_tag(),
                "Flow and Execution must produce identical hash-tags at index {index}"
            );
        }

        // count_for returns the same value.
        let config = PartitionConfig::default();
        assert_eq!(
            config.count_for(PartitionFamily::Flow),
            config.count_for(PartitionFamily::Execution),
            "count_for(Flow) == count_for(Execution) — both route via num_flow_partitions"
        );

        // A Partition minted with family=Execution produces the same
        // hash-tag as one minted with family=Flow, given the same index.
        // This is the key property cairn-fabric (and any other consumer
        // that constructs `Partition { family: Execution, .. }`) depends on.
        let p_exec = Partition { family: PartitionFamily::Execution, index: 42 };
        let p_flow = Partition { family: PartitionFamily::Flow, index: 42 };
        assert_eq!(p_exec.hash_tag(), p_flow.hash_tag());
        assert_eq!(p_exec.hash_tag(), "{fp:42}");
    }

    #[test]
    fn all_families_produce_distinct_tags() {
        // Post-RFC-011: Flow and Execution deliberately share `{fp:N}` — see
        // `execution_family_aliases_flow`. The three *distinct* tag spaces are
        // {fp:N}, {b:N}, {q:N}. This test asserts that alias-after-collapse.
        let tags: Vec<String> = [
            PartitionFamily::Flow,
            PartitionFamily::Budget,
            PartitionFamily::Quota,
        ]
        .iter()
        .map(|f| Partition { family: *f, index: 0 }.hash_tag())
        .collect();
        let unique: std::collections::HashSet<&String> = tags.iter().collect();
        assert_eq!(unique.len(), 3, "flow/budget/quota must produce distinct hash tags");
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
        // Indices match (RFC-011 co-location) but family is Execution —
        // preserves exec-scoped semantics at the metadata layer.
        assert_eq!(ep.index, fp.index);
        assert_eq!(ep.family, PartitionFamily::Execution);
        // Alias contract: Execution and Flow produce identical hash-tags.
        assert_eq!(ep.hash_tag(), fp.hash_tag());
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
        // Solo execs are execution-scoped — family is Execution, routing
        // is aliased to Flow's `{fp:N}` via PartitionFamily's prefix map.
        assert_eq!(p1.family, PartitionFamily::Execution);
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

    // ── SoloPartitioner trait (RFC-011 §5.6 mitigation #3) ──

    #[test]
    fn crc16_solo_partitioner_matches_legacy_behavior() {
        // The default Crc16SoloPartitioner must produce the same index as
        // the pre-trait solo_partition() function — otherwise installing
        // the trait under an existing deployment would silently re-route
        // every solo exec.
        let config = PartitionConfig::default();
        let lane = LaneId::new("workers-a");
        let default_idx = Crc16SoloPartitioner.partition_for_lane(&lane, &config);
        let expected = crc16::State::<crc16::XMODEM>::calculate(lane.as_str().as_bytes())
            % config.num_flow_partitions;
        assert_eq!(default_idx, expected);
    }

    #[test]
    fn solo_partition_with_custom_partitioner_routes_through_trait() {
        // Stub partitioner always returns index 0; a custom impl must
        // override the default routing.
        struct AlwaysZero;
        impl SoloPartitioner for AlwaysZero {
            fn partition_for_lane(&self, _lane: &LaneId, _config: &PartitionConfig) -> u16 {
                0
            }
        }
        let config = PartitionConfig::default();
        let lane = LaneId::new("pick-me");
        let p = solo_partition_with(&lane, &config, &AlwaysZero);
        assert_eq!(p.index, 0);
        // Solo execs → Execution family (aliases Flow for routing).
        assert_eq!(p.family, PartitionFamily::Execution);
    }

    #[test]
    fn solo_partition_default_matches_solo_partition_with_crc16() {
        // solo_partition() and solo_partition_with(Crc16SoloPartitioner)
        // must produce identical Partitions — this pins the default impl
        // as the identity override.
        let config = PartitionConfig::default();
        let lane = LaneId::new("workers-b");
        let default = solo_partition(&lane, &config);
        let explicit = solo_partition_with(&lane, &config, &Crc16SoloPartitioner);
        assert_eq!(default, explicit);
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
