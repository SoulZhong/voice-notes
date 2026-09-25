import { describe, expect, it } from "vitest";
import { watchJob, type Unlisten } from "./jobWatch";

type Ev = { note_id: string; state: string };

function deferred<T>() {
  let resolve!: (v: T) => void;
  let reject!: (e: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}
const flush = () => new Promise((r) => setTimeout(r, 0));

/** 可控的事件源:listen 何时挂到位由用例决定。 */
function source() {
  let cb: ((e: Ev) => void) | null = null;
  const listened = deferred<Unlisten>();
  let unlistened = 0;
  return {
    subscribe: (f: (e: Ev) => void) => {
      cb = f;
      return listened.promise;
    },
    attach: () => listened.resolve(() => (unlistened += 1)),
    fail: () => listened.reject(new Error("listen failed")),
    emit: (e: Ev) => cb?.(e),
    unlistened: () => unlistened,
  };
}

describe("watchJob", () => {
  it("补问只在监听挂到位之后发出", async () => {
    const src = source();
    let asked = 0;
    watchJob<Ev, boolean>({
      subscribe: src.subscribe,
      onEvent: () => {},
      snapshot: async () => {
        asked += 1;
        return true;
      },
    });
    await flush();
    expect(asked, "监听未就位前不得补问").toBe(0);
    src.attach();
    await flush();
    expect(asked).toBe(1);
  });

  it("只处理相关事件", async () => {
    const src = source();
    const seen: string[] = [];
    watchJob<Ev>({ subscribe: src.subscribe, matches: (e) => e.note_id === "N1", onEvent: (e) => seen.push(e.state) });
    src.emit({ note_id: "N2", state: "running" });
    src.emit({ note_id: "N1", state: "running" });
    expect(seen).toEqual(["running"]);
  });

  it("补问期间来了作废事件:快照让路", async () => {
    const src = source();
    const snap = deferred<boolean>();
    const applied: boolean[] = [];
    watchJob<Ev, boolean>({
      subscribe: src.subscribe,
      onEvent: () => {},
      snapshot: () => snap.promise,
      onSnapshot: (s) => void applied.push(s),
    });
    src.attach();
    await flush();
    src.emit({ note_id: "N1", state: "done" });
    snap.resolve(true);
    await flush();
    expect(applied, "迟到的「在跑」快照不得覆盖终态").toEqual([]);
  });

  it("非作废事件不影响快照(AI 整理的中间阶段)", async () => {
    const src = source();
    const snap = deferred<boolean>();
    const applied: boolean[] = [];
    watchJob<Ev, boolean>({
      subscribe: src.subscribe,
      onEvent: () => {},
      invalidates: (e) => e.state === "done",
      snapshot: () => snap.promise,
      onSnapshot: (s) => void applied.push(s),
    });
    src.attach();
    await flush();
    src.emit({ note_id: "N1", state: "llm" });
    snap.resolve(true);
    await flush();
    expect(applied).toEqual([true]);
  });

  it("live() 在作废事件到达后转 false,供轮询复核停下", async () => {
    const src = source();
    let live: (() => boolean) | null = null;
    watchJob<Ev, boolean>({
      subscribe: src.subscribe,
      onEvent: () => {},
      snapshot: async () => true,
      onSnapshot: (_s, l) => void (live = l),
    });
    src.attach();
    await flush();
    expect(live!()).toBe(true);
    src.emit({ note_id: "N1", state: "done" });
    expect(live!()).toBe(false);
  });

  it("监听就位前就切走:就位时立刻解绑,不补问", async () => {
    const src = source();
    let asked = 0;
    const dispose = watchJob<Ev, boolean>({
      subscribe: src.subscribe,
      onEvent: () => {},
      snapshot: async () => {
        asked += 1;
        return true;
      },
    });
    dispose();
    src.attach();
    await flush();
    expect(src.unlistened()).toBe(1);
    expect(asked).toBe(0);
  });

  it("切走后:解绑、事件与快照一律丢弃、不报已确定", async () => {
    const src = source();
    const snap = deferred<boolean>();
    const seen: string[] = [];
    let settled = 0;
    const dispose = watchJob<Ev, boolean>({
      subscribe: src.subscribe,
      onEvent: (e) => seen.push(e.state),
      snapshot: () => snap.promise,
      onSnapshot: () => void seen.push("snapshot"),
      onSettled: () => (settled += 1),
    });
    src.attach();
    await flush();
    dispose();
    expect(src.unlistened()).toBe(1);
    src.emit({ note_id: "N1", state: "running" });
    snap.resolve(true);
    await flush();
    expect(seen).toEqual([]);
    expect(settled).toBe(0);
  });

  it("补问成败都报已确定;订阅本身失败也报", async () => {
    for (const mode of ["ok", "snapshotFails", "listenFails"] as const) {
      const src = source();
      let settled = 0;
      watchJob<Ev, boolean>({
        subscribe: src.subscribe,
        onEvent: () => {},
        snapshot: async () => {
          if (mode === "snapshotFails") throw new Error("x");
          return false;
        },
        onSettled: () => (settled += 1),
      });
      if (mode === "listenFails") src.fail();
      else src.attach();
      await flush();
      expect(settled, mode).toBe(1);
    }
  });

  it("没有补问的纯订阅:不报已确定", async () => {
    const src = source();
    let settled = 0;
    watchJob<Ev>({ subscribe: src.subscribe, onEvent: () => {}, onSettled: () => (settled += 1) });
    src.attach();
    await flush();
    expect(settled).toBe(0);
  });

  it("切走之后订阅才失败:不报已确定", async () => {
    const src = source();
    let settled = 0;
    const dispose = watchJob<Ev, boolean>({
      subscribe: src.subscribe,
      onEvent: () => {},
      snapshot: async () => true,
      onSettled: () => (settled += 1),
    });
    dispose();
    src.fail();
    await flush();
    expect(settled).toBe(0);
  });

  it("onSnapshot 抛异常:仍报已确定", async () => {
    const src = source();
    let settled = 0;
    watchJob<Ev, boolean>({
      subscribe: src.subscribe,
      onEvent: () => {},
      snapshot: async () => true,
      onSnapshot: async () => {
        throw new Error("reload failed");
      },
      onSettled: () => (settled += 1),
    });
    src.attach();
    await flush();
    expect(settled).toBe(1);
  });
});
