import { describe, it, expect } from "vitest";
import { computeNoteGain, MAX_BOOST, CEILING } from "./gain";
import type { TrackInfo } from "./notes";

const track = (waveform: number[] | null): TrackInfo => ({
  source: "mic",
  path: "x",
  offset_ms: 0,
  duration_ms: 1000,
  waveform,
});

describe("computeNoteGain", () => {
  it("放大很轻的笔记:gain>1 且放大后峰值不削波", () => {
    const g = computeNoteGain([track(new Array(260).fill(20))]);
    expect(g).toBeGreaterThan(1);
    expect(20 * g).toBeLessThanOrEqual(CEILING);
  });

  it("已够响的笔记:gain 钳到 1(只增不减)", () => {
    const g = computeNoteGain([track(new Array(260).fill(250))]);
    expect(g).toBe(1);
  });

  it("全静音笔记:gain=1(除零守卫)", () => {
    expect(computeNoteGain([track(new Array(260).fill(0))])).toBe(1);
  });

  it("某轨无波形:gain=1(不猜)", () => {
    expect(computeNoteGain([track(null)])).toBe(1);
  });

  it("空 tracks:gain=1", () => {
    expect(computeNoteGain([])).toBe(1);
  });

  it("极轻音频:gain 不超过 MAX_BOOST", () => {
    const g = computeNoteGain([track(new Array(260).fill(1))]);
    expect(g).toBeLessThanOrEqual(MAX_BOOST);
  });

  it("整条笔记一个增益:一条近满幅的轨把整条钳到 1", () => {
    const g = computeNoteGain([
      track(new Array(260).fill(20)),
      track(new Array(260).fill(250)),
    ]);
    expect(g).toBe(1);
  });
});
