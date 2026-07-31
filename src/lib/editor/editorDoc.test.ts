import { describe, expect, it } from "vitest";
import type { RefinedDoc } from "../notes";
import {
  isBlockTextEmpty,
  normalizeOrigIndices,
  rebaseQueuedRefinedSave,
  refinedSavePayload,
  refinedToBlocks,
  sameSegmentSkeleton,
  savedBaseline,
  savedIndexRemap,
  segmentCommitDecision,
} from "./editorDoc";

function doc(partial: Partial<RefinedDoc> = {}): RefinedDoc {
  return {
    schema_version: 2,
    generated_at: "2026-07-30T00:00:00Z",
    stages: { filter: "done", recluster: "done", llm: "done" },
    discarded_seqs: [],
    paragraphs: [],
    revision: 3,
    ...partial,
  };
}

describe("refinedToBlocks", () => {
  it("有 live mention 的段产出 runs,无 mention 的段产出 markdown", () => {
    const d = doc({
      paragraphs: [
        {
          speaker: "R1", start_ms: 0, end_ms: 1000, source_seqs: [1],
          text: "张三在会上发言",
          mentions: [{ id: "m1", entity: "P1", start: 0, end: 2 }],
        },
        { speaker: "R2", start_ms: 1000, end_ms: 2000, source_seqs: [2], text: "无实体段落" },
      ],
    });
    const blocks = refinedToBlocks(d);
    expect(blocks[0].kind).toBe("runs");
    expect(blocks[0].runs).toEqual([
      { text: "张三", entityId: "P1" },
      { text: "在会上发言", entityId: null },
    ]);
    expect(blocks[0].origIndex).toBe(0);
    expect(blocks[0].speaker).toBe("R1");
    expect(blocks[1].kind).toBe("markdown");
    expect(blocks[1].markdown).toBe("无实体段落");
  });

  it("graph_support_mentions 中的 mention 被过滤(不产出实体 run)", () => {
    const d = doc({
      graph_support_mentions: ["m1"],
      paragraphs: [{
        speaker: "R1", start_ms: 0, end_ms: 1, source_seqs: [1],
        text: "张三在会上发言",
        mentions: [{ id: "m1", entity: "P1", start: 0, end: 2 }],
      }],
    });
    expect(refinedToBlocks(d)[0].kind).toBe("markdown");
  });
});

describe("refinedSavePayload", () => {
  const base = doc({
    paragraphs: [
      { speaker: "R1", start_ms: 0, end_ms: 1, source_seqs: [1], text: "第一段" },
      { speaker: "R2", start_ms: 1, end_ms: 2, source_seqs: [2], text: "第二段" },
    ],
  });
  const baseline = new Map([[0, "第一段"], [1, "第二段"]]);

  it("未改动的段 dirty=false,文本取当前序列化结果", () => {
    const p = refinedSavePayload(base, [
      { origIndex: 0, markdown: "第一段" },
      { origIndex: 1, markdown: "第二段" },
    ], baseline);
    expect(p.revision).toBe(3);
    expect(p.paragraphs).toEqual([
      { orig_index: 0, text: "第一段", dirty: false },
      { orig_index: 1, text: "第二段", dirty: false },
    ]);
  });

  it("文本变化的段与新插入块 dirty=true;空白块被丢弃", () => {
    const p = refinedSavePayload(base, [
      { origIndex: 0, markdown: "改过的第一段" },
      { origIndex: null, markdown: "## 新标题" },
      { origIndex: null, markdown: "   " },
      { origIndex: 1, markdown: "第二段" },
    ], baseline);
    expect(p.paragraphs).toEqual([
      { orig_index: 0, text: "改过的第一段", dirty: true },
      { orig_index: null, text: "## 新标题", dirty: true },
      { orig_index: 1, text: "第二段", dirty: false },
    ]);
  });

  it("revision 缺省按 0", () => {
    const d = doc({ paragraphs: [] });
    delete (d as any).revision;
    expect(refinedSavePayload(d, [], new Map()).revision).toBe(0);
  });
});

