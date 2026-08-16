/** 录制页实时音轨的取值与整形(纯函数,便于单测)。
 *
 * 为什么需要整形:后端给的是瞬时 RMS,页面按 -50..0dBFS 线性映射成 0..100 的 pct。
 * 直接把 pct 当条高画出来有三个毛病,真机上就是一条横贯全屏的红虚线:
 *   ① 静音也画:原实现给每根条兜底 6% 高度并涂满 record 红,一行 1300px 里绝大多数
 *      采样是静音,于是满屏红破折号——既难看,也违反"录制红点是唯一常驻彩色信号";
 *   ② 动态范围浪费:正常说话约 -30dBFS 只到 40%,32px 高的行里只占 13px,峰谷差看不出来;
 *   ③ 音节撕裂:120ms 采样撞上 ~4Hz 的音节调制,音节间的低谷被打到底,读起来是断续栅栏。
 *
 * 对应三步:噪声门 + gamma 展开 + 滚动峰值归一,再加一条快起慢落的包络。
 */

/** 噪声门(pct)。≈ -44dBFS 以下当静音——只画基线,不画红条。 */
export const WAVE_GATE_PCT = 12;
/** 门限之上的展开指数。<1 抬低端,让普通说话音量占到可见高度的一半以上。 */
export const WAVE_GAMMA = 0.7;
/** 包络释放系数(每 120ms 一帧):新值取 max(当前采样, 上一帧 × 系数)。
 *  快起慢落是录音机电平表的常规做法,用来填平音节间的低谷(低谷通常只有 1~2 帧)。
 *  0.7 对应的实际拖尾(单测锁死):满量程 → 噪声门 ≈0.72s,常见说话档(pct 50)→ 门限 ≈0.60s。
 *  别再往大调:0.85 时说话档拖尾 1.05s、满量程 1.57s,静音之后还会红上一秒多,
 *  等于告诉用户"还在收音"(Codex 审查指出的口径不实)。 */
export const WAVE_RELEASE = 0.7;
/** 归一化的峰值下限(pct)。安静房间里若按窗口实际峰值归一,会把底噪放大成满格波形;
 *  取 ≈ -35dBFS 作下限,意思是"比这还轻的整段,就该显得轻"。 */
export const WAVE_MIN_PEAK_PCT = 30;

/** 包络推进一帧:快起(直接取采样)慢落(按 release 衰减)。 */
export function envelopeStep(prev: number, sample: number, release = WAVE_RELEASE): number {
  return Math.max(sample, prev * release);
}

/** 门限 + gamma:pct(0..100) → 0..1 的整形值,0 表示"静音,只画基线"。 */
export function shapeLevel(pct: number, gate = WAVE_GATE_PCT, gamma = WAVE_GAMMA): number {
  if (!Number.isFinite(pct) || pct <= gate) return 0;
  const norm = (Math.min(100, pct) - gate) / (100 - gate);
  return Math.pow(norm, gamma);
}

/** 滚动峰值归一:窗口内最响的一根顶到满格,但峰值不低于 WAVE_MIN_PEAK_PCT 对应的整形值。
 *  返回 0..1,0 仍表示静音。 */
export function normalizeBars(shaped: number[], minPeakPct = WAVE_MIN_PEAK_PCT): number[] {
  const floorPeak = shapeLevel(minPeakPct);
  let peak = floorPeak;
  for (const v of shaped) if (v > peak) peak = v;
  if (peak <= 0) return shaped.map(() => 0);
  return shaped.map((v) => (v <= 0 ? 0 : Math.min(1, v / peak)));
}

/** 一根条的最终几何/着色。静音走 1px 基线色,有声按归一值给高度与不透明度。
 *  maxPx 是峰值高度(留头:容器比它高几像素,避免顶到边)。 */
export function barStyle(v: number, maxPx: number): { height: number; silent: boolean; opacity: number } {
  if (v <= 0) return { height: 1, silent: true, opacity: 1 };
  return {
    height: Math.max(3, Math.round(v * maxPx)),
    silent: false,
    // 弱信号退到 0.6:红只给真正有声的段落,轻声不喧宾夺主。
    opacity: Math.round((0.6 + 0.4 * Math.min(1, v / 0.5)) * 100) / 100,
  };
}

/** 容器宽度 → 画多少根条(节距 4px = 2px 条 + 2px 间隔)。
 *  宽屏画得更密(历史更长),窄屏自动减根数,不再是"240 根拉满任意宽度"。 */
export function barCountFor(widthPx: number, cap: number): number {
  if (!Number.isFinite(widthPx) || widthPx <= 0) return Math.min(60, cap);
  return Math.max(24, Math.min(cap, Math.floor(widthPx / 4)));
}
