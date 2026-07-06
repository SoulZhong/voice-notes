#!/usr/bin/env python3
"""生成两枚 44x44 RGBA 托盘图标（纯 stdlib，手写 PNG chunk）。

为什么纯 stdlib：托盘图标是构建期产物、要提交入库，不该为它引入 Pillow 之类
第三方依赖；PNG 的 IHDR/IDAT/IEND chunk 用 struct 打包、zlib 压缩+CRC 即可完整手写。

产物：
  - tray-idle.png：黑色圆环（线宽约 3px，中心透明）。作为 macOS 模板图（template），
    系统会按亮/暗菜单栏自动反色，故只需画黑色 + alpha，颜色本身会被系统抹掉。
  - tray-recording.png：实心圆 #ff6161（直径约 28px 居中）。录制态要显红色，
    故运行时不作为模板图渲染（模板会把颜色抹成单色）。

抗锯齿：按像素中心到圆心的距离对边缘做 1px 线性渐变 alpha（disk_alpha），
简单可靠，无需超采样。脚本幂等，可重复运行覆盖产物。
"""

import os
import struct
import zlib

SIZE = 44
# 圆心取几何中心：像素 (x, y) 采样点为 (x+0.5, y+0.5)，故中心 = SIZE/2。
CENTER = SIZE / 2.0


def disk_alpha(dist, radius):
    """半径 radius 的实心圆在距圆心 dist 处的覆盖率(0..1)：边缘 1px 内线性渐变。"""
    return max(0.0, min(1.0, radius - dist + 0.5))


def ring_pixel(x, y):
    """黑色圆环：外径 20、内径 17（线宽 3），中心透明。返回 (r,g,b,a)。"""
    dx = x + 0.5 - CENTER
    dy = y + 0.5 - CENTER
    d = (dx * dx + dy * dy) ** 0.5
    inside_outer = disk_alpha(d, 20.0)          # 在外圆之内
    outside_inner = max(0.0, min(1.0, d - 17.0 + 0.5))  # 在内圆之外（反向渐变）
    a = min(inside_outer, outside_inner)         # 环 = 外圆内 ∩ 内圆外
    return (0, 0, 0, round(a * 255))


def dot_pixel(x, y):
    """实心红圆 #ff6161：半径 14（直径 28）居中。返回 (r,g,b,a)。"""
    dx = x + 0.5 - CENTER
    dy = y + 0.5 - CENTER
    d = (dx * dx + dy * dy) ** 0.5
    a = disk_alpha(d, 14.0)
    return (0xFF, 0x61, 0x61, round(a * 255))


def write_png(path, pixel_fn):
    """按 pixel_fn(x,y)->(r,g,b,a) 逐像素渲染并写出 8-bit RGBA PNG。"""
    raw = bytearray()
    for y in range(SIZE):
        raw.append(0)  # 每行过滤器类型 0（None）
        for x in range(SIZE):
            raw += bytes(pixel_fn(x, y))

    def chunk(tag, data):
        return (
            struct.pack(">I", len(data))
            + tag
            + data
            + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
        )

    ihdr = struct.pack(">IIBBBBB", SIZE, SIZE, 8, 6, 0, 0, 0)  # 8-bit, 色型6=RGBA
    idat = zlib.compress(bytes(raw), 9)
    with open(path, "wb") as f:
        f.write(b"\x89PNG\r\n\x1a\n")
        f.write(chunk(b"IHDR", ihdr))
        f.write(chunk(b"IDAT", idat))
        f.write(chunk(b"IEND", b""))


def main():
    icons_dir = os.path.join(os.path.dirname(__file__), "..", "src-tauri", "icons")
    icons_dir = os.path.abspath(icons_dir)
    write_png(os.path.join(icons_dir, "tray-idle.png"), ring_pixel)
    write_png(os.path.join(icons_dir, "tray-recording.png"), dot_pixel)
    print(f"已生成 tray-idle.png / tray-recording.png（{SIZE}x{SIZE}）于 {icons_dir}")


if __name__ == "__main__":
    main()
