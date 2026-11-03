<script lang="ts">
  // 录制页原文流里的一条书签:插在备注时间点上,全文展示、不折叠。
  // 录制中可改文字、可删(删后由页面给「撤销」);时间点在会后笔记页调整。
  import { t } from "$lib/i18n/index.svelte";
  import { editMemo, deleteMemo, fmtClock, MEMO_MAX_CHARS, type Memo } from "$lib/memos";
  import { Button } from "$lib/ui/components/button";
  import { Textarea } from "$lib/ui/components/textarea";

  let {
    memo,
    noteId,
    flash = false,
    hit = false,
    onDeleted,
    onError,
  }: {
    memo: Memo;
    noteId: string;
    flash?: boolean;
    hit?: boolean;
    onDeleted?: (m: Memo) => void;
    onError?: (msg: string) => void;
  } = $props();

  let editing = $state(false);
  let draft = $state("");

  function begin() {
    draft = memo.text;
    editing = true;
  }

  async function commit() {
    if (!editing) return;
    editing = false;
    const next = draft.trim();
    if (next === memo.text || next.length > MEMO_MAX_CHARS) return;
    try {
      await editMemo(noteId, memo, { text: next });
    } catch (e) {
      onError?.(t("memo.failed", { e: String(e) }));
    }
  }

  async function remove() {
    try {
      const gone = await deleteMemo(noteId, memo.id);
      onDeleted?.(gone);
    } catch (e) {
      onError?.(t("memo.failed", { e: String(e) }));
    }
  }
</script>

<div class="memo-bookmark" id="memo-{memo.id}" class:flash class:hit>
  <svg class="flag" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
    <path d="M3.5 14V2.5M3.5 3h8l-1.8 2.8L11.5 8.5h-8" />
  </svg>
  <span class="clock">{fmtClock(memo.at_ms)}</span>
  <div class="body">
    {#if editing}
      <!-- svelte-ignore a11y_autofocus -->
      <Textarea
        class="min-h-0 resize-y rounded-sm bg-card px-1.5 py-0.5 text-[length:inherit] leading-[inherit] field-sizing-fixed md:text-[length:inherit] dark:bg-card"
        autofocus
        rows={2}
        bind:value={draft}
        onkeydown={(e) => {
          if (e.isComposing) return;
          if (e.key === "Enter" && !e.shiftKey) {
            e.preventDefault();
            void commit();
          } else if (e.key === "Escape") {
            editing = false;
          }
        }}
        onblur={() => void commit()}
      />
    {:else if memo.text}
      <span class="text">{memo.text}</span>
    {:else}
      <span class="marker">{t("memo.marker")}</span>
    {/if}
  </div>
  {#if !editing}
    <span class="acts">
      <Button variant="ghost" size="xs" class="h-5 px-1 font-normal text-ring hover:text-ring" onclick={begin}>{memo.text ? t("memo.action.edit") : t("memo.action.addText")}</Button>
      <Button variant="ghost" size="xs" class="h-5 px-1 font-normal text-ring hover:text-ring" onclick={() => void remove()}>{t("memo.action.delete")}</Button>
    </span>
  {/if}
</div>

<style>
  .memo-bookmark {
    position: relative;
    display: flex;
    align-items: flex-start;
    gap: 0.45rem;
    margin: 6px 0 10px -12px;
    padding: 0.4rem 0.6rem 0.4rem 0.7rem;
    border-left: 2px solid var(--accent);
    background: var(--accent-tint);
    border-radius: 0 var(--radius-md) var(--radius-md) 0;
    font-size: 0.92rem;
    line-height: 1.55;
    transition: background 0.3s ease;
  }
  .memo-bookmark.flash {
    background: var(--surface-press);
  }
  .memo-bookmark.hit {
    outline: 1px solid var(--accent);
  }
  .flag {
    flex: none;
    margin-top: 0.2rem;
    color: var(--accent);
  }
  .clock {
    flex: none;
    font-variant-numeric: tabular-nums;
    color: var(--ink-secondary);
    font-size: 0.82rem;
    margin-top: 0.1rem;
  }
  .body {
    flex: 1;
    min-width: 0;
  }
  .text {
    white-space: pre-wrap;
    word-break: break-word;
    color: var(--ink);
  }
  .marker {
    color: var(--ink-secondary);
  }
  .acts {
    flex: none;
    display: flex;
    gap: 0.2rem;
    opacity: 0;
    transition: opacity 0.15s ease;
  }
  .memo-bookmark:hover .acts,
  .memo-bookmark:focus-within .acts {
    opacity: 1;
  }
</style>
