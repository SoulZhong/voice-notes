import { invoke } from "@tauri-apps/api/core";
import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { t } from "$lib/i18n/index.svelte";
import type {
  BackfillFailure,
  BackfillPreview,
  BackfillProgress,
  BackfillRequest,
} from "./knowledge";
import type { GraphIndexStatus } from "./knowledgeGovernance";

export type RelationBackfillPhase =
  | "idle"
  | "preview-loading"
  | "preview-ready"
  | "preview-error"
  | "starting"
  | "running"
  | "cancel-requested"
  | "index-retrying"
  | "waiting-for-index"
  | "index-failed"
  | "completed"
  | "partial"
  | "failed"
  | "cancelled";

export interface RelationBackfillState {
  phase: RelationBackfillPhase;
  preview: BackfillPreview | null;
  acknowledged: boolean;
  runId: string | null;
  completed: number;
  total: number;
  currentNoteId: string | null;
  failures: BackfillFailure[];
  rebuildGeneration: number | null;
  terminalPhase: RelationBackfillTerminalPhase | null;
  published: boolean;
  publishedGeneration: number | null;
  error: string;
  technicalError: string;
  indexError: string;
}

export type RelationBackfillTerminalPhase = Extract<
  RelationBackfillPhase,
  "completed" | "partial" | "failed" | "cancelled"
>;

export interface RelationBackfillApi {
  preview(noteIds?: string[]): Promise<BackfillPreview>;
  start(request: BackfillRequest): Promise<void>;
  cancel(runId: string): Promise<void>;
  retryIndex(): Promise<number>;
  subscribe(handler: (progress: BackfillProgress) => void): Promise<UnlistenFn>;
  subscribeIndex(handler: (status: GraphIndexStatus) => void): Promise<UnlistenFn>;
  createRunId(): string;
}

export interface RelationBackfillController {
  readonly state: RelationBackfillState;
  subscribe(handler: (state: RelationBackfillState) => void): () => void;
  preview(noteIds?: string[]): Promise<void>;
  acknowledge(value: boolean): void;
  start(): Promise<void>;
  cancel(): Promise<void>;
  retryIndex(): Promise<void>;
  resume(): Promise<void>;
  close(): void;
  dispose(): void;
}

const initialState = (): RelationBackfillState => ({
  phase: "idle",
  preview: null,
  acknowledged: false,
  runId: null,
  completed: 0,
  total: 0,
  currentNoteId: null,
  failures: [],
  rebuildGeneration: null,
  terminalPhase: null,
  published: false,
  publishedGeneration: null,
  error: "",
  technicalError: "",
  indexError: "",
});

function errorMessage(error: unknown): string {
  if (error instanceof Error && error.message.trim()) return error.message;
  const text = String(error).trim();
  return text || t("governance.unknownError");
}

function failureDetails(failures: BackfillFailure[]): string {
  return failures
    .map((failure) => t("governance.backfill.failureLine", { name: failure.note_id || t("governance.backfill.indexRebuild"), error: failure.error }))
    .join("\n");
}

export const previewRelationBackfill = (noteIds?: string[]) =>
  invoke<BackfillPreview>("preview_relation_backfill", { noteIds: noteIds ?? null });

export const startRelationBackfill = (request: BackfillRequest) =>
  invoke<void>("start_relation_backfill", { request });

export const cancelRelationBackfill = (runId: string) =>
  invoke<void>("cancel_relation_backfill", { runId });

export const retryRelationBackfillIndex = () =>
  invoke<number>("retry_relation_backfill_index");

export function subscribeRelationBackfill(
  handler: (progress: BackfillProgress) => void,
): Promise<UnlistenFn> {
  return listen<BackfillProgress>("relation_backfill_progress", (event) => handler(event.payload));
}

export function subscribeRelationBackfillIndexStatus(
  handler: (status: GraphIndexStatus) => void,
): Promise<UnlistenFn> {
  return listen<GraphIndexStatus>("graph_index_status", (event) => handler(event.payload));
}

function createRunId(): string {
  return `run-${globalThis.crypto.randomUUID()}`;
}

export const relationBackfillApi: RelationBackfillApi = {
  preview: previewRelationBackfill,
  start: startRelationBackfill,
  cancel: cancelRelationBackfill,
  retryIndex: retryRelationBackfillIndex,
  subscribe: subscribeRelationBackfill,
  subscribeIndex: subscribeRelationBackfillIndexStatus,
  createRunId,
};

/**
 * One controller owns one dialog lifetime. Session and backend run IDs are
 * independent: the first rejects stale promises, the second rejects stale
 * events even when they arrive through a newly installed listener.
 */
