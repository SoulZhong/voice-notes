#!/bin/bash
# beforeBundleCommand（tauri.conf.json）：打包前修正主二进制的动态库引用。
#
# 为什么需要：webrtc-audio-processing 链接的是 Homebrew abseil，引用写死
# /opt/homebrew/opt/abseil/lib/... 绝对路径——没有装 brew abseil 的用户机器
# 上 dyld 直接启动崩溃。这里把绝对路径改写成 @rpath/，配合 build.rs 注入的
# @executable_path/../Frameworks rpath 与 bundle.macOS.frameworks 打进包里的
# dylib 副本，让 .app 自带全部非系统依赖。
# sherpa/onnxruntime 本身就是 @rpath 引用，无需改写，只需打包（frameworks 列表）。
#
# install_name_tool 会使 arm64 的 linker 签名失效，改完必须 ad-hoc 重签，
# 否则 macOS 拒绝加载（Killed: 9）。
set -euo pipefail

BIN="src-tauri/target/release/voice-notes"
ABSL_DIR="/opt/homebrew/opt/abseil/lib"
ABSL_VER="2407.0.0"

for lib in libabsl_base libabsl_raw_logging_internal libabsl_log_severity \
           libabsl_spinlock_wait libabsl_strings; do
    install_name_tool -change \
        "$ABSL_DIR/$lib.$ABSL_VER.dylib" \
        "@rpath/$lib.$ABSL_VER.dylib" \
        "$BIN"
done

codesign --force --sign - "$BIN"

# 守卫：不允许残留任何 /opt/homebrew 绝对路径引用（abseil 升级改名时在这里暴露，
# 而不是等用户装完崩溃才发现）。
if otool -L "$BIN" | grep -q /opt/homebrew; then
    echo "fix-bundle-dylibs: 主二进制仍引用 Homebrew 绝对路径:" >&2
    otool -L "$BIN" | grep /opt/homebrew >&2
    exit 1
fi
echo "fix-bundle-dylibs: OK（abseil 引用已改写为 @rpath 并重签）"
