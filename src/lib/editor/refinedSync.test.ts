import { describe, expect, it } from "vitest";
import type { ParagraphPayload, RefinedDoc } from "$lib/notes";
import { RefinedSync, type RefinedEditorHandle, type RefinedSyncPorts } from "./refinedSync.svelte";

// ── 测试夹具:可控的假后端与假编辑器 ──

type Deferred<T> = { promise: Promise<T>; resolve: (v: T) => void; reject: (e: unknown) => void };
function deferred<T>(): Deferred<T> {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

/** 把排队的微任务跑干净(await 链里的回调都落地)。 */
const flush = () => new Promise((r) => setTimeout(r, 0));

function doc(revision: number, text = `r${revision}`): RefinedDoc {
  return { revision, paragraphs: [{ text }] } as unknown as RefinedDoc;
}
const paras = (text: string): ParagraphPayload[] => [{ orig_index: 0, text, dirty: true }];

/** 每次调用都排进队列,由测试决定何时、以何值落地。 */
class FakeBackend implements RefinedSyncPorts {
  constructor(private log: string[] = []) {}
  gets: { id: string; d: Deferred<RefinedDoc | null> }[] = [];
  saves: { id: string; revision: number; paragraphs: ParagraphPayload[]; d: Deferred<number> }[] = [];
  getRefined(id: string) {
    const d = deferred<RefinedDoc | null>();
    this.log.push("get");
    this.gets.push({ id, d });
    return d.promise;
  }
  saveRefined(id: string, revision: number, paragraphs: ParagraphPayload[]) {
    const d = deferred<number>();
    this.saves.push({ id, revision, paragraphs, d });
    return d.promise;
  }
}

class FakeEditor implements RefinedEditorHandle {
  constructor(private log: string[] = []) {}
  focused = false;
  set: RefinedDoc[] = [];
  saved: number[] = [];
  failed = 0;
  hasFocus() {
    return this.focused;
  }
  setRefined(d: RefinedDoc) {
    this.set.push(d);
  }
  markSaved(r: number) {
    this.log.push("markSaved");
    this.saved.push(r);
  }
  markSaveFailed() {
    this.log.push("markSaveFailed");
    this.failed += 1;
  }
}

function setup(id = "N1") {
  const log: string[] = [];
  const be = new FakeBackend(log);
  const ed = new FakeEditor(log);
  const cur = { id, editor: ed as FakeEditor | null };
  const sync = new RefinedSync(be, {
    currentId: () => cur.id,
    editor: () => cur.editor,
    saveFailedText: (e) => `保存失败: ${e}`,
    drainFailedText: (e) => `收尾失败: ${e}`,
  });
  return { be, ed, cur, sync, log };
}

/** 把编辑器同步到一份稿(模拟同步 effect 首跑),之后保存以这篇为目标。 */
function mounted(id = "N1") {
  const s = setup(id);
  s.sync.syncEditor(s.ed, doc(1));
  return s;
}

// ── 取稿:末次请求赢 ──

describe("取稿闸门", () => {
  it("后发先至:旧请求晚回来作废,在途数归零", async () => {
    const { be, sync } = setup();
    void sync.load("N1");
    void sync.load("N1");
    expect(sync.loading).toBe(2);
    be.gets[1].d.resolve(doc(2));
    be.gets[0].d.resolve(doc(1));
    await flush();
    expect(sync.doc?.revision).toBe(2);
    expect(sync.loading).toBe(0);
    expect(sync.stale).toBe(false);
  });

  it("最新一次取稿失败 → 标为可能过期;下一次成功解除", async () => {
    const { be, sync } = setup();
    void sync.load("N1");
    be.gets[0].d.reject(new Error("io"));
    await flush();
    expect(sync.stale).toBe(true);
    void sync.load("N1");
    be.gets[1].d.resolve(doc(3));
    await flush();
    expect(sync.stale).toBe(false);
  });

  it("被更新请求顶掉的失败不算过期", async () => {
    const { be, sync } = setup();
    void sync.load("N1");
    void sync.load("N1");
    be.gets[0].d.reject(new Error("io"));
    be.gets[1].d.resolve(doc(2));
    await flush();
    expect(sync.stale).toBe(false);
  });

  it("取稿途中切走笔记:回来的旧篇稿子不落地", async () => {
    const { be, cur, sync } = setup();
    void sync.load("N1");
    cur.id = "N2";
    be.gets[0].d.resolve(doc(9));
    await flush();
    expect(sync.doc).toBeNull();
  });
});

// ── 编辑器同步:身份闸门 ──

describe("编辑器同步", () => {
  it("同一份稿 + 同一个实例只同步一次", () => {
    const { ed, sync } = setup();
    const d = doc(1);
    sync.syncEditor(ed, d);
    sync.syncEditor(ed, d);
    expect(ed.set).toEqual([d]);
  });

  it("视图来回切出新实例、稿子没变 → 新实例照样收到(Fix Round 2 的空白页)", () => {
    const { ed, sync } = setup();
    const d = doc(1);
    sync.syncEditor(ed, d);
    const fresh = new FakeEditor();
    sync.syncEditor(fresh, d);
    expect(fresh.set).toEqual([d]);
  });

  it("正在打字不打断", () => {
    const { ed, sync } = setup();
    ed.focused = true;
    sync.syncEditor(ed, doc(1));
    expect(ed.set).toEqual([]);
  });
});

// ── 保存 ──

describe("保存", () => {
  it("成功:先 markSaved 再回读;回读后的稿记为已同步,同步 effect 不再重建", async () => {
    const { be, ed, sync, log } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("改") });
    be.saves[0].d.resolve(2);
    await flush();
    expect(ed.saved).toEqual([2]);
    expect(log, "markSaved 必须先于回读(空窗里的下一次自动保存会撞冲突)").toEqual(["markSaved", "get"]);
    expect(be.gets).toHaveLength(1);
    const latest = doc(2, "改");
    be.gets[0].d.resolve(latest);
    await flush();
    expect(sync.doc).toBe(latest);
    ed.set = [];
    sync.syncEditor(ed, latest);
    expect(ed.set, "失焦保存后不得用回读稿重建编辑器(会吹掉紧接着的输入)").toEqual([]);
  });

  it("成功会清掉之前的保存错误", async () => {
    const { be, sync } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("a") });
    be.saves[0].d.reject("Aing 进行中");
    await flush();
    expect(sync.saveErr).not.toBe("");
    void sync.save({ revision: 1, paragraphs: paras("a") });
    be.saves[1].d.resolve(2);
    await flush();
    be.gets[0].d.resolve(doc(2));
    await flush();
    expect(sync.saveErr).toBe("");
  });

  it("被拒:markSaveFailed 无条件调用,不回读,错误文案不重复刷新", async () => {
    const { be, ed, sync } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("a") });
    be.saves[0].d.reject("Aing 进行中");
    await flush();
    expect(ed.failed).toBe(1);
    expect(be.gets).toHaveLength(0);
    const first = sync.saveErr;
    void sync.save({ revision: 1, paragraphs: paras("a") });
    be.saves[1].d.reject("Aing 进行中");
    await flush();
    expect(ed.failed).toBe(2);
    expect(sync.saveErr).toBe(first);
  });

  it("乐观并发冲突:markSaveFailed 之后重载盘上稿并推给编辑器", async () => {
    const { be, ed, sync, log } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("a") });
    be.saves[0].d.reject("修订稿已在别处更新");
    await flush();
    expect(ed.failed).toBe(1);
    expect(log, "markSaveFailed 先于冲突重载").toEqual(["markSaveFailed", "get"]);
    const latest = doc(5, "别处");
    be.gets[0].d.resolve(latest);
    await flush();
    expect(sync.doc).toBe(latest);
    expect(ed.set.at(-1)).toBe(latest);
  });

  it("保存在途切走笔记:回执作废,不碰新笔记的编辑器", async () => {
    const { be, ed, cur, sync } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("a") });
    cur.id = "N2";
    sync.syncEditor(ed, doc(1, "N2 的稿")); // 新笔记载入,loadedId 随之翻到 N2
    be.saves[0].d.resolve(2);
    await flush();
    expect(ed.saved).toEqual([]);
    expect(be.saves[0].id, "保存仍落在发起时那篇").toBe("N1");
  });
});

