#!/usr/bin/env python3
"""生成菜单栏（托盘）动画帧：静止 idle + 录制/精修时的「疯狂记笔记」抖动帧。

设计背景：用户要求菜单栏图标直接用 App Logo（戴眼镜的小姑娘拿笔记本），并在
录制中 / 精修中让她「疯狂记笔记」。macOS 菜单栏图标是静态 NSImage，不解析 GIF，
所以「动」只能由运行时按定时器逐帧切图实现（见 src-tauri/src/tray.rs 的动画控制器）。
本脚本从 App Logo 裁出小姑娘，生成一枚静止帧 + 若干抖动帧（轻微旋转+位移，模拟
埋头疾书的忙碌感）。彩色非模板图（Logo 要显色，不走 macOS 模板反色）。

依赖 Pillow：这是构建期一次性产物生成工具（帧 PNG 提交入库、运行时 include_bytes），
Pillow 只是开发机手动跑本脚本时需要，不进 Cargo/运行时依赖。旋转/缩放真实照片式
Logo 必须用真正的图像库，无法像 gen_tray_icons.py 那样纯 stdlib 手写。

产物（写入 src-tauri/icons/）：
  - tray-logo-idle.png       静止（小姑娘端正，空闲时显示）
  - tray-logo-rec-0..5.png   6 帧抖动（录制/精修时循环播放）
所有帧 44x44 RGBA（2x 视网膜；菜单栏按 22pt 显示）。幂等，可重复运行覆盖。
"""

import os
from PIL import Image

HERE = os.path.dirname(os.path.abspath(__file__))
ICONS = os.path.join(HERE, "..", "src-tauri", "icons")
SRC = os.path.join(ICONS, "icon.png")
SZ = 44
# 填充画布比例（1.0=铺满，略过扫可裁掉圆角背景让主体更大）。
FIT = 1.02
# 变焦：裁取小姑娘中央这一比例的方形（<1 即放大，笔也随之变大）。
ZOOM = 0.82
# 裁剪中心相对几何中心下移的比例：突出下方的笔记本+笔（把「笔」抬到更显眼处）。
Y_BIAS = 0.06

# 抖动节奏：旋转角(度) + 竖直位移(px)，逐帧交替 = 埋头疾书的忙碌抖动。
# 幅度克制（≤5°、≤1px）：菜单栏 22px 下过猛会像抽搐。
ANGLES = [-4, 3, -4, 4, -3, 3]
BOBS = [0, -1, 1, 0, -1, 1]


def load_girl():
    src = Image.open(SRC).convert("RGBA")
    girl = src.crop(src.getbbox())  # 去掉圆角外的透明边
    w, h = girl.size
    # 方形变焦裁剪：以中央（略下移）为中心取 ZOOM 比例的方块 → 放大主体与笔。
    side = int(min(w, h) * ZOOM)
    cx = w // 2
    cy = int(h * (0.5 + Y_BIAS))
    left = max(0, min(w - side, cx - side // 2))
    top = max(0, min(h - side, cy - side // 2))
    crop = girl.crop((left, top, left + side, top + side))
    target = int(SZ * FIT)
    return crop.resize((target, target), Image.LANCZOS)


def frame(girl, angle, dy):
    g = girl.rotate(angle, resample=Image.BICUBIC, expand=True) if angle else girl
    canvas = Image.new("RGBA", (SZ, SZ), (0, 0, 0, 0))
    canvas.alpha_composite(g, ((SZ - g.width) // 2, (SZ - g.height) // 2 + dy))
    return canvas


def main():
    girl = load_girl()
    frame(girl, 0, 0).save(os.path.join(ICONS, "tray-logo-idle.png"))
    for i, (a, b) in enumerate(zip(ANGLES, BOBS)):
        frame(girl, a, b).save(os.path.join(ICONS, f"tray-logo-rec-{i}.png"))
    print(f"wrote tray-logo-idle.png + {len(ANGLES)} 抖动帧 → {os.path.normpath(ICONS)}")


if __name__ == "__main__":
    main()
