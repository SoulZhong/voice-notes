// 原始稿 schema:transcript_segment 自定义块,一段=一节点,attrs 锁定段身份。
// 结构锁定:任何改变段骨架(段数/顺序/seq/speaker)的事务一律拒绝——增删段与
// 改说话人只走命令按钮(delete_segment/set_segment_speaker),键盘只能改段内文本。
import { $nodeSchema, $prose } from "@milkdown/kit/utils";
import { t } from "$lib/i18n/index.svelte";
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
  onBadgeClick: (seq: number, speaker: string | null, source: Source, rect: DOMRect) => void;
  onPlayFrom: (startMs: number) => void;
  onDeleteClick: (seq: number, rect: DOMRect) => void;
};

/** 段 NodeView:徽章(点击→说话人菜单浮层)、时间戳(跳播)、正文 contentDOM、
    行尾删除按钮(点击→页面确认浮层)。data-seq 保留给播放高亮/滚动跟随定位。 */
export function makeSegmentView(cb: SegmentViewCallbacks) {
  return (node: PMNode, _view: EditorView, _getPos: () => number | undefined) => {
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
    badge.title = cb.canEdit() ? t("notes.editor.changeSpeaker") : "";
    badge.onclick = (e) => {
      e.preventDefault();
      if (cb.canEdit())
        cb.onBadgeClick(
          node.attrs.seq as number,
          node.attrs.speaker as string | null,
          node.attrs.source as Source,
          badge.getBoundingClientRect(),
        );
    };
    const ts = document.createElement("button");
    ts.type = "button";
    ts.className = "ts ts-btn";
    ts.contentEditable = "false";
    ts.title = t("notes.editor.playFromHere");
    ts.textContent = cb.formatTs(node.attrs.startMs as number);
    ts.onclick = () => cb.onPlayFrom(node.attrs.startMs as number);
    const content = document.createElement("span");
    content.className = "seg-text";
    const actions = document.createElement("span");
    actions.className = "seg-actions";
    actions.contentEditable = "false";
    if (cb.canEdit()) {
      const del = document.createElement("button");
      del.type = "button";
      del.className = "link";
      del.textContent = t("notes.editor.delete");
      del.onclick = () => cb.onDeleteClick(node.attrs.seq as number, del.getBoundingClientRect());
      actions.append(del);
    }
    dom.append(badge, ts, content, actions);
    return {
      dom,
      contentDOM: content,
      stopEvent: (e: Event) =>
        e.target instanceof HTMLElement && e.target.closest("button") !== null,
      ignoreMutation: (m: ViewMutationRecord) => !content.contains(m.target),
    };
  };
}
