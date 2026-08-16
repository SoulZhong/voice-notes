import { beforeEach, describe, expect, it } from "vitest";
import { hasNavigated, markNavigated, resetNavIntentForTest } from "./navIntent";

describe("显式导航意图", () => {
  beforeEach(() => resetNavIntentForTest());

  it("默认未导航:冷启动时自动重定向照常工作", () => {
    expect(hasNavigated()).toBe(false);
  });

  it("标记之后自动重定向一律让路", () => {
    markNavigated();
    expect(hasNavigated()).toBe(true);
  });

  it("单向不复位:标记多次仍是已导航", () => {
    // 语义刻意如此——自动重定向只服务"刚启动、用户还没表达意图"那一小段,
    // 之后由用户掌舵;若能复位,后到的异步重定向又会把人拽走。
    markNavigated();
    markNavigated();
    expect(hasNavigated()).toBe(true);
  });
});
