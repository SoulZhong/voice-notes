#!/usr/bin/env python3
"""E1 标定刺激音生成器(issue #100 条 3)。

为什么不用等间隔 click:互相关在周期性刺激下会在 ±半周期处出现同样高的旁瓣,
延迟估计可能整体锁到错误的那一支。500ms 周期的 click track 实测就撞上过——
E1 标定里 440s 那一窗给出 +198ms 的离群值,恰是半个周期。

伪随机间隔消除这个歧义:间隔在 [min, max] 内均匀抖动,自相关除零点外没有
第二个高峰,窗内延迟只有一个解。种子固定 → 同一条刺激可复现。

用法:
    python3 scripts/gen_calibration_clicks.py out.wav [--minutes 12]

生成的 WAV 用系统播放器循环播放即可(见 scripts/drift-calibration.md 第 1 步)。
"""

import argparse
import array
import math
import random
import wave

SAMPLE_RATE = 48_000
# click 本体:2ms 的窄带脉冲,带 raised-cosine 包络。
# 纯冲激的能量集中在奈奎斯特附近,经蓝牙 HFP(16k)一压就没了;2ms/3kHz
# 在两条链路上都留得下,又足够窄以保证互相关峰锐利。
CLICK_MS = 2.0
CLICK_HZ = 3_000.0
AMPLITUDE = 0.6
# 间隔抖动区间(秒)。下限保证两次 click 的相关峰不会互相污染(远大于
# MAX_DELAY_MS 量级),上限保证每个 10s 分析窗里都有足够多的 click。
MIN_GAP_S = 0.30
MAX_GAP_S = 0.75
SEED = 20260812


def click_waveform() -> array.array:
    """click 本体,直接出 int16——刺激绝大部分是静音,只有这一小段需要算。"""
    n = int(SAMPLE_RATE * CLICK_MS / 1000.0)
    out = array.array("h")
    for i in range(n):
        # raised-cosine 窗:两端归零,避免咔哒声本身引入宽带边缘。
        env = 0.5 * (1.0 - math.cos(2.0 * math.pi * i / max(n - 1, 1)))
        s = AMPLITUDE * env * math.sin(2.0 * math.pi * CLICK_HZ * i / SAMPLE_RATE)
        out.append(int(max(-1.0, min(1.0, s)) * 32767))
    return out


def build(minutes: float, seed: int) -> array.array:
    """整条刺激用一个 int16 array 承载,只在 click 位置写入。

    不要用 `[0.0] * total` 再逐样本 struct.pack:12 分钟 = 3456 万样本,那样
    会物化上千万个 Python float/bytes 对象,峰值内存 1-2GB 且慢得离谱
    (Codex review P1)。array('h') 是紧凑缓冲(12 分钟仅 ~69MB),写入量也
    从"总样本数"降到"click 数 × click 长度"(约 15 万次)。
    """
    rng = random.Random(seed)
    total = int(SAMPLE_RATE * minutes * 60)
    buf = array.array("h", bytes(2 * total))  # 全零,一次性分配
    click = click_waveform()
    n = len(click)
    pos = int(SAMPLE_RATE * MIN_GAP_S)
    count = 0
    while pos + n < total:
        buf[pos : pos + n] = click  # 静音底噪上直接覆盖,无需逐样本相加
        pos += int(SAMPLE_RATE * rng.uniform(MIN_GAP_S, MAX_GAP_S))
        count += 1
    print(f"{count} 个 click,{minutes:.1f} 分钟,间隔 {MIN_GAP_S}-{MAX_GAP_S}s(seed={seed})")
    return buf


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--minutes", type=float, default=12.0)
    ap.add_argument("--seed", type=int, default=SEED)
    args = ap.parse_args()

    buf = build(args.minutes, args.seed)
    with wave.open(args.out, "wb") as w:
        w.setnchannels(1)
        w.setsampwidth(2)
        w.setframerate(SAMPLE_RATE)
        # array 已是小端 int16(本仓库目标平台均为小端);byteswap 一下更保险。
        import sys

        if sys.byteorder == "big":
            buf.byteswap()
        w.writeframes(buf.tobytes())
    print(f"已写出 {args.out}")


if __name__ == "__main__":
    main()
