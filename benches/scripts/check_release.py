#!/usr/bin/env python3
"""Compare current-run bench reports against a prior release snapshot.

Called by `benches/check-release.sh`. Arguments are the directory layout
check-release.sh already maintains — no discovery logic here.

Thresholds (confirmed by manager in Phase A approval):
  * throughput regression > 10% → FAIL
  * p99 regression      > 20% → FAIL
  * throughput regression > 5%  → WARN
  * p99 regression      > 10% → WARN

The WARN and FAIL branches both print to stderr; only FAIL exits
non-zero. Green runs print a one-line summary per scenario and exit 0.
"""

import argparse
import glob
import json
import os
import sys
from typing import Optional


def load(path: str) -> dict:
    with open(path, "r", encoding="utf-8") as fh:
        return json.load(fh)


def find_current(scenario: str, current_dir: str, sha: str) -> Optional[str]:
    cand = os.path.join(current_dir, f"{scenario}-{sha}.json")
    return cand if os.path.isfile(cand) else None


def find_baseline(scenario: str, baseline_dir: str) -> Optional[str]:
    # Baseline files were written with whatever sha was current at tag
    # time — match by scenario prefix, pick the lexically-latest if
    # multiple (e.g. a retry-baseline). Lex-latest works because sha
    # ordering is not meaningful; any deterministic choice is fine.
    pattern = os.path.join(baseline_dir, f"{scenario}-*.json")
    matches = sorted(glob.glob(pattern))
    return matches[-1] if matches else None


def pct_change(current: float, baseline: float) -> float:
    """Positive = regression (slower / lower throughput).

    * Throughput drops → (baseline - current) / baseline > 0
    * p99 increases    → (current - baseline) / baseline > 0

    Callers invert sign for throughput themselves so both metrics read
    "higher = worse" uniformly in the report.
    """
    if baseline == 0:
        return 0.0
    return (current - baseline) / baseline * 100.0


def classify(throughput_delta_pct: float, p99_delta_pct: float) -> str:
    # Negative throughput delta (current < baseline) means we got
    # SLOWER — that's the regression direction.
    throughput_regression = -throughput_delta_pct
    p99_regression = p99_delta_pct

    if throughput_regression > 10.0 or p99_regression > 20.0:
        return "FAIL"
    if throughput_regression > 5.0 or p99_regression > 10.0:
        return "WARN"
    return "OK"


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--current-dir", required=True)
    ap.add_argument("--current-sha", required=True)
    ap.add_argument("--baseline-dir", required=True)
    ap.add_argument("--scenarios", nargs="+", required=True)
    args = ap.parse_args()

    had_fail = False
    had_warn = False
    for scenario in args.scenarios:
        curr_path = find_current(scenario, args.current_dir, args.current_sha)
        base_path = find_baseline(scenario, args.baseline_dir)

        if curr_path is None:
            print(
                f"[check-release] MISSING current run for {scenario} "
                f"(expected {scenario}-{args.current_sha}.json)",
                file=sys.stderr,
            )
            had_fail = True
            continue
        if base_path is None:
            print(
                f"[check-release] WARN: no baseline for {scenario} in "
                f"{args.baseline_dir} — cannot compare",
                file=sys.stderr,
            )
            had_warn = True
            continue

        curr = load(curr_path)
        base = load(base_path)
        t_cur = float(curr["throughput_ops_per_sec"])
        t_base = float(base["throughput_ops_per_sec"])
        p99_cur = float(curr["latency_ms"]["p99"])
        p99_base = float(base["latency_ms"]["p99"])
        t_delta = pct_change(t_cur, t_base)
        p99_delta = pct_change(p99_cur, p99_base)
        verdict = classify(t_delta, p99_delta)

        line = (
            f"[check-release] {scenario}: {verdict} "
            f"throughput {t_cur:.1f} (base {t_base:.1f}, delta {t_delta:+.1f}%) "
            f"p99 {p99_cur:.2f}ms (base {p99_base:.2f}ms, delta {p99_delta:+.1f}%)"
        )
        if verdict == "FAIL":
            print(line, file=sys.stderr)
            had_fail = True
        elif verdict == "WARN":
            print(line, file=sys.stderr)
            had_warn = True
        else:
            print(line)

    if had_fail:
        print("[check-release] FAIL — release should be blocked", file=sys.stderr)
        return 1
    if had_warn:
        print("[check-release] WARN — review before tagging, not blocking")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
