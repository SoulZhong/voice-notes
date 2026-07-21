<script lang="ts">
  import { onDestroy, onMount } from "svelte";
  import {
    createRelationBackfillController,
    type RelationBackfillState,
  } from "./relationBackfill";

  let {
    open,
    noteIds = undefined,
    onClose,
    onCompleted = () => {},
  }: {
    open: boolean;
    noteIds?: string[];
    onClose: () => void;
    onCompleted?: () => void | Promise<void>;
  } = $props();

  const controller = createRelationBackfillController();
  let state = $state<RelationBackfillState>(controller.state);
  let dialog: HTMLDialogElement;
  let closeButton: HTMLButtonElement;
  let unsubscribeState: (() => void) | null = null;
  let lastReportedGeneration: number | null = null;

  onMount(() => {
    unsubscribeState = controller.subscribe((next) => {
      state = next;
      if (
        next.published &&
        next.publishedGeneration !== null &&
        next.publishedGeneration !== lastReportedGeneration
      ) {
        lastReportedGeneration = next.publishedGeneration;
        void onCompleted();
      }
    });
  });

  onDestroy(() => {
    unsubscribeState?.();
    unsubscribeState = null;
    controller.dispose();
  });

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) {
      dialog.showModal();
      closeButton?.focus();
      void controller.preview(noteIds);
    } else if (!open && dialog.open) {
      controller.close();
      dialog.close();
    }
  });

  const busy = $derived(
    state.phase === "starting" ||
      state.phase === "running" ||
      state.phase === "cancel-requested" ||
      state.phase === "index-retrying" ||
      state.phase === "waiting-for-index",
  );
  const providerLabel = $derived(state.preview?.provider === "agent" ? "本机 Agent" : "在线接口");

  function closeDialog() {
    if (busy) return;
    controller.close();
    if (dialog.open) dialog.close();
    onClose();
  }

  function handleCancel(event: Event) {
    if (busy) {
      event.preventDefault();
      if (state.phase === "running") void controller.cancel();
      return;
    }
    controller.close();
    onClose();
  }

  function start() {
    void controller.start().catch(() => {});
  }

  function cancel() {
    void controller.cancel().catch(() => {});
  }

  function resume() {
    void controller.resume().catch(() => {});
  }

  function retryIndex() {
    void controller.retryIndex().catch(() => {});
  }
</script>

