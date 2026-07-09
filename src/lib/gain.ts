import type { TrackInfo } from "./notes";

// 增益常量(0..255 绝对峰值桶量纲;冒烟可调)。
export const TARGET = 170; // 目标响度代理:≈良好录音的常态电平
export const CEILING = 250; // 放大后峰值上限,留余量不削顶(<255)
export const MAX_BOOST = 8; // 最大放大倍数:防把噪声地板轰起来

// 响度代理取非零桶的 90 百分位,避开单个瞬态尖峰。
const LOUDNESS_PERCENTILE = 0.9;

/**
 * 回放响度归一化增益:整条笔记一个增益,只增不减。
 * 输入各轨 waveform(0..255 绝对峰值桶)。数据不足 / 已够响时返回 1(不归一)。
 */
export function computeNoteGain(tracks: TrackInfo[]): number {
  const buckets: number[] = [];
  for (const t of tracks) {
    if (!t.waveform) continue;
    for (const v of t.waveform) buckets.push(v);
  }
  const nonzero = buckets.filter((v) => v > 0);
  if (nonzero.length === 0) return 1; // 无有效样本 / 全静音:不归一

  const peak = Math.max(...buckets); // 绝对峰值(0..255)
  if (peak <= 0) return 1;

  const sorted = [...nonzero].sort((a, b) => a - b);
  const idx = Math.min(sorted.length - 1, Math.floor(sorted.length * LOUDNESS_PERCENTILE));
  const loud = sorted[idx]; // 响度代理

  // CEILING/peak 保证放大后峰值 < 255,构造上不削波,无需限幅器。
  const gain = Math.min(TARGET / loud, CEILING / peak, MAX_BOOST);
  return Math.max(1, gain); // 只增不减
}
