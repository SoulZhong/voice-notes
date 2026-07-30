<script lang="ts" module>
  export type BadgeAttrs = {
    seq?: number;
    origIndex?: number | null;
    speaker: string;
    name: string | null;
    personId: string | null;
    startMs: number;
  };
</script>

<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import {
    Editor,
    defaultValueCtx,
    editorViewCtx,
    parserCtx,
    rootCtx,
    schemaCtx,
    serializerCtx,
  } from "@milkdown/kit/core";
  import type { Ctx } from "@milkdown/kit/ctx";
  import { commonmark } from "@milkdown/kit/preset/commonmark";
  import { history } from "@milkdown/kit/plugin/history";
  // Svelte 5 保留顶层标识符的 `$` 前缀(runes),故重命名导入以避开编译错误。
  import { $prose as proseFactory } from "@milkdown/kit/utils";
  import { Plugin, PluginKey } from "@milkdown/kit/prose/state";
  import type { Node as PMNode, Fragment } from "@milkdown/kit/prose/model";
  import { formatTs, type RefinedDoc, type ParagraphPayload } from "../notes";
  import {
    refinedSavePayload,
    refinedToBlocks,
    type EditedBlock,
  } from "./editorDoc";
  import {
    entityMentionSchema,
    makeRefinedParagraphView,
    refinedNormalizePlugin,
    refinedParagraphSchema,
  } from "./refinedSchema";

  let {
    mode,
    editable = true,
    onSaveRefined,
    onBadgeClick,
    onPlayFrom,
    onEntityOpen,
    speakerBadge,
  }: {
    mode: "refined" | "segments";
    editable?: boolean;
    onSaveRefined?: (payload: { revision: number; paragraphs: ParagraphPayload[] }) => void;
    onBadgeClick?: (attrs: BadgeAttrs, rect: DOMRect) => void;
    onPlayFrom?: (startMs: number) => void;
    onEntityOpen?: (entityId: string) => void;
    speakerBadge: (attrs: BadgeAttrs) => { label: string; bg: string; ink: string };
  } = $props();

  let rootEl: HTMLDivElement;
  let editor: Editor | null = null;
  let ctxRef: Ctx | null = null;
  // 精修稿保存状态:载入 doc + 各 origIndex 的序列化基线 + 上次已保存载荷指纹
  let loadedDoc: RefinedDoc | null = null;
  let baseline = new Map<number, string>();
  let lastSaved = "";
  let idleTimer: ReturnType<typeof setTimeout> | null = null;
  const IDLE_SAVE_MS = 2000;

  /** 顶层块 → markdown。自定义块(refined_paragraph/transcript_segment)按段落
      输出行内内容;标准块直接序列化。往返兜底:结果必须 parse 回同构文本,
      失败(理论不发生)退回 textContent,绝不静默改写。 */
  function serializeBlock(ctx: Ctx, node: PMNode): string {
    const schema = ctx.get(schemaCtx);
    const serializer = ctx.get(serializerCtx);
    try {
      const doc = schema.topNodeType.createChecked(null, [node]);
      return serializer(doc).trim();
    } catch (err) {
      console.warn("serializeBlock 失败,退回纯文本", err);
      return node.textContent.trim();
    }
  }

  /** 行内 markdown → Fragment。解析出多块(理论上段内不该有)时退回字面文本。 */
  function parseInline(ctx: Ctx, text: string): Fragment | PMNode[] {
    const schema = ctx.get(schemaCtx);
    if (!text) return [];
    try {
      const parsed = ctx.get(parserCtx)(text);
      if (parsed && parsed.childCount === 1 && parsed.firstChild!.isTextblock) {
        return parsed.firstChild!.content;
      }
    } catch {
      /* 落到字面文本 */
    }
    return [schema.text(text)];
  }

  export function setRefined(doc: RefinedDoc) {
    if (!ctxRef) return;
    const ctx = ctxRef;
    loadedDoc = doc;
    baseline = new Map();
    const view = ctx.get(editorViewCtx);
    const schema = ctx.get(schemaCtx);
    const paras = refinedToBlocks(doc).map((b) => {
      const content =
        b.kind === "runs"
          ? b.runs
              .filter((r) => r.text)
              .map((r) =>
                r.entityId
                  ? schema.text(r.text, [schema.marks.entity_mention.create({ entityId: r.entityId })])
                  : schema.text(r.text),
              )
          : parseInline(ctx, b.markdown);
      return schema.nodes.refined_paragraph.createChecked(
        { origIndex: b.origIndex, speaker: b.speaker, name: b.name, personId: b.personId, startMs: b.startMs },
        content,
      );
    });
    const docNode = schema.topNodeType.createChecked(
      null,
      paras.length ? paras : [schema.nodes.paragraph.createAndFill()!],
    );
    view.dispatch(
      view.state.tr
        .replaceWith(0, view.state.doc.content.size, docNode.content)
        .setMeta("addToHistory", false)
        .setMeta("external-load", true),
    );
    paras.forEach((p) => {
      if (p.attrs.origIndex !== null) baseline.set(p.attrs.origIndex as number, serializeBlock(ctx, p));
    });
    lastSaved = JSON.stringify(refinedSavePayload(doc, collectBlocks(), baseline).paragraphs);
  }

  function collectBlocks(): EditedBlock[] {
    if (!ctxRef) return [];
    const ctx = ctxRef;
    const view = ctx.get(editorViewCtx);
    const blocks: EditedBlock[] = [];
    view.state.doc.forEach((node) => {
      blocks.push({
        origIndex: node.type.name === "refined_paragraph" ? (node.attrs.origIndex as number | null) : null,
        markdown: serializeBlock(ctx, node),
      });
    });
    return blocks;
  }

  export function flushRefined() {
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = null;
    if (!loadedDoc || !onSaveRefined) return;
    const payload = refinedSavePayload(loadedDoc, collectBlocks(), baseline);
    const fingerprint = JSON.stringify(payload.paragraphs);
    if (fingerprint === lastSaved) return;
    onSaveRefined(payload);
  }

  /** 保存成功回执:当前内容成为新基线(origIndex 重排为保存后的段序)。 */
  export function markSaved(newRevision: number) {
    if (!ctxRef || !loadedDoc) return;
    const ctx = ctxRef;
    const view = ctx.get(editorViewCtx);
    let tr = view.state.tr;
    let nextIndex = 0;
    baseline = new Map();
    view.state.doc.forEach((node, pos) => {
      const md = serializeBlock(ctx, node);
      if (!md) return;
      if (node.type.name === "refined_paragraph") {
        if (node.attrs.origIndex !== nextIndex) {
          tr = tr.setNodeMarkup(pos, undefined, { ...node.attrs, origIndex: nextIndex });
        }
        baseline.set(nextIndex, md);
      }
      nextIndex += 1;
    });
    tr = tr.setMeta("addToHistory", false).setMeta("external-load", true);
    if (tr.docChanged || tr.steps.length > 0) view.dispatch(tr);
    loadedDoc = { ...loadedDoc, revision: newRevision };
    lastSaved = JSON.stringify(refinedSavePayload(loadedDoc, collectBlocks(), baseline).paragraphs);
  }

  export function hasFocus(): boolean {
    if (!ctxRef) return false;
    return ctxRef.get(editorViewCtx).hasFocus();
  }

  function scheduleIdleSave() {
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = setTimeout(() => flushRefined(), IDLE_SAVE_MS);
  }

  const uiPlugins = proseFactory(
    () =>
      new Plugin({
        key: new PluginKey("md-editor-ui"),
        props: {
          editable: () => editable,
          nodeViews: {
            refined_paragraph: makeRefinedParagraphView({
              speakerBadge: (attrs) => speakerBadge(attrs as BadgeAttrs),
              formatTs,
              onBadgeClick: (attrs, rect) => onBadgeClick?.(attrs as BadgeAttrs, rect),
              onPlayFrom: (ms) => onPlayFrom?.(ms),
            }),
          },
          handleDOMEvents: {
            // 实体高亮:悬浮弹层的打开走页面回调(点击进编辑光标,不抢手势)
            mouseover: (_view, event) => {
              const t = event.target as HTMLElement;
              const span = t.closest?.("span[data-entity-id]") as HTMLElement | null;
              if (span) rootEl.dispatchEvent(new CustomEvent("entityhover", { detail: { entityId: span.dataset.entityId, rect: span.getBoundingClientRect() } }));
              return false;
            },
            focusout: () => {
              if (mode === "refined") flushRefined();
              return false;
            },
          },
        },
        view: () => ({
          update: (view, prev) => {
            if (mode === "refined" && !view.state.doc.eq(prev.doc)) scheduleIdleSave();
          },
        }),
      }),
  );

  onMount(async () => {
    editor = await Editor.make()
      .config((ctx) => {
        ctx.set(rootCtx, rootEl);
        ctx.set(defaultValueCtx, "");
      })
      .use(commonmark)
      .use(history)
      .use(refinedParagraphSchema)
      .use(entityMentionSchema)
      .use(refinedNormalizePlugin)
      .use(uiPlugins)
      .create();
    editor.action((ctx) => {
      ctxRef = ctx;
    });
    rootEl.addEventListener("entityhover", ((e: CustomEvent) => {
      // 页面若关心实体悬浮,经 onEntityOpen 之外的浮层处理;此处只转发打开回调的数据源
      void e;
    }) as EventListener);
  });

  onDestroy(() => {
    if (idleTimer) clearTimeout(idleTimer);
    editor?.destroy();
  });
</script>

<div class="md-editor" bind:this={rootEl} data-mode={mode}></div>

<style>
  .md-editor :global(.ProseMirror) {
    outline: none;
    white-space: pre-wrap;
  }
  .md-editor :global(.md-para) {
    margin: 0 0 6px;
  }
  .md-editor :global(.md-para .para-text) {
    display: inline;
  }
</style>
