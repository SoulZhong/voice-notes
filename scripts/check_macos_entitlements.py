#!/usr/bin/env python3
import json
import plistlib
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
CONFIG = ROOT / "src-tauri" / "tauri.conf.json"


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

    if entitlements.get("com.apple.security.cs.disable-library-validation") is not True:
        fail("disable-library-validation entitlement must be true")

    print("check_macos_entitlements: OK")


if __name__ == "__main__":
    main()
