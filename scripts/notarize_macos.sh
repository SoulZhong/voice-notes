#!/usr/bin/env bash
# macOS 公证:提交 DMG 给 Apple 盖章 → 把票据钉进包里 → 验证 Gatekeeper 放行。
#
# 前置(一次性):
#   xcrun notarytool store-credentials voice-notes-notary \
#     --apple-id "<Apple ID 邮箱>" --team-id "<Team ID>" --password "<App 专用密码>"
#
# 用法:
#   ./scripts/notarize_macos.sh [dmg 路径]      # 省略则取 release 产物里最新的 dmg
#
# 为什么要 staple:公证结果存在 Apple 服务器上,首次打开时 Gatekeeper 会联网查询。
# staple 把票据写进包内,用户离线也能通过校验——分发前必做,否则断网环境仍会被拦。
set -euo pipefail

PROFILE="${NOTARY_PROFILE:-voice-notes-notary}"
DMG="${1:-}"

if [[ -z "$DMG" ]]; then
  DMG=$(ls -t src-tauri/target/release/bundle/dmg/*.dmg 2>/dev/null | head -1 || true)
fi
if [[ -z "$DMG" || ! -f "$DMG" ]]; then
  echo "找不到 dmg。先跑 npm run tauri build,或把路径作为参数传入。" >&2
  exit 1
fi

echo "==> 检查签名(公证要求 Developer ID + Hardened Runtime)"
APP="src-tauri/target/release/bundle/macos/voice-notes.app"
if [[ -d "$APP" ]]; then
  codesign -dv --verbose=2 "$APP" 2>&1 | grep -E 'Authority=Developer ID|TeamIdentifier|flags' || true
  if ! codesign -dv "$APP" 2>&1 | grep -q 'flags=.*runtime'; then
    echo "警告:未检测到 Hardened Runtime,公证很可能被拒。" >&2
  fi
fi

echo "==> 提交公证:$DMG"
xcrun notarytool submit "$DMG" --keychain-profile "$PROFILE" --wait

echo "==> 钉入票据"
xcrun stapler staple "$DMG"

echo "==> 验证"
xcrun stapler validate "$DMG"
spctl -a -vv -t open --context context:primary-signature "$DMG"
echo "完成。期望上方出现 accepted / source=Notarized Developer ID。"
