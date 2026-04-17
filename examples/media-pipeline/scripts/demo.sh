#!/usr/bin/env bash
# End-to-end media-pipeline demo. Starts three workers in the background,
# submits a pipeline, runs the reviewer, and reports pass/fail.
#
# Prerequisites (run once):
#   scripts/setup.sh              # whisper.cpp + ggml model
#   scripts/download-models.sh    # qwen + minilm
#   scripts/generate-sample.sh    # three test WAVs at samples/*.wav
#
# FlowFabric server is assumed to be running on FF_SERVER (default
# http://localhost:9090) against a Valkey on :6379.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

SERVER="${FF_SERVER:-http://localhost:9090}"
# generate-sample.sh writes samples/{story,tech,conversation}.wav.
# story.wav is the shortest clip (~16s of audio → ~3 whisper segments)
# and keeps demo wall time under a minute on CPU-only boxes.
SAMPLE="${SAMPLE:-samples/story.wav}"
LOGDIR="${LOGDIR:-target/demo-logs}"

if [ ! -f "$SAMPLE" ]; then
    echo "no sample at $SAMPLE — run scripts/generate-sample.sh first" >&2
    exit 1
fi

mkdir -p "$LOGDIR"
PIDS=()

cleanup() {
    echo "[demo] tearing down"
    for pid in "${PIDS[@]:-}"; do
        kill "$pid" 2>/dev/null || true
    done
}
trap cleanup EXIT INT TERM

echo "[demo] building workspace"
cargo build --quiet --bin transcribe --bin summarize --bin embed --bin submit --bin review

echo "[demo] starting transcribe worker"
./target/debug/transcribe > "$LOGDIR/transcribe.log" 2>&1 &
PIDS+=($!)

echo "[demo] starting summarize worker"
./target/debug/summarize > "$LOGDIR/summarize.log" 2>&1 &
PIDS+=($!)

echo "[demo] starting embed worker"
./target/debug/embed > "$LOGDIR/embed.log" 2>&1 &
PIDS+=($!)

sleep 1  # workers take a beat to connect

echo "[demo] submitting pipeline for $SAMPLE"
./target/debug/submit \
    --audio "$SAMPLE" \
    --title "media-pipeline demo" \
    --server "$SERVER" \
    2>&1 | tee "$LOGDIR/submit.log" &
SUBMIT_PID=$!
PIDS+=($SUBMIT_PID)

# Extract the summarize execution ID once the submit CLI logs it.
SUMMARIZE_EID=""
for _ in $(seq 1 60); do
    if grep -q '\[submit\] summarize=' "$LOGDIR/submit.log" 2>/dev/null; then
        SUMMARIZE_EID="$(grep '\[submit\] summarize=' "$LOGDIR/submit.log" | head -1 | sed 's/.*summarize=//')"
        break
    fi
    sleep 1
done

if [ -z "$SUMMARIZE_EID" ]; then
    echo "[demo] could not extract summarize execution id" >&2
    exit 1
fi

echo "[demo] summarize eid = $SUMMARIZE_EID"
echo "[demo] running review --auto-approve"
./target/debug/review \
    --execution-id "$SUMMARIZE_EID" \
    --server "$SERVER" \
    --auto-approve \
    2>&1 | tee "$LOGDIR/review.log"

echo "[demo] waiting on submit"
if wait "$SUBMIT_PID"; then
    echo "[demo] PASS — pipeline completed"
else
    echo "[demo] FAIL — submit exited non-zero" >&2
    exit 1
fi
