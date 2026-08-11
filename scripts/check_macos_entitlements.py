#!/usr/bin/env python3
"""校验 macOS 授权声明的源码配置:entitlements 与 Info.plist 的必备键。

只看源码 plist(打包后的合并结果不在此校验范围——那需要真跑 tauri bundle,
留给发布抽验);表驱动,新增权限在 REQUIRED_* 里加一行即可。
"""
import json
import plistlib
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "src-tauri" / "tauri.conf.json"
INFO_PLIST = ROOT / "src-tauri" / "Info.plist"

# entitlement 键 -> 期望值。
REQUIRED_ENTITLEMENTS = {
    "com.apple.security.cs.disable-library-validation": True,
    "com.apple.security.device.audio-input": True,
    "com.apple.security.personal-information.calendars": True,
}

# Info.plist 用途声明键:必须存在且非空字符串。
REQUIRED_USAGE_KEYS = [
    "NSMicrophoneUsageDescription",
    "NSAudioCaptureUsageDescription",
    "NSCalendarsUsageDescription",
    "NSCalendarsFullAccessUsageDescription",
]


def fail(message: str) -> None:
    print(f"check_macos_entitlements: {message}", file=sys.stderr)
    raise SystemExit(1)


def main() -> None:
    config = json.loads(CONFIG.read_text())
    macos = config.get("bundle", {}).get("macOS", {})
    entitlements_name = macos.get("entitlements")
    if not entitlements_name:
        fail("bundle.macOS.entitlements is not configured")

    entitlements_path = CONFIG.parent / entitlements_name
    if not entitlements_path.is_file():
        fail(f"entitlements file does not exist: {entitlements_path}")

    with entitlements_path.open("rb") as f:
        entitlements = plistlib.load(f)

    for key, expected in REQUIRED_ENTITLEMENTS.items():
        if entitlements.get(key) != expected:
            fail(f"entitlement {key} must be {expected!r}")

    if not INFO_PLIST.is_file():
        fail(f"Info.plist does not exist: {INFO_PLIST}")
    with INFO_PLIST.open("rb") as f:
        info = plistlib.load(f)
    for key in REQUIRED_USAGE_KEYS:
        value = info.get(key)
        if not isinstance(value, str) or not value.strip():
            fail(f"Info.plist {key} must be a non-empty usage description")

    print("check_macos_entitlements: OK")


if __name__ == "__main__":
    main()
