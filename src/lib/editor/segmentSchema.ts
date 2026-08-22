// 原始稿 schema:transcript_segment 自定义块,一段=一节点,attrs 锁定段身份。
// 结构锁定:任何改变段骨架(段数/顺序/seq/speaker)的事务一律拒绝——增删段与
// 改说话人只走命令按钮(delete_segment/set_segment_speaker),键盘只能改段内文本。
import { $nodeSchema, $prose } from "@milkdown/kit/utils";
// NodeView 在创建它的 effect 销毁后仍被编辑器调用,必须用不建依赖的 tStatic
// (普通 t() 会触发 svelte derived_inert 并中断渲染,页面停在加载态)。
import { tStatic } from "$lib/i18n/index.svelte";
import { Plugin, PluginKey } from "@milkdown/kit/prose/state";
import type { Node as PMNode } from "@milkdown/kit/prose/model";
import type { EditorView, ViewMutationRecord } from "@milkdown/kit/prose/view";
import type { Source } from "../events";
import { sameSegmentSkeleton, type SegSkeleton } from "./editorDoc";

export const transcriptSegmentSchema = $nodeSchema("transcript_segment", () => ({
  content: "inline*",
  group: "block",
  defining: true,
  attrs: {
    seq: { default: 0 },
    source: { default: "mic" },
    speaker: { default: null },
    startMs: { default: 0 },
    /** 徽章上那行字。**它必须进 attrs**,哪怕 NodeView 自己也能算出来。
     *
     *  显示名不是段的属性,而是外部 speakers 映射算出来的:同一个 `speaker: "S4"`,
     *  在关联到声纹库人物之前显示「新说话人 4」、之后显示「刘光浚」。而 ProseMirror
     *  判定节点是否变化只看 type/attrs/content——三者全同就 `node.eq()`,DOM 更新算法
     *  直接复用旧 NodeView,既不重建也不调 update()。于是**即便整份 replaceWith 重建
     *  文档,徽章也永远停在第一次渲染的那个名字上**(2026-08-19 线上 bug:段落显示
     *  「新说话人 4」而顶部胶囊与改派菜单同时显示「刘光浚」,同一份数据两个答案)。
     *
     *  把显示名放进 attrs,人物一变节点就不再 eq,PM 才看得见这个变化。
     *  不参与 docSkeleton(只读 seq/speaker),因此不会被 segmentLockPlugin 误伤。 */
    badgeKey: { default: "" },
  },
  parseDOM: [{ tag: "div[data-seq]" }],
  toDOM: (node) => ["div", { "data-seq": String(node.attrs.seq), class: "md-seg" }, 0],
  parseMarkdown: { match: () => false, runner: () => {} },
  toMarkdown: {
    match: (node) => node.type.name === "transcript_segment",
    runner: (state, node) => {
      state.openNode("paragraph");
      state.next(node.content);
      state.closeNode();
    },
  },
}));

export function docSkeleton(doc: PMNode): SegSkeleton[] {
  const out: SegSkeleton[] = [];
  doc.forEach((n) => {
    if (n.type.name === "transcript_segment") {
      out.push({ seq: n.attrs.seq as number, speaker: n.attrs.speaker as string | null });
    } else {
      out.push({ seq: -1, speaker: null }); // 非段节点混入 = 骨架已破坏,必然拒绝
    }
  });
  return out;
}

export const segmentLockPlugin = $prose(
  () =>
    new Plugin({
      key: new PluginKey("segment-lock"),
      filterTransaction(tr, state) {
        if (!tr.docChanged || tr.getMeta("external-load")) return true;
        return sameSegmentSkeleton(docSkeleton(state.doc), docSkeleton(tr.doc));
      },
    }),
);

