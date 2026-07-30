// 精修稿 schema:refined_paragraph 自定义块(携带说话人/时间戳/origIndex)+
// entity_mention 行内 mark(实体高亮)。markdown 无对应语法,parseMarkdown 永不
// 命中(文档只由 setRefined 程序化构建);toMarkdown 按普通段落/纯文本输出,
// 保证 serializeBlock 与保存载荷可用。
import { $markSchema, $nodeSchema, $prose } from "@milkdown/kit/utils";
import { Plugin, PluginKey } from "@milkdown/kit/prose/state";
import type { Node as PMNode, Fragment } from "@milkdown/kit/prose/model";
import type { EditorView, ViewMutationRecord } from "@milkdown/kit/prose/view";
import type { MarkSerializerSpec } from "@milkdown/kit/transformer";
import { normalizeOrigIndices } from "./editorDoc";

/** 剥掉 Fragment 尾部的 hardbreak 节点(如果有)。commonmark 段落序列化靠
    preset-commonmark 的 __internal__/serializeText 做同样的事,但该辅助未从包
    入口导出(见 node_modules/@milkdown/preset-commonmark/src/index.ts,只
    re-export ./node ./mark ./plugin ./composed ./commands,不含 __internal__),
    故本地按同样语义手写一份,避免尾随 hardbreak 被序列化成多余的换行/`<br />`。 */
function dropTrailingHardbreak(content: Fragment): Fragment {
  const last = content.lastChild;
  if (last && last.type.name === "hardbreak") {
    return content.cut(0, content.size - last.nodeSize);
  }
  return content;
}

export const refinedParagraphSchema = $nodeSchema("refined_paragraph", () => ({
  content: "inline*",
  group: "block",
  defining: true,
  attrs: {
    origIndex: { default: null },
    speaker: { default: "" },
    name: { default: null },
    personId: { default: null },
    startMs: { default: 0 },
  },
  parseDOM: [{ tag: "div[data-refined-paragraph]" }],
  toDOM: () => ["div", { "data-refined-paragraph": "", class: "md-para" }, 0],
  parseMarkdown: { match: () => false, runner: () => {} },
  toMarkdown: {
    match: (node) => node.type.name === "refined_paragraph",
    runner: (state, node) => {
      state.openNode("paragraph");
      state.next(dropTrailingHardbreak(node.content));
      state.closeNode();
    },
  },
}));

/** entity_mention 的 toMarkdown 规则,单独导出以便
    refinedSchema.serialize.test.ts 用真实序列化管线(而非重新实现一份影子逻辑)
    验证不重复文本。 */
export const entityMentionToMarkdown: MarkSerializerSpec = {
  // 序列化时实体标注不落盘(mentions 生命周期由后端管),文本原样透传。
  match: (mark) => mark.type.name === "entity_mention",
  runner: (state, _mark, node) => {
    state.addNode("text", undefined, node.text ?? "");
    // 返回 true 是 prevent-default:milkdown SerializerState#runNode 里,
    // mark runner 返回真值会跳过该文本节点自身的默认 text runner
    // (node_modules/@milkdown/transformer/src/serializer/state.ts #runNode:
    // `unPreventNext = marks.every(mark => !runProseMark(...))`)。不返回 true
    // 时默认 runner 还会再输出一次同样的文本,导致带 mark 的文本被序列化两次。
    return true;
  },
};

export const entityMentionSchema = $markSchema("entity_mention", () => ({
  attrs: { entityId: { default: "" } },
  inclusive: false,
  parseDOM: [
    {
      tag: "span[data-entity-id]",
      getAttrs: (dom) => ({ entityId: (dom as HTMLElement).dataset.entityId ?? "" }),
    },
  ],
  toDOM: (mark) => [
    "span",
    { "data-entity-id": mark.attrs.entityId as string, class: "entity-mention" },
  ],
  parseMarkdown: { match: () => false, runner: () => {} },
  toMarkdown: entityMentionToMarkdown,
}));

/** Enter 分段/粘贴复制块属性 → origIndex 重复。规整:保首个,其余降级为用户新块
    (origIndex=null 且清空说话人属性)。纯判定在 editorDoc.normalizeOrigIndices。 */
export const refinedNormalizePlugin = $prose(
  () =>
    new Plugin({
      key: new PluginKey("refined-normalize"),
      appendTransaction(trs, _old, state) {
        if (!trs.some((tr) => tr.docChanged)) return null;
        const entries: { node: PMNode; pos: number; idx: number | null }[] = [];
        state.doc.forEach((node, pos) => {
          entries.push({
            node,
            pos,
            idx: node.type.name === "refined_paragraph" ? (node.attrs.origIndex as number | null) : null,
          });
        });
        const fixed = normalizeOrigIndices(entries.map((e) => e.idx));
        let tr: typeof state.tr | null = null;
        entries.forEach((e, i) => {
          if (e.node.type.name !== "refined_paragraph" || fixed[i] === e.idx) return;
          tr = (tr ?? state.tr).setNodeMarkup(e.pos, undefined, {
            ...e.node.attrs,
            origIndex: null,
            speaker: "",
            name: null,
            personId: null,
            startMs: 0,
          });
        });
        return tr;
      },
    }),
);

export type BadgeCallbacks = {
  speakerBadge: (attrs: Record<string, unknown>) => { label: string; bg: string; ink: string };
  formatTs: (ms: number) => string;
  onBadgeClick: (attrs: Record<string, unknown>, rect: DOMRect) => void;
  onPlayFrom: (startMs: number) => void;
};

/** refined_paragraph NodeView:说话人徽章 + 时间戳按钮为不可编辑前缀,正文是
    contentDOM。徽章/时间戳交互经回调抛给页面(Svelte 渲染浮层),不在 PM 里做 UI。 */
export function makeRefinedParagraphView(cb: BadgeCallbacks) {
  return (node: PMNode, _view: EditorView, _getPos: () => number | undefined) => {
    const dom = document.createElement("div");
    dom.className = "md-para";
    dom.dataset.refinedParagraph = "";
    if (node.attrs.speaker || node.attrs.name || node.attrs.personId) {
      const { label, bg, ink } = cb.speakerBadge(node.attrs);
      const badge = document.createElement("button");
      badge.type = "button";
      badge.className = "badge as-btn";
      badge.contentEditable = "false";
      badge.textContent = label;
      badge.style.background = bg;
      badge.style.color = ink;
      badge.onclick = (e) => {
        e.preventDefault();
        cb.onBadgeClick(node.attrs, badge.getBoundingClientRect());
      };
      const ts = document.createElement("button");
      ts.type = "button";
      ts.className = "ts ts-btn";
      ts.contentEditable = "false";
      ts.textContent = cb.formatTs(node.attrs.startMs as number);
      ts.title = "从此处播放";
      ts.onclick = () => cb.onPlayFrom(node.attrs.startMs as number);
      dom.append(badge, ts);
    }
    const contentDOM = document.createElement("span");
    contentDOM.className = "para-text";
    dom.append(contentDOM);
    return {
      dom,
      contentDOM,
      stopEvent: (e: Event) =>
        e.target instanceof HTMLElement && e.target.closest("button") !== null,
      ignoreMutation: (m: ViewMutationRecord) => !contentDOM.contains(m.target),
    };
  };
}
