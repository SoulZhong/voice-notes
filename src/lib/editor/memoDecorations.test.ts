// 书签装饰(会议备注)的三条硬约束,对真实 prosemirror Schema/EditorState 验证:
// (a) 挂/撤书签不改 view.state.doc,原始稿骨架锁照常生效;
// (b) 修订稿保存载荷(collectBlocks 的口径:doc.forEach 顶层块)不含书签;
// (c) 放置规则:挂在最后一个 start_ms <= at_ms 的块之后,无时间块不推进。
// node 环境无 DOM:widget 的 toDOM 是惰性的,构建 DecorationSet 不会调用它。
import { describe, expect, it } from "vitest";
import { Schema, type Node as PMNode } from "@milkdown/kit/prose/model";
import { EditorState, Plugin, PluginKey, TextSelection } from "@milkdown/kit/prose/state";
import type { DecorationSet } from "@milkdown/kit/prose/view";
import type { Memo } from "../memos";
import { docSkeleton } from "./segmentSchema";
import { refinedSavePayload, sameSegmentSkeleton, type EditedBlock } from "./editorDoc";
import {
  blockStarts,
  buildMemoDecorations,
  memoBookmarkPlugin,
  memoPlacements,
  type MemoDecoInput,
} from "./memoDecorations";

const schema = new Schema({
  nodes: {
    doc: { content: "block+" },
    transcript_segment: {
      content: "text*",
      group: "block",
      attrs: { seq: { default: 0 }, source: { default: "mic" }, speaker: { default: null }, startMs: { default: 0 } },
    },
    refined_paragraph: {
      content: "text*",
      group: "block",
      attrs: {
        origIndex: { default: null },
        speaker: { default: "" },
        name: { default: null },
        personId: { default: null },
        startMs: { default: 0 },
      },
    },
    paragraph: { content: "text*", group: "block" },
    text: { group: "inline" },
  },
} as never);

const seg = (seq: number, startMs: number, text = `段${seq}`) =>
  schema.nodes.transcript_segment!.createChecked({ seq, speaker: "S1", startMs }, [schema.text(text)]);
const para = (origIndex: number | null, startMs: number, speaker = "S1", text = "正文") =>
  schema.nodes.refined_paragraph!.createChecked({ origIndex, speaker, startMs }, [schema.text(text)]);
const memo = (id: string, at_ms: number, text = ""): Memo => ({
  id,
  at_ms,
  text,
  origin: "note",
  created_at: `2026-11-03T00:00:0${id.length}Z`,
  updated_at: "2026-11-03T00:00:00Z",
});

const lock = new Plugin({
  key: new PluginKey("segment-lock-test"),
  // 与 segmentSchema.segmentLockPlugin 同一判据(那个是 milkdown $prose 包装,这里取其内核)
  filterTransaction(tr, state) {
    if (!tr.docChanged || tr.getMeta("external-load")) return true;
    return sameSegmentSkeleton(docSkeleton(state.doc), docSkeleton(tr.doc));
  },
});

function withMemos(doc: PMNode, initial: Memo[]) {
  let input: MemoDecoInput = { version: 1, memos: initial, cuts: [], draft: null };
  const plugin = memoBookmarkPlugin(
    () => input,
    () => {
      throw new Error("node 环境不该渲染 widget DOM");
    },
  );
  const state = EditorState.create({ doc, plugins: [lock, plugin] });
  const decos = () => plugin.props.decorations!.call(plugin, state) as DecorationSet;
  const set = (memos: Memo[]) => {
    input = { ...input, version: input.version + 1, memos };
  };
  return { state, plugin, decos, set };
}

describe("书签不进文档", () => {
  it("(a) 挂上/撤掉书签前后 doc 完全相同,插件不带事务钩子", () => {
    const doc = schema.node("doc", null, [seg(1, 0), seg(2, 5000), seg(3, 9000)]);
    const h = withMemos(doc, []);
    const before = h.state.doc;
    expect(h.decos().find().length).toBe(0);
    h.set([memo("a", 6000, "定了"), memo("b", 100)]);
    expect(h.decos().find().length).toBe(2);
    h.set([]);
    expect(h.decos().find().length).toBe(0);
    expect(h.state.doc).toBe(before);
    expect(h.state.doc.childCount).toBe(3);
    // 插件只有 decorations:没有 state 字段,不过滤/追加事务
    expect(h.plugin.spec.state).toBeUndefined();
    expect(h.plugin.spec.filterTransaction).toBeUndefined();
    expect(h.plugin.spec.appendTransaction).toBeUndefined();
  });

  it("(a) 有书签时骨架锁照常:插入非段节点被拒,段内改字放行", () => {
    const doc = schema.node("doc", null, [seg(1, 0), seg(2, 5000)]);
    const h = withMemos(doc, [memo("a", 6000, "定了")]);
    void h.decos();
    const bad = h.state.tr.insert(h.state.doc.content.size, schema.nodes.paragraph!.createChecked());
    expect(h.state.applyTransaction(bad).state.doc).toBe(h.state.doc); // 被 filterTransaction 拒
    const ok = h.state.tr.insertText("改", 2);
    const after = h.state.applyTransaction(ok).state;
    expect(after.doc).not.toBe(h.state.doc);
    expect(docSkeleton(after.doc)).toEqual(docSkeleton(h.state.doc));
  });

  it("(b) 修订稿保存载荷(按顶层块收集)不含书签", () => {
    const doc = schema.node("doc", null, [para(0, 0, "S1", "甲"), para(1, 4000, "S2", "乙")]);
    const collect = (d: PMNode): EditedBlock[] => {
      const out: EditedBlock[] = [];
      d.forEach((n) => out.push({ origIndex: n.attrs.origIndex as number | null, markdown: n.textContent }));
      return out;
    };
    const refined = {
      revision: 1,
      paragraphs: [
        { speaker: "S1", start_ms: 0, end_ms: 4000, text: "甲" },
        { speaker: "S2", start_ms: 4000, end_ms: 8000, text: "乙" },
      ],
    } as never;
    const without = refinedSavePayload(refined, collect(doc), new Map());
    const h = withMemos(doc, [memo("a", 100, "备注一"), memo("b", 5000, "备注二")]);
    expect(h.decos().find().length).toBe(2);
    const withMemo = refinedSavePayload(refined, collect(h.state.doc), new Map());
    expect(withMemo).toEqual(without);
    expect(JSON.stringify(withMemo)).not.toContain("备注");
  });

  it("文档变化后装饰按新 doc 现算(全量重载后自动重新挂上)", () => {
    const doc = schema.node("doc", null, [seg(1, 0), seg(2, 5000)]);
    const h = withMemos(doc, [memo("a", 6000)]);
    const tr = h.state.tr
      .replaceWith(0, h.state.doc.content.size, [seg(1, 0), seg(2, 3000), seg(3, 7000)])
      .setMeta("external-load", true);
    const next = h.state.applyTransaction(tr).state;
    const set = h.plugin.props.decorations!.call(h.plugin, next) as DecorationSet;
    const [d] = set.find();
    // 6000 落在 seg2(3000) 之后、seg3(7000) 之前
    expect(d.from).toBe(next.doc.child(0).nodeSize + next.doc.child(1).nodeSize);
  });
});

