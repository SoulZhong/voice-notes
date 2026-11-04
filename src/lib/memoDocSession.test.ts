import { beforeEach, describe, expect, it, vi } from "vitest";

// 模拟后端:盘上一份文本 + 版本号;save 慢一点,便于制造「在途保存 + 卸载保存」重叠。
let disk = "";
const rev = (t: string) => (t ? `r:${t}` : "0");
vi.mock("./memos", () => ({
  saveMemoDoc: async (_id: string, text: string, baseRev: string) => {
    await new Promise((r) => setTimeout(r, 5));
    if (baseRev !== rev(disk)) return { kind: "conflict", doc: { text: disk, rev: rev(disk) } };
    disk = text;
    return { kind: "saved", doc: { text, rev: rev(text) } };
  },
}));

const { memoDocSession, saveSerial } = await import("./memoDocSession");

describe("saveSerial", () => {
  beforeEach(() => {
    disk = "";
  });

  it("在途保存与离开页面时的最后一份串行执行,最后一份不丢", async () => {
    const s = memoDocSession("n1");
    const p1 = saveSerial(s, "原稿新字\n", "");
    const p2 = saveSerial(s, "原稿新字再补\n", "原稿新字\n"); // 卸载时交的最后一份:基线是在途那份
    await Promise.all([p1, p2]);
    expect(disk).toBe("原稿新字再补\n");
  });

  it("别处追加过:冲突后按行合并重试,两边都在", async () => {
    const s = memoDocSession("n2");
    await saveSerial(s, "a <!-- 00:01 -->\n", "");
    disk = "a <!-- 00:01 -->\n\n📍 <!-- 00:40 -->\n"; // 快捷键在别处追加
    const out = await saveSerial(s, "a 改 <!-- 00:01 -->\n", "a <!-- 00:01 -->\n");
    expect(disk).toBe("a 改 <!-- 00:01 -->\n\n📍 <!-- 00:40 -->\n");
    expect(out.merged).toBe(true);
    expect(s.recent.has(disk)).toBe(true);
  });

  it("排队的旧快照不覆盖前一次刚合进来的别处改动", async () => {
    const s = memoDocSession("n3");
    disk = "原稿\n";
    s.base = { text: "原稿\n", rev: rev("原稿\n") };
    const p1 = saveSerial(s, "原稿新字\n", "原稿\n");
    disk = "原稿\n\n远端备注\n"; // S1 执行前,浮窗追加了一行(S1 会冲突并合并)
    const p2 = saveSerial(s, "原稿新字再补\n", "原稿新字\n"); // 离页时排队的最后一份(基线 = 在途那份)
    await Promise.all([p1, p2]);
    expect(disk).toBe("原稿新字再补\n\n远端备注\n");
    expect(s.pending).toBe(0);
  });

  it("每篇笔记各自一个会话", () => {
    expect(memoDocSession("x")).toBe(memoDocSession("x"));
    expect(memoDocSession("x")).not.toBe(memoDocSession("y"));
  });
});
