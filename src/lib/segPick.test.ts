import { describe, expect, it } from "vitest";
import { contiguousRun, overlappedMicSeqs, seqRange } from "./segPick";

const segs = [
  { seq: 10, speaker: "S1" },
  { seq: 11, speaker: "S1" },
  { seq: 12, speaker: "S2" },
  { seq: 13, speaker: "S1" },
  { seq: 14, speaker: null },
  { seq: 15, speaker: null },
];

describe("contiguousRun", () => {
  it("向两侧扩到说话人变化为止", () => {
    expect(contiguousRun(segs, 11)).toEqual([10, 11]);
    expect(contiguousRun(segs, 13)).toEqual([13]);
    // 未标注(null)也按同类连续
    expect(contiguousRun(segs, 15)).toEqual([14, 15]);
  });
  it("未知 seq 返回空", () => {
    expect(contiguousRun(segs, 99)).toEqual([]);
  });
});

describe("seqRange", () => {
  it("端点顺序无关,含两端", () => {
    expect(seqRange(segs, 13, 11)).toEqual([11, 12, 13]);
    expect(seqRange(segs, 10, 10)).toEqual([10]);
  });
});

describe("overlappedMicSeqs", () => {
  it("mic 段被 system 覆盖 ≥80% 才入选;system 段与独立 mic 段不选", () => {
    const segs = [
      { seq: 1, source: "system", start_ms: 0, end_ms: 10_000 },
      { seq: 2, source: "mic", start_ms: 1000, end_ms: 3000 }, // 全覆盖 → 选
      { seq: 3, source: "mic", start_ms: 9000, end_ms: 15_000 }, // 覆盖 1/6 → 不选
      { seq: 4, source: "mic", start_ms: 20_000, end_ms: 22_000 }, // 独立 → 不选
    ];
    expect(overlappedMicSeqs(segs)).toEqual([2]);
  });
});
