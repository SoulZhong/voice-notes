import { describe, expect, it } from "vitest";
import {
  effectiveChars,
  lowDensityStat,
  shouldOfferBetterEngine,
  LOW_DENSITY_HINT_MIN_COUNT,
} from "./lowDensity";

const seg = (s: number, e: number, text: string) => ({ start_ms: s, end_ms: e, text });

describe("疑似识别失败段", () => {
  it("标点不算有效字符", () => {
    expect(effectiveChars(".")).toBe(0);
    expect(effectiveChars("。？！ ，")).toBe(0);
    expect(effectiveChars("应该我觉得。")).toBe(5);
    expect(effectiveChars("Do.")).toBe(2);
    expect(effectiveChars("A1")).toBe(2);
  });

  it("命中真实事故形态:十几秒只出一个句号", () => {
    // 2026-08-16 实测原样:14.6 秒 → "."、10.7 秒 → 「应该我觉得。」(5 字,不命中)。
    const st = lowDensityStat([seg(0, 14_600, "."), seg(20_000, 30_700, "应该我觉得。")]);
    expect(st.count).toBe(1);
    expect(st.seconds).toBe(14);
  });

  it("短段不算:三秒以内本就可能只有两三个字", () => {
    expect(lowDensityStat([seg(0, 2_900, "。"), seg(0, 1_000, "嗯")]).count).toBe(0);
  });

  it("长段但有内容不算", () => {
    expect(lowDensityStat([seg(0, 12_000, "这一段说了很多话确实转出来了")]).count).toBe(0);
  });

  it("边界:恰好 3 秒且恰好 2 个有效字符算命中", () => {
    expect(lowDensityStat([seg(0, 3_000, "对。吧")]).count).toBe(1);
    expect(lowDensityStat([seg(0, 3_000, "对吧啊")]).count).toBe(0);
  });

  it("时长累计按秒向下取整", () => {
    const st = lowDensityStat([seg(0, 3_500, "."), seg(0, 4_400, "，")]);
    expect(st.count).toBe(2);
    expect(st.seconds).toBe(7);
  });
});

describe("要不要建议换引擎", () => {
  const base = { noteEngine: "sense_voice", ready: true, actionable: true };
  const many = { count: 9, seconds: 90 };

  it("过线、模型在本地、此刻能跑:建议", () => {
    expect(shouldOfferBetterEngine({ count: LOW_DENSITY_HINT_MIN_COUNT, seconds: 20 }, base)).toBe(true);
  });

  it("零星一两段不打扰", () => {
    expect(shouldOfferBetterEngine({ count: 2, seconds: 8 }, base)).toBe(false);
  });

  it("这篇本来就是 FireRed 转的就没得换", () => {
    expect(shouldOfferBetterEngine(many, { ...base, noteEngine: "firered" })).toBe(false);
  });

  it("云端转的场次不在此列", () => {
    expect(shouldOfferBetterEngine(many, { ...base, noteEngine: "cloud:volcano" })).toBe(false);
  });

  it("老笔记没记引擎:按值得一试处理", () => {
    expect(shouldOfferBetterEngine(many, { ...base, noteEngine: undefined })).toBe(true);
  });

  it("模型没下载就不建议(点了也跑不起来)", () => {
    expect(shouldOfferBetterEngine(many, { ...base, ready: false })).toBe(false);
  });

  it("此刻不能跑(录制中/精修中/已有重转写)就不给可点的按钮", () => {
    expect(shouldOfferBetterEngine(many, { ...base, actionable: false })).toBe(false);
  });
});
