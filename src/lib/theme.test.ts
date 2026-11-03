import { describe, expect, it } from "vitest";
import { normalizeFamily, normalizeMode, resolveMode, APPEARANCE_CACHE_KEY } from "./theme";

describe("外观", () => {
  it("未知皮肤/明暗回退默认", () => {
    expect(normalizeFamily("paper")).toBe("paper");
    expect(normalizeFamily("dracula")).toBe("classic");
    expect(normalizeFamily(undefined)).toBe("classic");
    expect(normalizeMode("dark")).toBe("dark");
    expect(normalizeMode("sepia")).toBe("system");
  });
  it("system 跟系统,手动不受系统影响", () => {
    expect(resolveMode("system", true)).toBe("dark");
    expect(resolveMode("system", false)).toBe("light");
    expect(resolveMode("light", true)).toBe("light");
  });
  it("首帧脚本与控制器用同一个缓存键", async () => {
    const html = (await import("../app.html?raw")).default as string;
    expect(html).toContain(APPEARANCE_CACHE_KEY);
  });
});