// markSaved 的 in-flight 分支(Round 3 Important 2):invoke 往返期间用户继续输入时,
// 保存后的段序只能从**已发送的载荷**推导,不能按当前文档顺序顺次编号——窗口内 Enter
// 新增的块会被编进服务端不存在的下标,下次保存被"orig_index 越界"拒绝,而该错误串
// 不含"已在别处更新",宿主不走重载分支,markSaveFailed 每 2s 重试同一份毒载荷。
describe("savedIndexRemap / savedBaseline", () => {
  const sent = [
    { orig_index: null, text: "## 新标题", dirty: true },
    { orig_index: 0, text: "第一段", dirty: false },
    { orig_index: 2, text: "改过的第三段", dirty: true },
  ];

  it("载荷第 i 段即盘上第 i 段:old origIndex → i;新块不进映射", () => {
    expect([...savedIndexRemap(sent)]).toEqual([
      [0, 1],
      [2, 2],
    ]);
  });

  it("载荷里没出现的 origIndex 查不到(调用方据此把该块置 null)", () => {
    // 原第 1 段在 T0 是空块 → 被载荷构建器丢弃 → 盘上已不存在。
    expect(savedIndexRemap(sent).get(1)).toBeUndefined();
  });

  it("基线取载荷文本(= 盘上现值),窗口内改过的段下一轮才会被判成 dirty", () => {
    const baseline = savedBaseline(sent);
    expect([...baseline]).toEqual([
      [0, "## 新标题"],
      [1, "第一段"],
      [2, "改过的第三段"],
    ]);
    // 重排后的文档 + 该基线 → 只有 in-flight 改过的段是 dirty(宁多 dirty 不丢字)
    const after = doc({
      revision: 4,
      paragraphs: [
        { speaker: "", start_ms: 0, end_ms: 0, source_seqs: [], text: "## 新标题" },
        { speaker: "R1", start_ms: 0, end_ms: 1, source_seqs: [1], text: "第一段" },
        { speaker: "R3", start_ms: 2, end_ms: 3, source_seqs: [3], text: "改过的第三段" },
      ],
    });
    const payload = refinedSavePayload(
      after,
      [
        { origIndex: null, markdown: "## 新标题" },
        { origIndex: 1, markdown: "第一段窗口内又改了" },
        { origIndex: 2, markdown: "改过的第三段" },
      ],
      baseline,
    );
    expect(payload.paragraphs).toEqual([
      { orig_index: null, text: "## 新标题", dirty: true },
      { orig_index: 1, text: "第一段窗口内又改了", dirty: true },
      { orig_index: 2, text: "改过的第三段", dirty: false },
    ]);
  });
});

describe("rebaseQueuedRefinedSave", () => {
  const sent = [
    { orig_index: null, text: "首轮新增标题", dirty: true },
    { orig_index: 0, text: "第一段首轮内容", dirty: true },
    { orig_index: 2, text: "第三段", dirty: false },
  ];

  it("把保存期间的后续编辑重基到首轮保存返回的新 revision 和段序", () => {
    expect(
      rebaseQueuedRefinedSave(8, sent, [
        { orig_index: null, text: "首轮新增标题", dirty: true },
        { orig_index: 0, text: "第一段离开前又改了", dirty: true },
        { orig_index: 2, text: "第三段", dirty: false },
      ]),
    ).toEqual({
      revision: 8,
      paragraphs: [
        { orig_index: null, text: "首轮新增标题", dirty: true },
        { orig_index: 1, text: "第一段离开前又改了", dirty: true },
        { orig_index: 2, text: "第三段", dirty: false },
      ],
    });
  });

  it("首轮删除后又重新输入的旧段按新段保存", () => {
    expect(
      rebaseQueuedRefinedSave(3, sent, [{ orig_index: 1, text: "第二段重新输入", dirty: true }]),
    ).toEqual({
      revision: 3,
      paragraphs: [{ orig_index: null, text: "第二段重新输入", dirty: true }],
    });
  });
});

describe("normalizeOrigIndices", () => {
  it("重复 origIndex 保首个,其余置 null;null 与唯一值原样保留", () => {
    expect(normalizeOrigIndices([0, 0, 1, null, 1])).toEqual([0, null, 1, null, null]);
  });
});