<dialog bind:this={dialog} oncancel={handleCancel} aria-labelledby="relation-backfill-title">
  <div class="dialog-shell">
    <header>
      <div>
        <p class="eyebrow">语义关系维护</p>
        <h2 id="relation-backfill-title">补建历史关系</h2>
      </div>
      <button
        bind:this={closeButton}
        class="close-button"
        type="button"
        aria-label="关闭关系补建"
        disabled={busy}
        onclick={closeDialog}
      >关闭</button>
    </header>

    <div class="status" aria-live="polite" aria-atomic="false">
      {#if state.phase === "preview-loading"}
        <p class="lead">正在读取可补建的笔记与当前执行体。</p>
      {:else if state.phase === "preview-error"}
        <p class="message error">{state.error}</p>
        {#if state.technicalError}
          <details class="technical"><summary>技术详情</summary><pre>{state.technicalError}</pre></details>
        {/if}
        <button class="secondary" type="button" onclick={() => controller.preview(noteIds)}>重新预览</button>
      {:else if state.preview && (state.phase === "preview-ready")}
        <p class="lead">开始前请核对这次补建会处理什么，以及内容将交给谁。</p>
        <dl class="facts">
          <div><dt>笔记数量</dt><dd>{state.preview.note_ids.length}</dd></div>
          <div><dt>执行体</dt><dd>{providerLabel}</dd></div>
          <div><dt>精确模型</dt><dd>{state.preview.model}</dd></div>
          <div><dt>契约版本</dt><dd>{state.preview.contract_version}</dd></div>
        </dl>
        <details class="selection">
          <summary>查看本次笔记 ID</summary>
          <ul>
            {#each state.preview.note_ids as noteId (noteId)}<li>{noteId}</li>{/each}
          </ul>
        </details>
        {#if state.preview.note_ids.length === 0}
          <p class="message">没有需要补建的笔记。现有转写稿不会改变。</p>
        {:else}
          <label class="consent">
            <input
              type="checkbox"
              checked={state.acknowledged}
              onchange={(event) => controller.acknowledge(event.currentTarget.checked)}
            />
            <span>我已确认：将把修订稿发送给当前配置的执行体。补建只更新关系图谱产物，不修改转写段落与笔记顺序。</span>
          </label>
        {/if}
      {:else if busy || state.phase === "index-failed" || state.phase === "completed" || state.phase === "partial" || state.phase === "failed" || state.phase === "cancelled"}
        <div class="progress-heading">
          <p class="lead">
            {#if state.phase === "completed"}补建已完成
            {:else if state.phase === "partial"}部分笔记未完成
            {:else if state.phase === "failed"}补建未完成
            {:else if state.phase === "index-failed"}索引发布未完成
            {:else if state.phase === "cancelled"}补建已取消
            {:else if state.phase === "starting"}正在建立安全连接
            {:else if state.phase === "cancel-requested"}正在安全停止
            {:else if state.phase === "index-retrying"}正在重试图谱索引
            {:else if state.phase === "waiting-for-index"}正在发布图谱索引
            {:else}正在补建关系{/if}
          </p>
          <strong>{state.completed} / {state.total}</strong>
        </div>
        <progress max={Math.max(state.total, 1)} value={state.completed}>{state.completed} / {state.total}</progress>
        {#if state.currentNoteId}
          <div class="current"><span>当前笔记</span><strong>{state.currentNoteId}</strong></div>
        {/if}
        {#if state.error}<p class="message error">{state.error}</p>{/if}
        {#if state.technicalError && state.failures.length === 0}
          <details class="technical"><summary>技术详情</summary><pre>{state.technicalError}</pre></details>
        {/if}
        {#if state.indexError}
          <details class="technical"><summary>索引技术详情</summary><pre>{state.indexError}</pre></details>
        {/if}
        {#if state.failures.length > 0}
          <section class="failures" aria-labelledby="relation-backfill-failures">
            <h3 id="relation-backfill-failures">失败详情</h3>
            <ul>
              {#each state.failures as failure (`${failure.note_id}:${failure.error}`)}
                <li>
                  <strong>{failure.note_id || "图谱索引"}</strong>
                  <span>此项未完成，可以重新预览后重试。</span>
                  <details class="technical"><summary>技术详情</summary><pre>{failure.error}</pre></details>
                </li>
              {/each}
            </ul>
          </section>
        {/if}
      {/if}
    </div>

    <footer>
      {#if state.phase === "preview-ready"}
        <button
          class="primary"
          type="button"
          disabled={!state.acknowledged || !state.preview || state.preview.note_ids.length === 0}
          onclick={start}
        >开始补建</button>
      {:else if state.phase === "running"}
        <button class="secondary danger" type="button" onclick={cancel}>取消补建</button>
      {:else if state.phase === "cancel-requested"}
        <button class="secondary" type="button" disabled>等待取消</button>
      {:else if state.phase === "index-failed"}
        <button class="primary" type="button" onclick={retryIndex}>重试索引</button>
      {:else if state.phase === "failed" || state.phase === "partial" || state.phase === "cancelled"}
        <button class="primary" type="button" onclick={resume}>继续未完成笔记</button>
      {/if}
    </footer>
  </div>
</dialog>

<style>
  dialog {
    width: min(42rem, calc(100vw - 2rem));
    max-height: min(46rem, calc(100dvh - 2rem));
    padding: 0;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    background: var(--canvas);
    color: var(--ink);
    box-shadow: 0 24px 72px color-mix(in srgb, var(--ink) 20%, transparent);
  }
  dialog::backdrop { background: color-mix(in srgb, var(--ink) 32%, transparent); }
  .dialog-shell { display: grid; max-height: inherit; grid-template-rows: auto minmax(0, 1fr) auto; }
  header, footer { display: flex; align-items: center; justify-content: space-between; gap: 1rem; padding: 1rem 1.25rem; }
  header { border-bottom: 1px solid var(--hairline); }
  footer { justify-content: flex-end; border-top: 1px solid var(--hairline); }
  h2, h3, p { margin: 0; }
  h2 { margin-top: 0.15rem; font-size: 1.15rem; font-weight: 500; letter-spacing: -0.01em; }
  h3 { font-size: 0.83rem; font-weight: 500; }
  .eyebrow { color: var(--ink-faint); font-size: 0.72rem; letter-spacing: 0.08em; }
  .status { overflow: auto; padding: clamp(1rem, 3vw, 1.5rem); }
  .lead { color: var(--ink-secondary); font-size: 0.9rem; line-height: 1.6; }
  .facts { display: grid; grid-template-columns: repeat(2, minmax(0, 1fr)); margin: 1.25rem 0; border-block: 1px solid var(--hairline); }
  .facts div { min-width: 0; padding: 0.85rem 0; }
  .facts div:nth-child(odd) { padding-right: 1rem; }
  .facts div:nth-child(even) { padding-left: 1rem; border-left: 1px solid var(--hairline); }
  .facts div:nth-child(n + 3) { border-top: 1px solid var(--hairline); }
  dt { margin-bottom: 0.2rem; color: var(--ink-faint); font-size: 0.75rem; }
  dd { margin: 0; color: var(--ink); font-size: 0.9rem; font-weight: 500; overflow-wrap: anywhere; }
  .selection { margin: 0 0 1rem; color: var(--ink-secondary); font-size: 0.82rem; }
  .selection summary { cursor: pointer; min-height: 2.75rem; display: flex; align-items: center; }
  .selection ul, .failures ul { margin: 0; padding: 0; list-style: none; }
  .selection li { padding: 0.35rem 0; color: var(--ink); overflow-wrap: anywhere; }
  .consent { display: grid; grid-template-columns: auto 1fr; gap: 0.75rem; align-items: start; padding: 1rem; background: var(--surface); border-radius: var(--radius-md); color: var(--ink); font-size: 0.86rem; line-height: 1.6; cursor: pointer; }
  .consent input { width: 1.1rem; height: 1.1rem; margin-top: 0.2rem; accent-color: var(--accent); }
  .progress-heading { display: flex; align-items: baseline; justify-content: space-between; gap: 1rem; }
  .progress-heading strong { font-variant-numeric: tabular-nums; }
  progress { width: 100%; height: 0.45rem; margin: 1rem 0; accent-color: var(--accent); }
  .current { display: grid; gap: 0.2rem; padding: 0.8rem 0; }
  .current span { color: var(--ink-faint); font-size: 0.75rem; }
  .current strong { font-size: 0.88rem; overflow-wrap: anywhere; }
  .message { margin-top: 0.75rem; color: var(--ink-secondary); font-size: 0.85rem; line-height: 1.55; overflow-wrap: anywhere; }
  .message.error { color: var(--danger); }
  .failures { margin-top: 1.25rem; }
  .failures li { display: grid; gap: 0.25rem; padding: 0.75rem 0; border-bottom: 1px solid var(--hairline); overflow-wrap: anywhere; }
  .failures li span { color: var(--ink-secondary); font-size: 0.82rem; line-height: 1.5; }
  .technical { margin-top: 0.65rem; color: var(--ink-secondary); font-size: 0.78rem; }
  .technical summary { min-height: 2.25rem; display: flex; align-items: center; cursor: pointer; }
  .technical pre { max-height: 12rem; margin: 0.35rem 0 0; overflow: auto; white-space: pre-wrap; overflow-wrap: anywhere; color: var(--ink); font: 0.76rem/1.55 ui-monospace, SFMono-Regular, Menlo, monospace; }
  button { min-height: 2.25rem; padding: 0.45rem 0.85rem; border-radius: var(--radius-md); font: inherit; cursor: pointer; }
  button:focus-visible, input:focus-visible, summary:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  button:disabled { cursor: default; opacity: 0.48; }
  .primary { border: 1px solid var(--primary); border-radius: var(--radius-full); background: var(--primary); color: var(--on-primary); box-shadow: var(--shadow-btn); }
  .primary:hover:not(:disabled) { background: var(--primary-pressed); }
  .primary:active:not(:disabled) { transform: translateY(0.5px); }
  .secondary, .close-button { border: 1px solid var(--hairline); background: var(--surface); color: var(--ink); }
  .danger { color: var(--danger); }
  @media (max-width: 34rem) {
    dialog { width: calc(100vw - 1rem); max-height: calc(100dvh - 1rem); }
    header, footer { padding: 0.8rem 1rem; }
    .facts { grid-template-columns: 1fr; }
    .facts div:nth-child(n) { padding: 0.7rem 0; border-left: 0; }
    .facts div:nth-child(n + 2) { border-top: 1px solid var(--hairline); }
  }
  @media (pointer: coarse) { button, .selection summary, .consent, .technical summary { min-height: 44px; } }
  @media (prefers-reduced-motion: reduce) { dialog { scroll-behavior: auto; } }
</style>
