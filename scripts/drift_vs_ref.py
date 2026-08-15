#!/usr/bin/env python3
"""参考文件 vs 采集轨:逐窗互相关求错位曲线,线性拟合出 ppm(E3 spike 的分析侧工具)。

为什么不用 `bin/xcorr_align`:
1. 它的搜索窗只有 ±rate/4(±0.25s),而"先起录后放音"的起始差是秒级,够不着;
2. 它只收 corr>0.5 的窗。与外部参考比对时,峰**位置**仍然正确、但峰**值**会被采集端
   48k→16k 无抗混叠抽取的相位滑移压低(实测大量窗落在 0.1~0.2),按 0.5 卡会丢掉九成
   数据点。本工具打印全部窗的 (t, offset, corr) 供人眼核对,拟合默认用全部窗。
轨间(mic vs system)比对**不要**用本工具,用 `bin/xcorr_align`:那条路两轨走同一条抽取,
0.5 阈值是合适的,且判据 1 的口径就是它。

符号约定与 xcorr_align 一致:slope(a,b) ≈ rate(b) - rate(a)。这里 a=参考(真时间轴)、
b=采集轨,正值 = 采集轨里的内容比参考跑得快。

用法:
    python3 scripts/drift_vs_ref.py <参考.wav> <采集.wav> [--min-corr 0.0]
两个文件都要 16-bit mono 且采样率相同。
"""
import struct
import sys
import wave

import numpy as np


def read(path):
    w = wave.open(path)
    assert w.getnchannels() == 1 and w.getsampwidth() == 2, f"{path} 需要 16-bit mono"
    n = w.getnframes()
    s = np.array(struct.unpack("<%dh" % n, w.readframes(n)), dtype=np.float32) / 32768.0
    return s, w.getframerate()


def norm_xcorr_at(a, b, t0, win, lo, hi):
    """在 b 的 [t0+lo, t0+hi] 里找与 a[t0:t0+win] 最像的偏移。返回 (offset, corr)。"""
    seg = a[t0 : t0 + win]
    ea = float(np.dot(seg, seg))
    if ea < 1e-6:
        return None
    start, end = t0 + lo, t0 + hi + win
    if start < 0 or end > len(b):
        return None
    window = b[start:end]
    corr = np.correlate(window, seg, mode="valid")
    cs = np.concatenate(([0.0], np.cumsum(window.astype(np.float64) ** 2)))
    energies = cs[win:] - cs[: len(cs) - win]
    ncorr = corr / np.sqrt(ea * np.maximum(energies, 1e-12))
    k = int(np.argmax(ncorr))
    return (lo + k, float(ncorr[k]))


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        sys.exit(2)
    ref_path, cap_path = sys.argv[1], sys.argv[2]
    min_corr = 0.0
    if "--min-corr" in sys.argv:
        min_corr = float(sys.argv[sys.argv.index("--min-corr") + 1])
    ref, r1 = read(ref_path)
    cap, r2 = read(cap_path)
    assert r1 == r2, f"两个文件采样率不同: {r1} vs {r2}"
    rate = r1
    win = rate  # 1s 窗,与 xcorr_align 一致

    # 粗对齐:头部一个长窗在 ±10s 里找起始偏移(t0 必须 ≥ 搜索半径,否则起点为负)。
    coarse = norm_xcorr_at(ref, cap, rate * 15, win * 2, -10 * rate, 10 * rate)
    if coarse is None:
        print("粗对齐失败:文件太短或全静音")
        sys.exit(1)
    lag0, c0 = coarse
    print(f"粗对齐: 采集轨比参考晚 {lag0/rate:+.3f}s (corr {c0:.3f})")

    # 精测:搜索半径 ±0.2s 足够覆盖 500ppm×几百秒的漂移。
    pts, t, step = [], rate * 2, rate * 10
    print("t_s\toffset_ms\tcorr")
    while t + win < len(ref) and t + lag0 + win + rate // 5 < len(cap):
        r = norm_xcorr_at(ref, cap, t, win, lag0 - rate // 5, lag0 + rate // 5)
        if r and r[1] >= min_corr:
            off_ms = (r[0] - lag0) * 1000.0 / rate
            print(f"{t/rate:.0f}\t{off_ms:+.3f}\t{r[1]:.2f}")
            pts.append((t / rate, off_ms))
        t += step
    if len(pts) < 2:
        print("有效窗不足,无法拟合")
        sys.exit(1)

    xs = np.array([p[0] for p in pts])
    ys = np.array([p[1] for p in pts])
    slope_ms_per_s, intercept = np.polyfit(xs, ys, 1)
    ppm = slope_ms_per_s * 1000.0
    resid = ys - (slope_ms_per_s * xs + intercept)
    print(
        f"\n窗数 {len(pts)} / 斜率 {ppm:+.2f}ppm = {ppm*3.6:+.1f}ms/小时 "
        f"/ 拟合残差 rms {resid.std():.3f}ms max|resid| {np.abs(resid).max():.3f}ms"
    )
    print(f"整段错位变化: 首窗 {ys[0]:+.3f}ms → 末窗 {ys[-1]:+.3f}ms (跨度 {xs[-1]-xs[0]:.0f}s)")
    print("提醒:斜率里含 master 时钟相对真实时间的**共模**误差,它不产生轨间错位。")


main()
