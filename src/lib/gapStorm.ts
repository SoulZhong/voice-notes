/** 每源当前的断流风暴强度(窗内无音频帧的时长占比,整数百分比);null = 没在风暴中。 */
export type GapStormState = { mic: number | null; system: number | null };

/**
 * 边沿事件 → 新状态。纯函数、与 svelte 运行时无关,便于把时序问题
 * (在途事件、停录竞态)钉成用例——放 .svelte.ts 里会被 $app/* 依赖拖住测不了。
 *
 * `isLive=false` 时一律不收(Codex 二轮 P2):停录后仍可能有在途事件,收下它就会
 * 重新点亮横幅,而那之后 tap 已经结束,永远不会再有平息沿——横幅就永久挂着了。
 */
export function nextGapStorm(
  prev: GapStormState,
  ev: { source?: string; state: string; gap_pct?: number },
  isLive: boolean,
): GapStormState {
  if (!isLive) return prev;
  const src = ev.source === "system" ? "system" : "mic";
  if (ev.state === "gap_storm") return { ...prev, [src]: ev.gap_pct ?? 0 };
  if (ev.state === "gap_storm_over") return { ...prev, [src]: null };
  return prev;
}
