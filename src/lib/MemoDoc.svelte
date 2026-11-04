<script lang="ts">
  // 会议备注编辑器:整场一篇 Markdown(录制页与笔记页共用),右侧通长一栏,可收起成细轨。
  // - 每个段落/标题/列表项开始写字那一刻自动记时间点(见 editor/memoDocSchema.ts),显示在左侧槽里,
  //   点一下跳到原文对应位置。
  // - 自动保存:停手约 0.8 秒整篇写盘,带版本号;别处同时改过(快捷键追加、浮窗补字)→ 按行三方
  //   合并后重存(memoDocMerge.ts),两边都不丢。
  // - 收起后是 40px 细轨:时间点小点(点了跳原文),录制中再画一条「现在」。
  import { onDestroy, onMount, untrack } from "svelte";
  import { Editor, editorViewCtx, parserCtx, remarkStringifyOptionsCtx, rootCtx, serializerCtx } from "@milkdown/kit/core";
  import { commonmark } from "@milkdown/kit/preset/commonmark";
  import { gfm } from "@milkdown/kit/preset/gfm";
  import { history } from "@milkdown/kit/plugin/history";
  import { $prose as proseFactory } from "@milkdown/kit/utils";
  import { Selection, TextSelection } from "@milkdown/kit/prose/state";
  import { t } from "$lib/i18n/index.svelte";
  import { fmtClock, memoDoc, MEMO_DOC_MAX_CHARS, onMemosChanged, seqOf, type MemoDoc as Doc } from "$lib/memos";
  import { mergeMemoDoc } from "$lib/memoDocMerge";
  import { clearDraft, memoDocIdle, saveSerial, stashDraft, takeDraft, type SaveOp } from "$lib/memoDocSession";
  import { memoDocPlugin, SKIP_STAMP, stampSchemaConfig } from "$lib/editor/memoDocSchema";

  let {
    noteId,
    open,
    onToggle,
    now,
    onPickMs,
    elapsedMs = null,
    durationMs = 0,
    editable = true,
  }: {
    /** 只在创建时读一次:组件属于这一篇,宿主切笔记要用 {#key} 重建(见下方 myId)。 */
    noteId: string;
    open: boolean;
    onToggle: () => void;
    /** 新写一行时取的时间点;null = 沿用上一行的时间(会后没放过录音)。 */
    now: () => number | null;
    onPickMs: (ms: number) => void;
    /** 录制中的当前录音时间(细轨上画「现在」);会后传 null。 */
    elapsedMs?: number | null;
    /** 会后的笔记时长(细轨比例尺)。 */
    durationMs?: number;
    editable?: boolean;
  } = $props();

  // 组件所属笔记:创建时定死。prop 是响应式 getter,切笔记时旧组件卸载那一刻读到的已是新笔记
  // ——不定死的话,旧笔记没存的字会写进新笔记(Codex 审出 P1)。
  const myId = untrack(() => noteId);

  let host = $state<HTMLElement | null>(null);
  let editor: Editor | null = null;
  let stamps = $state<number[]>([]);
  let error = $state("");
  /** 首次取盘完成前不许编辑:否则取盘回来整篇替换会盖掉已经打的字(Codex 审出)。 */
  let loaded = false;
  const span = $derived(Math.max(elapsedMs ?? 0, durationMs, 60_000, ...stamps.map((s) => s + 1)));

  // ── 与盘同步的状态(本编辑器实例私有) ──
  // base = 编辑器内容所依据的盘面(已应用到编辑器的最新一版);baseSer = 它经本编辑器序列化后
  // 的样子(行格式与 serialize() 一致,「有没有改动」与合并都拿它比,不冒假差异)。
  // 新旧只看版本号(后端写入序号,只增不减):比 base 旧或相等的一律不理。
  let base: Doc = { text: "", rev: "0" };
  let baseSer = "";
  /** 见过的最高版本号(事件带来的):高于 base 说明盘上有还没应用的新内容,空闲时补取。 */
  let wantSeq = 0;
  /** 本编辑器一次只有一份保存在途。 */
  let inflight: SaveOp | null = null;
  let again = false;
  let syncing = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let retryTimer: ReturnType<typeof setTimeout> | undefined;
  let destroyed = false;
  // 插件闭包里读的「此刻」、回调与可编辑:用普通变量转一手,不在 effect 外读 prop。
  let nowFn: () => number | null = () => null;
  let pickFn: (ms: number) => void = () => {};
  let editableNow = true;
  $effect(() => {
    nowFn = now;
    pickFn = onPickMs;
    editableNow = editable;
  });

  function view() {
    return editor?.action((ctx) => ctx.get(editorViewCtx));
  }
  /** 空段落序列化成空行:开头的空行去掉、连着的空行并成一个,免得文件里攒一堆空行。 */
  const tidy = (md: string) => md.replace(/^\n+/, "").replace(/\n{3,}/g, "\n\n");
  function serialize(): string {
    if (!editor) return "";
    return tidy(editor.action((ctx) => ctx.get(serializerCtx)(ctx.get(editorViewCtx).state.doc)));
  }
  function normalize(md: string): string {
    if (!editor) return md;
    return tidy(editor.action((ctx) => ctx.get(serializerCtx)(ctx.get(parserCtx)(md))));
  }
  /** 整篇替换内容(载入/合并):不打时间点、不进撤销栈,光标尽量留在原位。 */
  function setContent(md: string) {
    editor?.action((ctx) => {
      const v = ctx.get(editorViewCtx);
      const doc = ctx.get(parserCtx)(md);
      const { state } = v;
      const tr = state.tr.replaceWith(0, state.doc.content.size, doc.content);
      const pos = Math.min(state.selection.from, tr.doc.content.size);
      try {
        tr.setSelection(TextSelection.near(tr.doc.resolve(pos)));
      } catch {
        tr.setSelection(Selection.atStart(tr.doc));
      }
      tr.setMeta(SKIP_STAMP, true).setMeta("addToHistory", false);
      v.dispatch(tr);
    });
  }

  function setBase(doc: Doc) {
    base = doc;
    baseSer = normalize(doc.text);
  }

  /** 应用一版比 base 新的盘面:编辑器没改动就直接换成它;有改动就把改动接到它上面,稍后再存。 */
  function applyRemote(doc: Doc) {
    if (!editor || seqOf(doc) <= seqOf(base)) return;
    const local = serialize();
    const prevSer = baseSer;
    setBase(doc);
    if (local === prevSer) {
      if (local !== baseSer) setContent(doc.text);
      if (baseSer !== doc.text) scheduleSave();
      return;
    }
    const merged = mergeMemoDoc(prevSer, local, baseSer);
    if (merged !== local) setContent(merged);
    scheduleSave();
  }

  function scheduleSave(ms = 800) {
    clearTimeout(timer);
    timer = setTimeout(() => void save(), ms);
  }

  async function save() {
    if (!editor || !loaded || destroyed) return;
    if (inflight) {
      again = true;
      return;
    }
    const text = serialize();
    // 与盘面原文一致才不存:只差格式(后端写入的转义/列表记号与编辑器写法不同)也存一次,
    // 让盘面收敛到编辑器的写法,之后三方合并不再冒假差异(Codex 审出)。
    if (text === base.text) return;
    if (text.length > MEMO_DOC_MAX_CHARS) {
      error = t("memo.doc.tooLong", { n: MEMO_DOC_MAX_CHARS });
      return;
    }
    const op = saveSerial(myId, text, { base: base.text, prev: null });
    inflight = op;
    try {
      const doc = await op.done;
      clearDraft(myId);
      if (destroyed || !editor) return;
      error = "";
      // 回执是后端锁内合并后的结果(含保存期间别处的改动);这期间新打的字接到它上面
      if (seqOf(doc) > seqOf(base)) {
        const now = serialize();
        setBase(doc);
        if (baseSer !== now) setContent(now === text ? doc.text : mergeMemoDoc(text, now, baseSer));
      }
    } catch (e) {
      if (destroyed) return;
      error = t("memo.failed", { e: String(e) });
      // 失败:改动还在编辑器里,过一会儿重试
      clearTimeout(retryTimer);
      retryTimer = setTimeout(() => void save(), 3000);
    } finally {
      inflight = null;
      if (!destroyed && editor) {
        void sync();
        const queued = again;
        again = false;
        if (queued || serialize() !== baseSer) scheduleSave(queued ? 0 : 800);
      }
    }
  }

  /** 盘上有还没应用的新版本(wantSeq > base)且没有保存在途:取盘应用。取回来的比 base 旧
      (取盘期间又有保存/事件推进了 base)就丢掉;仍落后就再取一次。 */
  async function sync() {
    if (!editor || destroyed || inflight || syncing || wantSeq <= seqOf(base)) return;
    syncing = true;
    try {
      const doc = await memoDoc(myId);
      if (destroyed) return;
      if (!inflight) applyRemote(doc);
    } catch {
      /* 取不到就等下一次事件 */
    } finally {
      syncing = false;
    }
    if (!destroyed && !inflight && wantSeq > seqOf(base)) setTimeout(() => void sync(), 300);
  }

  let unlisten: Promise<() => void> | null = null;

  onMount(async () => {
    editor = await Editor.make()
      .config((ctx) => {
        ctx.set(rootCtx, host!);
        stampSchemaConfig(ctx);
        // 列表记号写成 `-`(remark 默认 `*`):与快捷键/迁移写出的格式一致,在应用外打开也顺眼。
        ctx.update(remarkStringifyOptionsCtx, (prev) => ({ ...prev, bullet: "-" as const }));
      })
      .use(commonmark)
      .use(gfm)
      .use(history)
      .use(
        proseFactory(() =>
          memoDocPlugin({
            now: () => nowFn(),
            onPick: (ms) => pickFn(ms),
            onStamps: (s) => (stamps = s),
            placeholder: () => t("memo.doc.placeholder"),
            jumpTitle: () => t("memo.doc.jump"),
          }),
        ),
      )
      .create();
    if (destroyed) {
      void editor.destroy();
      editor = null;
      return;
    }
    const v = view()!;
    v.setProps({
      editable: () => loaded && editableNow,
      dispatchTransaction(tr) {
        const next = v.state.apply(tr);
        v.updateState(next);
        if (tr.docChanged && !tr.getMeta(SKIP_STAMP)) scheduleSave();
      },
      handleKeyDown(_v, e) {
        if (e.key === "Escape") {
          (v.dom as HTMLElement).blur();
          return true;
        }
        return false;
      },
    });
    // 事件只记「见过的最高版本」并在空闲时应用;保存在途时等回执后由 sync 补取。
    unlisten = onMemosChanged((e) => {
      if (e.note_id !== myId || destroyed) return;
      wantSeq = Math.max(wantSeq, seqOf(e.doc));
      if (!loaded || inflight) return;
      applyRemote(e.doc);
    });
    await unlisten;
    // 上一个编辑器实例离开时交的最后一份先落定,再取盘;取盘期间若有更新的事件,再取一次。
    await memoDocIdle(myId);
    // 取盘失败(锁超时/临时 IO)不放弃:退避重试,成功取得一致快照后才开放编辑(Codex 审出)
    for (let attempt = 0; !destroyed; attempt++) {
      try {
        const doc = await memoDoc(myId);
        if (destroyed) return;
        if (seqOf(doc) >= seqOf(base)) {
          setBase(doc);
          setContent(doc.text);
        }
        error = "";
        break;
      } catch (e) {
        error = t("memo.failed", { e: String(e) });
        await new Promise((r) => setTimeout(r, Math.min(8000, 500 * 2 ** attempt)));
      }
    }
    if (destroyed) return;
    loaded = true;
    v.setProps({});
    // 上次离页没存上的草稿:合回来再存
    const draft = takeDraft(myId);
    if (draft) {
      const merged = mergeMemoDoc(normalize(draft.base), normalize(draft.text), baseSer);
      if (merged !== serialize()) setContent(merged);
    }
    // 剩余的「已知更新版本」交给持续同步;盘面写法不同或有草稿就存一次
    void sync();
    if (serialize() !== base.text) scheduleSave();
  });

  onDestroy(() => {
    destroyed = true;
    clearTimeout(timer);
    clearTimeout(retryTimer);
    // 离开页面/切笔记:最后一份改动交给保存队列(排在在途那份之后,失败在队列里重试)。
    if (editor) {
      if (loaded) {
        const text = serialize();
        // 有保存在途:总是追加一份(在途那份若失败,这份以原盘面为基线把它的内容一并存下;
        // 若成功且内容相同,后端判无变化不写)。没在途:与盘面原文不同才交。
        const need = inflight ? true : text !== base.text;
        if (need && text.length <= MEMO_DOC_MAX_CHARS) {
          stashDraft(myId, { text, base: base.text });
          void saveSerial(myId, text, { base: base.text, prev: inflight }, true)
            .done.then(() => clearDraft(myId, text))
            .catch(() => {});
        }
      }
      void editor.destroy();
      editor = null;
    }
    void unlisten?.then((f) => f());
  });

  /** 把光标放到文末的新一行(笔记页按 M / 收起态点「写」)。 */
  export function focusNewLine() {
    if (!open) onToggle();
    queueMicrotask(() => {
      const v = view();
      if (!v) return;
      const { state } = v;
      const last = state.doc.lastChild;
      let tr = state.tr;
      if (!(last && last.isTextblock && last.content.size === 0)) {
        tr = tr.insert(state.doc.content.size, state.schema.nodes.paragraph.create());
      }
      tr = tr.setSelection(Selection.atEnd(tr.doc)).scrollIntoView();
      v.dispatch(tr);
      v.focus();
    });
  }

  function focusEnd(e: MouseEvent) {
    if (e.target !== e.currentTarget) return;
    const v = view();
    if (!v) return;
    v.dispatch(v.state.tr.setSelection(Selection.atEnd(v.state.doc)));
    v.focus();
  }
