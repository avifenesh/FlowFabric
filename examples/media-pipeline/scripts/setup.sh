#!/usr/bin/env bash
# Idempotent setup for the media-pipeline example.
# Clones + builds whisper.cpp, downloads the ggml-tiny.en-q5_1 model,
# and generates sample WAVs. Re-running is cheap; each step skips
# if its output already exists.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
cd "$ROOT"

WHISPER_DIR="$ROOT/vendor/whisper.cpp"
WHISPER_BIN="$WHISPER_DIR/build/bin/whisper-cli"
MODEL_NAME="ggml-tiny.en-q5_1.bin"
MODEL_DST="$ROOT/models/$MODEL_NAME"

need() {
    command -v "$1" >/dev/null 2>&1 || {
        echo "missing: $1 — install via: $2" >&2
        exit 1
    }
}

warn_missing() {
    command -v "$1" >/dev/null 2>&1 || echo "warning: $1 not found ($2 will not work)" >&2
}

need cmake "sudo apt install cmake"
need git "sudo apt install git"
need make "sudo apt install build-essential"
# download-ggml-model.sh internally uses curl (preferred) or wget — make
# the dependency explicit so a missing fetcher fails early in the need
# block rather than obscurely inside the upstream script.
if ! command -v curl >/dev/null 2>&1 && ! command -v wget >/dev/null 2>&1; then
    echo "missing: curl or wget — install via: sudo apt install curl" >&2
    exit 1
fi
warn_missing espeak-ng "generate-sample.sh"
warn_missing ffmpeg "generate-sample.sh"

# Cap parallel compile jobs. cmake --build -j with no cap defaults to
# (nproc) which OOMs 2GB-RAM VMs on whisper.cpp — each g++ worker peaks
# around 700MB. Respect CMAKE_BUILD_PARALLEL_LEVEL if the user has set
# it; otherwise fall back to min(nproc, 4).
if [ -n "${CMAKE_BUILD_PARALLEL_LEVEL:-}" ]; then
    JOBS="$CMAKE_BUILD_PARALLEL_LEVEL"
else
    JOBS="$(nproc 2>/dev/null || echo 2)"
    if [ "$JOBS" -gt 4 ]; then JOBS=4; fi
fi

# 1. Clone whisper.cpp if missing
if [ ! -d "$WHISPER_DIR/.git" ]; then
    echo "[setup] cloning whisper.cpp into vendor/"
    mkdir -p "$ROOT/vendor"
    git clone --depth 1 https://github.com/ggerganov/whisper.cpp "$WHISPER_DIR"
else
    echo "[setup] whisper.cpp already cloned, skipping"
fi

# 2. Build whisper-cli if missing OR if the binary won't --help (interrupted
#    build, broken shared-lib link). Verifying with --help catches the
#    "binary exists but SIGSEGVs on startup" failure mode that a plain
#    executable-exists check silently skips past.
if [ -x "$WHISPER_BIN" ] && "$WHISPER_BIN" --help >/dev/null 2>&1; then
    echo "[setup] whisper-cli already built and runnable, skipping"
else
    if [ -x "$WHISPER_BIN" ]; then
        echo "[setup] whisper-cli present but not runnable — rebuilding"
    else
        echo "[setup] building whisper.cpp (release, -j$JOBS)"
    fi
    (cd "$WHISPER_DIR" && cmake -B build -DGGML_NATIVE=ON -DCMAKE_BUILD_TYPE=Release)
    (cd "$WHISPER_DIR" && cmake --build build -j "$JOBS" --config Release)
fi

# 3. Download model if missing. A plain "-f exists" check misses the
#    partial-download case where the wget/curl inside
#    download-ggml-model.sh is interrupted — the broken file is present
#    and the next run skips. Guard the skip with a size + magic check:
#    the q5_1 ggml blob is ~77 MiB and starts with the `ggml` / `GGUF`
#    magic (the exact 4 bytes vary by whisper.cpp version, so just
#    range-check the size).
mkdir -p "$ROOT/models"
MIN_MODEL_BYTES=20000000  # 20 MiB floor; tiny.en-q5_1 is ~31 MiB
model_looks_complete() {
    [ -f "$1" ] || return 1
    local size
    size=$(stat -c%s "$1" 2>/dev/null || stat -f%z "$1" 2>/dev/null || echo 0)
    [ "$size" -ge "$MIN_MODEL_BYTES" ]
}

if model_looks_complete "$MODEL_DST"; then
    echo "[setup] model already present at models/$MODEL_NAME, skipping"
else
    if [ -f "$MODEL_DST" ]; then
        echo "[setup] model at $MODEL_DST looks truncated — removing and re-downloading"
        rm -f "$MODEL_DST"
    fi
    # Purge any partial in the whisper.cpp-relative location too; otherwise
    # the upstream script's "already downloaded" path serves the stub.
    rm -f "$WHISPER_DIR/models/$MODEL_NAME"
    echo "[setup] downloading $MODEL_NAME (~77MB)"
    (cd "$WHISPER_DIR" && bash ./models/download-ggml-model.sh tiny.en-q5_1)
    if ! model_looks_complete "$WHISPER_DIR/models/$MODEL_NAME"; then
        echo "[setup] FATAL: downloaded model is smaller than ${MIN_MODEL_BYTES} bytes — network partial?" >&2
        exit 1
    fi
    mv "$WHISPER_DIR/models/$MODEL_NAME" "$MODEL_DST"
fi

# 4. Generate sample WAVs if missing
if [ ! -f "$ROOT/samples/story.wav" ] || [ ! -f "$ROOT/samples/tech.wav" ] || [ ! -f "$ROOT/samples/conversation.wav" ]; then
    echo "[setup] generating sample WAVs"
    bash "$HERE/generate-sample.sh"
else
    echo "[setup] sample WAVs already present, skipping"
fi

echo "[setup] done — whisper-cli: $WHISPER_BIN"
echo "[setup] done — model:       $MODEL_DST"
