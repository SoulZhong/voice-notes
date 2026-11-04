import { describe, expect, it } from "vitest";
import { mergeMemoDoc } from "./memoDocMerge";

const base = "决议 <!-- 00:16 -->\n\n- 周二上线 <!-- 00:16 -->\n";

describe("mergeMemoDoc", () => {
  it("一侧没动取另一侧", () => {
    expect(mergeMemoDoc(base, base, "x\n")).toBe("x\n");
    expect(mergeMemoDoc(base, "y\n", base)).toBe("y\n");
  });

  it("快捷键在末尾追加时,本地正在改的内容保留并接上新行", () => {
    const local = "决议改了 <!-- 00:16 -->\n\n- 周二上线 <!-- 00:16 -->\n";
    const remote = `${base}\n📍 <!-- 00:40 -->\n`;
    expect(mergeMemoDoc(base, local, remote)).toBe(
      "决议改了 <!-- 00:16 -->\n\n- 周二上线 <!-- 00:16 -->\n\n📍 <!-- 00:40 -->\n",
    );
  });

  it("浮窗把时间标记补成文字:本地那一行原地替换", () => {
    const b = "a <!-- 00:01 -->\n\n📍 <!-- 00:40 -->\n";
    const local = "a 改 <!-- 00:01 -->\n\n📍 <!-- 00:40 -->\n";
    const remote = "a <!-- 00:01 -->\n\n补的字 <!-- 00:40 -->\n";
    expect(mergeMemoDoc(b, local, remote)).toBe("a 改 <!-- 00:01 -->\n\n补的字 <!-- 00:40 -->\n");
  });

  it("本地已删掉的行被远端改了:新行追加,不丢字", () => {
    const b = "a <!-- 00:01 -->\n\n📍 <!-- 00:40 -->\n";
    const local = "a <!-- 00:01 -->\n\nb <!-- 00:02 -->\n";
    const remote = "a <!-- 00:01 -->\n\n补的字 <!-- 00:40 -->\n";
    expect(mergeMemoDoc(b, local, remote)).toBe("a <!-- 00:01 -->\n\nb <!-- 00:02 -->\n\n补的字 <!-- 00:40 -->\n");
  });

  it("远端删掉的行本地还在就删掉", () => {
    const b = "a\n\nb\n";
    expect(mergeMemoDoc(b, "a\n\nb\n\nc\n", "a\n")).toBe("a\n\nc\n");
  });

  it("两边写了同一行不重复", () => {
    expect(mergeMemoDoc("a\n", "a\n\nz\n", "a\n\nz\n")).toBe("a\n\nz\n");
  });
});
