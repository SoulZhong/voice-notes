import { describe, expect, it, vi } from "vitest";
import { keyedOnce } from "./keyedOnce";

describe("keyedOnce", () => {
  it("同一 key 重复触发只执行一次(effect 重跑不再放大成 IPC 风暴)", () => {
    const fn = vi.fn(async () => {});
    const once = keyedOnce(fn);
    once("P1");
    once("P1");
    once("P1");
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("不同 key 各执行一次", () => {
    const fn = vi.fn(async () => {});
    const once = keyedOnce(fn);
    once("P1");
    once("P2");
    once("P1");
    expect(fn).toHaveBeenCalledTimes(2);
    expect(fn).toHaveBeenNthCalledWith(1, "P1");
    expect(fn).toHaveBeenNthCalledWith(2, "P2");
  });

  it("执行中(未 resolve)再触发同 key 不重复调用", async () => {
    let resolve!: () => void;
    const fn = vi.fn(() => new Promise<void>((r) => (resolve = r)));
    const once = keyedOnce(fn);
    once("P1");
    once("P1");
    expect(fn).toHaveBeenCalledTimes(1);
    resolve();
    await Promise.resolve();
    once("P1");
    expect(fn).toHaveBeenCalledTimes(1);
  });

  it("拒绝被吞掉且不重试(调用方自己兜底空态)", async () => {
    const fn = vi.fn(async () => {
      throw new Error("ipc down");
    });
    const once = keyedOnce(fn);
    expect(() => once("P1")).not.toThrow();
    await Promise.resolve();
    once("P1");
    expect(fn).toHaveBeenCalledTimes(1);
  });
});
