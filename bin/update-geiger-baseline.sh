#!/usr/bin/env bash
# Regenerate .github/geiger-baseline.json from the current tree.
#
# Intended for a reviewer-approved PR that deliberately changes the
# unsafe count in a crate — run this locally, commit the diff, and
# the security-and-quality.yml ratchet check passes on the next push.
#
# Baseline shape:
#   {
#     "commit":        "<sha>",              // git HEAD at generation time
#     "generated_at":  "<iso8601>",
#     "crates": {
#       "<crate>": { "unsafe_expressions": N },
#       ...
#     }
#   }
#
# `unsafe_expressions` is summed across the 5 unsafety axes cargo-geiger
# reports (functions, expressions, impls, traits, methods) — matches
# the top-line "used unsafe" tally the tool surfaces in its human views.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

# name:manifest pairs so geiger runs against each crate's own Cargo.toml
# (the workspace root is a virtual manifest and geiger refuses to scan it).
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
OUT="$ROOT/.github/geiger-baseline.json"

command -v cargo-geiger >/dev/null 2>&1 || {
    echo "cargo-geiger missing. cargo install cargo-geiger --locked" >&2
    exit 1
}
command -v python3 >/dev/null 2>&1 || {
    echo "python3 missing — needed for JSON shaping" >&2
    exit 1
}

COMMIT="$(git rev-parse HEAD)"
NOW="$(date -u +%Y-%m-%dT%H:%M:%SZ)"

TMPDIR="$(mktemp -d)"
trap 'rm -rf "$TMPDIR"' EXIT

echo "[geiger-baseline] generating counts for ${#CRATES[@]} crates…"
names=()
for pair in "${CRATES[@]}"; do
    name="${pair%%:*}"
    manifest="${pair#*:}"
    names+=("$name")
    printf '  - %-15s' "$name"
    cargo geiger --output-format Json --all-targets --manifest-path "$ROOT/$manifest" \
        > "$TMPDIR/$name.json" 2>"$TMPDIR/$name.err" || {
        echo
        echo "cargo geiger failed for $name:" >&2
        tail -40 "$TMPDIR/$name.err" >&2
        exit 1
    }
    echo " ok"
done

GEIGER_TMPDIR="$TMPDIR" COMMIT="$COMMIT" NOW="$NOW" \
python3 - "$OUT" "${names[@]}" <<'PY'
import json, os, sys
from pathlib import Path

tmp = Path(os.environ["GEIGER_TMPDIR"])
commit = os.environ["COMMIT"]
now = os.environ["NOW"]
out_path, *crates = sys.argv[1:]

result = {"commit": commit, "generated_at": now, "crates": {}}

for crate in crates:
    raw = (tmp / f"{crate}.json").read_text()
    data = json.loads(raw)
    root = None
    for pkg in data.get("packages", []):
        name = pkg.get("package", {}).get("id", {}).get("name")
        if name == crate:
            root = pkg
            break
    if root is None:
        print(f"WARN: crate {crate} not found in geiger output", file=sys.stderr)
        result["crates"][crate] = {"unsafe_expressions": 0}
        continue
    used = root.get("unsafety", {}).get("used", {})
    total = sum(
        used.get(k, {}).get("unsafe_", 0)
        for k in ("functions", "exprs", "item_impls", "item_traits", "methods")
    )
    result["crates"][crate] = {"unsafe_expressions": int(total)}

Path(out_path).write_text(json.dumps(result, indent=2, sort_keys=True) + "\n")
print(f"[geiger-baseline] wrote {out_path}")
PY

echo "[geiger-baseline] done"
