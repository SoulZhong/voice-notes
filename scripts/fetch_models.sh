#!/usr/bin/env bash
set -euo pipefail
DIR="$(cd "$(dirname "$0")/../src-tauri/models" && pwd)"
cd "$DIR"
# sherpa-onnx 官方导出的 whisper-base（含 encoder/decoder onnx + tokens.txt）
URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-whisper-base.tar.bz2"
echo "下载 $URL ..."
curl -L -o whisper-base.tar.bz2 "$URL"
tar xjf whisper-base.tar.bz2
rm -f whisper-base.tar.bz2
echo "模型已就绪：$DIR/sherpa-onnx-whisper-base"

# Silero VAD 模型（单文件 onnx，用于语句分段）
if [ ! -f silero_vad.onnx ]; then
  VAD_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx"
  echo "下载 $VAD_URL ..."
  curl -L -o silero_vad.onnx "$VAD_URL"
  echo "VAD 模型已就绪：$DIR/silero_vad.onnx"
fi
