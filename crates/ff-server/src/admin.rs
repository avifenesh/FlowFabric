//! Administrative subcommands for operator use.
//!
//! Invoked as `ff-server admin <subcommand> [args]`. Subcommands are
//! read-only probes that share the `ff-server` binary + env-var conventions
//! but load only the minimal config subset they need (see
//! [`load_probe_inputs`]). They do not connect to Valkey, start HTTP, or
//! spawn scanners. Each probe completes synchronously and exits.
//!
//! # Subcommands
//!
//! - `partition-collisions` — RFC-011 §5.6 observability. Computes the
//!   solo-partition assignment for every configured lane and reports any
//!   that share a partition with another lane (birthday-paradox collision).

use ff_core::partition::{solo_partition, PartitionConfig};
use ff_core::types::LaneId;

/// Load the minimal config subset the probe needs, directly from env.
///
/// Unlike [`crate::config::ServerConfig::from_env`], this skips the HMAC
/// secret, CORS, listener address, and engine intervals — a probe doesn't
/// bind HTTP, start scanners, or touch signalling, so demanding those
/// variables just creates operator-facing friction. Reads:
///
/// - `FF_LANES` (default `"default"`)
/// - `FF_FLOW_PARTITIONS` (default `256`)
/// - `FF_BUDGET_PARTITIONS` (default `32`) — present for struct symmetry, unused by the collisions probe
/// - `FF_QUOTA_PARTITIONS` (default `32`) — same
///
/// Returns `Err(String)` with an operator-actionable message on invalid
/// values (empty `FF_LANES`, non-positive partition count, etc.).
pub fn load_probe_inputs() -> Result<(Vec<LaneId>, PartitionConfig), String> {
    // Parse + validate each lane name via try_new (length, ASCII-printable
    // shape per types.rs LANE_ID_MAX_BYTES). Reject duplicates so the
    // collision table is not polluted by a miscounted total.
    let raw = std::env::var("FF_LANES").unwrap_or_else(|_| "default".to_string());
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut lanes: Vec<LaneId> = Vec::new();
    for token in raw.split(',') {
        let trimmed = token.trim();
        if trimmed.is_empty() {
            continue;
        }
        let lane = LaneId::try_new(trimmed).map_err(|e| {
            format!("FF_LANES: invalid lane name '{trimmed}': {e}")
        })?;
        if !seen.insert(lane.as_str().to_string()) {
            return Err(format!(
                "FF_LANES: duplicate lane name '{trimmed}' — remove one of the entries"
            ));
        }
        lanes.push(lane);
    }
    if lanes.is_empty() {
        return Err(
            "FF_LANES: at least one non-empty lane name is required".to_string(),
        );
    }

    let num_flow_partitions = parse_u16_positive("FF_FLOW_PARTITIONS", 256)?;
    let num_budget_partitions = parse_u16_positive("FF_BUDGET_PARTITIONS", 32)?;
    let num_quota_partitions = parse_u16_positive("FF_QUOTA_PARTITIONS", 32)?;

    Ok((
        lanes,
        PartitionConfig {
            num_flow_partitions,
            num_budget_partitions,
            num_quota_partitions,
        },
    ))
}

fn parse_u16_positive(var: &str, default: u16) -> Result<u16, String> {
    match std::env::var(var) {
        Ok(s) => {
            let n: u16 = s.parse().map_err(|_| {
                format!("{var}: '{s}' is not a valid u16 (1-65535)")
            })?;
            if n == 0 {
                return Err(format!("{var}: must be > 0"));
            }
            Ok(n)
        }
        Err(_) => Ok(default),
    }
}

/// Result of a single lane's partition assignment during the
/// partition-collisions probe.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LanePartition {
    /// The lane id as configured.
    pub lane: LaneId,
    /// The partition index the lane routes to (0..num_flow_partitions).
    pub index: u16,
    /// Lanes that collide on this same index (excluding `self`). Empty if
    /// the lane is the sole occupant of its partition.
    pub collides_with: Vec<LaneId>,
}

/// Severity classification for a collision report. Matches the runbook's
/// thresholds at `docs/rfc011-operator-runbook.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CollisionSeverity {
    /// No collisions; every lane is alone in its partition.
    Clean,
    /// Some collisions but under 5% of lanes affected. Watch, don't remediate yet.
    Watch,
    /// 5-15% of lanes collide. Worth remediating (rename a lane or bump partitions).
    Elevated,
    /// >15% of lanes collide. Remediate; hot-spot risk is real under load.
    Remediate,
}