describe("(c) 放置规则", () => {
  const doc = schema.node("doc", null, [seg(1, 1000), seg(2, 5000), seg(3, 9000)]);
  const ends = [doc.child(0).nodeSize, doc.child(0).nodeSize + doc.child(1).nodeSize, doc.content.size];

  it("挂在最后一个 start_ms <= at_ms 的块之后;早于所有块放文首;晚于所有块放文末", () => {
    const p = memoPlacements(doc, [memo("late", 99000), memo("mid", 5000), memo("early", 0), memo("x", 6500)]);
    expect(p.map((x) => [x.item.kind === "memo" && x.item.memo.id, x.after, x.pos])).toEqual([
      ["early", -1, 0],
      ["mid", 1, ends[1]], // 恰等于块起点 → 挂在该块之后
      ["x", 1, ends[1]],
      ["late", 2, ends[2]],
    ]);
  });

  it("无时间跨度的块(修订稿自由块/新块、占位段落)不推进游标", () => {
    const d = schema.node("doc", null, [
      para(null, 0, "", "标题自由块"),
      para(0, 2000),
      para(null, 0, "", "中间自由块"),
      para(1, 8000),
      schema.nodes.paragraph!.createChecked(),
    ]);
    expect(blockStarts(d)).toEqual([null, 2000, null, 8000, null]);
    const [m] = memoPlacements(d, [memo("a", 5000)]);
    expect(m.after).toBe(1); // 挂在 para(2000) 之后,而不是中间自由块之后
    const [first] = memoPlacements(d, [memo("b", 500)]);
    expect(first.after).toBe(-1);
    expect(first.pos).toBe(0);
  });

  it("修订稿首段从 0 开始仍算有时间(有说话人即有时间跨度)", () => {
    const d = schema.node("doc", null, [para(0, 0), para(1, 4000)]);
    expect(blockStarts(d)).toEqual([0, 4000]);
    expect(memoPlacements(d, [memo("a", 1000)])[0].after).toBe(0);
  });

  it("删减区间内的备注带 cut 标;草稿排在同刻备注之后", () => {
    const p = memoPlacements(doc, [memo("a", 5000), memo("b", 6000)], [{ start_ms: 5500, end_ms: 7000 }], {
      at_ms: 5000,
      seq: 1,
    });
    expect(p.map((x) => (x.item.kind === "memo" ? `${x.item.memo.id}:${x.item.cut}` : "draft"))).toEqual([
      "a:false",
      "draft",
      "b:true",
    ]);
  });

  it("widget 选项:side -1、ignoreSelection、stopEvent 恒 true", () => {
    const set = buildMemoDecorations(doc, { version: 1, memos: [memo("a", 5000)], cuts: [], draft: null }, () => {
      throw new Error("unused");
    });
    const [d] = set.find();
    const spec = d.spec as { side: number; ignoreSelection: boolean; stopEvent: (e: Event) => boolean };
    expect(spec.side).toBe(-1);
    expect(spec.ignoreSelection).toBe(true);
    expect(spec.stopEvent({} as Event)).toBe(true);
    // 选区可以照常落在书签两侧的块里
    const st = EditorState.create({ doc });
    expect(() => TextSelection.create(st.doc, d.from + 1)).not.toThrow();
  });
});

describe("MarkdownEditor 接线(源码守卫)", () => {
  const src = import.meta.glob("./MarkdownEditor.svelte", { eager: true, query: "?raw", import: "default" }) as Record<
    string,
    string
  >;
  const editor = src["./MarkdownEditor.svelte"];
  it("书签刷新只用 setProps,不发事务", () => {
    const start = editor.indexOf("// 书签数据变化");
    expect(start).toBeGreaterThan(-1);
    const body = editor.indexOf("$effect(() => {", start);
    const effect = editor.slice(body, editor.indexOf("</script>", body));
    expect(effect).toContain("setProps({})");
    expect(effect).not.toContain("dispatch");
  });
  it("装饰插件读非响应式镜像,不读 prop", () => {
    expect(editor).toMatch(/memoBookmarkPlugin\(\s*\(\)\s*=>\s*memoInput/);
    expect(editor).toMatch(/onAction:\s*\(a\)\s*=>\s*memoActionNow\?\.\(a\)/);
  });
});
