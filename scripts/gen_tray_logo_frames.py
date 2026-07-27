#!/usr/bin/env python3
"""从当前 App 图标生成彩色托盘图标和录音脉冲动画。

所有帧都直接使用 src-tauri/icons/icon.png，避免托盘与任务栏出现不同人物版本。
空闲帧是铺满 44×44 安全区的头像；录音帧在右下角叠加六阶段红色脉冲点。
运行时仍由 src-tauri/src/tray.rs 按既有文件名逐帧切换。
"""

import os

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
ICONS = os.path.join(HERE, "..", "src-tauri", "icons")
SRC = os.path.join(ICONS, "icon.png")

SIZE = 44
AVATAR_SIZE = 42
PULSE_RADII = (4, 5, 6, 5, 4, 5)


def base_frame():
    source = Image.open(SRC).convert("RGBA")
    avatar = source.resize((AVATAR_SIZE, AVATAR_SIZE), Image.Resampling.LANCZOS)
    frame = Image.new("RGBA", (SIZE, SIZE), (0, 0, 0, 0))
    frame.alpha_composite(avatar, (1, 1))
    return frame


def recording_frame(radius):
    frame = base_frame()
    draw = ImageDraw.Draw(frame)
    cx, cy = 36, 36
    draw.ellipse(
        (cx - radius - 2, cy - radius - 2, cx + radius + 2, cy + radius + 2),
        fill=(255, 255, 255, 230),
    )
    draw.ellipse(
        (cx - radius, cy - radius, cx + radius, cy + radius),
        fill=(244, 67, 73, 255),
    )
    return frame


def main():
    base_frame().save(os.path.join(ICONS, "tray-logo-idle.png"))
    for i, radius in enumerate(PULSE_RADII):
        recording_frame(radius).save(os.path.join(ICONS, f"tray-logo-rec-{i}.png"))
    print(f"wrote latest-avatar tray idle + {len(PULSE_RADII)} recording frames")


if __name__ == "__main__":
    main()
