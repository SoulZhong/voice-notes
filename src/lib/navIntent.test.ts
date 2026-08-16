import { beforeEach, describe, expect, it } from "vitest";
import { markNavigated, navVersion, resetNavIntentForTest } from "./navIntent";

/** 模拟"等异步结果再跳"的自动重定向:回来时版本变了就放弃。 */
async function autoRedirect(work: () => Promise<void>): Promise<"跳转" | "让路"> {
  const v = navVersion();
  await work();
  return navVersion() === v ? "跳转" : "让路";
}

describe("显式导航意图", () => {
  beforeEach(() => resetNavIntentForTest());

  it("无人插队:自动重定向照常跳", async () => {
    expect(await autoRedirect(async () => {})).toBe("跳转");
  });

  it("await 期间有显式导航:让路", async () => {
    expect(await autoRedirect(async () => markNavigated())).toBe("让路");
  });

  it("显式导航发生在本次流程之前:不影响,照常跳", async () => {
    // 关键回归(Codex 四轮):早先用过一次托盘「打开设置」,之后再点侧栏回根路由,
    // 落地重定向必须照常工作——旧版用永不复位的布尔标记,根页面从此空白。
    markNavigated();
    expect(await autoRedirect(async () => {})).toBe("跳转");
  });

  it("两个并发流程各自比对,只有被插队的那个让路", async () => {
    const vA = navVersion();
    const vB = navVersion();
    markNavigated(); // A、B 都在等,插队发生
    expect(navVersion()).not.toBe(vA);
    expect(navVersion()).not.toBe(vB);
    // 插队之后新开的流程不受影响
    expect(await autoRedirect(async () => {})).toBe("跳转");
  });
});
