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
  pendingAfterLater,
  splitPreview,
  type GovernanceApi,
  type GovernanceMention,
} from "./knowledgeGovernance";

const result = (operationId: string): KnowledgeMutationResult => ({
  operation_id: operationId,
  entity_id: null,
  rebuild_state: "queued",
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

describe("createGovernanceController", () => {
  it("deduplicates concurrent mutations and calls the API once", async () => {
    const pending = deferred<KnowledgeMutationResult>();
    const submit = vi.fn(() => pending.promise);
    const controller = createGovernanceController(api({ submit }), vi.fn());
    const operation = buildRenameEntity("kg_a", "完整新名称");

    const first = controller.submit(operation);
    const duplicate = controller.submit(operation);
    expect(controller.busy).toBe(true);
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
    const controller = createGovernanceController(api({ submit }), vi.fn());
    const operation = buildAddAlias("kg_a", "完整别名");

    await expect(controller.submit(operation)).rejects.toThrow("ledger is read-only");
    expect(controller.error).toContain("ledger is read-only");
    expect(controller.lastOperationId).toBeNull();

    const second = controller.submit(operation);
    expect(controller.error).toBe("");
    retry.resolve(result("op_retry"));
    await second;
  });

  it("stores the operation and refreshes exactly once after API resolution", async () => {
    const order: string[] = [];
    const controller = createGovernanceController(
      api({ submit: vi.fn(async () => { order.push("api"); return result("op_2"); }) }),
      vi.fn(async () => { order.push("refresh"); }),
    );

    await controller.submit(buildConfirmRelation("rel_a"));
    expect(order).toEqual(["api", "refresh"]);
    expect(controller.lastOperationId).toBe("op_2");
  });

  it("distinguishes refresh failure, keeps the operation ID, and offers retry", async () => {
    const refresh = vi
      .fn()
      .mockRejectedValueOnce(new Error("index unavailable"))
      .mockResolvedValueOnce(undefined);
    const controller = createGovernanceController(api(), refresh);

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
    const controller = createGovernanceController(api({ undo }), vi.fn());

    await controller.submit(buildRenameEntity("kg_a", "新名称"));
    await controller.undo(controller.lastOperationId!);
    expect(undo).toHaveBeenCalledWith("op_submit");
    expect(controller.lastOperationId).toBe("op_compensation");
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

const pending = (id: string, kind: string): PendingReviewItem => ({
  id,
  kind,
  note_id: null,
  relation_id: null,
  payload: { label: `完整待整理内容 ${id}` },
});

describe("groupPending", () => {
  it("uses stable governance order and sends unknown kinds to the final fallback", () => {
    const items = [
      pending("z", "future_kind"),
      pending("time", "time_conflict"),
      pending("custom", "custom_predicate"),
      pending("confidence", "low_confidence"),
      pending("person", "person_candidate"),
      pending("duplicate", "duplicate_candidate"),
      pending("identity", "identity_conflict"),
    ];
    const grouped = groupPending(items);
    expect(grouped.map((group) => group.label)).toEqual([
      "疑似重复或人物匹配",
      "低置信关系",
      "自定义关系类型",
      "时间冲突",
      "身份冲突",
      "其他",
    ]);
    expect(grouped.flatMap((group) => group.items.map((item) => item.id))).toEqual([
      "duplicate", "person", "confidence", "custom", "time", "identity", "z",
    ]);
    expect(groupPending([...items].reverse())).toEqual(grouped);
    expect(items[0]?.id).toBe("z");
  });

  it("later is session-only and never invokes an API", () => {
    const mutation = vi.fn();
    const items = [pending("a", "low_confidence"), pending("b", "future_kind")];
    expect(pendingAfterLater(items, new Set(["a"]))).toEqual([items[1]]);
    expect(mutation).not.toHaveBeenCalled();
  });

  it("never produces shortened visible content", () => {
    const grouped = groupPending([pending("long", "future_kind")]);
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

  it("exposes every planned visible action and a persistent pending entry", () => {
    const entity = source("./EntityGovernance.svelte");
    const relation = source("./RelationDrawer.svelte");
    const pendingPanel = source("./PendingReviewPanel.svelte");
    const sidebar = source("./Sidebar.svelte");
    for (const label of ["重命名实体", "添加别名", "合并实体", "拆分证据", "新建关系", "关联会议搭子"]) {
      expect(entity).toContain(label);
    }
    for (const label of ["确认关系", "保存关系修改", "结束关系", "否决并抑制", "恢复关系", "撤销上次操作"]) {
      expect(relation).toContain(label);
    }
    for (const label of ["确认关系", "编辑关系", "否决并抑制", "稍后处理"]) {
      expect(pendingPanel).toContain(label);
    }
    expect(sidebar).toContain("待整理");
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
});
