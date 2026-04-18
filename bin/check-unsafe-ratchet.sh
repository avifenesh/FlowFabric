#!/usr/bin/env bash
# Count `unsafe` blocks / fn / impl / trait / method occurrences per crate
# using ripgrep and compare against the committed baseline. Replaces the
# cargo-geiger-based check which broke against newer cargo + our Cargo.lock
# (tool is unmaintained at 0.13.0 and crashes on thiserror@2.0.x).
#
# Exit 0: no crate exceeds the committed baseline.
# Exit 1: at least one crate grew — dumps per-crate delta to stderr.
# Exit 2: tool / parse / IO error.
#
# A PR that legitimately adds `unsafe` must commit a refreshed baseline
# in the same change (bin/update-unsafe-ratchet.sh writes the file).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASELINE="$ROOT/.github/geiger-baseline.json"

[ -f "$BASELINE" ] || {
    echo "baseline missing at $BASELINE" >&2
    exit 2
}

# Matches `unsafe` followed by a block opener or fn/impl/trait keyword.
# Uses grep -E (POSIX ERE) for runner portability (no ripgrep dep).
# \b -> [[:<:]] / [[:>:]] is non-portable; substitute a [[:space:]]-or-BOL
# prefix + [[:space:]] suffix, which catches real use sites and rejects
# identifiers like `unsafer` or `foo_unsafe`.
PATTERN='(^|[[:space:]])unsafe[[:space:]]+(fn[[:space:]]|impl[[:space:]]|trait[[:space:]]|\{)'

count_crate() {
    local path="$1"
    local raw
    # grep -c counts per file; sum. grep exits 1 when no matches — ignore.
    raw=$(grep -rEc --include='*.rs' "$PATTERN" "$path" 2>/dev/null || true)
    echo "$raw" | awk -F: '{s+=$NF} END {print s+0}'
}

echo "[unsafe-ratchet] scanning 8 crates…"
declare -A COUNTS
for entry in \
    "ff-core:crates/ff-core/src" \
    "ff-script:crates/ff-script/src" \
    "ff-engine:crates/ff-engine/src" \
    "ff-scheduler:crates/ff-scheduler/src" \
    "ff-sdk:crates/ff-sdk/src" \
    "ff-server:crates/ff-server/src" \
    "ff-test:crates/ff-test/src" \
    "ferriskey:ferriskey/src"
do
    name="${entry%%:*}"
    path="${entry#*:}"
    c=$(count_crate "$ROOT/$path")
    COUNTS[$name]=$c
    printf '  - %-15s %3d\n' "$name" "$c"
done

# Flatten counts for Python.
FLAT=""
for k in "${!COUNTS[@]}"; do
    FLAT+="$k=${COUNTS[$k]} "
done

BASELINE_PATH="$BASELINE" CURRENT_COUNTS="$FLAT" python3 <<'PY'
import json, os, sys
from pathlib import Path

baseline = json.loads(Path(os.environ["BASELINE_PATH"]).read_text())
print(f"[unsafe-ratchet] baseline from {baseline.get('commit', '<?>')[:10]}"
      f" @ {baseline.get('generated_at', '<?>')}")

current = {}
for kv in os.environ["CURRENT_COUNTS"].split():
    k, v = kv.split("=", 1)
    current[k] = int(v)

growth, shrink, parity = [], [], []
for crate, cur in sorted(current.items()):
    expected = baseline.get("crates", {}).get(crate, {}).get("unsafe_expressions", 0)
    delta = cur - expected
    entry = (crate, expected, cur, delta)
    (growth if delta > 0 else shrink if delta < 0 else parity).append(entry)

for crate, exp, cur, _ in parity:
    print(f"  OK     {crate:<15}  {cur:>3}  (baseline {exp})")
for crate, exp, cur, delta in shrink:
    print(f"  SHRINK {crate:<15}  {cur:>3}  (baseline {exp}, -{-delta}) "
          f"-- run bin/update-unsafe-ratchet.sh to ratchet down")
for crate, exp, cur, delta in growth:
    print(f"  GROW   {crate:<15}  {cur:>3}  (baseline {exp}, +{delta})",
          file=sys.stderr)

if growth:
    print(f"\n[unsafe-ratchet] FAIL: count increased in {len(growth)} crate(s).",
          file=sys.stderr)
    print("If the additions are intentional, re-run "
          "bin/update-unsafe-ratchet.sh in the same PR.", file=sys.stderr)
    sys.exit(1)

sys.exit(0)
PY