</script>

<aside class="memo-doc" class:collapsed={!open} aria-label={t("memo.column.title")}>
  {#if !open}
    <div class="track">
      <button class="toggle" onclick={onToggle} title={t("memo.column.open")} aria-label={t("memo.column.open")}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M10 3.5L5.5 8l4.5 4.5" /></svg>
      </button>
      <span class="count-dot">{stamps.length}</span>
      <div class="axis">
        {#each stamps as s, i (i)}
          <button class="dot" style="top: {(s / span) * 100}%" title={fmtClock(s)} onclick={() => onPickMs(s)} aria-label={fmtClock(s)}></button>
        {/each}
        {#if elapsedMs !== null}
          <span class="now" style="top: {(elapsedMs / span) * 100}%"></span>
        {/if}
      </div>
    </div>
  {/if}
  <div class="open" hidden={!open}>
    <header>
      <span class="title">{t("memo.column.title")}</span>
      <span class="count">{t("memo.doc.stamps", { n: stamps.length })}</span>
      <button class="toggle" onclick={onToggle} title={t("memo.column.close")} aria-label={t("memo.column.close")}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d="M6 3.5L10.5 8 6 12.5" /></svg>
      </button>
    </header>
    {#if error}<p class="err" role="status">{error}</p>{/if}
    <!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
    <div class="doc" bind:this={host} onclick={focusEnd}></div>
  </div>
</aside>

<style>
  .memo-doc {
    position: sticky;
    /* 吸顶在操作栏底缘(及其 14px 渐隐)之下。原文短时宿主页给正文区设了最小高度,
       保证这里总有吸顶余量,标题不会落进渐隐里被盖淡。 */
    top: calc(var(--memo-col-top, 4.5rem) + 14px);
    align-self: start;
    height: calc(100vh - var(--memo-col-top, 4.5rem) - 14px - 3rem);
    display: flex;
    flex-direction: column;
    border-left: 1px solid var(--hairline);
    font-size: 0.85rem;
    min-width: 0;
  }
  .open { display: flex; flex-direction: column; height: 100%; min-height: 0; padding-left: 0.5rem; }
  .open[hidden] { display: none; }
  header { display: flex; align-items: center; gap: 0.35rem; flex: none; padding: 0 0 0.35rem 2.6rem; }
  .title { font-weight: 600; font-size: 0.8rem; }
  .count { color: var(--ink-faint); font-size: 0.75rem; }
  .err { margin: 0 0 0.35rem 2.6rem; font-size: 0.75rem; color: var(--danger-ink, var(--danger)); }
  .toggle {
    all: unset;
    cursor: pointer;
    display: grid;
    place-items: center;
    width: 1.5rem;
    height: 1.5rem;
    border-radius: var(--radius-sm, 4px);
    color: var(--ink-faint);
    margin-left: auto;
  }
  .toggle:hover { background: var(--surface-soft); color: var(--ink); }
  .toggle:focus-visible { outline: 2px solid var(--ring, var(--accent)); }
  .doc { flex: 1; min-height: 0; overflow-y: auto; cursor: text; }

  /* Milkdown/ProseMirror 内容:左边留 2.6rem 槽放时间 */
  .doc :global(.ProseMirror) { outline: none; padding: 0 0.25rem 40vh 2.6rem; line-height: 1.55; color: var(--ink); min-height: 100%; white-space: pre-wrap; word-break: break-word; }
  .doc :global(.ProseMirror > *:first-child) { margin-top: 0; }
  .doc :global(.ProseMirror p) { margin: 0 0 0.45rem; position: relative; }
  .doc :global(.ProseMirror h1),
  .doc :global(.ProseMirror h2),
  .doc :global(.ProseMirror h3),
  .doc :global(.ProseMirror h4),
  .doc :global(.ProseMirror h5),
  .doc :global(.ProseMirror h6) { position: relative; margin: 0.7rem 0 0.35rem; line-height: 1.35; font-weight: 600; }
  .doc :global(.ProseMirror h1) { font-size: 1.05rem; }
  .doc :global(.ProseMirror h2) { font-size: 0.98rem; }
  .doc :global(.ProseMirror h3),
  .doc :global(.ProseMirror h4),
  .doc :global(.ProseMirror h5),
  .doc :global(.ProseMirror h6) { font-size: 0.9rem; }
  .doc :global(.ProseMirror ul),
  .doc :global(.ProseMirror ol) { margin: 0 0 0.45rem; padding-left: 1.1rem; }
  .doc :global(.ProseMirror li p) { margin: 0 0 0.15rem; }
  .doc :global(.ProseMirror li[data-item-type="task"]) { list-style: none; }
  .doc :global(.ProseMirror li[data-item-type="task"]::before) {
    content: "☐";
    position: absolute;
    margin-left: -1.1rem;
    color: var(--ink-secondary);
  }
  .doc :global(.ProseMirror li[data-item-type="task"][data-checked="true"]::before) { content: "☑"; }
  .doc :global(.ProseMirror li[data-item-type="task"][data-checked="true"] p) { color: var(--ink-faint); text-decoration: line-through; }
  .doc :global(.ProseMirror blockquote) { margin: 0 0 0.45rem; padding-left: 0.6rem; border-left: 2px solid var(--hairline-strong); color: var(--ink-secondary); }
  .doc :global(.ProseMirror code) { font-size: 0.8em; background: var(--surface-soft); padding: 0 0.25em; border-radius: 3px; }
  .doc :global(.ProseMirror a) { color: var(--accent); }
  .doc :global(.ProseMirror .is-empty::before) {
    content: attr(data-placeholder);
    color: var(--ink-faint);
    position: absolute;
    pointer-events: none;
  }
  .doc :global(.memo-stamp) {
    position: absolute;
    left: -2.6rem;
    width: 2.3rem;
    top: 0.12em;
    font-size: 0.7rem;
    font-weight: 400;
    font-style: normal;
    text-decoration: none;
    font-variant-numeric: tabular-nums;
    color: var(--ink-faint);
    cursor: pointer;
    user-select: none;
    line-height: 1.6;
  }
  /* 列表/引用里的段落:槽再往左让出缩进 */
  .doc :global(.ProseMirror li .memo-stamp) { left: calc(-2.6rem - 1.1rem); }
  .doc :global(.ProseMirror blockquote .memo-stamp) { left: calc(-2.6rem - 0.6rem - 2px); }
  .doc :global(.ProseMirror blockquote li .memo-stamp) { left: calc(-2.6rem - 1.7rem - 2px); }
  .doc :global(.memo-stamp:hover) { color: var(--accent); }

  /* 收起态:细轨 */
  .collapsed { align-items: center; }
  .track { display: flex; flex-direction: column; align-items: center; height: 100%; gap: 0.5rem; width: 100%; }
  .track .toggle { margin-left: 0; }
  .count-dot {
    font-size: 0.7rem;
    color: var(--ink-secondary);
    background: var(--surface-soft);
    border-radius: 999px;
    min-width: 1.3rem;
    text-align: center;
    padding: 0.05rem 0.3rem;
  }
  .axis { position: relative; flex: 1; width: 2px; background: var(--hairline); border-radius: 1px; }
  .dot {
    all: unset;
    position: absolute;
    left: 50%;
    width: 9px;
    height: 9px;
    border-radius: 50%;
    background: var(--accent);
    transform: translate(-50%, -50%);
    cursor: pointer;
    box-shadow: 0 0 0 2px var(--canvas);
  }
  .now { position: absolute; left: 50%; width: 12px; height: 2px; background: var(--record); transform: translate(-50%, -50%); }
</style>
