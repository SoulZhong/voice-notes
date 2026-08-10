import { describe, expect, it } from "vitest";
import { hasSeq, matchesSpeakerFilter, nearestIndexByMs, searchHits } from "./liveView";

describe("searchHits", () => {
  const lines = [{ text: "预算下周对齐" }, { text: "Budget 已批" }, { text: "无关" }];
  it("大小写不敏感子串命中", () => {
    expect(searchHits(lines, "budget")).toEqual([1]);
    expect(searchHits(lines, "预算")).toEqual([0]);
  });
  it("空/全空白 query 不命中(避免全场高亮)", () => {
    expect(searchHits(lines, "")).toEqual([]);
    expect(searchHits(lines, "  ")).toEqual([]);
  });
});

describe("matchesSpeakerFilter", () => {
  it("空集不过滤;非空并集;未标注行在过滤态隐藏", () => {
    expect(matchesSpeakerFilter({ speaker: null }, new Set())).toBe(true);
    expect(matchesSpeakerFilter({ speaker: "S1" }, new Set(["S1", "S2"]))).toBe(true);
    expect(matchesSpeakerFilter({ speaker: "S3" }, new Set(["S1"]))).toBe(false);
    expect(matchesSpeakerFilter({ speaker: null }, new Set(["S1"]))).toBe(false);
  });
});

describe("hasSeq", () => {
  it("已含该 seq 判 true;空数组/不含判 false", () => {
    expect(hasSeq([{ seq: 1 }, { seq: 2 }], 2)).toBe(true);
    expect(hasSeq([{ seq: 1 }], 3)).toBe(false);
    expect(hasSeq([], 5)).toBe(false);
  });
});

describe("nearestIndexByMs", () => {
  const lines = [{ start_ms: 0 }, { start_ms: 60_000 }, { start_ms: 120_000 }];
  it("取 start_ms 最近的行;空数组 -1", () => {
    expect(nearestIndexByMs(lines, 55_000)).toBe(1);
    expect(nearestIndexByMs(lines, 200_000)).toBe(2);
    expect(nearestIndexByMs([], 0)).toBe(-1);
  });
});
