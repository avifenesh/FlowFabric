#!/usr/bin/env bash
# lint-grafana-dashboard.sh — validates examples/grafana/flowfabric-ops.json.
#
# The dashboard is the only non-Cargo example, so run-all-examples.sh
# SKIPs it. This script is the standalone gate the §5 item 3 release
# check invokes for that one example — catches malformed JSON,
# missing top-level keys, and panels with missing required fields.

set -u
set -o pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DASHBOARD="$ROOT/examples/grafana/flowfabric-ops.json"

if [ ! -f "$DASHBOARD" ]; then
    echo "[grafana-lint] FATAL: $DASHBOARD not found" >&2
    exit 2
fi

if ! command -v jq >/dev/null 2>&1; then
    echo "[grafana-lint] FATAL: jq not installed" >&2
    exit 2
fi

echo "[grafana-lint] $DASHBOARD"

# 1. Valid JSON.
if ! jq empty "$DASHBOARD" 2>/dev/null; then
    echo "[grafana-lint] FAIL: invalid JSON"
    jq . "$DASHBOARD" 2>&1 | tail -5
    exit 1
fi
echo "[grafana-lint]   ✓ valid JSON"

# 2. Required top-level keys — shape a Grafana import expects.
required_top=(title panels schemaVersion uid)
missing=()
for key in "${required_top[@]}"; do
    if ! jq -e --arg k "$key" 'has($k)' "$DASHBOARD" >/dev/null; then
        missing+=("$key")
    fi
done
if [ "${#missing[@]}" -gt 0 ]; then
    echo "[grafana-lint] FAIL: missing top-level keys: ${missing[*]}"
    exit 1
fi
echo "[grafana-lint]   ✓ top-level keys present: ${required_top[*]}"

# 3. Non-empty panel array.
panel_count=$(jq '.panels | length' "$DASHBOARD")
if [ "$panel_count" -lt 1 ]; then
    echo "[grafana-lint] FAIL: .panels is empty"
    exit 1
fi
echo "[grafana-lint]   ✓ $panel_count panels"

# 4. Each panel has id, title, type, gridPos. Report the first
#    offender + a summary count; don't stop at the first one so
#    operators see the full picture.
required_panel=(id title type gridPos)
bad_panels=0
for i in $(seq 0 $((panel_count - 1))); do
    for key in "${required_panel[@]}"; do
        if ! jq -e --arg k "$key" --argjson i "$i" '.panels[$i] | has($k)' "$DASHBOARD" >/dev/null; then
            title=$(jq -r --argjson i "$i" '.panels[$i].title // "(untitled)"' "$DASHBOARD")
            echo "[grafana-lint] FAIL: panel[$i] \"$title\" missing .$key"
            bad_panels=$((bad_panels + 1))
        fi
    done
done
if [ "$bad_panels" -gt 0 ]; then
    echo "[grafana-lint] $bad_panels panel/field failures"
    exit 1
fi
echo "[grafana-lint]   ✓ every panel has: ${required_panel[*]}"

# 5. uid is a non-empty string (Grafana requires a stable uid for
#    provisioning / URL permalinks).
uid=$(jq -r '.uid' "$DASHBOARD")
if [ -z "$uid" ] || [ "$uid" = "null" ]; then
    echo "[grafana-lint] FAIL: .uid is empty/null"
    exit 1
fi
echo "[grafana-lint]   ✓ uid=$uid"

# 6. schemaVersion is numeric + >= 36 (pins we're on a Grafana 9+ era
#    schema; the dashboard file is authored against that surface).
schema=$(jq -r '.schemaVersion' "$DASHBOARD")
if ! [[ "$schema" =~ ^[0-9]+$ ]] || [ "$schema" -lt 36 ]; then
    echo "[grafana-lint] FAIL: schemaVersion=$schema (expected integer >= 36)"
    exit 1
fi
echo "[grafana-lint]   ✓ schemaVersion=$schema"

echo "[grafana-lint] PASS"
