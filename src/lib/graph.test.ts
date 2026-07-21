import { describe, expect, it, vi } from "vitest";
import { kindLabel } from "./graph";
import {
  applyKnowledgeOperation,
  cancelRelationBackfill,
  entityMentions,
  knowledgePath,
  mergeEntities,
  pendingReview,
  previewRelationBackfill,
  relationDetail,
  semanticEntityDetail,
  semanticGraph,
  splitEntity,
  startRelationBackfill,
  undoKnowledgeOperation,
  type BackfillProgress,
  type BackfillRequest,
  type KnowledgeFilter,
  type KnowledgeMutationResult,
  type KnowledgeOperationInput,
} from "./knowledge";

const { invokeMock } = vi.hoisted(() => ({ invokeMock: vi.fn() }));

vi.mock("@tauri-apps/api/core", () => ({
  invoke: invokeMock,
}));

if (false) {
  const invalidProvider: BackfillRequest = {
    note_ids: null,
    // @ts-expect-error Backfill providers are the two backend-supported executors.
    provider: "http",
  };
  const invalidBackfillState: BackfillProgress = {
    // @ts-expect-error Backfill progress exposes only terminal/running backend states.
    state: "done",
    completed: 0,
    total: 0,
    current_note_id: null,
    failed: [],
  };
  const invalidRebuildState: KnowledgeMutationResult = {
    operation_id: "op_1",
    entity_id: null,
    // @ts-expect-error Public mutation commands return only the post-scheduling queued state.
    rebuild_state: "committed",
  };
  void [invalidProvider, invalidBackfillState, invalidRebuildState];
}

describe("kindLabel", () => {
  it("已知 kind 给中文标签", () => {
    expect(kindLabel("person")).toBe("人");
    expect(kindLabel("org")).toBe("组织");
    expect(kindLabel("project")).toBe("项目");
    expect(kindLabel("product")).toBe("产品");
    expect(kindLabel("term")).toBe("术语");
    expect(kindLabel("decision")).toBe("决议");
    expect(kindLabel("task")).toBe("任务");
    expect(kindLabel("place")).toBe("地点");
    expect(kindLabel("date")).toBe("日期");
  });
  it("未知 kind 原样返回(前向兼容,不吞新类型)", () => {
    expect(kindLabel("tool")).toBe("tool");
    expect(kindLabel("")).toBe("");
  });
});

describe("semantic graph invoke wrappers", () => {
  const filter: KnowledgeFilter = {
    entity_kinds: ["person"],
    predicate_types: ["responsible_for"],
    from: null,
    to: null,
    include_history: false,
    include_cooccurrence: false,
  };

  it("uses exact command names and camelCase Tauri argument names", async () => {
    invokeMock.mockResolvedValue(undefined);
    await semanticGraph(filter);
    await semanticEntityDetail("kg_a", filter);
    await relationDetail("rel_a");
    await pendingReview(filter);
    await entityMentions("kg_a");
    await knowledgePath("kg_a", "kg_b", filter);

    expect(invokeMock.mock.calls).toEqual([
      ["semantic_graph", { filter }],
      ["semantic_entity_detail", { entityId: "kg_a", filter }],
      ["relation_detail", { relationId: "rel_a" }],
      ["pending_review", { filter }],
      ["entity_mentions", { entityId: "kg_a" }],
      ["shortest_path", { start: "kg_a", end: "kg_b", filter }],
    ]);
  });

  it("keeps operation and split requests nested and merge/undo IDs camelCase", async () => {
    invokeMock.mockReset().mockResolvedValue(undefined);
    const operation: KnowledgeOperationInput = {
      kind: "rename_entity",
      payload: { entity_id: "kg_a", name: "新名字" },
    };
    const request = {
      entity_id: "kg_a",
      name: "拆分实体",
      kind: null,
      aliases: [],
      mention_ids: ["mn_1"],
    };
    await applyKnowledgeOperation(operation);
    await splitEntity(request);
    await mergeEntities("kg_source", "kg_target");
    await undoKnowledgeOperation("op_1");

    expect(invokeMock.mock.calls).toEqual([
      ["apply_knowledge_operation", { operation }],
      ["split_entity", { request }],
      ["merge_entities", { sourceId: "kg_source", targetId: "kg_target" }],
      ["undo_knowledge_operation", { operationId: "op_1" }],
    ]);
  });

  it("uses noteIds only for preview and a nested request for start", async () => {
    invokeMock.mockReset().mockResolvedValue(undefined);
    const request = { note_ids: ["note-1"], provider: "openai" } satisfies BackfillRequest;
    await previewRelationBackfill();
    await previewRelationBackfill(["note-1"]);
    await startRelationBackfill(request);
    await cancelRelationBackfill();

    expect(invokeMock.mock.calls).toEqual([
      ["preview_relation_backfill", { noteIds: null }],
      ["preview_relation_backfill", { noteIds: ["note-1"] }],
      ["start_relation_backfill", { request }],
      ["cancel_relation_backfill"],
    ]);
  });
});

describe("KnowledgeOperationInput discriminants", () => {
  it("matches every Rust IPC operation variant", () => {
    const operations: KnowledgeOperationInput[] = [
      { kind: "rename_entity", payload: { entity_id: "a", name: "A" } },
      { kind: "add_alias", payload: { entity_id: "a", alias: "AA" } },
      { kind: "remove_alias", payload: { entity_id: "a", alias: "AA" } },
      { kind: "bind_mention", payload: { mention_id: "m", entity_id: "a" } },
      { kind: "confirm_relation", payload: { relation_id: "r" } },
      {
        kind: "edit_relation",
        payload: {
          relation_id: "r",
          subject_id: "a",
          predicate: { type: "custom", label: "推动" },
          object_id: "b",
          valid_from: null,
          valid_to: null,
          note: null,
        },
      },
      {
        kind: "suppress_relation",
        payload: { subject_id: "a", predicate: { type: "uses", label: null }, object_id: "b" },
      },
      { kind: "end_relation", payload: { relation_id: "r", valid_to: "2026-07-21T00:00:00Z" } },
      { kind: "restore_relation", payload: { operation_id: "op" } },
      { kind: "create_entity", payload: { kind: "project", name: "A", aliases: [] } },
      {
        kind: "create_relation",
        payload: {
          subject_id: "a",
          predicate: { type: "custom", label: "推动" },
          object_id: "b",
          valid_from: null,
          valid_to: null,
          note: "人工核对",
          evidence_ids: ["ev_1"],
          user_assertion: false,
        },
      },
    ];

    expect(operations.map((operation) => operation.kind)).toEqual([
      "rename_entity",
      "add_alias",
      "remove_alias",
      "bind_mention",
      "confirm_relation",
      "edit_relation",
      "suppress_relation",
      "end_relation",
      "restore_relation",
      "create_entity",
      "create_relation",
    ]);
  });
});
