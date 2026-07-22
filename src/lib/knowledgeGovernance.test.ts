import { describe, expect, it, vi } from "vitest";
import type {
  KnowledgeMutationResult,
  MentionEvidence,
  PendingReviewItem,
} from "./knowledge";
import {
  buildAddAlias,
  buildBindPerson,
  buildConfirmRelation,
  buildCreateEntity,
  buildCreateRelation,
  buildEditRelation,
  buildEndRelation,
  buildMergeEntities,
  buildRemoveAlias,
  buildRenameEntity,
  buildRestoreRelation,
  buildSuppressRelation,
  buildSplitEntity,
  buildUndo,
  canSubmitSplit,
  createGovernanceController,
  groupPending,
  pendingReviewModel,
  pendingAfterLater,
  prepareRebuildWait,
  retainLastKnownRelation,
  splitPreview,
  type GraphIndexStatus,
  type GovernanceApi,
  type GovernanceMention,
  type PrepareRebuildWait,
  type RebuildWaitHandle,
} from "./knowledgeGovernance";

const result = (operationId: string, rebuildGeneration = 1): KnowledgeMutationResult => ({
  operation_id: operationId,
  entity_id: null,
  rebuild_state: "queued",
  rebuild_generation: rebuildGeneration,
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

function api(overrides: Partial<GovernanceApi> = {}): GovernanceApi {
  return {
    submit: vi.fn().mockResolvedValue(result("op_submit")),
    split: vi.fn().mockResolvedValue(result("op_split")),
    merge: vi.fn().mockResolvedValue(result("op_merge")),
    undo: vi.fn().mockResolvedValue(result("op_undo")),
    ...overrides,
  };
}

const status = (
  state: GraphIndexStatus["state"],
  generation: number,
  error: string | null = null,
): GraphIndexStatus => ({
  generation,
  state,
  error,
  stats: null,
});

function immediateRebuildWait(): PrepareRebuildWait {
  return vi.fn(async () => ({
    waitFor: vi.fn(async (generation: number) => status("ready", generation)),
    cancel: vi.fn(),
  }));
}

describe("createGovernanceController", () => {
  it("deduplicates concurrent mutations and calls the API once", async () => {
    const pending = deferred<KnowledgeMutationResult>();
    const submit = vi.fn(() => pending.promise);
    const controller = createGovernanceController(api({ submit }), vi.fn(), immediateRebuildWait());
    const operation = buildRenameEntity("kg_a", "完整新名称");

    const first = controller.submit(operation);
    const duplicate = controller.submit(operation);
    expect(controller.busy).toBe(true);
    await Promise.resolve();
    expect(submit).toHaveBeenCalledTimes(1);

    pending.resolve(result("op_1"));
    await expect(first).resolves.toEqual(result("op_1"));
    await expect(duplicate).resolves.toEqual(result("op_1"));
    expect(controller.busy).toBe(false);
  });

  it("preserves an actionable mutation error and clears it when retry begins", async () => {
    const retry = deferred<KnowledgeMutationResult>();
    const submit = vi
      .fn()
      .mockRejectedValueOnce(new Error("ledger is read-only"))
      .mockImplementationOnce(() => retry.promise);
    const controller = createGovernanceController(api({ submit }), vi.fn(), immediateRebuildWait());
    const operation = buildAddAlias("kg_a", "完整别名");

    await expect(controller.submit(operation)).rejects.toThrow("ledger is read-only");
    expect(controller.error).toContain("ledger is read-only");
    expect(controller.lastOperationId).toBeNull();

    const second = controller.submit(operation);
    expect(controller.error).toBe("");
    retry.resolve(result("op_retry"));
    await second;
  });

  it("subscribes before mutation and ignores the coalesced old generation until the queued generation is ready", async () => {
    const order: string[] = [];
    let emit!: (value: GraphIndexStatus) => void;
    const cancel = vi.fn();
    const mutation = deferred<KnowledgeMutationResult>();
    const prepare = vi.fn(async () => prepareRebuildWait(async (listener) => {
      order.push("subscribe");
      emit = listener;
      return cancel;
    }));
    const controller = createGovernanceController(
      api({ submit: vi.fn(() => { order.push("api"); return mutation.promise; }) }),
      vi.fn(async () => { order.push("refresh"); }),
      prepare,
    );

    const operation = controller.submit(buildConfirmRelation("rel_a"));
    await vi.waitFor(() => expect(order).toEqual(["subscribe", "api"]));
    emit(status("building", 41));
    mutation.resolve(result("op_2", 42));
    await Promise.resolve();
    emit(status("ready", 41));
    await Promise.resolve();
    expect(order).toEqual(["subscribe", "api"]);
    emit(status("building", 42));
    emit(status("ready", 42));
    await operation;
    expect(order).toEqual(["subscribe", "api", "refresh"]);
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(controller.lastOperationId).toBe("op_2");
  });

  it("keeps the committed operation when the observed rebuild ends in error", async () => {
    let emit!: (value: GraphIndexStatus) => void;
    const prepare = vi.fn(async () => prepareRebuildWait(async (listener) => {
      emit = listener;
      const unlisten: () => void = vi.fn();
      return unlisten;
    }));
    const refresh = vi.fn();
    const controller = createGovernanceController(
      api({ submit: vi.fn().mockResolvedValue(result("op_submit", 7)) }),
      refresh,
      prepare,
    );

    const operation = controller.submit(buildConfirmRelation("rel_a"));
    await vi.waitFor(() => expect(prepare).toHaveBeenCalledTimes(1));
    emit(status("building", 7));
    emit(status("error", 7, "semantic index unavailable"));

    await expect(operation).resolves.toEqual(result("op_submit", 7));
    expect(controller.lastOperationId).toBe("op_submit");
    expect(controller.error).toBe("");
    expect(controller.refreshError).toContain("索引重建失败");
    expect(controller.refreshError).toContain("稍后重试");
    expect(refresh).not.toHaveBeenCalled();
  });

  it("cancels the prepared listener when mutation fails", async () => {
    const cancel = vi.fn();
    const handle: RebuildWaitHandle = {
      waitFor: vi.fn(() => new Promise<GraphIndexStatus>(() => {})),
      cancel,
    };
    const submit = vi.fn().mockRejectedValue(new Error("ledger is read-only"));
    const refresh = vi.fn();
    const controller = createGovernanceController(api({ submit }), refresh, vi.fn(async () => handle));

    await expect(controller.submit(buildConfirmRelation("rel_a"))).rejects.toThrow("ledger is read-only");
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(refresh).not.toHaveBeenCalled();
  });

  it("cancels the prepared listener and refreshes once for a non-queued result", async () => {
    const cancel = vi.fn();
    const refresh = vi.fn();
    const nonQueued = {
      ...result("op_immediate"),
      rebuild_state: "ready",
      rebuild_generation: null,
    } as unknown as KnowledgeMutationResult;
    const controller = createGovernanceController(
      api({ submit: vi.fn().mockResolvedValue(nonQueued) }),
      refresh,
      vi.fn(async () => ({
        waitFor: vi.fn(() => new Promise<GraphIndexStatus>(() => {})),
        cancel,
      })),
    );

    await expect(controller.submit(buildConfirmRelation("rel_a"))).resolves.toEqual(nonQueued);
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(refresh).toHaveBeenCalledTimes(1);
  });

  it("fails safely when a queued mutation response omits its generation", async () => {
    const cancel = vi.fn();
    const waitFor = vi.fn(() => new Promise<GraphIndexStatus>(() => {}));
    const refresh = vi.fn();
    const malformed = { ...result("op_missing_generation"), rebuild_generation: null };
    const controller = createGovernanceController(
      api({ submit: vi.fn().mockResolvedValue(malformed) }),
      refresh,
      vi.fn(async () => ({ waitFor, cancel })),
    );

    await expect(controller.submit(buildConfirmRelation("rel_a"))).resolves.toEqual(malformed);
    expect(controller.lastOperationId).toBe("op_missing_generation");
    expect(controller.refreshError).toContain("generation");
    expect(waitFor).not.toHaveBeenCalled();
    expect(cancel).toHaveBeenCalledTimes(1);
    expect(refresh).not.toHaveBeenCalled();
  });

  it("distinguishes refresh failure, keeps the operation ID, and offers retry", async () => {
    const refresh = vi
      .fn()
      .mockRejectedValueOnce(new Error("index unavailable"))
      .mockResolvedValueOnce(undefined);
    const controller = createGovernanceController(api(), refresh, immediateRebuildWait());

    await expect(controller.submit(buildConfirmRelation("rel_a"))).resolves.toEqual(
      result("op_submit"),
    );
    expect(controller.lastOperationId).toBe("op_submit");
    expect(controller.refreshError).toContain("index unavailable");
    expect(controller.error).toBe("");

    await controller.retryRefresh();
    expect(refresh).toHaveBeenCalledTimes(2);
    expect(controller.refreshError).toBe("");
  });

  it("undo uses the returned operation ID and stores the compensating operation", async () => {
    const undo = vi.fn().mockResolvedValue(result("op_compensation"));
    const controller = createGovernanceController(api({ undo }), vi.fn(), immediateRebuildWait());

    await controller.submit(buildRenameEntity("kg_a", "新名称"));
    await controller.undo(controller.lastOperationId!);
    expect(undo).toHaveBeenCalledWith("op_submit");
    expect(controller.lastOperationId).toBe("op_compensation");
  });
});

describe("prepareRebuildWait", () => {
  it("accepts only the requested generation terminal and unsubscribes once", async () => {
    let emit!: (value: GraphIndexStatus) => void;
    const unlisten = vi.fn();
    const handle = await prepareRebuildWait(async (listener) => {
      emit = listener;
      return unlisten;
    });
    let settled = false;
    const terminal = handle.waitFor(12);
    void terminal.then(() => { settled = true; });

    emit(status("building", 11));
    emit(status("ready", 11));
    await Promise.resolve();
    expect(settled).toBe(false);
    emit(status("building", 12));
    emit(status("ready", 12));
    await expect(terminal).resolves.toEqual(status("ready", 12));
    expect(unlisten).toHaveBeenCalledTimes(1);
  });
});

describe("operation builders", () => {
  it("matches every Task 10/Rust discriminant and payload exactly", () => {
    expect(buildRenameEntity("a", "A")).toEqual({ kind: "rename_entity", payload: { entity_id: "a", name: "A" } });
    expect(buildAddAlias("a", "AA")).toEqual({ kind: "add_alias", payload: { entity_id: "a", alias: "AA" } });
    expect(buildRemoveAlias("a", "AA")).toEqual({ kind: "remove_alias", payload: { entity_id: "a", alias: "AA" } });
    expect(buildBindPerson("mention", "person")).toEqual({ kind: "bind_mention", payload: { mention_id: "mention", entity_id: "person" } });
    expect(buildConfirmRelation("r")).toEqual({ kind: "confirm_relation", payload: { relation_id: "r" } });
    expect(buildEditRelation("r", "a", { type: "custom", label: "完整关系" }, "b", null, null, "人工核对")).toEqual({
      kind: "edit_relation",
      payload: { relation_id: "r", subject_id: "a", predicate: { type: "custom", label: "完整关系" }, object_id: "b", valid_from: null, valid_to: null, note: "人工核对" },
    });
    expect(buildSuppressRelation("a", { type: "uses", label: null }, "b")).toEqual({ kind: "suppress_relation", payload: { subject_id: "a", predicate: { type: "uses", label: null }, object_id: "b" } });
    expect(buildEndRelation("r", "2026-07-21T00:00:00Z")).toEqual({ kind: "end_relation", payload: { relation_id: "r", valid_to: "2026-07-21T00:00:00Z" } });
    expect(buildRestoreRelation("op_suppress")).toEqual({ kind: "restore_relation", payload: { operation_id: "op_suppress" } });
    expect(buildCreateEntity("project", "A", ["AA"])).toEqual({ kind: "create_entity", payload: { kind: "project", name: "A", aliases: ["AA"] } });
    expect(buildCreateRelation("a", { type: "custom", label: "完整关系" }, "b", null, null, "说明", ["ev_1"], false)).toEqual({
      kind: "create_relation",
      payload: { subject_id: "a", predicate: { type: "custom", label: "完整关系" }, object_id: "b", valid_from: null, valid_to: null, note: "说明", evidence_ids: ["ev_1"], user_assertion: false },
    });
    expect(buildMergeEntities("source", "target")).toEqual({ sourceId: "source", targetId: "target" });
    expect(buildSplitEntity("source", "拆分实体", "project", ["别名"], ["m_1"])).toEqual({ entity_id: "source", name: "拆分实体", kind: "project", aliases: ["别名"], mention_ids: ["m_1"] });
    expect(buildUndo("op_1")).toEqual({ operationId: "op_1" });
  });
});

const mention = (
  id: string,
  noteId: string,
  relationIds: string[] = [],
): GovernanceMention => ({
  id,
  note_id: noteId,
  entity_id: "kg_a",
  paragraph_index: 0,
  start_offset: 0,
  end_offset: 4,
  quote: `完整证据 ${id}`,
  relation_ids: relationIds,
});

describe("splitPreview", () => {
  it("deduplicates IDs and counts moved notes, mentions, and affected relations exactly", () => {
    const total = [mention("m_1", "n_2", ["r_2", "r_1"]), mention("m_2", "n_1", ["r_2"]), mention("m_3", "n_1", ["r_3"])];
    const selected = [total[1]!, total[0]!, { ...total[0] }];
    const before = structuredClone({ total, selected });

    expect(splitPreview(selected, total)).toEqual({
      noteCount: 2,
      mentionCount: 2,
      affectedRelationCount: 2,
      selectedMentionIds: ["m_1", "m_2"],
    });
    expect({ total, selected }).toEqual(before);
    expect(splitPreview([...selected].reverse(), [...total].reverse())).toEqual(splitPreview(selected, total));
  });

  it("disables an empty or unknown selection", () => {
    const total: MentionEvidence[] = [mention("m_1", "n_1")];
    expect(canSubmitSplit(splitPreview([], total))).toBe(false);
    expect(canSubmitSplit(splitPreview([mention("missing", "n_2")], total))).toBe(false);
    expect(canSubmitSplit(splitPreview(total, total))).toBe(true);
  });
});

const pending = (
  id: string,
  kind: string,
  payload: PendingReviewItem["payload"],
  noteId: string | null = null,
  relationId: string | null = null,
): PendingReviewItem => ({
  id,
  kind,
  note_id: noteId,
  relation_id: relationId,
  payload,
});

const pendingFixtures = [
  pending("invalid", "invalid_document", { kind: "invalid_document", note_id: "note_invalid", message: "frontmatter 无效" }, "note_invalid"),
  pending("identity", "identity_conflict", { kind: "identity_conflict", note_id: "note_identity", local_entity_id: "local_alice", candidates: ["kg_alice", "kg_alicia"], reason: "名称存在多个匹配" }, "note_identity"),
  pending("stale", "stale_evidence", { kind: "stale_evidence", note_id: "note_stale", relation_id: "rel_stale", evidence_id: "ev_stale" }, "note_stale", "rel_stale"),
  pending("split", "split_conflict", { kind: "split_conflict", note_id: "note_split", relation_id: "rel_split", evidence_id: "ev_split" }, "note_split", "rel_split"),
  pending("review", "relation_review", { kind: "relation_review", note_id: "note_review", relation_id: "rel_review" }, "note_review", "rel_review"),
  pending("time", "time_conflict", { kind: "time_conflict", relation_ids: ["rel_early", "rel_late"] }),
  pending("unknown", "future_kind", { kind: "future_kind", message: "未知完整内容" }),
] satisfies PendingReviewItem[];

describe("groupPending", () => {
  it("groups the six real backend payloads and sends unknown kinds to the final fallback", () => {
    const items = [...pendingFixtures].reverse();
    const grouped = groupPending(items);
    expect(grouped.map((group) => group.label)).toEqual([
      "身份冲突",
      "待确认关系",
      "时间冲突",
      "证据已失效",
      "拆分冲突",
      "文档错误",
      "其他",
    ]);
    expect(grouped.flatMap((group) => group.items.map((item) => item.id))).toEqual([
      "identity", "review", "time", "stale", "split", "invalid", "unknown",
    ]);
    expect(groupPending([...items].reverse())).toEqual(grouped);
    expect(items[0]?.id).toBe("unknown");
  });

  it("derives honest navigation and direct-action capability from real payload fields", () => {
    expect(pendingReviewModel(pendingFixtures[1]!)).toMatchObject({
      kind: "identity_conflict",
      noteId: "note_identity",
      localEntityId: "local_alice",
      candidateEntityIds: ["kg_alice", "kg_alicia"],
      relationIds: [],
      canConfirm: false,
    });
    expect(pendingReviewModel(pendingFixtures[2]!)).toMatchObject({
      kind: "stale_evidence",
      noteId: "note_stale",
      evidenceId: "ev_stale",
      relationIds: ["rel_stale"],
      canConfirm: false,
    });
    expect(pendingReviewModel(pendingFixtures[4]!)).toMatchObject({
      relationIds: ["rel_review"],
      canConfirm: true,
    });
    expect(pendingReviewModel(pendingFixtures[5]!)).toMatchObject({
      relationIds: ["rel_early", "rel_late"],
      canConfirm: false,
    });
  });

  it("later is session-only and never invokes an API", () => {
    const mutation = vi.fn();
    const items = pendingFixtures.slice(0, 2);
    expect(pendingAfterLater(items, new Set(["invalid"]))).toEqual([items[1]]);
    expect(mutation).not.toHaveBeenCalled();
  });

  it("never produces shortened visible content", () => {
    const grouped = groupPending([pending("long", "future_kind", { message: "完整待整理内容" })]);
    const serialized = JSON.stringify(grouped);
    expect(serialized).not.toContain("…");
    expect(serialized).not.toMatch(/\.\.\.(?:"|$)/);
  });
});

describe("governance UI source contract", () => {
  const sources = import.meta.glob(["./*.svelte", "../routes/graph/+page.svelte"], {
    eager: true,
    query: "?raw",
    import: "default",
  }) as Record<string, string>;
  const source = (name: string) => {
    const value = sources[name];
    if (value === undefined) throw new Error(`Missing source fixture: ${name}`);
    return value;
  };

  it("keeps the graph canvas mounted beside query-driven inspectors", () => {
    const route = source("../routes/graph/+page.svelte");
    expect(route).toContain('class="canvas-shell"');
    expect(route).toContain("<ForceGraph");
    expect(route).toContain("<EntityGovernance");
    expect(route).not.toMatch(/\{#if selected[^}]*\}[\s\S]{0,500}<ForceGraph/);
  });

  it("keeps valid governance actions available without a persistent pending entry", () => {
    const entity = source("./EntityGovernance.svelte");
    const relation = source("./RelationDrawer.svelte");
    const pendingPanel = source("./PendingReviewPanel.svelte");
    const sidebar = source("./Sidebar.svelte");
    for (const label of ["重命名实体", "添加别名", "合并实体", "拆分证据", "新建关系", "关联会议搭子"]) {
      expect(entity).toContain(label);
    }
    for (const label of ["确认关系", "保存关系修改", "结束关系", "否决并抑制", "撤销上次操作"]) {
      expect(relation).toContain(label);
    }
    for (const label of ["确认关系", "查看关系", "否决并抑制", "稍后处理"]) {
      expect(pendingPanel).toContain(label);
    }
    expect(sidebar).not.toContain("待整理");
  });

  it("shows note-level evidence in ordinary details and reserves mention offsets for governance", () => {
    const entity = source("./EntityGovernance.svelte");
    expect(entity).toContain('class="source-notes"');
    expect(entity).toContain('{simple ? "关联笔记" : "证据"}');
    expect(entity).toContain("listNotes().catch(() => [])");
    expect(entity).toContain("noteTitles.get(noteId)?.trim() || `笔记 ${noteId}`");
    expect(entity).toContain("noteStartedAt = new Map(notes.map((note) => [note.id, note.started_at]))");
    expect(entity).toContain("noteDurations = new Map(notes.map((note) => [note.id, note.duration_secs]))");
    expect(entity).toContain("formatDate(startedAt)");
    expect(entity).toContain("formatDuration(noteDurations.get(noteId) ?? null)");
    expect(entity).toContain("<span>{group.title}</span>");
    expect(entity).toContain('{#if group.time && group.duration}<span aria-hidden="true">·</span>{/if}');
    expect(entity).toContain(".source-notes .note-link { display: flex; align-items: baseline; justify-content: space-between;");
    expect(entity).toContain("{#if simple}");
    expect(entity).toContain("{#each group.items as mention (mention.id)}");
    expect(entity).toContain("第 {mention.paragraph_index + 1} 段 · 字符 {mention.start_offset}–{mention.end_offset}");
    expect(entity.indexOf('class="source-notes"')).toBeLessThan(entity.indexOf("{#each group.items as mention (mention.id)}"));
  });

  it("dispatches pending actions by real kind without fabricating relation triples", () => {
    const pendingPanel = source("./PendingReviewPanel.svelte");
    for (const kind of ["invalid_document", "identity_conflict", "stale_evidence", "split_conflict", "relation_review", "time_conflict"]) {
      expect(pendingPanel).toContain(kind);
    }
    expect(pendingPanel).toContain("candidateEntityIds");
    expect(pendingPanel).toContain("encodeURIComponent(candidateId)");
    expect(pendingPanel).not.toContain("payload?.subject_id");
    expect(pendingPanel).not.toContain("payload?.predicate_type");
  });

  it("keeps tombstone mutation feedback and undo available without claiming complete history", () => {
    const relation = source("./RelationDrawer.svelte");
    expect(relation).toContain("lastKnown");
    expect(relation).toContain("semanticEntityDetail");
    expect(relation).toContain("接口仅返回所选单条关系");
    expect(relation).toContain("{#if controller.lastOperationId}");
    expect(relation).not.toContain("restoreOperationId");
    expect(relation).not.toContain("当前索引没有这条关系的历史版本");
  });

  it("includes explicit no-evidence copy and accessibility markers", () => {
    const split = source("./EntitySplitDialog.svelte");
    const relation = source("./RelationDrawer.svelte");
    const route = source("../routes/graph/+page.svelte");
    expect(split).toContain("<dialog");
    expect(split).toContain("showModal()");
    expect(split).toContain("aria-live");
    expect(split).toContain("撤销本次拆分");
    expect(relation).toContain("用户直接声明");
    expect(relation).toContain("aria-live");
    expect(route).toMatch(/\.key\s*!==?\s*"Escape"|\.key\s*===?\s*"Escape"/);
    for (const content of [split, relation, route]) {
      expect(content).not.toContain("line-clamp");
      expect(content).not.toContain("text-overflow: ellipsis");
    }
  });

  it("explains why relation filters and backfill actions have no candidates", () => {
    const toolbar = source("./KnowledgeGraphToolbar.svelte");
    const backfill = source("./RelationBackfillDialog.svelte");
    expect(toolbar).toContain("尚无语义关系类型。完成关系补建后可在这里筛选。");
    expect(backfill).toContain("关系已是最新，或笔记尚未形成可用的实体上下文");
  });

  it("lets users leave the modal after a backfill cancel request is accepted", () => {
    const backfill = source("./RelationBackfillDialog.svelte");
    expect(backfill).toContain('busy && state.phase !== "cancel-requested"');
    expect(backfill).toContain("当前模型请求结束或超时后会停止，结果不会写入。你可以关闭窗口继续使用应用。");
    expect(backfill).toContain('<button class="secondary" type="button" onclick={closeDialog}>关闭窗口</button>');
    expect(backfill).not.toContain('<button class="secondary" type="button" disabled>等待取消</button>');
  });
});

describe("relation drawer tombstone state", () => {
  const relation = {
    relation: {
      id: "rel_a",
      subject_id: "kg_a",
      object_id: "kg_b",
      predicate_type: "uses",
      predicate_label: null,
      status: "current" as const,
      confidence: 1,
      origin: "confirmed" as const,
      evidence_count: 0,
      note_count: 0,
      valid_from: null,
      valid_to: null,
    },
    provider: null,
    model: null,
    note_ids: [],
    evidence: [],
  };

  it("retains the last relation through suppress -> null and replaces it after undo reload", () => {
    const beforeSuppress = retainLastKnownRelation(relation, null);
    const tombstone = retainLastKnownRelation(null, beforeSuppress);
    const restored = { ...relation, relation: { ...relation.relation, origin: "manual" as const } };
    const afterUndo = retainLastKnownRelation(restored, tombstone);

    expect(tombstone).toBe(relation);
    expect(afterUndo).toBe(restored);
  });
});
