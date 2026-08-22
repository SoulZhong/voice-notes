import { describe, expect, it } from "vitest";
import { splitNotices, type Notice } from "./notices";

const n = (key: string, level: Notice["level"], epoch?: string): Notice => ({
  key,
  level,
  text: key,
  epoch,
});

describe("splitNotices", () => {
  it("错误各自成行,其余取首条为上条、剩余进抽屉", () => {
    const got = splitNotices(
      [n("e1", "error"), n("a1", "action"), n("s1", "suggest"), n("i1", "info")],
      {},
    );
    expect(got.errors.map((x) => x.key)).toEqual(["e1"]);
    expect(got.head?.key).toBe("a1");
    expect(got.others.map((x) => x.key)).toEqual(["s1", "i1"]);
  });

  it("知道了按 (key, epoch) 记忆;epoch 变化自动失效", () => {
    const list = [n("a", "action", "v1"), n("b", "info")];
    expect(splitNotices(list, { a: "v1" }).head?.key).toBe("b");
    // 数据变了:epoch v2 ≠ 记忆的 v1,提示回来
    expect(splitNotices([n("a", "action", "v2"), n("b", "info")], { a: "v1" }).head?.key).toBe("a");
    // 无 epoch 的按 "1" 记忆
    expect(splitNotices(list, { a: "v1", b: "1" }).head).toBeNull();
  });

  it("全部点掉后三段皆空", () => {
    const got = splitNotices([n("a", "suggest", "x")], { a: "x" });
    expect(got.errors).toEqual([]);
    expect(got.head).toBeNull();
    expect(got.others).toEqual([]);
  });
});