/// Aggregated output of the partition-collisions probe.
#[derive(Debug, Clone)]
pub struct PartitionCollisionsReport {
    pub partitions: u16,
    pub total_lanes: usize,
    pub colliding_lanes: usize,
    pub severity: CollisionSeverity,
    /// Per-lane results, sorted by partition index (then by lane name within
    /// a partition) for deterministic output.
    pub entries: Vec<LanePartition>,
}

impl PartitionCollisionsReport {
    /// Compute the report for a set of configured lanes under the given
    /// partition config.
    ///
    /// Pure function — no Valkey connection, no IO. Uses
    /// [`ff_core::partition::solo_partition`] (and therefore
    /// `ff_core::partition::Crc16SoloPartitioner`). Deployments that have
    /// installed a custom `ff_core::partition::SoloPartitioner` at boot
    /// time need to feed the alternate partitioner in explicitly — not
    /// yet wired through this subcommand; the current probe assumes the
    /// default.
    pub fn compute(lanes: &[LaneId], config: &PartitionConfig) -> Self {
        // Group lane indices by the partition they hash to.
        let mut by_partition: std::collections::BTreeMap<u16, Vec<LaneId>> =
            std::collections::BTreeMap::new();
        for lane in lanes {
            let p = solo_partition(lane, config);
            by_partition.entry(p.index).or_default().push(lane.clone());
        }

        // Build per-lane entries. Sort each partition's siblings ONCE
        // up-front (O(M log M) per partition), then derive the
        // collides_with set by filtering the already-sorted slice — an
        // O(M) walk per lane. Previous shape re-sorted the siblings slice
        // for every lane in the partition, hitting O(M² log M) when a
        // partition holds many lanes. For normal deployments where most
        // partitions have at most 1 lane, both shapes are fast; the fix
        // matters only under severe collision density, but the cheaper
        // shape is also clearer to read.
        // Guard against a caller passing a lane list with duplicate names.
        // The CLI path already rejects duplicates in [`load_probe_inputs`],
        // but `compute` is reachable from library callers too. For each
        // lane, build `collides_with` by filtering the sorted sibling
        // slice but skipping exactly ONE occurrence of the same name —
        // via a `seen_self` guard. Any remaining matching entries are
        // real duplicates and correctly appear in collides_with.
        let mut entries: Vec<LanePartition> = Vec::with_capacity(lanes.len());
        let mut colliding_lanes = 0usize;
        for (index, siblings) in &by_partition {
            let mut sorted_siblings: Vec<LaneId> = siblings.clone();
            sorted_siblings.sort_by(|a, b| a.as_str().cmp(b.as_str()));
            for lane in siblings {
                let mut seen_self = false;
                let others: Vec<LaneId> = sorted_siblings
                    .iter()
                    .filter(|sib| {
                        if sib.as_str() == lane.as_str() && !seen_self {
                            // Skip the first occurrence only.
                            seen_self = true;
                            false
                        } else {
                            true
                        }
                    })
                    .cloned()
                    .collect();
                if !others.is_empty() {
                    colliding_lanes += 1;
                }
                entries.push(LanePartition {
                    lane: lane.clone(),
                    index: *index,
                    collides_with: others,
                });
            }
        }
        // Sort by (index, lane) for deterministic output. BTreeMap above
        // already orders by index; within an index we need lane ordering.
        entries.sort_by(|a, b| {
            a.index
                .cmp(&b.index)
                .then_with(|| a.lane.as_str().cmp(b.lane.as_str()))
        });

        let severity = classify_severity(colliding_lanes, lanes.len());

        Self {
            partitions: config.num_flow_partitions,
            total_lanes: lanes.len(),
            colliding_lanes,
            severity,
            entries,
        }
    }

