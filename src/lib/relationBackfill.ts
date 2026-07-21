import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import type {
  BackfillFailure,
  BackfillPreview,
  BackfillProgress,
  BackfillRequest,
} from "./knowledge";

export type RelationBackfillPhase =
  | "idle"
  | "preview-loading"
  | "preview-ready"
  | "preview-error"
  | "running"
  | "cancel-requested"
  | "completed"
  | "failed"
  | "cancelled";

export interface RelationBackfillState {
  phase: RelationBackfillPhase;
  preview: BackfillPreview | null;
  acknowledged: boolean;
  completed: number;
  total: number;
  currentNoteId: string | null;
  failures: BackfillFailure[];
  error: string;
}

export interface RelationBackfillApi {
  preview(noteIds?: string[]): Promise<BackfillPreview>;
  start(request: BackfillRequest): Promise<void>;
  cancel(): Promise<void>;
  subscribe(handler: (progress: BackfillProgress) => void): Promise<UnlistenFn>;
}

export interface RelationBackfillController {
  readonly state: RelationBackfillState;
  subscribe(handler: (state: RelationBackfillState) => void): () => void;
  preview(noteIds?: string[]): Promise<void>;
  acknowledge(value: boolean): void;
  start(): Promise<void>;
  cancel(): Promise<void>;
  resume(): Promise<void>;
  close(): void;
  dispose(): void;
}

const initialState = (): RelationBackfillState => ({
  phase: "idle",
  preview: null,
  acknowledged: false,
  completed: 0,
  total: 0,
  currentNoteId: null,
  failures: [],
  error: "",
});

function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message;
  const text = String(error).trim();
  return text || "未知错误";
}

export const previewRelationBackfill = (noteIds?: string[]) =>
  invoke<BackfillPreview>("preview_relation_backfill", { noteIds: noteIds ?? null });

export const startRelationBackfill = (request: BackfillRequest) =>
  invoke<void>("start_relation_backfill", { request });

export const cancelRelationBackfill = () =>
  invoke<void>("cancel_relation_backfill");

export function subscribeRelationBackfill(
  handler: (progress: BackfillProgress) => void,
): Promise<UnlistenFn> {
  return listen<BackfillProgress>("relation_backfill_progress", (event) => handler(event.payload));
}

export const relationBackfillApi: RelationBackfillApi = {
  preview: previewRelationBackfill,
  start: startRelationBackfill,
  cancel: cancelRelationBackfill,
  subscribe: subscribeRelationBackfill,
};

/**
 * One controller owns one dialog lifetime. Its monotonically increasing session
 * token rejects both late previews and progress queued by a previous run.
 */
export function createRelationBackfillController(
  api: RelationBackfillApi = relationBackfillApi,
): RelationBackfillController {
  let state = initialState();
  let session = 0;
  let activeUnlisten: UnlistenFn | null = null;
  const subscribers = new Set<(value: RelationBackfillState) => void>();

  const publish = (next: RelationBackfillState) => {
    state = next;
    for (const subscriber of subscribers) subscriber(state);
  };
  const patch = (next: Partial<RelationBackfillState>) => publish({ ...state, ...next });
  const cleanup = () => {
    const unlisten = activeUnlisten;
    activeUnlisten = null;
    unlisten?.();
  };

  const controller: RelationBackfillController = {
    get state() {
      return state;
    },
    subscribe(handler) {
      subscribers.add(handler);
      handler(state);
      return () => subscribers.delete(handler);
    },
    async preview(noteIds) {
      const token = ++session;
      cleanup();
      publish({ ...initialState(), phase: "preview-loading" });
      try {
        const value = await api.preview(noteIds);
        if (token !== session) return;
        publish({
          ...initialState(),
          phase: "preview-ready",
          preview: value,
          total: value.note_ids.length,
        });
      } catch (cause) {
        if (token !== session) return;
        publish({
          ...initialState(),
          phase: "preview-error",
          error: `无法预览关系补建：${errorMessage(cause)}。请检查执行体配置后重新预览。`,
        });
      }
    },
    acknowledge(value) {
      if (state.phase !== "preview-ready") return;
      patch({ acknowledged: value });
    },
    async start() {
      const selected = state.preview;
      if (state.phase !== "preview-ready" || !selected) {
        throw new Error("请先完成补建预览。");
      }
      if (!state.acknowledged) {
        throw new Error("请先确认隐私提示与本次补建范围。");
      }

      const token = session;
      cleanup();
      try {
        const unlisten = await api.subscribe((event) => {
          if (token !== session) return;
          const terminal = event.state === "completed" || event.state === "failed" || event.state === "cancelled";
          publish({
            ...state,
            phase: event.state,
            completed: event.completed,
            total: event.total,
            currentNoteId: event.current_note_id,
            failures: [...event.failed],
            error: event.state === "failed" && event.failed.length === 0
              ? "关系补建未完成，执行体没有返回详细失败原因。可以重新预览未完成笔记。"
              : state.error,
          });
          if (terminal) cleanup();
        });
        if (token !== session) {
          unlisten();
          return;
        }
        activeUnlisten = unlisten;
        // Listener installation must finish before the command can emit its
        // first running event.
        await api.start({ note_ids: [...selected.note_ids], provider: selected.provider });
        if (token !== session) return;
        if (state.phase === "preview-ready") {
          patch({
            phase: "running",
            completed: 0,
            total: selected.note_ids.length,
            currentNoteId: null,
            failures: [],
            error: "",
          });
        }
      } catch (cause) {
        if (token !== session) return;
        cleanup();
        patch({
          phase: "failed",
          error: `关系补建未能启动：${errorMessage(cause)}。可以重新预览未完成笔记后重试。`,
        });
        throw cause;
      }
    },
    async cancel() {
      if (state.phase !== "running") return;
      const token = session;
      patch({ phase: "cancel-requested", error: "" });
      try {
        await api.cancel();
      } catch (cause) {
        if (token !== session) return;
        patch({
          phase: "running",
          error: `取消请求未送达：${errorMessage(cause)}。补建可能仍在继续，请再次尝试。`,
        });
        throw cause;
      }
    },
    async resume() {
      if (state.phase !== "failed" && state.phase !== "cancelled") {
        throw new Error("只有失败或已取消的补建可以继续。");
      }
      // Undefined intentionally asks the backend for its durable unfinished set.
      await controller.preview(undefined);
    },
    close() {
      ++session;
      cleanup();
      publish(initialState());
    },
    dispose() {
      controller.close();
      subscribers.clear();
    },
  };
  return controller;
}
