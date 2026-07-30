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
    isBlockTextEmpty,
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

  // 精修稿保存生命周期契约(宿主/Task 7 必须遵守):onSaveRefined(payload) 发出
  // 后 saveInFlight=true;宿主保存成功 → 调 markSaved(newRevision) 确认落定
  // (复位 saveInFlight,重建 baseline/origIndex);宿主保存失败(拒绝并不罕见:
  // Aing 中/录制中/revision 冲突等)→ 必须调 markSaveFailed() 复位 saveInFlight
  // 并重新排队,否则 saveInFlight 卡 true,自动保存永久停摆。冲突类失败宿主
  // 通常还会紧接着 setRefined() 整份重载(它自己也会复位 saveInFlight);非冲突
  // 失败(如 Aing/录制中被拒)只需调 markSaveFailed() 让编辑继续按 idle 定时器
  // 重试,不必重载文档。
  let rootEl: HTMLDivElement;
  let editor: Editor | null = null;
  let ctxRef: Ctx | null = null;
  let destroyed = false;
  // 精修稿保存状态:载入 doc + 各 origIndex 的序列化基线 + 上次已保存载荷指纹
  let loadedDoc: RefinedDoc | null = null;
  let baseline = new Map<number, string>();
  let lastSaved = "";
  let idleTimer: ReturnType<typeof setTimeout> | null = null;
  const IDLE_SAVE_MS = 2000;
  // ctxRef 尚未就绪(onMount 的 Editor.make() 还没 resolve)时到达的 setRefined
  // 文档;ctx 就绪后在 onMount 里重放,避免静默丢文档。
  let pendingDoc: RefinedDoc | null = null;
  // 保存请求已发出、等待宿主页面回调 markSaved 确认落定;避免同一份未确认的
  // 保存被并发/重复的 flushRefined 再次发出。
  let saveInFlight = false;
  // setRefined/markSaved 程序化 dispatch(meta "external-load")后,PM 的
  // view.update 回调拿不到触发它的 tr,只能用这个标志显式跳过下一次 update ->
  // 避免程序化载入被误判成用户编辑而 schedule 幻影保存。
  let suppressNextUpdate = false;

  /** 顶层块 → markdown。自定义块(refined_paragraph/transcript_segment)按段落
      输出行内内容;标准块直接序列化。往返兜底:结果必须 parse 回同构文本,
      失败(理论不发生)退回 textContent,绝不静默改写——除非 fallbackToText 为
      false(精修稿保存路径):此时失败返回 null,交给调用方中止整次保存,而不是
      悄悄用可能污染共享 SerializerState 之后的残缺文本继续写盘。 */
  function serializeBlock(ctx: Ctx, node: PMNode, fallbackToText: boolean): string | null {
    const schema = ctx.get(schemaCtx);
    const serializer = ctx.get(serializerCtx);
    try {
      const doc = schema.topNodeType.createChecked(null, [node]);
      return serializer(doc).trim();
    } catch (err) {
      if (fallbackToText) {
        console.warn("serializeBlock 失败,退回纯文本", err);
        return node.textContent.trim();
      }
      console.warn("serializeBlock 失败", err);
      return null;
    }
  }

  /** 单一判定源:collectBlocks(保存载荷)与 markSaved(origIndex 重排)都必须
      走这个函数,不能各自直接调 serializeBlock——否则两处对"块是不是空的"会
      产生分歧(commonmark 空 paragraph 序列化出 truthy 的 "<br />"),同一份
      文档数出两种不同的块集合,origIndex 就会错位(Round 2 Critical 1)。 */
  function blockMarkdown(ctx: Ctx, node: PMNode, fallbackToText: boolean): string | null {
    if (isBlockTextEmpty(node.textContent)) return "";
    return serializeBlock(ctx, node, fallbackToText);
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
    if (!ctxRef) {
      // Editor 还没 create() 完(onMount 是 async 的),先攒着,onMount 里重放。
      pendingDoc = doc;
      return;
    }
    const ctx = ctxRef;
    loadedDoc = doc;
    baseline = new Map();
    saveInFlight = false;
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
    suppressNextUpdate = true;
    view.dispatch(
      view.state.tr
        .replaceWith(0, view.state.doc.content.size, docNode.content)
        .setMeta("addToHistory", false)
        .setMeta("external-load", true),
    );
    paras.forEach((p) => {
      if (p.attrs.origIndex !== null) {
        const md = serializeBlock(ctx, p, true);
        if (md !== null) baseline.set(p.attrs.origIndex as number, md);
      }
    });
    lastSaved = JSON.stringify(refinedSavePayload(doc, collectBlocks() ?? [], baseline).paragraphs);
  }

  /** 顶层块 → EditedBlock[],用于保存载荷。空块(blockMarkdown 判定)markdown
      记为 "",载荷构建器按 trim 后为空丢弃这类块。任一非空块序列化失败
      (fallbackToText=false)返回 null,视为整批失败,调用方应中止本次保存,
      而不是带着残缺内容继续写盘。判空标准与 markSaved 共用 blockMarkdown,
      两处必须数出同一个块集合,否则 origIndex 会错位。 */
  function collectBlocks(): EditedBlock[] | null {
    if (!ctxRef) return [];
    const ctx = ctxRef;
    const view = ctx.get(editorViewCtx);
    const blocks: EditedBlock[] = [];
    let failed = false;
    view.state.doc.forEach((node) => {
      if (failed) return;
      const md = blockMarkdown(ctx, node, false);
      if (md === null) {
        failed = true;
        return;
      }
      blocks.push({
        origIndex: node.type.name === "refined_paragraph" ? (node.attrs.origIndex as number | null) : null,
        markdown: md,
      });
    });
    return failed ? null : blocks;
  }

  export function flushRefined() {
    if (idleTimer) clearTimeout(idleTimer);
    idleTimer = null;
    if (!loadedDoc || !onSaveRefined) return;
    if (saveInFlight) {
      // 上一次保存还没等到 markSaved 回执,别再并发发一次;等它落定后再算。
      scheduleIdleSave();
      return;
    }
    const blocks = collectBlocks();
    if (blocks === null) {
      console.warn("精修稿序列化失败,本次不保存");
      return;
    }
    const payload = refinedSavePayload(loadedDoc, blocks, baseline);
    if (payload.paragraphs.length === 0) return; // 全选删除不该经定时器抹掉整份精修稿
    const fingerprint = JSON.stringify(payload.paragraphs);
    if (fingerprint === lastSaved) return;
    saveInFlight = true;
    onSaveRefined(payload);
  }

  /** 保存被拒(参见组件顶部的保存生命周期契约)。宿主必须在 onSaveRefined 的
      失败分支调用本方法复位 saveInFlight,否则一次拒绝就会让自动保存永久
      停摆——下次编辑的 flushRefined 会一直因 saveInFlight 为 true 而重新排队,
      永远发不出去。 */
  export function markSaveFailed() {
    saveInFlight = false;
    scheduleIdleSave();
  }

  /** 保存成功回执:当前内容成为新基线(origIndex 重排为保存后的段序)。
      baseline 重建 fail-closed:blockMarkdown 传 fallbackToText=false,任一块
      序列化失败就整体放弃本次重建——旧 baseline/origIndex/revision 原样保留,
      不用半可信数据(或 fallback 出的剥格式纯文本)当基线,否则下次保存会把
      它误判成"没变"/"变了"静默写盘,而且共享 SerializerState 污染还可能级联
      到后面的块。放弃时 console.warn,宿主应重新 setRefined() 整份重载。 */
  export function markSaved(newRevision: number) {
    saveInFlight = false;
    if (!ctxRef || !loadedDoc) return;
    const ctx = ctxRef;
    const view = ctx.get(editorViewCtx);
    let tr = view.state.tr;
    let nextIndex = 0;
    const nextBaseline = new Map<number, string>();
    let failed = false;
    view.state.doc.forEach((node, pos) => {
      if (failed) return;
      const md = blockMarkdown(ctx, node, false);
      if (md === null) {
        failed = true;
        return;
      }
      if (!md) {
        // 空块没进保存载荷(refinedSavePayload 会丢弃),服务端已不存在该段;
        // 清掉它的 origIndex/说话人属性,否则它会顶着过期 origIndex 一直留在
        // 文档里,后续 normalize/保存都会拿它当已知段处理。
        if (node.type.name === "refined_paragraph" && node.attrs.origIndex !== null) {
          tr = tr.setNodeMarkup(pos, undefined, {
            ...node.attrs,
            origIndex: null,
            speaker: "",
            name: null,
            personId: null,
            startMs: 0,
          });
        }
        return;
      }
      if (node.type.name === "refined_paragraph") {
        if (node.attrs.origIndex !== nextIndex) {
          tr = tr.setNodeMarkup(pos, undefined, { ...node.attrs, origIndex: nextIndex });
        }
        nextBaseline.set(nextIndex, md);
      }
      nextIndex += 1;
    });
    if (failed) {
      console.warn("markSaved 序列化失败,放弃本次基线重建;宿主应重新 setRefined() 重载精修稿");
      return;
    }
    baseline = nextBaseline;
    tr = tr.setMeta("addToHistory", false).setMeta("external-load", true);
    if (tr.docChanged || tr.steps.length > 0) {
      suppressNextUpdate = true;
      view.dispatch(tr);
    }
    loadedDoc = { ...loadedDoc, revision: newRevision };
    lastSaved = JSON.stringify(refinedSavePayload(loadedDoc, collectBlocks() ?? [], baseline).paragraphs);
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
            // entityhover 的镜像:鼠标离开实体高亮 span 时通知页面隐藏浮层。
            mouseout: (_view, event) => {
              const t = event.target as HTMLElement;
              const span = t.closest?.("span[data-entity-id]") as HTMLElement | null;
              if (span) rootEl.dispatchEvent(new CustomEvent("entityleave", { detail: { entityId: span.dataset.entityId } }));
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
            if (suppressNextUpdate) {
              // setRefined/markSaved 的程序化 dispatch(external-load meta);
              // update(view, prev) 拿不到触发它的 tr,靠这个标志显式跳过,
              // 不把程序化载入误判成用户编辑去 schedule 幻影保存。
              suppressNextUpdate = false;
              return;
            }
            if (mode === "refined" && !view.state.doc.eq(prev.doc)) scheduleIdleSave();
          },
        }),
      }),
  );

  onMount(async () => {
    const created = await Editor.make()
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
    if (destroyed) {
      // onDestroy 已经跑过(组件在 Editor.make() resolve 前就被卸载):别再
      // 挂 ctxRef/事件,立刻销毁刚创建出来的 editor,避免它悬空监听/泄漏。
      created.destroy();
      return;
    }
    editor = created;
    editor.action((ctx) => {
      ctxRef = ctx;
    });
    if (pendingDoc) {
      const doc = pendingDoc;
      pendingDoc = null;
      setRefined(doc);
    }
  });

  onDestroy(() => {
    destroyed = true;
    if (idleTimer) clearTimeout(idleTimer);
    editor?.destroy();
  });

  // editable 变化即时生效:PM 的 editable prop 只在事务分发时被重新读取,
  // 派发一个空事务强制它重读(依赖 editable 建立 $effect 追踪)。
  $effect(() => {
    void editable;
    if (ctxRef) {
      const view = ctxRef.get(editorViewCtx);
      view.dispatch(view.state.tr);
    }
  });
</script>

<div class="md-editor" bind:this={rootEl} data-mode={mode}></div>

<style>
  .md-editor :global(.ProseMirror) {
    outline: none;
    white-space: pre-wrap;
  }
  .md-editor :global(.ProseMirror > p) {
    margin: 0 0 6px;
  }
  .md-editor :global(.md-para) {
    margin: 0 0 6px;
  }
  .md-editor :global(.md-para .para-text) {
    display: inline;
  }
</style>
