// 备注文档编辑器(Milkdown)的时间点:段落/标题节点带 `at` 属性(毫秒,null = 没有时间点)。
//
// Markdown 里时间点写在行尾的 HTML 注释 `<!-- mm:ss -->`(见后端 store/memos.rs 文件格式)。
// 解析:remark 把它读成段落/标题的最后一个 inline html 子节点 → 摘下来放进 `at`;
// 序列化:内容之后补一个 html 节点写回去。编辑器里不显示注释,时间画在左侧槽里(装饰)。
//
// 打时间点的规则(appendTransaction):有字但还没 `at` 的段落/标题 → 取 `now()`;
// `now()` 给 null(会后笔记页没放过录音)就沿用文档里前一个有时间的块,再没有记 0。
// 变空的块清掉 `at`——列表回车拆出的新项会继承上一项的属性,不清就会全挂同一个时间。
import { headingSchema, paragraphSchema } from "@milkdown/kit/preset/commonmark";
import type { MarkdownNode, ParserState, SerializerState } from "@milkdown/kit/transformer";
import type { Ctx } from "@milkdown/kit/ctx";
import type { Node as PMNode } from "@milkdown/kit/prose/model";
import { Fragment } from "@milkdown/kit/prose/model";
import { Plugin, PluginKey, type EditorState, type Transaction } from "@milkdown/kit/prose/state";
import { Decoration, DecorationSet } from "@milkdown/kit/prose/view";
import { fmtClock, parseClock } from "../memos";

/** 带这个 meta 的事务(载入/整篇替换)不打时间点。 */
export const SKIP_STAMP = "memoDocSkipStamp";

const STAMP_RE = /^<!--\s*(\d{1,3}(?::\d{1,3}){1,2})\s*-->$/;

export function parseStampHtml(value: string): number | null {
  const m = STAMP_RE.exec(value.trim());
  return m ? parseClock(m[1]) : null;
}

export function stampHtml(ms: number): string {
  return `<!-- ${fmtClock(ms)} -->`;
}

/** 摘掉 mdast 子节点末尾的时间注释(及其前面的空白)。 */
export function takeStamp(children: readonly MarkdownNode[]): { children: MarkdownNode[]; at: number | null } {
  const list = [...children];
  const last = list[list.length - 1];
  if (!last || last.type !== "html") return { children: list, at: null };
  const at = parseStampHtml(String(last.value ?? ""));
  if (at === null) return { children: list, at: null };
  list.pop();
  const prev = list[list.length - 1];
  if (prev && prev.type === "text") {
    const v = String(prev.value ?? "").replace(/\s+$/, "");
    if (v === "") list.pop();
    else list[list.length - 1] = { ...prev, value: v };
  }
  return { children: list, at };
}

/** 同 commonmark 内部 serializeText:末尾的硬换行不写出去。 */
function serializeInline(state: SerializerState, node: PMNode) {
  if (!(node.childCount >= 1 && node.lastChild?.type.name === "hardbreak")) {
    state.next(node.content);
    return;
  }
  const arr: PMNode[] = [];
  node.content.forEach((n, _, i) => {
    if (i < node.childCount - 1) arr.push(n);
  });
  state.next(Fragment.fromArray(arr));
}

function writeStamp(state: SerializerState, node: PMNode) {
  const at = node.attrs.at as number | null;
  if (at === null || at === undefined) return;
  // 前导空格放进 html 值里:html 原样输出,不会被当成行尾空白转义或吞掉。
  state.addNode("html", undefined, ` ${stampHtml(at)}`);
}

const atAttr = { at: { default: null as number | null } };

type SchemaFn = Parameters<Parameters<typeof paragraphSchema.extendSchema>[0]>[0];

// 注意:不能用 extendSchema 另注册同名节点——那会把 paragraph 挪到 schema 末尾,
// ProseMirror 填充 doc 的 block+ 时先选到 blockquote(内容又是 block+)→ 无限递归。
// 改为在 config 阶段覆盖 commonmark 自己的 schema 定义槽(见 stampSchemaConfig)。
const stampedParagraph = (prev: SchemaFn): SchemaFn => (ctx) => {
  const base = prev(ctx);
  return {
    ...base,
    attrs: { ...(base.attrs ?? {}), ...atAttr },
    parseMarkdown: {
      match: (node: MarkdownNode) => node.type === "paragraph",
      runner: (state: ParserState, node: MarkdownNode, type) => {
        const { children, at } = takeStamp(node.children ?? []);
        state.openNode(type, { at });
        if (node.children) state.next(children);
        else state.addText(String(node.value ?? ""));
        state.closeNode();
      },
    },
    toMarkdown: {
      match: (node: PMNode) => node.type.name === "paragraph",
      runner: (state: SerializerState, node: PMNode) => {
        state.openNode("paragraph");
        // 空段落不写 `<br />`(commonmark 默认会写):备注里的空行没有保存价值。
        serializeInline(state, node);
        writeStamp(state, node);
        state.closeNode();
      },
    },
  };
};

