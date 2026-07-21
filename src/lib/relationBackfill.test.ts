import { describe, expect, it, vi } from "vitest";
import type { BackfillPreview, BackfillProgress, BackfillRequest } from "./knowledge";
import type { GraphIndexStatus } from "./knowledgeGovernance";
import {
  cancelRelationBackfill,
  createRelationBackfillController,
  previewRelationBackfill,
  startRelationBackfill,
  subscribeRelationBackfill,
  subscribeRelationBackfillIndexStatus,
  type RelationBackfillApi,
} from "./relationBackfill";

const { invokeMock, listenMock } = vi.hoisted(() => ({
  invokeMock: vi.fn(),
  listenMock: vi.fn(),
}));

vi.mock("@tauri-apps/api/core", () => ({ invoke: invokeMock }));
vi.mock("@tauri-apps/api/event", () => ({ listen: listenMock }));

const preview = (noteIds = ["note-a", "note-b"]): BackfillPreview => ({
  consent_token: "backfill-preview-consent-a",
  note_ids: noteIds,
  provider: "agent",
  model: "claude-sonnet-4-5",
  contract_version: 1,
});

const progress = (
  runId: string,
  state: BackfillProgress["state"],
  completed: number,
  overrides: Partial<BackfillProgress> = {},
): BackfillProgress => ({
  run_id: runId,
  state,
  completed,
  total: 2,
  current_note_id: null,
  failed: [],
  rebuild_generation: null,
  ...overrides,
});

const indexStatus = (
  generation: number,
  state: GraphIndexStatus["state"],
  error: string | null = null,
): GraphIndexStatus => ({ generation, state, error, stats: null });

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
    subscribeIndex: vi.fn().mockResolvedValue(vi.fn()),
    createRunId: vi.fn().mockReturnValue("run-test"),
    ...overrides,
  };
}

describe("relation backfill invoke contract", () => {
  it("uses exact immutable request, run-scoped cancel, and both event channels", async () => {
    invokeMock.mockResolvedValue(undefined);
    const unlisten = vi.fn();
    listenMock.mockResolvedValue(unlisten);
    const progressHandler = vi.fn();
    const indexHandler = vi.fn();
    const request: BackfillRequest = {
      run_id: "run-wrapper",
      consent_token: "backfill-preview-wrapper",
      note_ids: ["note-a"],
      provider: "agent",
      model: "claude-sonnet-4-5",
      contract_version: 1,
    };

    await previewRelationBackfill();
    await previewRelationBackfill(["note-a"]);
    await startRelationBackfill(request);
    await cancelRelationBackfill("run-wrapper");
    await subscribeRelationBackfill(progressHandler);
    await subscribeRelationBackfillIndexStatus(indexHandler);

    expect(invokeMock.mock.calls).toEqual([
      ["preview_relation_backfill", { noteIds: null }],
      ["preview_relation_backfill", { noteIds: ["note-a"] }],
      ["start_relation_backfill", { request }],
      ["cancel_relation_backfill", { runId: "run-wrapper" }],
    ]);
    expect(listenMock.mock.calls.map(([event]) => event)).toEqual([
      "relation_backfill_progress",
      "graph_index_status",
    ]);
  });
});