    /// Render the report as a plain-text table, deterministic and
    /// operator-friendly. Emits to stdout in the CLI; returned as a
    /// `String` for unit testing.
    pub fn format_plain(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "FlowFabric partition-collisions probe (RFC-011 §5.6)\n\
             \n\
             num_flow_partitions: {partitions}\n\
             lanes configured:    {total}\n\
             lanes colliding:     {colliding} ({pct:.1}%)\n\
             severity:            {severity:?}\n\
             \n",
            partitions = self.partitions,
            total = self.total_lanes,
            colliding = self.colliding_lanes,
            pct = if self.total_lanes == 0 {
                0.0
            } else {
                100.0 * self.colliding_lanes as f64 / self.total_lanes as f64
            },
            severity = self.severity,
        ));

        // Lane column width adapts to the longest configured lane name
        // so tables stay aligned even with long names (LaneId::try_new
        // permits up to 64 bytes). Minimum width 16 keeps the table
        // readable on deployments with very short names.
        let lane_width = self
            .entries
            .iter()
            .map(|e| e.lane.as_str().len())
            .max()
            .unwrap_or(0)
            .max(16);
        out.push_str(&format!(
            "{:>9} | {:<width$} | collides_with\n",
            "partition",
            "lane",
            width = lane_width,
        ));
        out.push_str(&format!(
            "{} | {} | {}\n",
            "-".repeat(9),
            "-".repeat(lane_width),
            "-".repeat(40),
        ));
        for entry in &self.entries {
            let collides = if entry.collides_with.is_empty() {
                "—".to_string()
            } else {
                entry
                    .collides_with
                    .iter()
                    .map(|l| l.as_str().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            out.push_str(&format!(
                "{:>9} | {:<width$} | {}\n",
                entry.index,
                entry.lane.as_str(),
                collides,
                width = lane_width,
            ));
        }

        if self.colliding_lanes > 0 {
            // Backslash line-continuation strips leading whitespace, so the
            // numbered list is spelled as explicit "\n  1. ..." entries
            // (NOT multi-line raw strings with visually-indented lines).
            out.push('\n');
            out.push_str("Remediation (see docs/rfc011-operator-runbook.md §Partition-collision observability):\n");
            out.push_str("  1. Rename a colliding lane to hash differently (cheapest).\n");
            out.push_str("  2. Bump FF_FLOW_PARTITIONS to halve collision probability (requires clean state).\n");
            out.push_str("  3. Install a custom SoloPartitioner via solo_partition_with (advanced; requires fork).\n");
        }
        out
    }
}