// ── 收尾保存排队 ──

describe("收尾保存(drain)", () => {
  it("有在途保存时排队:等它落定,按新 revision 重基后再发", async () => {
    const { be, ed, sync } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("a") });
    sync.queueDrain({ revision: 1, paragraphs: paras("ab") });
    expect(be.saves).toHaveLength(1);
    be.saves[0].d.resolve(2);
    await flush();
    expect(be.saves).toHaveLength(2);
    expect(be.saves[1].revision).toBe(2);
    be.saves[1].d.resolve(3);
    // 在途保存自己的回读 + 排空后的同步回读
    await flush();
    for (const g of be.gets) g.d.resolve(doc(3, "ab"));
    await flush();
    expect(ed.set.at(-1)?.revision).toBe(3);
  });

  it("连续排两份只发最后一份", async () => {
    const { be, sync } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("a") });
    sync.queueDrain({ revision: 1, paragraphs: paras("ab") });
    sync.queueDrain({ revision: 1, paragraphs: paras("abc") });
    be.saves[0].d.resolve(2);
    await flush();
    expect(be.saves).toHaveLength(2);
    expect(be.saves[1].paragraphs).toEqual(paras("abc"));
  });

  it("排空后用户在打字:不把盘上稿推回编辑器", async () => {
    const { be, ed, sync } = mounted();
    sync.queueDrain({ revision: 1, paragraphs: paras("a") });
    ed.focused = true;
    be.saves[0].d.resolve(2);
    await flush();
    expect(be.gets).toHaveLength(0);
    expect(ed.set).toHaveLength(1); // 只有挂载那次
  });

  it("失败进保存错误横幅", async () => {
    const { be, sync } = mounted();
    sync.queueDrain({ revision: 1, paragraphs: paras("a") });
    be.saves[0].d.reject("磁盘满");
    await flush();
    expect(sync.saveErr).toContain("收尾失败");
  });
});

