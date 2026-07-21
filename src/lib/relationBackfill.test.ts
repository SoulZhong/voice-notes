import { describe, expect, it, vi } from "vitest";
import type { BackfillPreview, BackfillProgress, BackfillRequest } from "./knowledge";
import {
  createRelationBackfillController,
  previewRelationBackfill,
  startRelationBackfill,
  cancelRelationBackfill,
  subscribeRelationBackfill,
  type RelationBackfillApi,
} from "./relationBackfill";

const { invokeMock, listenMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));

const preview = (noteIds = ["note-a", "note-b"]): BackfillPreview => ({
  note_ids: noteIds,
  provider: "agent",
  model: "claude-sonnet-4-5",
  contract_version: 1,
});

const progress = (
  state: BackfillProgress["state"],
  completed: number,
  overrides: Partial<BackfillProgress> = {},
): BackfillProgress => ({
  state,
  completed,
  total: 2,
  current_note_id: null,
  failed: [],
  ...overrides,
});

function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

function api(overrides: Partial<RelationBackfillApi> = {}): RelationBackfillApi {
  return {
    preview: vi.fn().mockResolvedValue(preview()),
    start: vi.fn().mockResolvedValue(undefined),
    cancel: vi.fn().mockResolvedValue(undefined),
    subscribe: vi.fn().mockResolvedValue(vi.fn()),
    ...overrides,
  };
}

describe("relation backfill invoke contract", () => {
  it("uses exact commands, casing, and event payload", async () => {
    invokeMock.mockResolvedValue(undefined);
    const unlisten = vi.fn();
    listenMock.mockResolvedValue(unlisten);
    const handler = vi.fn();
    const request: BackfillRequest = { note_ids: ["note-a"], provider: "agent" };

    await previewRelationBackfill();
    await previewRelationBackfill(["note-a"]);
    await startRelationBackfill(request);
    await cancelRelationBackfill();
    await subscribeRelationBackfill(handler);

    expect(invokeMock.mock.calls).toEqual([
      ["preview_relation_backfill", { noteIds: null }],
      ["preview_relation_backfill", { noteIds: ["note-a"] }],
      ["start_relation_backfill", { request }],
      ["cancel_relation_backfill"],
    ]);
    expect(listenMock).toHaveBeenCalledWith("relation_backfill_progress", expect.any(Function));
    const listener = listenMock.mock.calls[0]![1] as (event: { payload: BackfillProgress }) => void;
    listener({ payload: progress("running", 1) });
    expect(handler).toHaveBeenCalledWith(progress("running", 1));
  });
});

