#!/usr/bin/env bash
# Idempotent setup for the media-pipeline example.
# Clones + builds whisper.cpp, downloads the ggml-tiny.en-q5_0 model,
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
warn_missing espeak-ng "generate-sample.sh"
warn_missing ffmpeg "generate-sample.sh"

# 1. Clone whisper.cpp if missing
if [ ! -d "$WHISPER_DIR/.git" ]; then
    echo "[setup] cloning whisper.cpp into vendor/"
    mkdir -p "$ROOT/vendor"
    git clone --depth 1 https://github.com/ggerganov/whisper.cpp "$WHISPER_DIR"
else
    echo "[setup] whisper.cpp already cloned, skipping"
fi

# 2. Build whisper-cli if missing
if [ ! -x "$WHISPER_BIN" ]; then
    echo "[setup] building whisper.cpp (release)"
    (cd "$WHISPER_DIR" && cmake -B build -DGGML_NATIVE=ON -DCMAKE_BUILD_TYPE=Release)
    (cd "$WHISPER_DIR" && cmake --build build -j --config Release)
else
    echo "[setup] whisper-cli already built, skipping"
fi

# 3. Download model if missing (script writes into vendor/whisper.cpp/models/,
#    then we move it to the example-level models/ so W3's qwen model can live
#    alongside it).
mkdir -p "$ROOT/models"
if [ ! -f "$MODEL_DST" ]; then
    echo "[setup] downloading $MODEL_NAME (~77MB)"
    (cd "$WHISPER_DIR" && bash ./models/download-ggml-model.sh tiny.en-q5_1)
    mv "$WHISPER_DIR/models/$MODEL_NAME" "$MODEL_DST"
else
    echo "[setup] model already present at models/$MODEL_NAME, skipping"
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