describe("createRelationBackfillController", () => {
  it("moves from loading to ready and keeps technical preview errors behind a summary", async () => {
    const failing = createRelationBackfillController(api({
      preview: vi.fn().mockRejectedValue(new Error("/private/user/notes: provider secret failed")),
    }));
    const pending = failing.preview();
    expect(failing.state.phase).toBe("preview-loading");
    await pending;
    expect(failing.state.phase).toBe("preview-error");
    expect(failing.state.error).not.toContain("/private/user");
    expect(failing.state.technicalError).toContain("/private/user/notes");

    const succeeding = createRelationBackfillController(api());
    await succeeding.preview();
    expect(succeeding.state.phase).toBe("preview-ready");
    expect(succeeding.state.preview).toEqual(preview());
    expect(succeeding.state.acknowledged).toBe(false);
  });

  it("sets a synchronous starting guard and installs both listeners before one start", async () => {
    const order: string[] = [];
    let emitProgress!: (event: BackfillProgress) => void;
    const started = deferred<void>();
    const start = vi.fn(async () => {
      order.push("start");
      await started.promise;
    });
    const subscribe = vi.fn(async (handler: (event: BackfillProgress) => void) => {
      order.push("progress-listener");
      emitProgress = handler;
      return vi.fn();
    });
    const subscribeIndex = vi.fn(async () => {
      order.push("index-listener");
      return vi.fn();
    });
    const controller = createRelationBackfillController(api({ start, subscribe, subscribeIndex }));
    await controller.preview(["note-a", "note-b"]);

    await expect(controller.start()).rejects.toThrow("确认");
    controller.acknowledge(true);
    const first = controller.start();
    const second = controller.start();
    expect(first).toBe(second);
    expect(controller.state.phase).toBe("starting");
    await vi.waitFor(() => expect(start).toHaveBeenCalledTimes(1));
    expect(order).toEqual(["progress-listener", "index-listener", "start"]);
    expect(start).toHaveBeenCalledWith({
      run_id: "run-test",
      consent_token: "backfill-preview-consent-a",
      note_ids: ["note-a", "note-b"],
      provider: "agent",
      model: "claude-sonnet-4-5",
      contract_version: 1,
    });
    emitProgress(progress("run-test", "running", 0));
    expect(controller.state.phase).toBe("running");
    started.resolve();
    await first;
  });

  it("filters old run events even when they arrive through the new listener", async () => {
    const handlers: Array<(event: BackfillProgress) => void> = [];
    const runIds = ["run-one", "run-two"];
    const controller = createRelationBackfillController(api({
      createRunId: vi.fn(() => runIds.shift()!),
      preview: vi.fn()
        .mockResolvedValueOnce(preview(["note-a"]))
        .mockResolvedValueOnce(preview(["note-b"])),
      subscribe: vi.fn(async (handler) => {
        handlers.push(handler);
        return vi.fn();
      }),
    }));
    await controller.preview(["note-a"]);
    controller.acknowledge(true);
    await controller.start();
    handlers[0]!(progress("run-one", "failed", 0, {
      failed: [{ note_id: "note-a", error: "retry" }],
    }));
    await controller.resume();
    controller.acknowledge(true);
    await controller.start();

    handlers[1]!(progress("run-one", "completed", 1, { rebuild_generation: 8 }));
    expect(controller.state.runId).toBe("run-two");
    expect(controller.state.phase).toBe("running");
    handlers[0]!(progress("run-one", "completed", 1, { rebuild_generation: 8 }));
    expect(controller.state.phase).toBe("running");
  });

  it("uses exact run id for delayed cancel and ignores its late resolution after close", async () => {
    let emitProgress!: (event: BackfillProgress) => void;
    const delayed = deferred<void>();
    const cancel = vi.fn(() => delayed.promise);
    const controller = createRelationBackfillController(api({
      cancel,
      subscribe: vi.fn(async (handler) => {
        emitProgress = handler;
        return vi.fn();
      }),
    }));
    await controller.preview();
    controller.acknowledge(true);
    await controller.start();
    emitProgress(progress("run-test", "running", 1));
    const cancelling = controller.cancel();
    expect(cancel).toHaveBeenCalledWith("run-test");
    expect(controller.state.phase).toBe("cancel-requested");
    controller.close();
    await controller.preview(["note-a"]);
    delayed.resolve();
    await cancelling;
    expect(controller.state.phase).toBe("preview-ready");
  });

  it("waits for the exact rebuild generation before completion and cleans both listeners once", async () => {
    let emitProgress!: (event: BackfillProgress) => void;
    let emitIndex!: (event: GraphIndexStatus) => void;
    const unlistenProgress = vi.fn();
    const unlistenIndex = vi.fn();
    const controller = createRelationBackfillController(api({
      subscribe: vi.fn(async (handler) => {
        emitProgress = handler;
        return unlistenProgress;
      }),
      subscribeIndex: vi.fn(async (handler) => {
        emitIndex = handler;
        return unlistenIndex;
      }),
    }));
    await controller.preview();
    controller.acknowledge(true);
    await controller.start();
    emitIndex(indexStatus(7, "ready"));
    emitProgress(progress("run-test", "completed", 2, { rebuild_generation: 8 }));
    expect(controller.state.phase).toBe("waiting-for-index");
    emitIndex(indexStatus(7, "error", "old rebuild failed"));
    expect(controller.state.phase).toBe("waiting-for-index");
    emitIndex(indexStatus(8, "ready"));
    expect(controller.state.phase).toBe("completed");
    emitIndex(indexStatus(8, "ready"));
    expect(unlistenProgress).toHaveBeenCalledTimes(1);
    expect(unlistenIndex).toHaveBeenCalledTimes(1);
    controller.close();
    expect(unlistenProgress).toHaveBeenCalledTimes(1);
    expect(unlistenIndex).toHaveBeenCalledTimes(1);
  });

  it("turns the matching rebuild error into a resumable failure", async () => {
    let emitProgress!: (event: BackfillProgress) => void;
    let emitIndex!: (event: GraphIndexStatus) => void;
    const controller = createRelationBackfillController(api({
      subscribe: vi.fn(async (handler) => {
        emitProgress = handler;
        return vi.fn();
      }),
      subscribeIndex: vi.fn(async (handler) => {
        emitIndex = handler;
        return vi.fn();
      }),
    }));
    await controller.preview();
    controller.acknowledge(true);
    await controller.start();
    emitProgress(progress("run-test", "completed", 2, { rebuild_generation: 12 }));
    emitIndex(indexStatus(12, "error", "/private/index.sqlite: disk error"));
    expect(controller.state.phase).toBe("failed");
    expect(controller.state.error).not.toContain("/private/index.sqlite");
    expect(controller.state.technicalError).toContain("/private/index.sqlite");
    await controller.resume();
    expect(controller.state.phase).toBe("preview-ready");
  });

  it("never accepts completed-with-failures and keeps partial runs resumable", async () => {
    let emitProgress!: (event: BackfillProgress) => void;
    const controller = createRelationBackfillController(api({
      subscribe: vi.fn(async (handler) => {
        emitProgress = handler;
        return vi.fn();
      }),
    }));
    await controller.preview();
    controller.acknowledge(true);
    await controller.start();
    emitProgress(progress("run-test", "completed", 2, {
      failed: [{ note_id: "note-b", error: "provider output contained /private/path" }],
      rebuild_generation: 4,
    }));
    expect(controller.state.phase).toBe("partial");
    expect(controller.state.error).not.toContain("/private/path");
    await controller.resume();
    expect(controller.state.phase).toBe("preview-ready");
  });

  it("unsubscribes both listeners exactly once on start error and unmount", async () => {
    const unlistenProgress = vi.fn();
    const unlistenIndex = vi.fn();
    const controller = createRelationBackfillController(api({
      start: vi.fn().mockRejectedValue(new Error("/private/bin/agent exited 1")),
      subscribe: vi.fn().mockResolvedValue(unlistenProgress),
      subscribeIndex: vi.fn().mockResolvedValue(unlistenIndex),
    }));
    await controller.preview();
    controller.acknowledge(true);
    await expect(controller.start()).rejects.toThrow("agent exited");
    expect(controller.state.phase).toBe("failed");
    expect(controller.state.error).not.toContain("/private/bin");
    expect(controller.state.technicalError).toContain("/private/bin");
    controller.close();
    controller.dispose();
    expect(unlistenProgress).toHaveBeenCalledTimes(1);
    expect(unlistenIndex).toHaveBeenCalledTimes(1);
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

  it("keeps full IDs and technical errors inspectable only through disclosure", () => {
    const dialog = source("./RelationBackfillDialog.svelte");
    for (const text of [
      "取消补建",
      "继续未完成笔记",
      "重新预览",
      "aria-live",
      "当前笔记",
      "失败详情",
      "技术详情",
    ]) {
      expect(dialog).toContain(text);
    }
    expect(dialog).toContain("<details");
    expect(dialog).not.toContain("text-overflow: ellipsis");
    expect(dialog).not.toContain("line-clamp");
    expect(dialog).not.toContain("…");
  });

  it("refreshes only after generation-ready completion without remounting ForceGraph", () => {
    const dialog = source("./RelationBackfillDialog.svelte");
    const graph = source("../routes/graph/+page.svelte");
    expect(dialog).toContain('next.phase === "completed"');
    expect(graph).toContain("onCompleted={refreshAfterBackfill}");
    expect(graph).toContain("loadSemantic(effectiveGraphFilter)");
    expect(graph).toContain("probeGlobalSemanticPresence()");
    expect(graph).not.toMatch(/\{#if backfillOpen[^}]*\}[\s\S]{0,400}<ForceGraph/);
  });
});
