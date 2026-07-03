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

# SenseVoice-small 多语言模型（zh/en/ja/ko/yue，2024-07-17）
if [ ! -d "sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17" ]; then
  SV_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17.tar.bz2"
  echo "下载 SenseVoice $SV_URL ..."
  curl -L -o sv.tar.bz2 "$SV_URL"
  tar xjf sv.tar.bz2
  rm -f sv.tar.bz2
  echo "SenseVoice 模型已就绪：$DIR/sherpa-onnx-sense-voice-zh-en-ja-ko-yue-2024-07-17"
fi

# 3D-Speaker CAM++ 中文声纹模型(说话人区分用)
SPK_MODEL="3dspeaker_speech_campplus_sv_zh-cn_16k-common.onnx"
if [ ! -f "$SPK_MODEL" ]; then
  SPK_URL="https://github.com/k2-fsa/sherpa-onnx/releases/download/speaker-recongition-models/$SPK_MODEL"
  echo "下载声纹模型 $SPK_URL ..."
  curl -fL -o "$SPK_MODEL" "$SPK_URL"
  echo "声纹模型已就绪：$DIR/$SPK_MODEL"
fi
