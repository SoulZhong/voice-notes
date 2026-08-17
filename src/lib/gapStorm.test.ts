import { describe, expect, it } from "vitest";
import { nextGapStorm, type GapStormState } from "./gapStorm";

const NONE: GapStormState = { mic: null, system: null };

describe("断流风暴状态转移", () => {
  it("起风暴按源点亮", () => {
    expect(nextGapStorm(NONE, { source: "mic", state: "gap_storm", gap_pct: 14 }, true))
      .toEqual({ mic: 14, system: null });
    expect(nextGapStorm(NONE, { source: "system", state: "gap_storm", gap_pct: 7 }, true))
      .toEqual({ mic: null, system: 7 });
  });

  it("平息沿只撤自己那一源", () => {
    const both: GapStormState = { mic: 14, system: 7 };
    expect(nextGapStorm(both, { source: "mic", state: "gap_storm_over" }, true))
      .toEqual({ mic: null, system: 7 });
  });

  /// Codex 二轮 P2:停录后仍在途的事件会把横幅重新点亮,而那之后 tap 已结束,
  /// 永远不会再有平息沿——横幅就永久挂着了。非录制期的事件一律不收。
  it("非录制期的在途事件一律不收", () => {
    expect(nextGapStorm(NONE, { source: "mic", state: "gap_storm", gap_pct: 14 }, false))
      .toEqual(NONE);
    // 连平息沿也不收:停录时状态已由 clear 归零,再处理只是徒增路径。
    expect(nextGapStorm({ mic: 14, system: null }, { source: "mic", state: "gap_storm_over" }, false))
      .toEqual({ mic: 14, system: null });
  });

  it("与风暴无关的健康事件不动状态", () => {
    const s: GapStormState = { mic: 14, system: null };
    expect(nextGapStorm(s, { source: "mic", state: "recovered" }, true)).toEqual(s);
    expect(nextGapStorm(s, { source: "mic", state: "lost" }, true)).toEqual(s);
  });

  it("缺 gap_pct 时按 0 记,仍算点亮(有总比没有强)", () => {
    expect(nextGapStorm(NONE, { source: "mic", state: "gap_storm" }, true))
      .toEqual({ mic: 0, system: null });
  });

  it("source 缺省按 mic 处理,不静默丢事件", () => {
    expect(nextGapStorm(NONE, { state: "gap_storm", gap_pct: 9 }, true))
      .toEqual({ mic: 9, system: null });
  });
});
