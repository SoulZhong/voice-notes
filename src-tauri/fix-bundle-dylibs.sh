#!/bin/bash
# beforeBundleCommand（tauri.conf.json）：打包前守卫——主二进制不得引用任何
# Homebrew 动态库。
#
# 历史:webrtc-audio-processing 曾经由 pkg-config 链接 brew abseil 共享库,
# 需要一套「暂存 dylib + install_name_tool 改写 + frameworks 打包」的修补流程
# (且与 brew abseil 版本强耦合,CI runner 的 brew 滚版本就断,2026-07-29 v0.6.0
# 首次发版实测 string_view 等库在 abseil 2601 已被合并,9 库清单失效)。
# 现改为:构建机不装(或 unlink)brew abseil,webrtc-audio-processing-sys 的
# pkg-config 探测失败即走 meson wrap 回退,拉取 abseil-cpp-20240722.0 子项目
# 静态内嵌——二进制零 abseil 动态依赖,无需任何修补与 frameworks。
#
# 本脚本只剩守卫职责:若本机 brew abseil 处于 linked 状态,meson 会重新捡起
# 共享链接,产出的包在用户机器必崩——在这里挡下并给出处置命令。
set -euo pipefail

BIN="src-tauri/target/release/voice-notes"

if otool -L "$BIN" | grep -qE '/opt/homebrew|@rpath/libabsl'; then
    echo "fix-bundle-dylibs: 主二进制引用了 Homebrew 动态库(用户机器上不存在,启动必崩):" >&2
    otool -L "$BIN" | grep -E '/opt/homebrew|@rpath/libabsl' >&2
    echo "处置:brew unlink abseil && cargo clean --release -p webrtc-audio-processing-sys,再重新构建" >&2
    echo "(webrtc-audio-processing 会自动改走 meson wrap 静态内嵌 abseil)" >&2
    exit 1
fi
echo "fix-bundle-dylibs: OK（主二进制无 Homebrew 动态依赖,abseil 已静态内嵌）"
