import { describe, it, expect } from "vitest";
import { splitMentions, exportFileName } from "./notes";
import type { GraphExtraction, RefinedDoc, RefinedDocV2, RelationFact } from "./notes";
import { zh as notesZh } from "./i18n/dict/notes";

// 兜底文件名走 i18n 字典(测试默认 locale=zh),断言从分片取值,不再硬编码中文。
const UNTITLED = notesZh["notes.untitled"];

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

describe("exportFileName", () => {
  it("组合标题与录音时间为文件名安全的默认名", () => {
    expect(exportFileName("周会纪要", "2026-07-16T15:30:09.894724+08:00")).toBe(
      "周会纪要-20260716-1530.md",
    );
  });
  it("清洗路径非法字符并兜底空标题", () => {
    expect(exportFileName('a/b\\c:d*e?f"g<h>i|j', "2026-07-16T15:30:09+08:00")).toBe(
      "a-b-c-d-e-f-g-h-i-j-20260716-1530.md",
    );
    expect(exportFileName("   ", "2026-07-16T15:30:09+08:00")).toBe(`${UNTITLED}-20260716-1530.md`);
  });
  it("时间解析失败时省略时间段", () => {
    expect(exportFileName("t", "not-a-date")).toBe("t.md");
  });
  it("全非法字符标题(清洗后只剩连字符)兜底「未命名」", () => {
    expect(exportFileName("///", "2026-07-16T15:30:09+08:00")).toBe(`${UNTITLED}-20260716-1530.md`);
  });
  it("剥控制字符与双向覆盖符,防换行入名与视觉欺骗", () => {
    expect(exportFileName("a\nb\u202ec", "not-a-date")).toBe("abc.md");
  });
  it("剥首部点(隐藏文件)与尾部点/空白(Windows 不允许)", () => {
    expect(exportFileName("..bashrc", "not-a-date")).toBe("bashrc.md");
    expect(exportFileName("笔记. ", "not-a-date")).toBe("笔记.md");
  });
  it("超长 CJK 标题截到 160 字节 UTF-8 边界(文件名 255 字节上限留量)", () => {
    const name = exportFileName("会".repeat(100), "2026-07-16T15:30:09+08:00");
    expect(new TextEncoder().encode(name).length).toBeLessThanOrEqual(255);
    // 160 字节 = 53 个 3 字节 CJK(159 字节),不落在半个字符中间。
    expect(name).toBe(`${"会".repeat(53)}-20260716-1530.md`);
  });
  it("Windows 保留设备名加尾缀避让(仅裸名命中时)", () => {
    expect(exportFileName("CON", "not-a-date")).toBe("CON_.md");
    expect(exportFileName("CON", "2026-07-16T15:30:09+08:00")).toBe("CON-20260716-1530.md");
  });
  it("ext 参数换扩展名(音频导出复用同一命名纪律)", () => {
    expect(exportFileName("会议", "2026-08-07T16:44:00", "m4a")).toMatch(/^会议-20260807-1644\.m4a$/);
  });
  it("ext 缺省仍是 md", () => {
    expect(exportFileName("会议", "2026-08-07T16:44:00")).toMatch(/\.md$/);
  });
});
