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

  it("硬换行连起来的多行段落整体追加,不拆开", () => {
    const b = "a <!-- 00:01 -->\n";
    const local = "a 改 <!-- 00:01 -->\n";
    const remote = "a <!-- 00:01 -->\n\n第一行\\\n第二行 <!-- 00:20 -->\n";
    expect(mergeMemoDoc(b, local, remote)).toBe("a 改 <!-- 00:01 -->\n\n第一行\\\n第二行 <!-- 00:20 -->\n");
  });

  it("按位置对齐:同文字的段不互相覆盖(Codex 反例)", () => {
    expect(mergeMemoDoc("A\n\nB\n", "B\n\nC\n", "A\n\nC\n")).toBe("B\n\nC\n");
  });

  it("两边都改同一行:都留", () => {
    expect(mergeMemoDoc("a\n", "a1\n", "a2\n")).toBe("a1\n\na2\n");
  });

  it("本地改了一行、远端在其后追加:改动保留,追加接在后面", () => {
    expect(mergeMemoDoc("a\n", "a 改\n", "a\n\n📍\n")).toBe("a 改\n\n📍\n");
  });

  it("远端补字、本地其后粘了两段:两段之间空行保留,不粘成一段", () => {
    expect(
      mergeMemoDoc("📍 <!-- 00:40 -->\n", "📍 <!-- 00:40 -->\n\n一 <!-- 00:41 -->\n\n二 <!-- 00:42 -->\n", "补 <!-- 00:40 -->\n"),
    ).toBe("补 <!-- 00:40 -->\n\n一 <!-- 00:41 -->\n\n二 <!-- 00:42 -->\n");
  });
});
