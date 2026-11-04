import { beforeEach, describe, expect, it, vi } from "vitest";

// 模拟后端:盘上一份文本 + 版本号;save 慢一点,便于制造在途重叠;failNext 让下一次保存抛错。
let disk = "";
let failNext = false;
const rev = (t: string) => (t ? `r:${t}` : "0");
vi.mock("./memos", () => ({
  saveMemoDoc: async (_id: string, text: string, baseRev: string) => {
    await new Promise((r) => setTimeout(r, 5));
    if (failNext) {
      failNext = false;
      throw new Error("io");
    }
    if (baseRev !== rev(disk)) return { kind: "conflict", doc: { text: disk, rev: rev(disk) } };
    disk = text;
    return { kind: "saved", doc: { text, rev: rev(text) } };
  },
}));

const { memoDocSession, saveSerial } = await import("./memoDocSession");

let n = 0;
/** 新会话,盘面与会话基线都是 `start`。 */
function fresh(start: string) {
  disk = start;
  failNext = false;
  const s = memoDocSession(`n${n++}`);
  s.base = { text: start, rev: rev(start) };
  return s;
}

describe("saveSerial", () => {
  beforeEach(() => {
    failNext = false;
  });

  it("在途保存 + 离页最后一份:最后一份以在途那份为前驱,原样落盘", async () => {
    const s = fresh("A\n");
    const p1 = saveSerial(s, "A\n\nB\n", { base: "A\n", prev: null });
    const p2 = saveSerial(s, "A\n\nB\n\nC\n", { base: "A\n", prev: "A\n\nB\n" });
    await Promise.all([p1, p2]);
    expect(disk).toBe("A\n\nB\n\nC\n");
    expect(s.pending).toBe(0);
  });

  it("前驱保存失败:后一份退回原基线,前驱里那段不丢(Codex 第三轮 P1)", async () => {
    const s = fresh("A\n");
    failNext = true;
    const p1 = saveSerial(s, "A\n\nB\n", { base: "A\n", prev: null }).catch(() => "failed");
    const p2 = saveSerial(s, "A\n\nB\n\nC\n", { base: "A\n", prev: "A\n\nB\n" });
    expect(await p1).toBe("failed");
    await p2;
    expect(disk).toBe("A\n\nB\n\nC\n");
  });

  it("保存在途时撤销回原样再离页:撤销生效,不被在途那份盖回(Codex 第三轮 P1)", async () => {
    const s = fresh("A\n");
    const p1 = saveSerial(s, "A\n\nB\n", { base: "A\n", prev: null });
    const p2 = saveSerial(s, "A\n", { base: "A\n", prev: "A\n\nB\n" });
    await Promise.all([p1, p2]);
    expect(disk).toBe("A\n");
  });

  it("别处追加过:冲突后按行合并重试,两边都在,读到的版本记入 seen", async () => {
    const s = fresh("a <!-- 00:01 -->\n");
    disk = "a <!-- 00:01 -->\n\n📍 <!-- 00:40 -->\n"; // 快捷键在别处追加
    const remoteRev = rev(disk);
    const out = await saveSerial(s, "a 改 <!-- 00:01 -->\n", { base: "a <!-- 00:01 -->\n", prev: null });
    expect(disk).toBe("a 改 <!-- 00:01 -->\n\n📍 <!-- 00:40 -->\n");
    expect(out.merged).toBe(true);
    expect(s.seen.has(remoteRev)).toBe(true); // 之后到达的这份事件会被忽略,不回放旧版本
    expect(s.seen.has(rev(disk))).toBe(true);
  });

  it("排队的旧快照不覆盖前一次刚合进来的别处改动", async () => {
    const s = fresh("原稿\n");
    const p1 = saveSerial(s, "原稿新字\n", { base: "原稿\n", prev: null });
    disk = "原稿\n\n远端备注\n"; // p1 执行前浮窗追加了一行
    const p2 = saveSerial(s, "原稿新字再补\n", { base: "原稿\n", prev: "原稿新字\n" });
    await Promise.all([p1, p2]);
    expect(disk).toBe("原稿新字再补\n\n远端备注\n");
  });

  it("每篇笔记各自一个会话", () => {
    expect(memoDocSession("x")).toBe(memoDocSession("x"));
    expect(memoDocSession("x")).not.toBe(memoDocSession("y"));
  });
});
