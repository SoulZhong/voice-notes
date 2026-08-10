import { describe, expect, it } from "vitest";
import { schemeToDefaultPlayback, shouldFallbackToDual } from "./audioScheme";

describe("schemeToDefaultPlayback(声音处理方案→默认回放)", () => {
  it("b 档默认成品轨", () => {
    expect(schemeToDefaultPlayback("b")).toBe("mixed");
  });
  it("a/ab 档默认双轨", () => {
    expect(schemeToDefaultPlayback("a")).toBe("dual");
    expect(schemeToDefaultPlayback("ab")).toBe("dual");
  });
  it("未知值容错按双轨(设置文件被手改坏也不炸回放)", () => {
    expect(schemeToDefaultPlayback("")).toBe("dual");
    expect(schemeToDefaultPlayback("banana")).toBe("dual");
  });
});

describe("shouldFallbackToDual(成品轨回落判定)", () => {
  const trusted = { track: { path: "mixed.m4a" }, untrusted: null };
  it("pending 期间一律不判(复位刚清的 null 是未知不是确无)", () => {
    expect(shouldFallbackToDual(true, "mixed", null)).toBe(false);
  });
  it("落定后无读数(取失败)→ 回落", () => {
    expect(shouldFallbackToDual(false, "mixed", null)).toBe(true);
  });
  it("落定后无轨/不可信 → 回落;有可信轨 → 保持", () => {
    expect(shouldFallbackToDual(false, "mixed", { track: null, untrusted: null })).toBe(true);
    expect(shouldFallbackToDual(false, "mixed", { track: trusted.track, untrusted: "口径不符" })).toBe(true);
    expect(shouldFallbackToDual(false, "mixed", trusted)).toBe(false);
  });
  it("双轨模式与判定无关", () => {
    expect(shouldFallbackToDual(false, "dual", null)).toBe(false);
  });
});
