#!/usr/bin/env bash
# Regenerate .github/geiger-baseline.json with the current per-crate
# `unsafe` counts from ripgrep. Run this when you intentionally add or
# remove unsafe code.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BASELINE="$ROOT/.github/geiger-baseline.json"
PATTERN='(^|[[:space:]])unsafe[[:space:]]+(fn[[:space:]]|impl[[:space:]]|trait[[:space:]]|\{)'

count_crate() {
    local path="$1"
    local raw
    raw=$(grep -rEc --include='*.rs' "$PATTERN" "$path" 2>/dev/null || true)
    echo "$raw" | awk -F: '{s+=$NF} END {print s+0}'
}

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
    COUNTS[$name]=$(count_crate "$ROOT/$path")
done

COMMIT=$(git -C "$ROOT" rev-parse HEAD)
TS=$(date -u +"%Y-%m-%dT%H:%M:%SZ")

FLAT=""
for k in "${!COUNTS[@]}"; do
    FLAT+="$k=${COUNTS[$k]} "
done

BASELINE_PATH="$BASELINE" CURRENT_COUNTS="$FLAT" COMMIT="$COMMIT" TS="$TS" \
python3 <<'PY'
import json, os
from pathlib import Path

current = {}
for kv in os.environ["CURRENT_COUNTS"].split():
    k, v = kv.split("=", 1)
    current[k] = int(v)

payload = {
    "commit": os.environ["COMMIT"],
    "crates": {k: {"unsafe_expressions": v} for k, v in sorted(current.items())},
    "generated_at": os.environ["TS"],
}

Path(os.environ["BASELINE_PATH"]).write_text(json.dumps(payload, indent=2) + "\n")
print(f"[unsafe-ratchet] wrote {os.environ['BASELINE_PATH']}")
for k, v in sorted(current.items()):
    print(f"  {k:<15} {v}")
PY
