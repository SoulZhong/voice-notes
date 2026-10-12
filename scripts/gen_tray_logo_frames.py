#!/usr/bin/env python3
"""生成菜单栏(托盘)动画帧:小姑娘手持大铅笔在本子上奋笔疾书。

macOS 菜单栏是静态 NSImage、不解析 GIF,「动」由运行时定时器逐帧切图实现
(src-tauri/src/tray.rs,FRAME_MS)。

源图 scripts/assets/tray-girl.png 是从 #49 当时的 App 图标抠出的小姑娘上半身
(已去背景,头 + 手 + 笔记本)。之后 App 图标换过,不再从 icon.png 现抠,
否则托盘形象会跟着变。

铅笔单独矢量绘制,便于逐帧控制。动作只在铅笔上:4 个姿势沿右下书写轴来回划写,
笔杆摆 ±9°、笔尖移 3-4px——原先 ±4°/1px,缩到 22pt 几乎看不出在动(2026-10-13
用户反馈)。动作做大而帧率不变(省电取决于每秒换几次图,与每帧动作大小无关)。

产物写入 src-tauri/icons/:
  - tray-logo-idle.png       静止帧(空闲显示)
  - tray-logo-rec-0..3.png   4 帧书写动画(录制时循环)
所有帧 44x44 RGBA(2x 视网膜;菜单栏按 22pt 显示)。幂等可重复运行。
依赖 Pillow,只在开发机手动跑。
"""

import math
import os

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
ICONS = os.path.join(HERE, "..", "src-tauri", "icons")
GIRL = os.path.join(HERE, "assets", "tray-girl.png")

SZ = 44           # 最终帧边长(2x of 22pt)
S = 4             # 超采样倍数,先在 SZ*S 上渲染再缩回,边缘抗锯齿
PENCIL_LEN = 32   # 铅笔长度(SZ 坐标系),加长后 22px 下才看得清
PENCIL_WD = 8     # 铅笔粗细

# 静止帧:书写轴居中。(笔杆角度°, 笔尖 x, 笔尖 y),SZ 坐标。
IDLE = (-33, 29, 40)
# 书写动画:左上 ↔ 右下来回划,一笔高一笔低。
WRITE_SEQ = [(-44, 26, 39), (-26, 33, 41), (-42, 27, 41), (-24, 34, 39)]


def make_pencil(length, width):
    """横向铅笔(笔尖朝右)。橡皮 - 金属箍 - 笔杆 - 木锥 - 石墨尖。
    返回 (图像, 笔尖锚点):锚点用于把笔尖精确定位到本子上。"""
    l, w = length, width
    im = Image.new("RGBA", (l, w), (0, 0, 0, 0))
    d = ImageDraw.Draw(im)
    er = int(l * 0.10)
    fer = max(2, int(l * 0.04))
    wood = int(l * 0.14)
    tip = int(l * 0.07)
    body_end = l - wood - tip
    d.rounded_rectangle([0, 0, er, w - 1], radius=w // 2, fill=(242, 150, 158, 255))   # 橡皮
    d.rectangle([er, 0, er + fer, w - 1], fill=(196, 198, 204, 255))                   # 金属箍
    d.rectangle([er + fer, 0, body_end, w - 1], fill=(250, 196, 30, 255))              # 笔杆
    d.rectangle([er + fer, int(w * 0.60), body_end, w - 1], fill=(212, 156, 14, 255))  # 笔杆暗面
    d.line([er + fer, 1, body_end, 1], fill=(255, 228, 130, 255), width=2)             # 笔杆高光
    d.polygon([(body_end, 0), (body_end, w - 1), (body_end + wood, w // 2)], fill=(226, 186, 126, 255))  # 木锥
    d.polygon([(body_end + wood, int(w * 0.22)), (body_end + wood, int(w * 0.78)), (l - 1, w // 2)],
              fill=(46, 42, 44, 255))                                                  # 石墨尖
    return im, (l - 1, w / 2)


def place(canvas, img, angle, anchor, target):
    """把 img 旋转 angle(度,CCW)后贴到 canvas,使 img 内的 anchor 点落在 target。"""
    r = img.rotate(angle, expand=True, resample=Image.BICUBIC)
    ox, oy = img.width / 2, img.height / 2
    nx, ny = r.width / 2, r.height / 2
    th = math.radians(angle)
    ax, ay = anchor
    dx, dy = ax - ox, ay - oy
    rax = nx + dx * math.cos(th) + dy * math.sin(th)
    ray = ny - dx * math.sin(th) + dy * math.cos(th)
    canvas.alpha_composite(r, (int(round(target[0] - rax)), int(round(target[1] - ray))))


def frame(girl, angle, tipx, tipy):
    """一帧:小姑娘铺满图标 + 大铅笔叠在手上(笔尖落在 (tipx, tipy))。"""
    C = Image.new("RGBA", (SZ * S, SZ * S), (0, 0, 0, 0))
    gh = int(SZ * S)
    gw = int(gh * girl.width / girl.height)
    g = girl.resize((gw, gh), Image.LANCZOS)
    C.alpha_composite(g, (int(SZ * S * 0.5 - gw * 0.5), 0))
    pen, anchor = make_pencil(int(PENCIL_LEN * S), int(PENCIL_WD * S))
    place(C, pen, angle, anchor, (tipx * S, tipy * S))
    return C.resize((SZ, SZ), Image.LANCZOS)


def main():
    girl = Image.open(GIRL).convert("RGBA")
    frame(girl, *IDLE).save(os.path.join(ICONS, "tray-logo-idle.png"))
    for i, f in enumerate(WRITE_SEQ):
        frame(girl, *f).save(os.path.join(ICONS, f"tray-logo-rec-{i}.png"))
    print(f"wrote tray-logo-idle.png + {len(WRITE_SEQ)} 帧书写动画 → {os.path.normpath(ICONS)}")


if __name__ == "__main__":
    main()
