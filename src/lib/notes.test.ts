import { describe, it, expect } from "vitest";
import { splitMentions } from "./notes";
import type { GraphExtraction, RefinedDoc, RefinedDocV2, RelationFact } from "./notes";

const legacyGraphFixture: RefinedDoc = {
  schema_version: 1,
  generated_at: "2026-07-01T09:00:00+08:00",
  stages: { filter: "done", recluster: "done", llm: "done" },
  discarded_seqs: [],
  paragraphs: [{ speaker: "S1", start_ms: 0, end_ms: 500, text: "旧稿", source_seqs: [] }],
};

const graphWriteShape: Pick<RefinedDoc, "graph_extraction" | "relations"> = {
  graph_extraction: {
    contract_version: 2,
    provider: "test",
    model: "test-model",
    run_id: "run-1",
    generated_at: "2026-07-01T09:00:00+08:00",
    source_hash: "hash",
    mode: "full",
  } satisfies GraphExtraction,
  relations: [] satisfies RelationFact[],
};

const v2RelationsOffWriteFixture: RefinedDocV2 = {
  schema_version: 2,
  generated_at: "2026-07-21T00:00:00+08:00",
  stages: { filter: "done", recluster: "done", llm: "done", entities: "done", relations: "done" },
  discarded_seqs: [],
  graph_extraction: null,
  relations: [],
  paragraphs: [{
    speaker: "S1",
    start_ms: 0,
    end_ms: 1000,
    text: "灯塔计划启动",
    source_seqs: [7],
    mentions: [{ id: "mn_000000000000000000000000", entity: "ent_1", start: 0, end: 4 }],
  }],
};

const v2RelationWriteFixture: RefinedDocV2 = {
  ...v2RelationsOffWriteFixture,
  graph_extraction: graphWriteShape.graph_extraction!,
  relations: [{
    id: "rf_000000000000000000000000",
    subject: "ent_1",
    predicate: { type: "related_to" },
    object: "ent_2",
    subject_mentions: ["mn_000000000000000000000000"],
    object_mentions: [],
    confidence: 0.9,
    evidence: [{
      id: "ev_000000000000000000000000",
      paragraph_index: 0,
      start: 0,
      end: 4,
      quote: "灯塔计划",
      source_seqs: [7],
      source_hash: "source-hash",
    }],
  }],
};

const v2MissingMentionId: RefinedDocV2 = {
  ...v2RelationsOffWriteFixture,
  paragraphs: [{
    ...v2RelationsOffWriteFixture.paragraphs[0],
    // @ts-expect-error Schema-v2 mentions require stable IDs.
    mentions: [{ entity: "ent_1", start: 0, end: 4 }],
  }],
};

const v2MissingEvidenceIds: RefinedDocV2 = {
  ...v2RelationsOffWriteFixture,
  relations: [{
    id: "rf_000000000000000000000000",
    subject: "ent_1",
    predicate: { type: "related_to" },
    object: "ent_2",
    subject_mentions: [],
    object_mentions: [],
    confidence: 0.9,
    // @ts-expect-error Schema-v2 evidence requires its own ID and source hash.
    evidence: [{ paragraph_index: 0, start: 0, end: 4, quote: "灯塔计划", source_seqs: [7] }],
  }],
};

describe("graph type compatibility", () => {
  it("accepts a schema-v1 document without graph fields", () => {
    expect(legacyGraphFixture.graph_extraction).toBeUndefined();
    expect(legacyGraphFixture.relations).toBeUndefined();
    expect(graphWriteShape.relations).toEqual([]);
    expect(v2RelationsOffWriteFixture.graph_extraction).toBeNull();
    expect(v2RelationWriteFixture.relations).toHaveLength(1);
  });
});

describe("splitMentions", () => {
  it("splits a paragraph into plain + entity segments by char offset", () => {
    // "灯塔计划下周启动":实体在 char 0..4
    const segs = splitMentions("灯塔计划下周启动", [{ entity: "ent_1", start: 0, end: 4 }]);
    expect(segs).toEqual([
      { text: "灯塔计划", entityId: "ent_1" },
      { text: "下周启动", entityId: null },
    ]);
  });
  it("handles a mention in the middle (中英混排 char 下标)", () => {
    // "我们叫它 Lighthouse 吧":Lighthouse 在 char 5..15
    const segs = splitMentions("我们叫它 Lighthouse 吧", [{ entity: "e1", start: 5, end: 15 }]);
    expect(segs.map((s) => s.text).join("")).toBe("我们叫它 Lighthouse 吧");
    expect(segs.find((s) => s.entityId === "e1")?.text).toBe("Lighthouse");
  });
  it("empty / missing mentions → single plain segment", () => {
    expect(splitMentions("你好", [])).toEqual([{ text: "你好", entityId: null }]);
    expect(splitMentions("你好", undefined)).toEqual([{ text: "你好", entityId: null }]);
  });
  it("sorts and skips overlapping mentions without crashing", () => {
    const segs = splitMentions("ABCDEF", [
      { entity: "b", start: 3, end: 5 },
      { entity: "a", start: 0, end: 2 },
      { entity: "x", start: 1, end: 4 }, // 与 a、b 重叠 → 跳过
    ]);
    expect(segs.filter((s) => s.entityId).map((s) => s.entityId)).toEqual(["a", "b"]);
  });
  it("ignores out-of-range mentions", () => {
    expect(splitMentions("AB", [{ entity: "z", start: 0, end: 99 }])).toEqual([{ text: "AB", entityId: null }]);
  });
});