export function createRelationBackfillController(
  api: RelationBackfillApi = relationBackfillApi,
): RelationBackfillController {
  let state = initialState();
  let session = 0;
  let progressUnlisten: UnlistenFn | null = null;
  let indexUnlisten: UnlistenFn | null = null;
  let startInFlight: Promise<void> | null = null;
  let indexRetryInFlight: Promise<void> | null = null;
  let runSettled = false;
  let targetGeneration: number | null = null;
  const bufferedIndexTerminals = new Map<number, GraphIndexStatus>();
  const subscribers = new Set<(value: RelationBackfillState) => void>();

  const publish = (next: RelationBackfillState) => {
    state = next;
    for (const subscriber of subscribers) subscriber(state);
  };
  const patch = (next: Partial<RelationBackfillState>) => publish({ ...state, ...next });
  const cleanup = () => {
    const progress = progressUnlisten;
    const index = indexUnlisten;
    progressUnlisten = null;
    indexUnlisten = null;
    progress?.();
    index?.();
  };
  const resetRunTracking = () => {
    runSettled = false;
    targetGeneration = null;
    bufferedIndexTerminals.clear();
  };
  const settle = (
    phase: RelationBackfillTerminalPhase,
    summary = "",
    technicalError = "",
    published = false,
  ) => {
    if (runSettled) return;
    runSettled = true;
    patch({
      phase,
      terminalPhase: phase,
      published,
      publishedGeneration: published ? state.rebuildGeneration : null,
      currentNoteId: null,
      error: summary,
      technicalError,
      indexError: "",
    });
    cleanup();
  };
  const terminalSummary = (phase: RelationBackfillTerminalPhase) => {
    if (phase === "partial") return t("governance.backfill.summaryPartial");
    if (phase === "cancelled") return t("governance.backfill.summaryCancelled");
    if (phase === "failed") return t("governance.backfill.summaryFailed");
    return "";
  };
  const failIndex = (phase: RelationBackfillTerminalPhase, detail: string) => {
    if (runSettled) return;
    runSettled = true;
    patch({
      phase: "index-failed",
      terminalPhase: phase,
      published: false,
      publishedGeneration: null,
      currentNoteId: null,
      error: t("governance.backfill.indexPublishFailed"),
      indexError: detail || t("governance.backfill.noIndexDetail"),
    });
    cleanup();
  };
  const waitForIndex = (phase: RelationBackfillTerminalPhase, generation: number) => {
    targetGeneration = generation;
    patch({
      phase: "waiting-for-index",
      terminalPhase: phase,
      published: false,
      publishedGeneration: null,
      rebuildGeneration: generation,
      error: t("governance.backfill.waitingPublish"),
      indexError: "",
    });
    const buffered = bufferedIndexTerminals.get(generation);
    bufferedIndexTerminals.clear();
    if (buffered) handleIndexTerminal(buffered);
  };
  function handleIndexTerminal(status: GraphIndexStatus) {
    if (runSettled || targetGeneration === null || status.generation !== targetGeneration) return;
    const terminalPhase = state.terminalPhase;
    if (!terminalPhase) return;
    if (status.state === "ready") {
      settle(terminalPhase, terminalSummary(terminalPhase), state.technicalError, true);
    } else if (status.state === "error") {
      failIndex(terminalPhase, status.error || t("governance.backfill.noIndexDetail"));
    }
  }

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
      resetRunTracking();
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
          error: t("governance.backfill.previewFailed"),
          technicalError: errorMessage(cause),
        });
      }
    },
    acknowledge(value) {
      if (state.phase !== "preview-ready") return;
      patch({ acknowledged: value });
    },
    start() {
      if (startInFlight) return startInFlight;
      const selected = state.preview;
      if (state.phase !== "preview-ready" || !selected) {
        return Promise.reject(new Error(t("governance.backfill.needPreview")));
      }
      if (!state.acknowledged) {
        return Promise.reject(new Error(t("governance.backfill.needConsent")));
      }

      const token = session;
      const runId = api.createRunId();
      const request: BackfillRequest = {
        run_id: runId,
        consent_token: selected.consent_token,
        note_ids: [...selected.note_ids],
        provider: selected.provider,
        model: selected.model,
        contract_version: selected.contract_version,
      };
      cleanup();
      resetRunTracking();
      patch({
        phase: "starting",
        runId,
        completed: 0,
        total: selected.note_ids.length,
        currentNoteId: null,
        failures: [],
        rebuildGeneration: null,
        terminalPhase: null,
        published: false,
        publishedGeneration: null,
        error: "",
        technicalError: "",
        indexError: "",
      });

      let task!: Promise<void>;
      task = (async () => {
        try {
          const progressListener = await api.subscribe((event) => {
            if (token !== session || event.run_id !== runId || runSettled) return;
            const failures = [...event.failed];
            const technicalError = failureDetails(failures);
            if (event.state === "running") {
              patch({
                phase: "running",
                completed: event.completed,
                total: event.total,
                currentNoteId: event.current_note_id,
                failures,
                error: failures.length > 0 ? t("governance.backfill.partialProgress") : "",
                technicalError,
                indexError: "",
              });
              return;
            }
            const terminalPhase: RelationBackfillTerminalPhase =
              event.state === "cancelled"
                ? "cancelled"
                : event.state === "partial" || (event.state === "completed" && failures.length > 0)
                  ? "partial"
                  : event.state === "failed"
                    ? "failed"
                    : "completed";
            patch({
              completed: event.completed,
              total: event.total,
              currentNoteId: null,
              failures,
              rebuildGeneration: event.rebuild_generation,
              terminalPhase,
              published: false,
              publishedGeneration: null,
              technicalError,
              indexError: event.index_error || "",
            });
            if (event.index_error) {
              failIndex(terminalPhase, event.index_error);
              return;
            }
            const generation = event.rebuild_generation;
            if (Number.isSafeInteger(generation) && (generation ?? 0) > 0) {
              waitForIndex(terminalPhase, generation as number);
              return;
            }
            if (terminalPhase === "completed") {
              failIndex(terminalPhase, "completed progress missing rebuild_generation");
              return;
            }
            settle(terminalPhase, terminalSummary(terminalPhase), technicalError);
          });
          if (token !== session || runSettled) {
            progressListener();
            return;
          }
          progressUnlisten = progressListener;

          const rebuildListener = await api.subscribeIndex((status) => {
            if (token !== session || runSettled) return;
            if (status.state !== "ready" && status.state !== "error") return;
            if (targetGeneration === null) {
              bufferedIndexTerminals.set(status.generation, status);
              if (bufferedIndexTerminals.size > 32) {
                const oldest = bufferedIndexTerminals.keys().next().value;
                if (oldest !== undefined) bufferedIndexTerminals.delete(oldest);
              }
              return;
            }
            handleIndexTerminal(status);
          });
          if (token !== session || runSettled) {
            rebuildListener();
            return;
          }
          indexUnlisten = rebuildListener;

          await api.start(request);
          if (token !== session || runSettled) return;
          if (state.phase === "starting") patch({ phase: "running" });
        } catch (cause) {
          if (token !== session || runSettled) return;
          settle(
            "failed",
            t("governance.backfill.startFailed"),
            errorMessage(cause),
          );
          throw cause;
        } finally {
          if (startInFlight === task) startInFlight = null;
        }
      })();
      startInFlight = task;
      return task;
    },
    async cancel() {
      const runId = state.runId;
      if (state.phase !== "running" || !runId) return;
      const token = session;
      patch({ phase: "cancel-requested", error: "", technicalError: "" });
      try {
        await api.cancel(runId);
      } catch (cause) {
        if (token !== session || state.runId !== runId || runSettled) return;
        patch({
          phase: "running",
          error: t("governance.backfill.cancelFailed"),
          technicalError: errorMessage(cause),
        });
        throw cause;
      }
    },
    retryIndex() {
      if (indexRetryInFlight) return indexRetryInFlight;
      const terminalPhase = state.terminalPhase;
      if (state.phase !== "index-failed" || !terminalPhase) {
        return Promise.reject(new Error(t("governance.backfill.retryIndexOnlyWhenFailed")));
      }

      const token = session;
      cleanup();
      resetRunTracking();
      patch({
        phase: "index-retrying",
        terminalPhase,
        published: false,
        publishedGeneration: null,
        rebuildGeneration: null,
        error: t("governance.backfill.republishing"),
        indexError: "",
      });

      let task!: Promise<void>;
      task = (async () => {
        try {
          const rebuildListener = await api.subscribeIndex((status) => {
            if (token !== session || runSettled) return;
            if (status.state !== "ready" && status.state !== "error") return;
            if (targetGeneration === null) {
              bufferedIndexTerminals.set(status.generation, status);
              if (bufferedIndexTerminals.size > 32) {
                const oldest = bufferedIndexTerminals.keys().next().value;
                if (oldest !== undefined) bufferedIndexTerminals.delete(oldest);
              }
              return;
            }
            handleIndexTerminal(status);
          });
          if (token !== session || runSettled) {
            rebuildListener();
            return;
          }
          indexUnlisten = rebuildListener;

          const generation = await api.retryIndex();
          if (token !== session || runSettled) return;
          if (!Number.isSafeInteger(generation) || generation <= 0) {
            throw new Error("index retry missing rebuild_generation");
          }
          waitForIndex(terminalPhase, generation);
        } catch (cause) {
          if (token !== session || runSettled) return;
          failIndex(terminalPhase, errorMessage(cause));
          throw cause;
        } finally {
          if (indexRetryInFlight === task) indexRetryInFlight = null;
        }
      })();
      indexRetryInFlight = task;
      return task;
    },
    async resume() {
      if (state.phase !== "failed" && state.phase !== "cancelled" && state.phase !== "partial") {
        throw new Error(t("governance.backfill.resumeOnlyWhenIncomplete"));
      }
      await controller.preview(undefined);
    },
    close() {
      ++session;
      cleanup();
      resetRunTracking();
      publish(initialState());
    },
    dispose() {
      controller.close();
      subscribers.clear();
    },
  };
  return controller;
}
