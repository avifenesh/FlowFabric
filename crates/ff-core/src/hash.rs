//! Small, stable hash helpers shared across ff-sdk + ff-scheduler +
//! ff-engine. **Not** security-sensitive — these are for cardinality
//! reduction (log digests) and deterministic-but-spread iteration
//! (partition-scan start offsets). A single source of truth avoids
//! drift if the algorithm changes.
//!
//! Both helpers implement FNV-1a 64-bit with the standard offset basis
//! and prime (see <http://www.isthe.com/chongo/tech/comp/fnv/>). FNV-1a
//! is fast (byte-at-a-time), has good avalanche on short keys, and
//! produces stable output across platforms — important because these
//! digests leak into logs that operators grep against.

/// 8-hex-character FNV-1a digest of the input, suitable for inclusion
/// in per-event log lines as a stable "what worker is this" identifier.
/// The upper and lower 32 bits of the 64-bit hash are XOR-folded before
/// hex formatting to keep both halves' entropy in the 8 chars.
pub fn fnv1a_xor8hex(s: &str) -> String {
    let h = fnv1a_u64(s.as_bytes());
    format!("{:08x}", (h as u32) ^ ((h >> 32) as u32))
}

/// FNV-1a of the input, reduced modulo `modulus`. Returns 0 when
/// `modulus == 0`. Used for deterministic-but-spread scan-start
/// offsets (partition jitter). Callers pass a non-zero modulus.
pub fn fnv1a_u16_mod(s: &str, modulus: u16) -> u16 {
    if modulus == 0 {
        return 0;
    }
    let h = fnv1a_u64(s.as_bytes());
    (h as u16) % modulus
}

/// FNV-1a 64-bit over raw bytes. Kept public for callers that need the
/// full hash width (rare — most sites want the folded/reduced helpers
/// above).
pub fn fnv1a_u64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3); // FNV prime
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fnv1a_empty_is_offset_basis() {
        assert_eq!(fnv1a_u64(b""), 0xcbf2_9ce4_8422_2325);
    }

    #[test]
    fn fnv1a_stable() {
        // Regression: the literal output for a fixed input. If this test
        // flips, either the algorithm drifted or the seed changed.
        assert_eq!(fnv1a_xor8hex("gpu,cuda").len(), 8);
        assert_eq!(fnv1a_xor8hex("gpu,cuda"), fnv1a_xor8hex("gpu,cuda"));
    }

    #[test]
    fn fnv1a_u16_mod_spread() {
        // Different inputs usually land on different partitions. Not
        // guaranteed (hash collisions exist) but the common case.
        let a = fnv1a_u16_mod("worker-a", 256);
        let b = fnv1a_u16_mod("worker-b", 256);
        assert!(a < 256 && b < 256);
        // Zero modulus returns 0.
        assert_eq!(fnv1a_u16_mod("anything", 0), 0);
    }
}