export type SegmentViewCallbacks = {
  speakerBadge: (attrs: Record<string, unknown>) => { label: string; bg: string; ink: string };
  formatTs: (ms: number) => string;
  canEdit: () => boolean;
  onBadgeClick: (seq: number, speaker: string | null, source: Source, rect: DOMRect, shiftKey: boolean) => void;
  onPlayFrom: (startMs: number) => void;
  onDeleteClick: (seq: number, rect: DOMRect) => void;
};

/** 段 NodeView:徽章(点击→说话人菜单浮层)、时间戳(跳播)、正文 contentDOM、
    行尾删除按钮(点击→页面确认浮层)。data-seq 保留给播放高亮/滚动跟随定位。 */
export function makeSegmentView(cb: SegmentViewCallbacks) {
  return (node: PMNode, _view: EditorView, _getPos: () => number | undefined) => {
    // 事件回调读的是**这个**引用而不是构造参数 node:update() 换过节点之后,
    // 点击必须按新节点的 seq/speaker 派发,否则改派菜单会带着旧身份开出来。
    let cur = node;
    const dom = document.createElement("div");
    dom.className = "md-seg";
    dom.dataset.seq = String(node.attrs.seq);
    const { label, bg, ink } = cb.speakerBadge(node.attrs);
    const badge = document.createElement("button");
    badge.type = "button";
    badge.className = "badge as-btn";
    badge.contentEditable = "false";
    badge.textContent = label;
    badge.style.background = bg;
    badge.style.color = ink;
    badge.disabled = !cb.canEdit();
    badge.title = cb.canEdit() ? tStatic("notes.editor.changeSpeaker") : "";
    badge.onclick = (e) => {
      e.preventDefault();
      if (cb.canEdit())
        cb.onBadgeClick(
          cur.attrs.seq as number,
          cur.attrs.speaker as string | null,
          cur.attrs.source as Source,
          badge.getBoundingClientRect(),
          e.shiftKey,
        );
    };
    const ts = document.createElement("button");
    ts.type = "button";
    ts.className = "ts ts-btn";
    ts.contentEditable = "false";
    ts.title = tStatic("notes.editor.playFromHere");
    ts.textContent = cb.formatTs(node.attrs.startMs as number);
    ts.onclick = () => cb.onPlayFrom(cur.attrs.startMs as number);
    const content = document.createElement("span");
    content.className = "seg-text";
    const actions = document.createElement("span");
    actions.className = "seg-actions";
    actions.contentEditable = "false";
    if (cb.canEdit()) {
      const del = document.createElement("button");
      del.type = "button";
      del.className = "link";
      del.textContent = tStatic("notes.editor.delete");
      del.onclick = () => cb.onDeleteClick(cur.attrs.seq as number, del.getBoundingClientRect());
      actions.append(del);
    }
    dom.append(badge, ts, content, actions);
    return {
      dom,
      contentDOM: content,
      /** 就地更新,不重建 DOM。**没有这个方法,徽章就永远是构造时那一份**——
       *  PM 对没有 update() 的 NodeView 只会整只销毁重建,而节点 attrs 不变时
       *  它连重建都不做(见 badgeKey 的说明)。
       *
       *  返回 false = 交还给 PM 重建(类型或段身份变了,不是"同一段的新版本")。
       *  contentDOM 原样保留:正文由 PM 自己 diff,这里只管徽章与时间戳。 */
      update: (next: PMNode) => {
        if (next.type !== cur.type || next.attrs.seq !== cur.attrs.seq) return false;
        cur = next;
        const b = cb.speakerBadge(next.attrs);
        badge.textContent = b.label;
        badge.style.background = b.bg;
        badge.style.color = b.ink;
        badge.disabled = !cb.canEdit();
        badge.title = cb.canEdit() ? tStatic("notes.editor.changeSpeaker") : "";
        ts.textContent = cb.formatTs(next.attrs.startMs as number);
        return true;
      },
      stopEvent: (e: Event) =>
        e.target instanceof HTMLElement && e.target.closest("button") !== null,
      ignoreMutation: (m: ViewMutationRecord) => !content.contains(m.target),
    };
  };
}