describe("isBlockTextEmpty", () => {
  it("空白 textContent(含纯空格/换行)判空", () => {
    expect(isBlockTextEmpty("")).toBe(true);
    expect(isBlockTextEmpty("   ")).toBe(true);
    expect(isBlockTextEmpty("\n\t ")).toBe(true);
  });

  it("非空文本判非空", () => {
    expect(isBlockTextEmpty("张三")).toBe(false);
    expect(isBlockTextEmpty("  张三  ")).toBe(false);
  });

  // Round 2 Critical 1 回归:MarkdownEditor.svelte 的 collectBlocks(保存载荷)
  // 与 markSaved(origIndex 重排)必须共用同一个"块是否为空"判定,否则会对
  // 同一份文档数出不同的块集合,origIndex 错位(orig_index 越界/元数据错配)。
  // 这里用 isBlockTextEmpty 模拟两处共用的 blockMarkdown 逻辑,证明:即使
  // "序列化器输出"是 truthy 的 "<br />" 字面量(commonmark 对空 paragraph 的
  // 真实行为,见 node_modules/@milkdown/preset-commonmark/src/node/paragraph.ts
  // `state.addNode('html', undefined, '<br />')`),只要 PM 节点的 textContent
  // 判空,判定就必须记为 "",refinedSavePayload 才会正确丢弃这个块——不能像
  // Round 1 之前的 markSaved 那样直接信任 serializer 输出的真值性。
  it("不变式:textContent 判空时不信任 serializer 的 truthy 输出(如 '<br />'),refinedSavePayload 必须丢弃该块", () => {
    const fakeSerializerOutput = "<br />"; // commonmark 空段的真实序列化输出,truthy 但语义为空
    const rawTextContent = "";
    // 模拟 blockMarkdown(ctx, node, fallbackToText):先判 textContent 是否为空,
    // 为空则直接记 "",完全不信任/不采用 serializer 的输出。
    const blockMarkdown = isBlockTextEmpty(rawTextContent) ? "" : fakeSerializerOutput;
    expect(blockMarkdown).toBe("");

    const d = doc({
      paragraphs: [{ speaker: "R1", start_ms: 0, end_ms: 1000, source_seqs: [1], text: "占位" }],
    });
    const payload = refinedSavePayload(d, [{ origIndex: 0, markdown: blockMarkdown }], new Map());
    expect(payload.paragraphs).toEqual([]);
  });
});

describe("segmentCommitDecision", () => {
  const args = {
    storedText: "原文",
    baselineMd: "原文",
    currentMd: "改后",
    currentPlain: "改后",
    reparsedMd: "改后",
  };
  it("未变或纯空白 → skip(空文本走显式删除按钮,不隐式删段)", () => {
    expect(segmentCommitDecision({ ...args, currentMd: "原文", currentPlain: "原文" })).toEqual({ kind: "skip" });
    expect(segmentCommitDecision({ ...args, currentMd: "", currentPlain: "  " })).toEqual({ kind: "skip" });
  });
  it("变化且往返稳定 → 按 markdown 提交", () => {
    expect(segmentCommitDecision(args)).toEqual({ kind: "commit", newText: "改后", roundTripOk: true });
  });
  it("往返不稳定 → 按纯文本提交(不静默改写);纯文本也没变则 skip", () => {
    expect(
      segmentCommitDecision({ ...args, currentMd: "改后*", reparsedMd: "改后\\*", currentPlain: "改后*" }),
    ).toEqual({ kind: "commit", newText: "改后*", roundTripOk: false });
    expect(
      segmentCommitDecision({ ...args, currentMd: "原文*", reparsedMd: "原文\\*", currentPlain: "原文", storedText: "原文" }),
    ).toEqual({ kind: "skip" });
  });
});

describe("sameSegmentSkeleton", () => {
  it("段数、seq、speaker 全同才为真", () => {
    const a = [{ seq: 1, speaker: "S1" }, { seq: 2, speaker: null }];
    expect(sameSegmentSkeleton(a, [{ seq: 1, speaker: "S1" }, { seq: 2, speaker: null }])).toBe(true);
    expect(sameSegmentSkeleton(a, [{ seq: 1, speaker: "S1" }])).toBe(false);
    expect(sameSegmentSkeleton(a, [{ seq: 1, speaker: "S1" }, { seq: 3, speaker: null }])).toBe(false);
    expect(sameSegmentSkeleton(a, [{ seq: 1, speaker: "S2" }, { seq: 2, speaker: null }])).toBe(false);
  });
});
