#!/usr/bin/env bash
# Verify the current tree's unsafe-expression count per crate is
# ≤ the committed baseline (.github/geiger-baseline.json).
#
# Exit 0: no crate has strictly more unsafe than baseline.
# Exit 1: at least one crate grew — dumps per-crate delta to stderr.
# Exit 2: tool / parse / IO error.
#
# A PR that legitimately adds `unsafe` must commit a refreshed
# baseline in the same change (bin/update-geiger-baseline.sh writes
# the file).

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASELINE="$ROOT/.github/geiger-baseline.json"

command -v cargo-geiger >/dev/null 2>&1 || {
    echo "cargo-geiger missing" >&2
    exit 2
}
[ -f "$BASELINE" ] || {
    echo "baseline missing at $BASELINE" >&2
    exit 2
}

CRATES=(
    "ff-core:crates/ff-core/Cargo.toml"
    "ff-script:crates/ff-script/Cargo.toml"
    "ff-engine:crates/ff-engine/Cargo.toml"
    "ff-scheduler:crates/ff-scheduler/Cargo.toml"
    "ff-sdk:crates/ff-sdk/Cargo.toml"
    "ff-server:crates/ff-server/Cargo.toml"
    "ff-test:crates/ff-test/Cargo.toml"
    "ferriskey:ferriskey/Cargo.toml"
)

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "[geiger-check] scanning ${#CRATES[@]} crates…"
names=()
for pair in "${CRATES[@]}"; do
    name="${pair%%:*}"
    manifest="${pair#*:}"
    names+=("$name")
    printf '  - %-15s' "$name"
    # cargo-geiger 0.13.0 sometimes exits non-zero after emitting valid
    # JSON (tool-side package-matching warnings on resolver v3). We care
    # about usable JSON, not the exit code — verify the target crate is
    # present in the output before giving up.
    cargo geiger --output-format Json --all-targets --manifest-path "$ROOT/$manifest" \
        > "$TMPDIR/$name.json" 2>"$TMPDIR/$name.err" || true
    if ! python3 -c "
import json, sys
try:
    data = json.load(open('$TMPDIR/$name.json'))
except Exception as e:
    print(f'JSON parse failed: {e}', file=sys.stderr); sys.exit(1)
if not any(p.get('package', {}).get('id', {}).get('name') == '$name' for p in data.get('packages', [])):
    print('target crate $name missing from geiger output', file=sys.stderr); sys.exit(1)
" 2>>"$TMPDIR/$name.err"; then
        echo
        echo "cargo geiger failed for $name:" >&2
        tail -40 "$TMPDIR/$name.err" >&2
        exit 2
    fi
    echo " ok"
done

# Compare current counts to baseline; print any growth.
GEIGER_TMPDIR="$TMPDIR" BASELINE_PATH="$BASELINE" \
python3 - "${names[@]}" <<'PY'
import json, os, sys
from pathlib import Path

tmp = Path(os.environ["GEIGER_TMPDIR"])
baseline = json.loads(Path(os.environ["BASELINE_PATH"]).read_text())
crates = sys.argv[1:]

print(f"[geiger-check] baseline from {baseline.get('commit', '<?>')[:10]}"
      f" @ {baseline.get('generated_at', '<?>')}")

growth = []
parity = []
shrink = []

for crate in crates:
    data = json.loads((tmp / f"{crate}.json").read_text())
    root = None
    for pkg in data.get("packages", []):
        if pkg.get("package", {}).get("id", {}).get("name") == crate:
            root = pkg; break
    used = (root or {}).get("unsafety", {}).get("used", {})
    current = sum(
        used.get(k, {}).get("unsafe_", 0)
        for k in ("functions", "exprs", "item_impls", "item_traits", "methods")
    )
    expected = baseline.get("crates", {}).get(crate, {}).get("unsafe_expressions", 0)
    delta = current - expected
    entry = (crate, expected, current, delta)
    if delta > 0:
        growth.append(entry)
    elif delta < 0:
        shrink.append(entry)
    else:
        parity.append(entry)

for crate, exp, cur, delta in parity:
    print(f"  OK     {crate:<15}  {cur:>3}  (baseline {exp})")
for crate, exp, cur, delta in shrink:
    print(f"  SHRINK {crate:<15}  {cur:>3}  (baseline {exp}, −{-delta}) "
          f"— run bin/update-geiger-baseline.sh to ratchet down")
for crate, exp, cur, delta in growth:
    print(f"  GROW   {crate:<15}  {cur:>3}  (baseline {exp}, +{delta})",
          file=sys.stderr)

if growth:
    print("", file=sys.stderr)
    print("[geiger-check] FAIL: unsafe expression count increased in "
          f"{len(growth)} crate(s). If the additions are intentional, "
          "re-run bin/update-geiger-baseline.sh in the same PR to "
          "update the committed baseline.", file=sys.stderr)
    sys.exit(1)

sys.exit(0)
PY
