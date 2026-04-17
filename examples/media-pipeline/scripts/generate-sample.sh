#!/usr/bin/env bash
# Generate three canned WAV samples at 16kHz mono (whisper's native format).
# Idempotent per-sample: skips regeneration if the output file already exists.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
OUT="$ROOT/samples"
# Per-invocation temp file so two concurrent runs on the same host don't
# clobber each other's intermediate espeak-ng output. Cleaned up via
# trap so a Ctrl-C or failed ffmpeg invocation doesn't leak /tmp cruft.
TMP="$(mktemp -t ff_mp_raw.XXXXXX.wav)"
trap 'rm -f "$TMP"' EXIT

mkdir -p "$OUT"

command -v espeak-ng >/dev/null || { echo "missing espeak-ng" >&2; exit 1; }
command -v ffmpeg >/dev/null || { echo "missing ffmpeg" >&2; exit 1; }

gen() {
    local name="$1"
    local text="$2"
    local dst="$OUT/$name.wav"
    if [ -f "$dst" ]; then
        echo "[gen] $name.wav exists, skip"
        return
    fi
    espeak-ng -v en-us -s 160 -w "$TMP" "$text"
    ffmpeg -y -loglevel error -i "$TMP" -ar 16000 -ac 1 "$dst"
    echo "[gen] wrote $dst"
}

gen story "A lighthouse keeper lived alone on a rocky island. Every evening she climbed the spiral stairs to light the lamp, guiding ships safely past the jagged cliffs. One stormy night a small boat appeared on the horizon, its sail torn and its lantern dark."

gen tech "Distributed consensus algorithms such as Raft and Paxos solve the problem of reaching agreement across unreliable networks. They elect a leader, replicate logs, and tolerate failures through quorum voting. Modern systems like etcd and FoundationDB rely on these primitives for strong consistency."

gen conversation "Hey, did you finish reviewing the pull request? Yeah, I left a few comments about the error handling. I think we should retry on transient failures instead of bailing out immediately. Makes sense. I will push an update this afternoon."

echo "[gen] done"
