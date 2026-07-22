import { describe, expect, it, vi } from "vitest";
import type { BackfillPreview, BackfillProgress, BackfillRequest } from "./knowledge";
import type { GraphIndexStatus } from "./knowledgeGovernance";
import {
  cancelRelationBackfill,
  createRelationBackfillController,
  previewRelationBackfill,
  retryRelationBackfillIndex,
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
  index_error: null,
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
    retryIndex: vi.fn().mockResolvedValue(1),
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
    await retryRelationBackfillIndex();
    await subscribeRelationBackfill(progressHandler);
    await subscribeRelationBackfillIndexStatus(indexHandler);

    expect(invokeMock.mock.calls).toEqual([
      ["preview_relation_backfill", { noteIds: null }],
      ["preview_relation_backfill", { noteIds: ["note-a"] }],
      ["start_relation_backfill", { request }],
      ["cancel_relation_backfill", { runId: "run-wrapper" }],
      ["retry_relation_backfill_index"],
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

  it("keeps a matching rebuild error isolated from resumable note failures", async () => {
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
    expect(controller.state.phase).toBe("index-failed");
    expect(controller.state.error).not.toContain("/private/index.sqlite");
    expect(controller.state.technicalError).toBe("");
    expect(controller.state.indexError).toContain("/private/index.sqlite");
    await expect(controller.resume()).rejects.toThrow("只有失败");
    expect(controller.state.phase).toBe("index-failed");
  });

  it("never accepts completed-with-failures and keeps partial runs resumable", async () => {
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
    emitProgress(progress("run-test", "completed", 2, {
      failed: [{ note_id: "note-b", error: "provider output contained /private/path" }],
      rebuild_generation: 4,
    }));
    expect(controller.state.phase).toBe("waiting-for-index");
    expect(controller.state.error).not.toContain("/private/path");
    emitIndex(indexStatus(4, "ready"));
    expect(controller.state.phase).toBe("partial");
    expect(controller.state.published).toBe(true);
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

  it("ignores a late start rejection after the exact run has already settled", async () => {
    let emitProgress!: (event: BackfillProgress) => void;
    let emitIndex!: (event: GraphIndexStatus) => void;
    const delayedStart = deferred<void>();
    const controller = createRelationBackfillController(api({
      start: vi.fn(() => delayedStart.promise),
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
    const task = controller.start();
    await vi.waitFor(() => expect(emitIndex).toBeTypeOf("function"));

    emitProgress(progress("run-test", "completed", 2, { rebuild_generation: 15 }));
    emitIndex(indexStatus(15, "ready"));
    expect(controller.state.phase).toBe("completed");

    const outcome = expect(task).resolves.toBeUndefined();
    delayedStart.reject(new Error("late transport rejection"));
    await outcome;
    expect(controller.state.phase).toBe("completed");
  });

  it("retries only the dirty index through exact error and ready generations", async () => {
    let emitProgress!: (event: BackfillProgress) => void;
    const indexHandlers: Array<(event: GraphIndexStatus) => void> = [];
    const order: string[] = [];
    const start = vi.fn().mockResolvedValue(undefined);
    const retryIndex = vi.fn()
      .mockImplementationOnce(async () => {
        order.push("retry-20");
        return 20;
      })
      .mockImplementationOnce(async () => {
        order.push("retry-21");
        return 21;
      });
    const controller = createRelationBackfillController(api({
      start,
      retryIndex,
      subscribe: vi.fn(async (handler) => {
        emitProgress = handler;
        return vi.fn();
      }),
      subscribeIndex: vi.fn(async (handler) => {
        order.push(`listen-${indexHandlers.length}`);
        indexHandlers.push(handler);
        return vi.fn();
      }),
    }));
    await controller.preview();
    controller.acknowledge(true);
    await controller.start();
    emitProgress(progress("run-test", "completed", 2, {
      index_error: "/private/index.sqlite: initial queue failure",
    }));
    expect(controller.state.phase).toBe("index-failed");
    expect(controller.state.failures).toEqual([]);
    expect(controller.state.error).not.toContain("/private/index.sqlite");
    expect(controller.state.indexError).toContain("/private/index.sqlite");

    await controller.retryIndex();
    expect(order.slice(-2)).toEqual(["listen-1", "retry-20"]);
    indexHandlers[1]!(indexStatus(19, "ready"));
    expect(controller.state.phase).toBe("waiting-for-index");
    indexHandlers[1]!(indexStatus(20, "error", "/private/index.sqlite: publish failed"));
    expect(controller.state.phase).toBe("index-failed");

    await controller.retryIndex();
    expect(order.slice(-2)).toEqual(["listen-2", "retry-21"]);
    indexHandlers[2]!(indexStatus(20, "ready"));
    expect(controller.state.phase).toBe("waiting-for-index");
    indexHandlers[2]!(indexStatus(21, "ready"));
    expect(controller.state.phase).toBe("completed");
    expect(controller.state.published).toBe(true);
    expect(controller.state.rebuildGeneration).toBe(21);
    expect(start).toHaveBeenCalledTimes(1);
    expect(retryIndex).toHaveBeenCalledTimes(2);
  });

  it.each(["partial", "cancelled"] as const)(
    "waits for committed %s work to publish before exposing the terminal state",
    async (terminalPhase) => {
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
      emitProgress(progress("run-test", terminalPhase, 1, {
        failed: terminalPhase === "partial" ? [{ note_id: "note-b", error: "provider failed" }] : [],
        rebuild_generation: 31,
      }));
      expect(controller.state.phase).toBe("waiting-for-index");
      expect(controller.state.rebuildGeneration).toBe(31);
      emitIndex(indexStatus(31, "ready"));
      expect(controller.state.phase).toBe(terminalPhase);
      expect(controller.state.published).toBe(true);
      expect(controller.state.publishedGeneration).toBe(31);
    },
  );

  it("reports each distinct published generation once across resumed runs in one dialog", async () => {
    const progressHandlers: Array<(event: BackfillProgress) => void> = [];
    const indexHandlers: Array<(event: GraphIndexStatus) => void> = [];
    const runIds = ["run-partial", "run-cancelled"];
    const controller = createRelationBackfillController(api({
      createRunId: vi.fn(() => runIds.shift()!),
      subscribe: vi.fn(async (handler) => {
        progressHandlers.push(handler);
        return vi.fn();
      }),
      subscribeIndex: vi.fn(async (handler) => {
        indexHandlers.push(handler);
        return vi.fn();
      }),
    }));
    const onCompleted = vi.fn();
    let lastReportedGeneration: number | null = null;
    controller.subscribe((next) => {
      if (
        next.publishedGeneration !== null &&
        next.publishedGeneration !== lastReportedGeneration
      ) {
        lastReportedGeneration = next.publishedGeneration;
        onCompleted(next.publishedGeneration);
      }
    });

    await controller.preview();
    controller.acknowledge(true);
    await controller.start();
    progressHandlers[0]!(progress("run-partial", "partial", 1, {
      failed: [{ note_id: "note-b", error: "provider failed" }],
      rebuild_generation: 41,
    }));
    indexHandlers[0]!(indexStatus(41, "ready"));
    indexHandlers[0]!(indexStatus(41, "ready"));
    expect(onCompleted).toHaveBeenCalledTimes(1);
    expect(onCompleted).toHaveBeenLastCalledWith(41);

    await controller.resume();
    controller.acknowledge(true);
    await controller.start();
    progressHandlers[1]!(progress("run-cancelled", "cancelled", 1, {
      rebuild_generation: 42,
    }));
    indexHandlers[1]!(indexStatus(41, "ready"));
    indexHandlers[1]!(indexStatus(42, "ready"));
    indexHandlers[1]!(indexStatus(42, "ready"));
    expect(onCompleted).toHaveBeenCalledTimes(2);
    expect(onCompleted).toHaveBeenLastCalledWith(42);
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
    expect(dialog).toContain("将把修订稿发送给当前配置的处理方式");
    expect(dialog).not.toMatch(/价格|费用|token|令牌估算/i);
  });

  it("keeps full IDs and technical errors inspectable only through disclosure", () => {
    const dialog = source("./RelationBackfillDialog.svelte");
    for (const text of [
      "停止分析",
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
    expect(dialog).toContain("next.published");
    expect(dialog).toContain("lastReportedGeneration");
    expect(dialog).toContain("next.publishedGeneration");
    expect(dialog).not.toMatch(/lastReportedGeneration = null;[\s\S]{0,80}dialog\.showModal/);
    expect(dialog).toContain("重试索引");
    expect(dialog).toContain("controller.retryIndex()");
    expect(graph).toContain("onCompleted={refreshAfterBackfill}");
    expect(graph).toContain("loadSemantic(effectiveGraphFilter)");
    expect(graph).toContain("probeGlobalSemanticPresence()");
    expect(graph).not.toMatch(/\{#if backfillOpen[^}]*\}[\s\S]{0,400}<ForceGraph/);
  });

  it("uses the design-system primary CTA and coarse disclosure targets", () => {
    const dialog = source("./RelationBackfillDialog.svelte");
    expect(dialog).toMatch(/\.primary\s*\{[^}]*background:\s*var\(--primary\)/s);
    expect(dialog).toMatch(/\.primary\s*\{[^}]*color:\s*var\(--on-primary\)/s);
    expect(dialog).toMatch(/\.primary\s*\{[^}]*border-radius:\s*var\(--radius-full\)/s);
    expect(dialog).toMatch(/\.primary\s*\{[^}]*box-shadow:\s*var\(--shadow-btn\)/s);
    expect(dialog).toMatch(/\.primary:hover:not\(:disabled\)[^}]*background:\s*var\(--primary-pressed\)/s);
    expect(dialog).toMatch(/\.primary:active:not\(:disabled\)[^}]*translateY\(0\.5px\)/s);
    expect(dialog).not.toMatch(/\.primary\s*\{[^}]*var\(--accent\)/s);
    expect(dialog).not.toMatch(/font-weight:\s*(?:620|650)/);
    expect(dialog).toMatch(/@media \(pointer: coarse\)\s*\{[^}]*\.technical summary[^}]*min-height:\s*44px/s);
  });
});