fn classify_severity(colliding: usize, total: usize) -> CollisionSeverity {
    if colliding == 0 {
        return CollisionSeverity::Clean;
    }
    if total == 0 {
        return CollisionSeverity::Clean;
    }
    // Integer arithmetic to avoid floating-point boundary surprises
    // (exactly 5% and exactly 15% previously landed on the stricter
    // bucket due to float comparison with `ratio < 0.05` / `< 0.15`).
    // Runbook contract:
    //   <5%    → Watch       (colliding * 100 < 5 * total)
    //   5-15%  → Elevated    (5 * total <= colliding * 100 <= 15 * total)
    //   >15%   → Remediate   (colliding * 100 > 15 * total)
    let colliding_bp = colliding.saturating_mul(100); // basis points ×100 (not bp proper)
    let five_pct = total.saturating_mul(5);
    let fifteen_pct = total.saturating_mul(15);
    if colliding_bp < five_pct {
        CollisionSeverity::Watch
    } else if colliding_bp <= fifteen_pct {
        CollisionSeverity::Elevated
    } else {
        CollisionSeverity::Remediate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(num_flow: u16) -> PartitionConfig {
        PartitionConfig {
            num_flow_partitions: num_flow,
            num_budget_partitions: 32,
            num_quota_partitions: 32,
        }
    }

    fn lane(name: &str) -> LaneId {
        LaneId::try_new(name).expect("valid lane id")
    }

    #[test]
    fn zero_lanes_is_clean() {
        let r = PartitionCollisionsReport::compute(&[], &cfg(256));
        assert_eq!(r.total_lanes, 0);
        assert_eq!(r.colliding_lanes, 0);
        assert_eq!(r.severity, CollisionSeverity::Clean);
        assert!(r.entries.is_empty());
    }

    #[test]
    fn single_lane_is_clean() {
        let lanes = vec![lane("default")];
        let r = PartitionCollisionsReport::compute(&lanes, &cfg(256));
        assert_eq!(r.colliding_lanes, 0);
        assert_eq!(r.severity, CollisionSeverity::Clean);
        assert_eq!(r.entries.len(), 1);
        assert!(r.entries[0].collides_with.is_empty());
    }

    #[test]
    fn forced_collision_via_tiny_partition_count() {
        // 3 lanes on 1 partition — all collide.
        let lanes = vec![lane("a"), lane("b"), lane("c")];
        let r = PartitionCollisionsReport::compute(&lanes, &cfg(1));
        assert_eq!(r.colliding_lanes, 3);
        assert_eq!(r.severity, CollisionSeverity::Remediate);
        // Every entry has the other two listed.
        for entry in &r.entries {
            assert_eq!(entry.index, 0);
            assert_eq!(entry.collides_with.len(), 2);
        }
    }

    #[test]
    fn severity_thresholds() {
        // 0% collision → Clean
        assert_eq!(classify_severity(0, 100), CollisionSeverity::Clean);
        // 4% → Watch
        assert_eq!(classify_severity(4, 100), CollisionSeverity::Watch);
        // 10% → Elevated
        assert_eq!(classify_severity(10, 100), CollisionSeverity::Elevated);
        // 20% → Remediate
        assert_eq!(classify_severity(20, 100), CollisionSeverity::Remediate);
        // Boundary cases per runbook: 5% → Elevated (inclusive),
        // 15% → Elevated (inclusive), 16% → Remediate.
        assert_eq!(classify_severity(5, 100), CollisionSeverity::Elevated);
        assert_eq!(classify_severity(15, 100), CollisionSeverity::Elevated);
        assert_eq!(classify_severity(16, 100), CollisionSeverity::Remediate);
    }

    #[test]
    fn entries_sorted_deterministically() {
        // Even with lanes passed in arbitrary order, output is sorted by
        // (partition, lane). Lets operators diff two runs cleanly.
        let lanes = vec![lane("zzz"), lane("aaa"), lane("mmm")];
        let r = PartitionCollisionsReport::compute(&lanes, &cfg(256));
        for pair in r.entries.windows(2) {
            let a = &pair[0];
            let b = &pair[1];
            assert!(
                a.index < b.index
                    || (a.index == b.index && a.lane.as_str() <= b.lane.as_str()),
                "entries not sorted: {a:?} before {b:?}"
            );
        }
    }

    #[test]
    fn format_plain_clean_deployment() {
        let lanes = vec![lane("default")];
        let r = PartitionCollisionsReport::compute(&lanes, &cfg(256));
        let out = r.format_plain();
        assert!(out.contains("num_flow_partitions: 256"));
        assert!(out.contains("lanes configured:    1"));
        assert!(out.contains("lanes colliding:     0"));
        assert!(out.contains("Clean"));
        assert!(out.contains("default"));
        // Clean deployments get NO remediation section.
        assert!(!out.contains("Remediation"));
    }

    #[test]
    fn format_plain_adapts_width_to_long_lane_name() {
        // LaneId permits up to 64 bytes. A 40-byte name should NOT
        // produce a broken-looking table — column width must adapt.
        let long = "x".repeat(40);
        let lanes = vec![lane(&long), lane("short")];
        let r = PartitionCollisionsReport::compute(&lanes, &cfg(256));
        let out = r.format_plain();
        // Each data row should have the lane section padded to the long
        // name's length. Split on '|' and confirm the middle column is
        // at least 40 chars wide (plus potential surrounding space).
        for line in out.lines().filter(|l| l.starts_with(|c: char| c.is_ascii_digit() || c == ' ')) {
            if let Some(middle) = line.split('|').nth(1) {
                let middle_trim_right = middle.trim_end();
                // At minimum the longest name must fit without truncation.
                if middle_trim_right.contains(&long) {
                    assert!(
                        middle.len() > long.len(),
                        "row middle too narrow for long lane: {middle:?}"
                    );
                }
            }
        }
        // And crucially: the long lane name appears intact.
        assert!(out.contains(&long));
    }

    #[test]
    fn format_plain_forced_collision_includes_remediation() {
        let lanes = vec![lane("a"), lane("b")];
        let r = PartitionCollisionsReport::compute(&lanes, &cfg(1));
        let out = r.format_plain();
        assert!(out.contains("Remediate"));
        assert!(out.contains("Remediation"));
        assert!(out.contains("FF_FLOW_PARTITIONS"));
        assert!(out.contains("SoloPartitioner"));
        // Each lane's row lists the other as collides_with.
        assert!(out.contains("a") && out.contains("b"));
        // Numbered list items retain the intended two-space indent. The
        // backslash line-continuation Rust string idiom strips leading
        // whitespace, so we explicitly push each numbered line; this test
        // pins that we didn't regress back to the continuation shape.
        assert!(
            out.contains("\n  1. Rename"),
            "remediation step 1 missing two-space indent in: {out:?}"
        );
        assert!(
            out.contains("\n  2. Bump FF_FLOW_PARTITIONS"),
            "remediation step 2 missing two-space indent in: {out:?}"
        );
        assert!(
            out.contains("\n  3. Install a custom SoloPartitioner"),
            "remediation step 3 missing two-space indent in: {out:?}"
        );
    }
}
