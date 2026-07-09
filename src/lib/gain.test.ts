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

  it("90 百分位避开瞬态尖峰:少量高峰不压制对安静主体的放大", () => {
    // 250 桶安静(20)+ 10 桶高峰(180)。响度代理取 90 百分位=20(不是峰值 180),
    // 故仍放大;峰值 180 让 CEILING/peak 成为约束项。
    const waveform = [...new Array(250).fill(20), ...new Array(10).fill(180)];
    const g = computeNoteGain([track(waveform)]);
    expect(g).toBeGreaterThan(1);
    expect(g).toBeCloseTo(CEILING / 180, 5); // CEILING/peak 绑定
    expect(180 * g).toBeLessThanOrEqual(CEILING); // 不削波不变量
  });

  it("中等电平:TARGET/loud 为约束项", () => {
    const g = computeNoteGain([track(new Array(260).fill(30))]);
    expect(g).toBeCloseTo(170 / 30, 5); // TARGET/loud 绑定(未撞 CEILING/MAX_BOOST)
  });
});
