import { describe, expect, it } from "vitest";
import { schemeToDefaultPlayback } from "./audioScheme";

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
