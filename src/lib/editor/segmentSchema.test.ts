// docSkeleton 是 segmentLockPlugin.filterTransaction 用来判断"段骨架是否被
// 破坏"的唯一数据源(结构锁定的核心判据),用真实 prosemirror-model Schema 验证
// 它对段节点/非段节点混入的输出,而不是重新实现一份影子逻辑。不需要 jsdom:
// 这里只构造纯 Schema/Node,不触碰 toDOM/parseDOM。
import { describe, expect, it } from "vitest";
import { Schema } from "@milkdown/kit/prose/model";
import { docSkeleton } from "./segmentSchema";
import { sameSegmentSkeleton } from "./editorDoc";

const schema = new Schema({
  nodes: {
    doc: { content: "block+" },
    transcript_segment: {
      content: "text*",
      group: "block",
      attrs: {
        seq: { default: 0 },
        source: { default: "mic" },
        speaker: { default: null },
        startMs: { default: 0 },
      },
    },
    // 非段块类型:混入文档时骨架必然判定"已破坏"。
    paragraph: { content: "text*", group: "block" },
    text: { group: "inline" },
  },
} as never);

function seg(seq: number, speaker: string | null = null) {
  return schema.nodes.transcript_segment!.createChecked({ seq, speaker });
}

describe("docSkeleton", () => {
  it("按文档顺序提取每个段节点的 seq/speaker", () => {
    const doc = schema.node("doc", null, [seg(1, "S1"), seg(2, null), seg(3, "S2")]);
    expect(docSkeleton(doc)).toEqual([
      { seq: 1, speaker: "S1" },
      { seq: 2, speaker: null },
      { seq: 3, speaker: "S2" },
    ]);
  });

  it("非段节点混入时该位置记 { seq: -1, speaker: null }(哨兵值,必然判定骨架已破坏)", () => {
    const doc = schema.node("doc", null, [
      seg(1, "S1"),
      schema.nodes.paragraph!.createChecked(null, [schema.text("误插入的段落")]),
    ]);
    expect(docSkeleton(doc)).toEqual([
      { seq: 1, speaker: "S1" },
      { seq: -1, speaker: null },
    ]);
  });

  it("与 sameSegmentSkeleton 组合:段内文本编辑(seq/speaker 不变)骨架相同", () => {
    const before = schema.node("doc", null, [seg(1, "S1"), seg(2, "S2")]);
    const after = schema.node("doc", null, [
      schema.nodes.transcript_segment!.createChecked({ seq: 1, speaker: "S1" }, [
        schema.text("编辑后的文本"),
      ]),
      seg(2, "S2"),
    ]);
    expect(sameSegmentSkeleton(docSkeleton(before), docSkeleton(after))).toBe(true);
  });

  it("与 sameSegmentSkeleton 组合:删段/改说话人会改变骨架", () => {
    const before = schema.node("doc", null, [seg(1, "S1"), seg(2, "S2")]);
    const deleted = schema.node("doc", null, [seg(1, "S1")]);
    const speakerChanged = schema.node("doc", null, [seg(1, "S9"), seg(2, "S2")]);
    expect(sameSegmentSkeleton(docSkeleton(before), docSkeleton(deleted))).toBe(false);
    expect(sameSegmentSkeleton(docSkeleton(before), docSkeleton(speakerChanged))).toBe(false);
  });
});
