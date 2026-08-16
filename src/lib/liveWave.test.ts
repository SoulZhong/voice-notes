import { describe, expect, it } from "vitest";
import {
  WAVE_GATE_PCT,
  WAVE_MIN_PEAK_PCT,
  barCountFor,
  barStyle,
  envelopeStep,
  normalizeBars,
  shapeLevel,
} from "./liveWave";

describe("噪声门与整形", () => {
  it("门限以下一律当静音(静音不该被画成红条铺满整行)", () => {
    expect(shapeLevel(0)).toBe(0);
    expect(shapeLevel(WAVE_GATE_PCT)).toBe(0);
    expect(shapeLevel(WAVE_GATE_PCT - 0.1)).toBe(0);
    // 底噪常见落点(-55~-45dBFS → 0~10)
    expect(shapeLevel(8)).toBe(0);
  });

  it("门限以上单调递增,满量程为 1", () => {
    const a = shapeLevel(20);
    const b = shapeLevel(50);
    const c = shapeLevel(80);
    expect(a).toBeGreaterThan(0);
    expect(b).toBeGreaterThan(a);
    expect(c).toBeGreaterThan(b);
    expect(shapeLevel(100)).toBeCloseTo(1, 6);
  });

  it("gamma 抬低端:普通说话(-30dBFS ≈ 40)要占到可见高度的三分之一以上", () => {
    // 这条是这次改版的动因之一:线性映射下 40% 在 32px 里只有 13px,峰谷读不出来。
    expect(shapeLevel(40)).toBeGreaterThan(0.4);
  });

  it("非有限值当静音处理,不产生 NaN 条高", () => {
    expect(shapeLevel(Number.NaN)).toBe(0);
    expect(shapeLevel(Number.POSITIVE_INFINITY)).toBe(0);
  });
});

describe("包络(快起慢落)", () => {
  it("上升沿直接取采样值", () => {
    expect(envelopeStep(0, 60)).toBe(60);
    expect(envelopeStep(20, 60)).toBe(60);
  });

  it("下降沿按释放系数衰减,而不是立刻归零", () => {
    // 音节间的低谷若直接归零,120ms 采样下画出来是断续栅栏。
    const after = envelopeStep(60, 0);
    expect(after).toBeCloseTo(42, 6);
    expect(after).toBeGreaterThan(0);
  });

  it("拖尾长度必须与注释一致:静音后不该继续红上一秒多", () => {
    // Codex 审查:0.85 时满量程要 13 帧(1.57s)才落到门限下,视觉上等于"还在收音"。
    const frames = (from: number) => {
      let v = from;
      let n = 0;
      while (shapeLevel(v) > 0 && n < 100) {
        v = envelopeStep(v, 0);
        n++;
      }
      return n;
    };
    expect(frames(100) * 0.12).toBeLessThan(0.8); // 满量程 → 门限 <0.8s
    expect(frames(50) * 0.12).toBeLessThan(0.65); // 常见说话档 → 门限 <0.65s
  });

  it("持续静音最终落到接近零(不会永远挂着一根高条)", () => {
    let v = 80;
    for (let i = 0; i < 40; i++) v = envelopeStep(v, 0);
    expect(v).toBeLessThan(0.5);
  });
});

describe("滚动峰值归一", () => {
  it("窗口内最响的一根顶到满格", () => {
    const shaped = [30, 60, 90].map((p) => shapeLevel(p));
    const out = normalizeBars(shaped);
    expect(out[2]).toBeCloseTo(1, 6);
    expect(out[0]).toBeLessThan(out[1]);
  });

  it("整段都很轻时不放大底噪:峰值有下限", () => {
    // 安静房间里若按窗口实际峰值归一,一点点底噪会被拉成满格波形。
    const quiet = [16, 18, 17].map((p) => shapeLevel(p));
    const out = normalizeBars(quiet);
    expect(Math.max(...out)).toBeLessThan(0.75);
    // 下限对应的电平本身应当接近满格
    const atFloor = normalizeBars([shapeLevel(WAVE_MIN_PEAK_PCT)]);
    expect(atFloor[0]).toBeCloseTo(1, 6);
  });

  it("静音保持为 0(归一不把 0 抬起来)", () => {
    const out = normalizeBars([0, shapeLevel(70), 0]);
    expect(out[0]).toBe(0);
    expect(out[2]).toBe(0);
  });
});

describe("条的几何", () => {
  it("静音条 1px 且标记为 silent(渲染侧据此换成基线色)", () => {
    const b = barStyle(0, 24);
    expect(b).toEqual({ height: 1, silent: true, opacity: 1 });
  });

  it("有声条按归一值给高度,最低 3px 保证看得见", () => {
    expect(barStyle(1, 24).height).toBe(24);
    expect(barStyle(0.5, 24).height).toBe(12);
    expect(barStyle(0.01, 24).height).toBe(3);
  });

  it("弱信号降不透明度,强信号满不透明", () => {
    expect(barStyle(0.05, 24).opacity).toBeLessThan(0.7);
    expect(barStyle(0.6, 24).opacity).toBe(1);
  });
});

describe("根数随容器宽度", () => {
  it("按 4px 节距算,并受上限约束", () => {
    expect(barCountFor(1200, 420)).toBe(300);
    expect(barCountFor(2000, 420)).toBe(420);
    expect(barCountFor(240, 420)).toBe(60);
  });

  it("宽度未知/为 0 时给一个保底根数,不渲染成空", () => {
    expect(barCountFor(0, 420)).toBe(60);
    expect(barCountFor(Number.NaN, 420)).toBe(60);
  });

  it("极窄容器也不少于 24 根(否则波形退化成几个孤点)", () => {
    expect(barCountFor(40, 420)).toBe(24);
  });
});
