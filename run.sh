#!/bin/zsh
# Starts the MLX speech sidecar and the Rust voice-agent server. Open http://127.0.0.1:47800
set -e
cd "$(dirname $0)"
# Dev Python: .venv with the same pinned packages the app installs on first run.
PY=.venv/bin/python
[ -x $PY ] || { uv venv --python 3.12 .venv && uv pip install --python $PY -r app/runtime/requirements.lock; }
# Sam Workspace: the invisible display for Sam's own browser windows.
[ -x sidecar/workspace/build/sam-workspace ] || ./sidecar/workspace/build.sh
# Qwen3-TTS 0.6B Base 8-bit cloning the voice profiles in voices/
# (consistent across sentences; quality option: ...-1.7B-Base-6bit). The server picks the
# profile per session (CHARACTER / SAM_VOICE, or the island's character menu).
TTS_REPO=${TTS_REPO:-mlx-community/Qwen3-TTS-12Hz-0.6B-Base-8bit}
# Speech recognition: Apple on-device by default — same accuracy and latency as
# parakeet, ~85 MB instead of ~1.7 GB. Languages: STT_LOCALE (default: system language + English,
# each utterance goes to the more confident one). STT=parakeet: the multilingual MLX model.
STT=${STT:-apple}
if [ "$STT" = apple ] && [ ! -x sidecar/applestt/build/applestt ]; then ./sidecar/applestt/build.sh || STT=parakeet; fi
STT_ARGS=(); [ "$STT" = apple ] && STT_ARGS=(--stt apple)
$PY sidecar/speech_server.py --tts "$TTS_REPO" $STT_ARGS ${TTS_GEN:+--tts-gen "$TTS_GEN"} &
SIDECAR=$!
# Mind sidecar (same venv): memory embeddings (Qwen3-Embedding-0.6B) + Laya micro-judgments.
# If it fails the agent still talks, just without long-term memory and judgments.
$PY sidecar/mind_server.py &
MIND=$!
trap "kill $SIDECAR $MIND 2>/dev/null" EXIT INT TERM
until nc -z 127.0.0.1 47801 2>/dev/null; do sleep 0.5; kill -0 $SIDECAR || exit 1; done
until nc -z 127.0.0.1 47802 2>/dev/null; do sleep 0.5; kill -0 $MIND || break; done
cd server && cargo run --release --bin voice-agent
