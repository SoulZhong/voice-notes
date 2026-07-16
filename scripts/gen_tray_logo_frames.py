#!/usr/bin/env python3
"""生成菜单栏(托盘)动画帧:小姑娘手持大铅笔在本子上「记笔记」。

设计背景:菜单栏图标直接用 App Logo 的小姑娘形象,但要求(1)去掉青绿方形背景、
(2)把她手里那支小铅笔夸张加长加粗(否则 22px 菜单栏根本看不到笔)、(3)给铅笔
加书写动画,让人一眼看出「正在记笔记」。macOS 菜单栏是静态 NSImage、不解析 GIF,
所以「动」由运行时定时器逐帧切图实现(见 src-tauri/src/tray.rs 的动画控制器)。

做法:
  1. 从 icon.png 抠出小姑娘:以四边为种子做泛洪,去掉与边界连通的背景像素
     (青绿 teal / 奶白 cream / 薄荷 mint 三类渐变)。她内部的青绿马甲被白衬衫
     包围、泛洪到不了,天然保住;她手持的笔记本也保留。
  2. 裁到上半身(头 + 手 + 笔记本),缩放铺满图标。
  3. 在她手上叠一支加长的大铅笔(单独矢量绘制,便于逐帧控制),笔尖沿自然书写
     方向(右下)插进本子;逐帧小幅摆动笔杆角度/位置 = 书写。
产物写入 src-tauri/icons/:
  - tray-logo-idle.png       静止帧(空闲显示)
  - tray-logo-rec-0..5.png   6 帧书写动画(录制时循环;停止即静止,见 tray.rs)
所有帧 44x44 RGBA(2x 视网膜;菜单栏按 22pt 显示)。彩色非模板图。幂等可重复运行。

依赖 Pillow:构建期一次性产物工具,帧 PNG 提交入库、运行时 include_bytes;Pillow
只在开发机手动跑本脚本时需要,不进 Cargo/运行时依赖。
"""

import math
import os
from collections import deque

from PIL import Image, ImageDraw

HERE = os.path.dirname(os.path.abspath(__file__))
ICONS = os.path.join(HERE, "..", "src-tauri", "icons")
SRC = os.path.join(ICONS, "icon.png")

SZ = 44           # 最终帧边长(2x of 22pt)
S = 4             # 超采样倍数,先在 SZ*S 上渲染再缩回,边缘抗锯齿
PENCIL_LEN = 32   # 铅笔长度(SZ 坐标系),加长后 22px 下才看得清
PENCIL_WD = 8     # 铅笔粗细


def is_bg(p):
    """背景像素判定:青绿/奶白/薄荷三类渐变。仅用于「与边界连通」的泛洪,
    她内部的青绿马甲因被白衬衫包围、泛洪到不了,不会被误删。"""
    r, g, b, a = p
    if a < 8:
        return False
    teal = g >= 90 and b >= 85 and g >= r + 15 and abs(g - b) <= 80 and r <= 150
    cream = r >= 205 and g >= 205 and 188 <= b <= 226 and (r - b) >= 18 and (g - b) >= 12
    mint = g >= r and g >= b and g >= 150 and b >= r - 30 and (g - r) >= 5
    return teal or cream or mint


def cutout_girl():
    """抠出小姑娘(去背景),裁到上半身(头 + 手 + 笔记本)。"""
    src = Image.open(SRC).convert("RGBA")
    W, H = src.size
    px = src.load()
    out = src.copy()
    opx = out.load()
    seen = bytearray(W * H)
    dq = deque()

    def push(x, y):
        if 0 <= x < W and 0 <= y < H and not seen[y * W + x]:
            seen[y * W + x] = 1
            dq.append((x, y))

    for x in range(W):
        push(x, 0)
        push(x, H - 1)
    for y in range(H):
        push(0, y)
        push(W - 1, y)
    while dq:
        x, y = dq.popleft()
        p = px[x, y]
        if p[3] == 0 or is_bg(p):
            if p[3] != 0:
                opx[x, y] = (0, 0, 0, 0)
            for dx, dy in ((1, 0), (-1, 0), (0, 1), (0, -1)):
                push(x + dx, y + dy)
    girl = out.crop(out.getbbox())
    # 裁到上半身:去掉四周透明留白、只留头+手+本子这段(基于 418x418 内容布局)
    return girl.crop((58, 26, 400, 412))


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
    """一帧:小姑娘铺满图标 + 大铅笔叠在手上(笔尖 target=(tipx,tipy),SZ 坐标)。"""
    C = Image.new("RGBA", (SZ * S, SZ * S), (0, 0, 0, 0))
    gh = int(SZ * S)
    gw = int(gh * girl.width / girl.height)
    g = girl.resize((gw, gh), Image.LANCZOS)
    C.alpha_composite(g, (int(SZ * S * 0.5 - gw * 0.5), 0))
    pen, anchor = make_pencil(int(PENCIL_LEN * S), int(PENCIL_WD * S))
    place(C, pen, angle, anchor, (tipx * S, tipy * S))
    return C.resize((SZ, SZ), Image.LANCZOS)


# 书写动画:笔杆沿右下书写轴(angle≈-33°)小幅摆动 + 笔尖位置微动 = 来回写字。
WRITE_SEQ = [(-35, 29, 40), (-29, 30, 41), (-37, 28, 39),
             (-31, 30, 40), (-34, 29, 41), (-30, 30, 40)]


def main():
    girl = cutout_girl()
    frame(girl, -33, 29, 40).save(os.path.join(ICONS, "tray-logo-idle.png"))  # 静止:书写轴居中一帧
    for i, (a, x, y) in enumerate(WRITE_SEQ):
        frame(girl, a, x, y).save(os.path.join(ICONS, f"tray-logo-rec-{i}.png"))
    print(f"wrote tray-logo-idle.png + {len(WRITE_SEQ)} 帧书写动画 → {os.path.normpath(ICONS)}")


if __name__ == "__main__":
    main()
