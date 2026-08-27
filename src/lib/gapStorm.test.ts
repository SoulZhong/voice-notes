import { describe, expect, it } from "vitest";
import { nextGapStorm, type GapStormState } from "./gapStorm";

const NONE: GapStormState = { mic: null, system: null, systemUnmonitored: false };

describe("断流风暴状态转移", () => {
  it("起风暴按源点亮", () => {
    expect(nextGapStorm(NONE, { source: "mic", state: "gap_storm", gap_pct: 14 }, true))
      .toEqual({ mic: 14, system: null, systemUnmonitored: false });
    expect(nextGapStorm(NONE, { source: "system", state: "gap_storm", gap_pct: 7 }, true))
      .toEqual({ mic: null, system: 7, systemUnmonitored: false });
  });

  it("平息沿只撤自己那一源", () => {
    const both: GapStormState = { mic: 14, system: 7, systemUnmonitored: false };
    expect(nextGapStorm(both, { source: "mic", state: "gap_storm_over" }, true))
      .toEqual({ mic: null, system: 7, systemUnmonitored: false });
  });

  /// Codex 二轮 P2:停录后仍在途的事件会把横幅重新点亮,而那之后 tap 已结束,
  /// 永远不会再有平息沿——横幅就永久挂着了。非录制期的事件一律不收。
  it("非录制期的在途事件一律不收", () => {
    expect(nextGapStorm(NONE, { source: "mic", state: "gap_storm", gap_pct: 14 }, false))
      .toEqual(NONE);
    // 连平息沿也不收:停录时状态已由 clear 归零,再处理只是徒增路径。
    expect(nextGapStorm({ mic: 14, system: null, systemUnmonitored: false }, { source: "mic", state: "gap_storm_over" }, false))
      .toEqual({ mic: 14, system: null, systemUnmonitored: false });
  });

  it("与风暴无关的健康事件不动状态", () => {
    const s: GapStormState = { mic: 14, system: null, systemUnmonitored: false };
    expect(nextGapStorm(s, { source: "mic", state: "recovered" }, true)).toEqual(s);
    expect(nextGapStorm(s, { source: "mic", state: "lost" }, true)).toEqual(s);
  });

  it("缺 gap_pct 时按 0 记,仍算点亮(有总比没有强)", () => {
    expect(nextGapStorm(NONE, { source: "mic", state: "gap_storm" }, true))
      .toEqual({ mic: 0, system: null, systemUnmonitored: false });
  });

  it("source 缺省按 mic 处理,不静默丢事件", () => {
    expect(nextGapStorm(NONE, { state: "gap_storm", gap_pct: 9 }, true))
      .toEqual({ mic: 9, system: null, systemUnmonitored: false });
  });

  /** Windows loopback 盲区声明(issue #125):system 的 unmonitored 置位且不被风暴
   *  沿清掉;mic 源不吃这个态;停录(isLive=false)不收。 */
  it("unmonitored 声明 Windows system 盲区", () => {
    const on = nextGapStorm(NONE, { source: "system", state: "unmonitored" }, true);
    expect(on).toEqual({ mic: null, system: null, systemUnmonitored: true });
    // 风暴沿不清声明:限制是平台属性,不随风暴起伏
    expect(nextGapStorm(on, { source: "mic", state: "gap_storm", gap_pct: 3 }, true))
      .toEqual({ mic: 3, system: null, systemUnmonitored: true });
    // mic 源的 unmonitored 无意义,不置位
    expect(nextGapStorm(NONE, { source: "mic", state: "unmonitored" }, true)).toEqual(NONE);
    // 停录不收
    expect(nextGapStorm(NONE, { source: "system", state: "unmonitored" }, false)).toEqual(NONE);
  });
});