describe("切笔记复位", () => {
  it("清展示态,但保留旧篇的保存目标(复位前的 flush 靠它)", async () => {
    const { be, cur, sync } = mounted();
    void sync.load("N1");
    be.gets[0].d.resolve(doc(2));
    await flush();
    cur.id = "N2";
    sync.reset();
    expect(sync.doc).toBeNull();
    expect(sync.synced).toBeNull();
    sync.queueDrain({ revision: 2, paragraphs: paras("旧篇最后一笔") });
    expect(be.saves[0].id).toBe("N1");
  });
});

// ── 补充守卫分支 ──

describe("守卫分支", () => {
  it("实体编辑后重取:失焦才推给编辑器;有焦点只更新 doc", async () => {
    const { be, ed, sync } = mounted();
    ed.focused = true;
    void sync.reloadIntoEditor("N1");
    be.gets[0].d.resolve(doc(2));
    await flush();
    expect(sync.doc?.revision).toBe(2);
    expect(ed.set).toHaveLength(1); // 只有挂载那次
    ed.focused = false;
    void sync.reloadIntoEditor("N1");
    be.gets[1].d.resolve(doc(3));
    await flush();
    expect(ed.set.at(-1)?.revision).toBe(3);
  });

  it("实体编辑后重取途中切走笔记:不落地、不推编辑器", async () => {
    const { be, ed, cur, sync } = mounted();
    void sync.reloadIntoEditor("N1");
    cur.id = "N2";
    be.gets[0].d.resolve(doc(2));
    await flush();
    expect(sync.doc).toBeNull();
    expect(ed.set).toHaveLength(1);
  });

  it("保存成功但回读为 null 或失败:doc 保持原样,不清空整篇", async () => {
    const { be, sync } = mounted();
    void sync.load("N1");
    be.gets[0].d.resolve(doc(1));
    await flush();
    const kept = sync.doc;
    void sync.save({ revision: 1, paragraphs: paras("a") });
    be.saves[0].d.resolve(2);
    await flush();
    be.gets[1].d.resolve(null);
    await flush();
    expect(sync.doc).toBe(kept);
    void sync.save({ revision: 2, paragraphs: paras("b") });
    be.saves[1].d.resolve(3);
    await flush();
    be.gets[2].d.reject(new Error("io"));
    await flush();
    expect(sync.doc).toBe(kept);
    expect(sync.loading).toBe(0);
  });

  it("冲突重载途中切走笔记:不把旧篇稿子推给新编辑器", async () => {
    const { be, ed, cur, sync } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("a") });
    be.saves[0].d.reject("修订稿已在别处更新");
    await flush();
    cur.id = "N2";
    be.gets[0].d.resolve(doc(7));
    await flush();
    expect(ed.set.map((d) => d.revision)).not.toContain(7);
  });

  it("带在途保存的收尾失败:进错误横幅,排队清空后可再发", async () => {
    const { be, sync } = mounted();
    void sync.save({ revision: 1, paragraphs: paras("a") });
    sync.queueDrain({ revision: 1, paragraphs: paras("ab") });
    be.saves[0].d.resolve(2);
    await flush();
    be.saves[1].d.reject("磁盘满");
    await flush();
    expect(sync.saveErr).toContain("收尾失败");
    sync.queueDrain({ revision: 2, paragraphs: paras("abc") });
    await flush();
    expect(be.saves).toHaveLength(3);
  });
});