const stampedHeading = (prev: SchemaFn): SchemaFn => (ctx) => {
  const base = prev(ctx);
  return {
    ...base,
    attrs: { ...(base.attrs ?? {}), ...atAttr },
    parseMarkdown: {
      match: (node: MarkdownNode) => node.type === "heading",
      runner: (state: ParserState, node: MarkdownNode, type) => {
        const { children, at } = takeStamp(node.children ?? []);
        state.openNode(type, { level: node.depth as number, at });
        state.next(children);
        state.closeNode();
      },
    },
    toMarkdown: {
      match: (node: PMNode) => node.type.name === "heading",
      runner: (state: SerializerState, node: PMNode) => {
        state.openNode("heading", undefined, { depth: node.attrs.level });
        serializeInline(state, node);
        writeStamp(state, node);
        state.closeNode();
      },
    },
  };
};

/** 在 Editor.make().config(...) 里调用:给 commonmark 的 paragraph/heading 加上时间点读写。 */
export function stampSchemaConfig(ctx: Ctx) {
  ctx.update(paragraphSchema.key, (prev) => stampedParagraph(prev as SchemaFn));
  ctx.update(headingSchema.key, (prev) => stampedHeading(prev as SchemaFn));
}

const isStampable = (n: PMNode) => n.type.name === "paragraph" || n.type.name === "heading";

/** 文档里所有带时间点的块(文档顺序)。 */
export function collectStamps(doc: PMNode): number[] {
  const out: number[] = [];
  doc.descendants((n) => {
    if (isStampable(n)) {
      if (n.attrs.at !== null && n.attrs.at !== undefined) out.push(n.attrs.at as number);
      return false;
    }
    return true;
  });
  return out;
}

/** 给该打时间点的块打上、给变空的块清掉;不需要改动返回 null。纯函数(配单测)。 */
export function stampTransaction(state: EditorState, now: () => number | null): Transaction | null {
  let tr: Transaction | null = null;
  let prevAt: number | null = null;
  let cached: number | null | undefined;
  const nowOnce = () => (cached === undefined ? (cached = now()) : cached);
  state.doc.descendants((node, pos) => {
    if (!isStampable(node)) return true;
    const at = node.attrs.at as number | null;
    const empty = node.textContent.trim() === "";
    if (empty && at !== null) {
      tr = (tr ?? state.tr).setNodeMarkup(pos, undefined, { ...node.attrs, at: null });
    } else if (!empty && at === null) {
      const v = Math.max(0, Math.round(nowOnce() ?? prevAt ?? 0));
      tr = (tr ?? state.tr).setNodeMarkup(pos, undefined, { ...node.attrs, at: v });
      prevAt = v;
    } else if (at !== null) {
      prevAt = at;
    }
    return false;
  });
  if (tr) (tr as Transaction).setMeta("addToHistory", false);
  return tr;
}

export const memoDocKey = new PluginKey("memo-doc");

export type MemoDocPluginOpts = {
  now: () => number | null;
  onPick: (ms: number) => void;
  onStamps: (stamps: number[]) => void;
  placeholder: () => string;
  jumpTitle: () => string;
};

export function memoDocPlugin(o: MemoDocPluginOpts): Plugin {
  return new Plugin({
    key: memoDocKey,
    appendTransaction(trs, _old, state) {
      if (!trs.some((t) => t.docChanged) || trs.some((t) => t.getMeta(SKIP_STAMP))) return null;
      return stampTransaction(state, o.now);
    },
    props: {
      decorations(state) {
        const decos: Decoration[] = [];
        state.doc.descendants((node, pos) => {
          if (!isStampable(node)) return true;
          const at = node.attrs.at as number | null;
          if (at !== null && at !== undefined) {
            decos.push(
              Decoration.widget(
                pos + 1,
                () => {
                  const el = document.createElement("span");
                  el.className = "memo-stamp";
                  el.contentEditable = "false";
                  el.textContent = fmtClock(at);
                  el.title = o.jumpTitle();
                  el.addEventListener("mousedown", (e) => {
                    e.preventDefault();
                    o.onPick(at);
                  });
                  return el;
                },
                { side: -1, key: `st-${at}`, ignoreSelection: true },
              ),
            );
          }
          return false;
        });
        const d = state.doc;
        const first = d.firstChild;
        if (d.childCount === 1 && first && first.isTextblock && first.content.size === 0) {
          decos.push(Decoration.node(0, first.nodeSize, { class: "is-empty", "data-placeholder": o.placeholder() }));
        }
        return DecorationSet.create(state.doc, decos);
      },
    },
    view: (view) => {
      o.onStamps(collectStamps(view.state.doc));
      return {
        update(v, prev) {
          if (v.state.doc !== prev.doc) o.onStamps(collectStamps(v.state.doc));
        },
      };
    },
  });
}
