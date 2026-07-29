#!/bin/bash
# beforeBundleCommand（tauri.conf.json）：打包前修正主二进制的动态库引用。
#
# 为什么需要：webrtc-audio-processing 链接的是 Homebrew abseil，引用写死
# /opt/homebrew/opt/abseil/lib/... 绝对路径——没有装 brew abseil 的用户机器
# 上 dyld 直接启动崩溃。这里把绝对路径改写成 @rpath/，配合 build.rs 注入的
# @executable_path/../Frameworks rpath 与 bundle.macOS.frameworks 打进包里的
# dylib 副本，让 .app 自带全部非系统依赖。
# sherpa/onnxruntime 已随官方 sherpa-onnx crate 迁移改为静态链接（2026-07-28），
# 本脚本只剩 abseil 一项职责。
#
# 版本无关化（2026-07-29）：abseil 版本随 brew 滚动（开发机 2407.0.0，CI runner
# 随镜像走），文件名里的版本号一旦钉死，任何一边 brew 升级都会在这里断。因此
# 暂存副本统一改名为免版本 libabsl_X.dylib，自身 ID、相互引用、主二进制引用
# 全部归一到免版本 @rpath 名——tauri.conf frameworks 列表从此与 abseil 版本解耦。
#
# --stage-only：只做「探测版本 + 暂存免版本副本」，不碰主二进制。给发布 CI 在
# cargo 构建前调用——tauri-build 在编译期就校验 frameworks 文件存在，等
# beforeBundleCommand 再暂存就晚了（本地首次构建同理）。
#
# install_name_tool 会使 arm64 的 linker 签名失效，改完必须 ad-hoc 重签，
# 否则 macOS 拒绝加载（Killed: 9）。
set -euo pipefail

STAGE_ONLY=0
[ "${1:-}" = "--stage-only" ] && STAGE_ONLY=1

BIN="src-tauri/target/release/voice-notes"
ABSL_DIR="$(brew --prefix abseil 2>/dev/null || echo /opt/homebrew/opt/abseil)/lib"

# 动态探测 brew abseil 版本(多版本并存取字典序最新)。
ABSL_BASE=$(ls "$ABSL_DIR"/libabsl_base.*.dylib 2>/dev/null | sort | tail -1)
if [ -z "$ABSL_BASE" ]; then
    echo "fix-bundle-dylibs: 未找到 brew abseil($ABSL_DIR)——先 brew install abseil" >&2
    exit 1
fi
ABSL_VER=$(basename "$ABSL_BASE" | sed -E 's/^libabsl_base\.(.+)\.dylib$/\1/')
echo "fix-bundle-dylibs: brew abseil 版本 $ABSL_VER($ABSL_DIR)"

LIBS="libabsl_base libabsl_raw_logging_internal libabsl_log_severity \
      libabsl_spinlock_wait libabsl_strings libabsl_strings_internal \
      libabsl_int128 libabsl_string_view libabsl_throw_delegate"

# abseil 的 9 个 dylib 暂存为可写副本(brew 原件 555 只读,bundler 保留权限后
# 签名前置的 xattr -cr 会 Permission denied)。tauri.conf frameworks 指向这里。
STAGE="src-tauri/target/bundle-libs"
rm -rf "$STAGE"
mkdir -p "$STAGE"
for lib in $LIBS; do
    cp -f "$ABSL_DIR/$lib.$ABSL_VER.dylib" "$STAGE/$lib.dylib"
    chmod u+w "$STAGE/$lib.dylib"
done

# 副本归一:自身 ID 与相互引用(brew 原件互引为带版本 @rpath 名)全部去版本号。
for lib in $LIBS; do
    f="$STAGE/$lib.dylib"
    install_name_tool -id "@rpath/$lib.dylib" "$f" 2>/dev/null
    for dep in $LIBS; do
        install_name_tool -change "@rpath/$dep.$ABSL_VER.dylib" "@rpath/$dep.dylib" "$f" 2>/dev/null
        install_name_tool -change "$ABSL_DIR/$dep.$ABSL_VER.dylib" "@rpath/$dep.dylib" "$f" 2>/dev/null
    done
    codesign --force --sign - "$f"
    # 守卫:副本不得残留绝对路径或带版本 @rpath 引用(abseil 布局变化在这里暴露)。
    if otool -L "$f" | tail -n +2 | grep -qE '/opt/homebrew|@rpath/libabsl_[a-z0-9_]+\.[0-9]'; then
        echo "fix-bundle-dylibs: $lib.dylib 仍有未归一引用:" >&2
        otool -L "$f" >&2
        exit 1
    fi
done

if [ "$STAGE_ONLY" = "1" ]; then
    echo "fix-bundle-dylibs: OK（--stage-only:免版本副本已暂存,未触碰主二进制）"
    exit 0
fi

# 主二进制:绝对路径引用 → 免版本 @rpath(仅 5 个是直接依赖,多改的为无害 no-op);
# 兼带把旧脚本产出的「带版本 @rpath」引用一并归一(增量构建不重链时会残留)。
for lib in $LIBS; do
    install_name_tool -change \
        "$ABSL_DIR/$lib.$ABSL_VER.dylib" \
        "@rpath/$lib.dylib" \
        "$BIN" 2>/dev/null
    install_name_tool -change \
        "@rpath/$lib.$ABSL_VER.dylib" \
        "@rpath/$lib.dylib" \
        "$BIN" 2>/dev/null
done

codesign --force --sign - "$BIN"

# 守卫：不允许残留 /opt/homebrew 绝对路径或带版本 @rpath 引用（abseil 升级改名
# 时在这里暴露，而不是等用户装完崩溃才发现）。
if otool -L "$BIN" | grep -qE '/opt/homebrew|@rpath/libabsl_[a-z0-9_]+\.[0-9]'; then
    echo "fix-bundle-dylibs: 主二进制仍有未归一的 abseil 引用:" >&2
    otool -L "$BIN" | grep -E '/opt/homebrew|@rpath/libabsl_[a-z0-9_]+\.[0-9]' >&2
    exit 1
fi
echo "fix-bundle-dylibs: OK（abseil 引用已归一为免版本 @rpath 并重签）"
