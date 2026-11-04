import { beforeEach, describe, expect, it, vi } from "vitest";
import { mergeMemoDoc } from "./memoDocMerge";

// 模拟后端 save_memo_doc:盘面 + 写入序号;盘面与 base 不同就三方合并(同后端 merge3);
// 慢一点便于制造在途重叠;failNext 让下一次保存抛错。
let disk = "";
let seq = 0;
let failNext = false;
vi.mock("./memos", () => ({
  saveMemoDoc: async (_id: string, text: string, base: string) => {
    await new Promise((r) => setTimeout(r, 5));
    if (failNext) {
      failNext = false;
      throw new Error("io");
    }
    const next = disk === base ? text : mergeMemoDoc(base, text, disk);
    if (next !== disk) {
      disk = next;
      seq++;
    }
    return { text: disk, rev: String(seq) };
  },
}));

const { saveSerial } = await import("./memoDocSession");

let n = 0;
const id = () => `n${n++}`;

describe("saveSerial", () => {
  beforeEach(() => {
    failNext = false;
    seq = 0;
  });

  it("在途保存 + 离页最后一份:以在途那份为前驱,原样落盘", async () => {
    disk = "A\n";
    const k = id();
    const p1 = saveSerial(k, "A\n\nB\n", { base: "A\n", prev: null });
    const p2 = saveSerial(k, "A\n\nB\n\nC\n", { base: "A\n", prev: p1 });
    await Promise.all([p1.done, p2.done]);
    expect(disk).toBe("A\n\nB\n\nC\n");
  });

  it("前驱保存失败:后一份退回原基线,前驱里那段不丢", async () => {
    disk = "A\n";
    const k = id();
    failNext = true;
    const p1 = saveSerial(k, "A\n\nB\n", { base: "A\n", prev: null });
    const p2 = saveSerial(k, "A\n\nB\n\nC\n", { base: "A\n", prev: p1 });
    await expect(p1.done).rejects.toThrow();
    await p2.done;
    expect(p1.ok).toBe(false);
    expect(disk).toBe("A\n\nB\n\nC\n");
  });

  it("保存在途时撤销回原样再离页:撤销生效", async () => {
    disk = "A\n";
    const k = id();
    const p1 = saveSerial(k, "A\n\nB\n", { base: "A\n", prev: null });
    const p2 = saveSerial(k, "A\n", { base: "A\n", prev: p1 });
    await Promise.all([p1.done, p2.done]);
    expect(disk).toBe("A\n");
  });

  it("别处追加过:后端按基线合并,两边都在,版本号递增", async () => {
    disk = "a\n\n📍 <!-- 00:40 -->\n"; // 编辑器依据的是 "a\n",快捷键已追加一行
    const doc = await saveSerial(id(), "a 改\n", { base: "a\n", prev: null }).done;
    expect(doc.text).toBe("a 改\n\n📍 <!-- 00:40 -->\n");
    expect(doc.rev).toBe("1");
  });

  it("排队的离页那份不覆盖前驱刚合进来的别处改动", async () => {
    disk = "原稿\n";
    const k = id();
    const p1 = saveSerial(k, "原稿新字\n", { base: "原稿\n", prev: null });
    disk = "原稿\n\n远端备注\n"; // p1 执行前浮窗追加了一行
    const p2 = saveSerial(k, "原稿新字再补\n", { base: "原稿\n", prev: p1 });
    await Promise.all([p1.done, p2.done]);
    expect(disk).toBe("原稿新字再补\n\n远端备注\n");
  });
});
