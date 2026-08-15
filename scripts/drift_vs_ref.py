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
import sys
import wave

import numpy as np


def read(path):
    w = wave.open(path)
    assert w.getnchannels() == 1 and w.getsampwidth() == 2, f"{path} 需要 16-bit mono"
    n = w.getnframes()
    # 直接按小端 i16 视图解,不走 struct.unpack:--native 的 6 分钟 48k 轨有 1800 万样本,
    # unpack 会先造出等量的 Python int 对象,峰值内存能奔着 GB 去(Codex 三轮 P2)。
    s = np.frombuffer(w.readframes(n), dtype="<i2").astype(np.float32) / 32768.0
    return s, w.getframerate()


# 相关系数的数值地板。低于它就当"根本没找到峰",直接丢掉这个窗。
# 为什么必须有:静音轨在每个滞后上的相关都是 0,argmax 会稳定地选中搜索边界,于是
# 每个窗的"错位"都等于同一个边界值——拟合出来是一条完美的 0.00ppm 直线、残差 0.000ms,
# 看着比真数据还漂亮(Codex 五轮 P2,拿一条 rms=0 的实测轨复现过)。
CORR_FLOOR = 0.02
# 窗内**去均值后**的 RMS 门槛(满量程归一后的幅度)。约 -80dBFS:低于它就当这段没内容,
# 免得拿一段恒定直流或极低电平的噪声去拟合。
RMS_FLOOR = 1e-4
# 峰显著性门槛(稳健 z 分数)。无关内容搜几千个滞后的最大值大约在 3~4.5σ,真峰远在此之上。
PEAK_Z = 6.0


def norm_xcorr_at(a, b, t0, win, lo, hi):
    """在 b 的 [t0+lo, t0+hi] 里找与 a[t0:t0+win] 最像的偏移。返回 (offset, corr, at_edge)。

    相关**去均值**(Pearson 口径),两侧都要过方差门槛。不去均值的话,一段恒定直流
    (比如 1 LSB 的"数字静音")在每个滞后上的相关都是 1.0,照样能拟合出完美直线
    (Codex 六轮 P2)。
    """
    seg = a[t0 : t0 + win].astype(np.float64)
    if len(seg) < win:
        return None  # 参考侧不够一个整窗:切片会静默变短,后面 cov/vary 长度对不上直接崩
    seg = seg - seg.mean()
    varx = float(np.dot(seg, seg))
    if varx / win < RMS_FLOOR**2:
        return None  # 参考侧这一段是静音或纯直流,没什么可对的
    start, end = t0 + lo, t0 + hi + win
    if start < 0 or end > len(b):
        return None
    window = b[start:end].astype(np.float64)
    # 逐滞后的局部均值/方差(cumsum 前缀和),再算 Pearson 相关。
    cs1 = np.concatenate(([0.0], np.cumsum(window)))
    cs2 = np.concatenate(([0.0], np.cumsum(window**2)))
    sum_y = cs1[win:] - cs1[: len(cs1) - win]
    sumsq_y = cs2[win:] - cs2[: len(cs2) - win]
    vary = sumsq_y - sum_y**2 / win
    # seg 已去均值,故 dot(seg, y) 天然等于去均值后的协方差分子。
    cov = np.correlate(window, seg, mode="valid")
    valid = vary / win >= RMS_FLOOR**2
    if not valid.any():
        return None  # 采集侧整段静音/纯直流
    ncorr = np.where(valid, cov / np.sqrt(varx * np.maximum(vary, 1e-12)), -1.0)
    k = int(np.argmax(ncorr))
    best = float(ncorr[k])
    if best < CORR_FLOOR:
        return None
    # 峰**显著性**检查:只看绝对相关值不够。搜几千个滞后,无关内容里最大的那个也能到
    # 0.02~0.03,足以骗过地板并拟合出一条像模像样的斜线(Codex 八轮实测两条独立噪声轨
    # 得出 +2052ppm)。真峰是一根尖刺,应远高于其余滞后的涨落:用中位数 + MAD 估背景,
    # 要求峰高出背景 PEAK_Z 个稳健标准差。本机真数据的窗即使 corr 只有 0.1,z 也在 10 以上。
    others = ncorr[valid]
    if others.size < 8:
        return None
    med = float(np.median(others))
    mad = float(np.median(np.abs(others - med))) * 1.4826
    if mad < 1e-9:
        # 背景涨落为 0 = 大量滞后并列(周期性内容如等间隔 click 串,或常数序列)。
        # 这时"显著性"无从谈起,原先按 inf 放行会让并列峰(常常就落在搜索边界)
        # 拟合出完美 0ppm(Codex 九轮 P2)。宁可丢掉这个窗。
        return None
    if (best - med) / mad < PEAK_Z:
        return None
    # 峰唯一性:主峰邻域之外若还有一个几乎一样高的峰,说明内容周期性/自相似,
    # 这个滞后读数没有意义(选中哪个纯看噪声)。邻域取 ±win/8。
    guard = max(1, win // 8)
    lo_i, hi_i = max(0, k - guard), min(len(ncorr), k + guard + 1)
    rest = np.concatenate([ncorr[:lo_i], ncorr[hi_i:]])
    if rest.size and float(rest.max()) > 0.9 * best:
        return None
    # 峰落在搜索边界上 = 真峰多半在窗外,读数不可信,交给调用方决定要不要用。
    at_edge = k == 0 or k == len(ncorr) - 1
    return (lo + k, best, at_edge)


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
        print("粗对齐失败:文件太短、全静音,或两者内容根本对不上(相关低于地板)")
        sys.exit(1)
    lag0, c0, c_edge = coarse
    print(f"粗对齐: 采集轨比参考晚 {lag0/rate:+.3f}s (corr {c0:.3f})")
    if c_edge:
        print("警告: 粗对齐的峰落在 ±10s 搜索边界上,起始差可能超出搜索范围,结果不可信")

    # 精测:搜索半径 ±0.2s 足够覆盖 500ppm×几百秒的漂移。
    pts, edge_hits, t, step = [], 0, rate * 2, rate * 10
    print("t_s\toffset_ms\tcorr")
    while t + win < len(ref) and t + lag0 + win + rate // 5 < len(cap):
        r = norm_xcorr_at(ref, cap, t, win, lag0 - rate // 5, lag0 + rate // 5)
        if r and r[1] >= min_corr:
            off_ms = (r[0] - lag0) * 1000.0 / rate
            edge = "  ←边界" if r[2] else ""
            print(f"{t/rate:.0f}\t{off_ms:+.3f}\t{r[1]:.2f}{edge}")
            edge_hits += 1 if r[2] else 0
            pts.append((t / rate, off_ms))
        t += step
    if edge_hits:
        print(f"警告: {edge_hits} 个窗的峰落在 ±0.2s 搜索边界上,漂移可能超出搜索范围")
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
    print(
        "提醒:这条斜率 = 补偿残余 + master 时钟误差 + **播放侧设备时钟误差**。"
        "前两项之外的第三项通常没人量过,所以不要拿它减一减就当成补偿残余的上界;"
        "master 误差还是两轨共模的,本来就不产生轨间错位。判据 1 要用声学口径量。"
    )


main()