describe("createRelationBackfillController", () => {
  it("moves from loading to ready and preserves an actionable preview error", async () => {
    const failing = createRelationBackfillController(api({
      preview: vi.fn().mockRejectedValue(new Error("没有配置执行体")),
    }));
    const pending = failing.preview();
    expect(failing.state.phase).toBe("preview-loading");
    await pending;
    expect(failing.state.phase).toBe("preview-error");
    expect(failing.state.error).toContain("没有配置执行体");

    const succeeding = createRelationBackfillController(api());
    await succeeding.preview();
    expect(succeeding.state.phase).toBe("preview-ready");
    expect(succeeding.state.preview).toEqual(preview());
    expect(succeeding.state.acknowledged).toBe(false);
  });

  it("requires explicit acknowledgement and subscribes before start", async () => {
    const order: string[] = [];
    let emit!: (event: BackfillProgress) => void;
    const start = vi.fn(async () => { order.push("start"); });
    const subscribe = vi.fn(async (handler: (event: BackfillProgress) => void) => {
      order.push("subscribe");
      emit = handler;
      return vi.fn();
    });
    const controller = createRelationBackfillController(api({ start, subscribe }));
    await controller.preview(["note-a", "note-b"]);

    await expect(controller.start()).rejects.toThrow("确认");
    expect(start).not.toHaveBeenCalled();
    controller.acknowledge(true);
    await controller.start();

    expect(order).toEqual(["subscribe", "start"]);
    expect(start).toHaveBeenCalledWith({ note_ids: ["note-a", "note-b"], provider: "agent" });
    expect(controller.state.phase).toBe("running");
    emit(progress("running", 1, { current_note_id: "完整会议标题或 note-a" }));
    expect(controller.state.completed).toBe(1);
    expect(controller.state.currentNoteId).toBe("完整会议标题或 note-a");
  });

  it("reduces failures, keeps cancel-requested distinct, and cleans terminal listeners once", async () => {
    let emit!: (event: BackfillProgress) => void;
    const unlisten = vi.fn();
    const cancel = deferred<void>();
    const controller = createRelationBackfillController(api({
      subscribe: vi.fn(async (handler) => { emit = handler; return unlisten; }),
      cancel: vi.fn(() => cancel.promise),
    }));
    await controller.preview();
    controller.acknowledge(true);
    await controller.start();
    emit(progress("running", 1, {
      current_note_id: "note-b-with-a-complete-inspectable-title",
      failed: [{ note_id: "note-a", error: "完整且可换行的失败原因" }],
    }));
    expect(controller.state.failures).toEqual([{ note_id: "note-a", error: "完整且可换行的失败原因" }]);

    const cancelling = controller.cancel();
    expect(controller.state.phase).toBe("cancel-requested");
    cancel.resolve();
    await cancelling;
    expect(controller.state.phase).toBe("cancel-requested");
    emit(progress("cancelled", 1));
    expect(controller.state.phase).toBe("cancelled");
    expect(unlisten).toHaveBeenCalledTimes(1);
    controller.close();
    expect(unlisten).toHaveBeenCalledTimes(1);
  });

  it("supports resumable failed/cancelled runs through a fresh default preview", async () => {
    let emit!: (event: BackfillProgress) => void;
    const previewApi = vi.fn()
      .mockResolvedValueOnce(preview(["note-a", "note-b"]))
      .mockResolvedValueOnce(preview(["note-b"]));
    const controller = createRelationBackfillController(api({
      preview: previewApi,
      subscribe: vi.fn(async (handler) => { emit = handler; return vi.fn(); }),
    }));
    await controller.preview(["note-a", "note-b"]);
    controller.acknowledge(true);
    await controller.start();
    emit(progress("failed", 1, { failed: [{ note_id: "note-b", error: "执行失败" }] }));
    expect(controller.state.phase).toBe("failed");

    await controller.resume();
    expect(previewApi).toHaveBeenLastCalledWith(undefined);
    expect(controller.state.phase).toBe("preview-ready");
    expect(controller.state.preview?.note_ids).toEqual(["note-b"]);
    expect(controller.state.acknowledged).toBe(false);
  });

  it("completes with exact progress and rejects stale preview/progress sessions", async () => {
    const older = deferred<BackfillPreview>();
    const newer = deferred<BackfillPreview>();
    const previewApi = vi.fn()
      .mockReturnValueOnce(older.promise)
      .mockReturnValueOnce(newer.promise)
      .mockResolvedValue(preview(["new"]));
    const handlers: Array<(event: BackfillProgress) => void> = [];
    const unlisteners = [vi.fn(), vi.fn()];
    const controller = createRelationBackfillController(api({
      preview: previewApi,
      subscribe: vi.fn(async (handler) => {
        handlers.push(handler);
        return unlisteners[handlers.length - 1]!;
      }),
    }));

    const oldPreview = controller.preview(["old"]);
    const newPreview = controller.preview(["new"]);
    newer.resolve(preview(["new"]));
    await newPreview;
    older.resolve(preview(["old"]));
    await oldPreview;
    expect(controller.state.preview?.note_ids).toEqual(["new"]);

    controller.acknowledge(true);
    await controller.start();
    const firstHandler = handlers[0]!;
    firstHandler(progress("failed", 0, { failed: [{ note_id: "new", error: "retry" }] }));
    await controller.resume();
    controller.acknowledge(true);
    await controller.start();
    firstHandler(progress("completed", 2));
    expect(controller.state.phase).toBe("running");
    handlers[1]!(progress("completed", 2));
    expect(controller.state.phase).toBe("completed");
    expect(controller.state.completed).toBe(2);
    expect(unlisteners[1]).toHaveBeenCalledTimes(1);
  });

  it("unsubscribes exactly once on start error and close/unmount", async () => {
    const unlisten = vi.fn();
    const controller = createRelationBackfillController(api({
      start: vi.fn().mockRejectedValue(new Error("启动失败")),
      subscribe: vi.fn().mockResolvedValue(unlisten),
    }));
    await controller.preview();
    controller.acknowledge(true);
    await expect(controller.start()).rejects.toThrow("启动失败");
    expect(controller.state.phase).toBe("failed");
    expect(unlisten).toHaveBeenCalledTimes(1);
    controller.close();
    controller.dispose();
    expect(unlisten).toHaveBeenCalledTimes(1);
  });
});

describe("backfill dialog source contract", () => {
  const sources = import.meta.glob(
    ["./RelationBackfillDialog.svelte", "../routes/ai/+page.svelte", "../routes/graph/+page.svelte"],
    { eager: true, query: "?raw", import: "default" },
  ) as Record<string, string>;
  const source = (name: string) => {
    const value = sources[name];
    if (value === undefined) throw new Error(`Missing source fixture: ${name}`);
    return value;
  };

  it("uses one native dialog in both entry points with complete consent facts", () => {
    const dialog = source("./RelationBackfillDialog.svelte");
    const ai = source("../routes/ai/+page.svelte");
    const graph = source("../routes/graph/+page.svelte");
    expect(ai).toContain("<RelationBackfillDialog");
    expect(graph).toContain("<RelationBackfillDialog");
    expect(dialog).toContain("<dialog");
    expect(dialog).toContain("showModal()");
    expect(dialog).toContain("笔记数量");
    expect(dialog).toContain("执行体");
    expect(dialog).toContain("精确模型");
    expect(dialog).toContain("契约版本");
    expect(dialog).toContain("将把修订稿发送给当前配置的执行体");
    expect(dialog).not.toMatch(/价格|费用|token|令牌估算/i);
  });

  it("keeps full titles/errors inspectable with progress, cancellation, resume and live status", () => {
    const dialog = source("./RelationBackfillDialog.svelte");
    for (const text of ["取消补建", "继续未完成笔记", "重新预览", "aria-live", "当前笔记", "失败详情"]) {
      expect(dialog).toContain(text);
    }
    expect(dialog).not.toContain("text-overflow: ellipsis");
    expect(dialog).not.toContain("line-clamp");
    expect(dialog).not.toContain("…");
  });

  it("refreshes semantic data after completion without remounting ForceGraph", () => {
    const graph = source("../routes/graph/+page.svelte");
    expect(graph).toContain("onCompleted={refreshAfterBackfill}");
    expect(graph).toContain("loadSemantic(effectiveGraphFilter)");
    expect(graph).toContain("probeGlobalSemanticPresence()");
    expect(graph).not.toMatch(/\{#if backfillOpen[^}]*\}[\s\S]{0,400}<ForceGraph/);
  });
});
