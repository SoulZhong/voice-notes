// 笔记页书签(会议备注):ProseMirror 装饰,不进编辑器文档。
// 设计:docs/superpowers/specs/2026-11-03-meeting-memo-design.md「定死的选择 5」。
//
// 为什么是装饰而不是节点:原始稿编辑器有骨架锁(非段节点混入即判骨架破坏,
// filterTransaction 只放行 external-load 事务),修订稿编辑器会把每个顶层块收进
// 保存载荷。书签若进文档,前者整条拒绝、后者被当正文写盘。装饰由插件的
// `decorations` prop 按 (doc, 备注版本) 现算,备注变化时宿主 `view.setProps({})`
// 让 PM 重读——全程**不发事务**,文档一字不变。
//
// 本文件只放纯逻辑(放置规则 + DecorationSet 构建 + 插件),DOM 在 memoWidget.ts。
import { Plugin, PluginKey } from "@milkdown/kit/prose/state";
import { Decoration, DecorationSet } from "@milkdown/kit/prose/view";
import type { Node as PMNode } from "@milkdown/kit/prose/model";
import { anchorIndex, inCut, sortMemos, type Memo } from "../memos";

export type MemoCut = { start_ms: number; end_ms: number };

/** 未保存的草稿书签(笔记页按 M / 点「加备注」插入,回车保存、Esc 丢弃)。 */
export type MemoDraft = { at_ms: number; seq: number };

/** 编辑器渲染书签所需的全部输入。由宿主组件维护成**非响应式**镜像(PM 回调里
    不得读 Svelte prop,见 MarkdownEditor 的 editableNow 注释),每次改动 version+1。 */
export type MemoDecoInput = {
  version: number;
  memos: readonly Memo[];
  cuts: readonly MemoCut[];
  draft: MemoDraft | null;
};

/** 书签上的用户动作:编辑器只渲染,数据与 IO 归页面。返回 Promise 的,
    reject = 失败(组件保持编辑态,错误提示由页面负责)。 */
export type MemoAction =
  | { kind: "play"; memo: Memo }
  | { kind: "edit"; memo: Memo; text: string }
  | { kind: "adjust"; memo: Memo; at_ms: number }
  | { kind: "delete"; memo: Memo }
  | { kind: "draftSave"; at_ms: number; text: string }
  | { kind: "draftCancel" };

export type MemoPlacementItem =
  | { kind: "memo"; memo: Memo; cut: boolean }
  | { kind: "draft"; draft: MemoDraft };

export type MemoPlacement = {
  /** 文档位置:顶层块之间的边界(0 = 文首)。 */
  pos: number;
  /** 挂在哪个顶层块之后(-1 = 所有块之前)。 */
  after: number;
  item: MemoPlacementItem;
};

/**
 * 每个顶层块的起始时间(null = 无时间跨度,不推进书签游标)。
 * - 原始稿 transcript_segment:段 startMs;
 * - 修订稿 refined_paragraph:startMs(展示时基,与播放器同口径);没有说话人的
 *   自由块(盘上 start==end==0)与新敲的块记 null;
 * - 其它块(占位空段落等):null。
 */
export function blockStarts(doc: PMNode): (number | null)[] {
  const out: (number | null)[] = [];
  doc.forEach((node) => {
    const name = node.type.name;
    if (name === "transcript_segment") {
      out.push(Number(node.attrs.startMs) || 0);
    } else if (name === "refined_paragraph") {
      // 与 refinedSchema NodeView 同一判据:有说话人 = 有真实时间跨度的段落
      // (自由块与新敲的块 speaker/name/personId 全空,startMs 只是默认 0)。
      const a = node.attrs;
      out.push(a.speaker || a.name || a.personId ? Number(a.startMs) || 0 : null);
    } else {
      out.push(null);
    }
  });
  return out;
}

/** 各顶层块**之后**的文档位置(ends[i] = 块 i 的结束边界)。 */
function blockEnds(doc: PMNode): number[] {
  const ends: number[] = [];
  doc.forEach((node, offset) => ends.push(offset + node.nodeSize));
  return ends;
}

/**
 * 书签放置:挂在「最后一个 start_ms <= at_ms 的块」之后;早于所有块的放文首。
 * 同位置多条按时间(再按创建时间)排序;草稿与同刻备注并列时排在其后。
 */
export function memoPlacements(
  doc: PMNode,
  memos: readonly Memo[],
  cuts: readonly MemoCut[] = [],
  draft: MemoDraft | null = null,
): MemoPlacement[] {
  const starts = blockStarts(doc);
  const ends = blockEnds(doc);
  const items: { at: number; item: MemoPlacementItem }[] = sortMemos(memos).map((m) => ({
    at: m.at_ms,
    item: { kind: "memo", memo: m, cut: inCut(m.at_ms, cuts) },
  }));
  if (draft) {
    // 插到最后一个 at <= draft.at_ms 的备注之后(sortMemos 已升序,稳定插入)。
    let k = items.length;
    while (k > 0 && items[k - 1].at > draft.at_ms) k--;
    items.splice(k, 0, { at: draft.at_ms, item: { kind: "draft", draft } });
  }
  return items.map(({ at, item }) => {
    const after = anchorIndex(starts, at);
    return { pos: after < 0 ? 0 : ends[after], after, item };
  });
}

/** widget 的身份键:键相同 PM 复用旧 DOM(编辑中的输入框不被吹掉),
    备注一改 updated_at 就变,DOM 随之重建。 */
export function placementKey(item: MemoPlacementItem): string {
  return item.kind === "memo"
    ? `memo:${item.memo.id}:${item.memo.updated_at}:${item.cut ? 1 : 0}`
    : `memo-draft:${item.draft.seq}`;
}

export type MemoWidgetRenderer = (item: MemoPlacementItem) => HTMLElement;

export function buildMemoDecorations(
  doc: PMNode,
  input: MemoDecoInput,
  render: MemoWidgetRenderer,
): DecorationSet {
  const placements = memoPlacements(doc, input.memos, input.cuts, input.draft);
  if (placements.length === 0) return DecorationSet.empty;
  const decos = placements.map(({ pos, item }) =>
    Decoration.widget(pos, () => render(item), {
      key: placementKey(item),
      // side:-1 让 widget 贴在边界前一侧,光标落在下一块开头时不会被它"吸进去"。
      side: -1,
      // 书签内部的选区变化/按钮点击与编辑器无关:PM 不据此重读选区、不处理事件。
      ignoreSelection: true,
      stopEvent: () => true,
    }),
  );
  return DecorationSet.create(doc, decos);
}

export const memoBookmarkKey = new PluginKey("memo-bookmarks");

/**
 * 书签插件:**只有 decorations prop**——没有 state 字段、没有 filterTransaction /
 * appendTransaction,不产生也不改写任何事务(单测守住这一点)。
 * 结果按 (doc 身份, input.version) 缓存:用户打字时每次事务只多一次 O(块数) 扫描。
 */
export function memoBookmarkPlugin(source: () => MemoDecoInput, render: MemoWidgetRenderer): Plugin {
  let cache: { doc: PMNode; version: number; set: DecorationSet } | null = null;
  return new Plugin({
    key: memoBookmarkKey,
    props: {
      decorations(state) {
        const input = source();
        if (cache && cache.doc === state.doc && cache.version === input.version) return cache.set;
        const set = buildMemoDecorations(state.doc, input, render);
        cache = { doc: state.doc, version: input.version, set };
        return set;
      },
    },
  });
}
