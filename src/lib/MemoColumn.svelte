<script lang="ts">
  // 笔记页备注栏:按时间列出全部备注。点一条 = 滚到正文里对应的书签并闪一下,
  // **不跳播**(2026-11-03 拍板:看备注和听录音是两件事)。收起时只剩一枚竖条按钮。
  import { t } from "$lib/i18n/index.svelte";
  import { fmtClock, inCut, sortMemos, type Memo } from "$lib/memos";

  let {
    memos,
    cuts = [],
    open,
    onToggle,
    onPick,
  }: {
    memos: readonly Memo[];
    cuts?: readonly { start_ms: number; end_ms: number }[];
    open: boolean;
    onToggle: () => void;
    onPick: (m: Memo) => void;
  } = $props();

  const sorted = $derived(sortMemos(memos));
</script>

{#if open}
  <aside class="memo-col" aria-label={t("memo.column.title")}>
    <div class="head">
      <span class="title">{t("memo.column.title")} · {t("memo.column.count", { n: sorted.length })}</span>
      <button class="toggle" title={t("memo.column.close")} aria-label={t("memo.column.close")} onclick={onToggle}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M6 4l4 4-4 4" />
        </svg>
      </button>
    </div>
    {#if sorted.length === 0}
      <p class="empty">{t("memo.column.emptyNote")}</p>
    {:else}
      <ul class="list">
        {#each sorted as m (m.id)}
          <li>
            <button class="item" onclick={() => onPick(m)}>
              <span class="row">
                <span class="time">{fmtClock(m.at_ms)}</span>
                {#if inCut(m.at_ms, cuts)}<span class="cut">{t("memo.cut")}</span>{/if}
              </span>
              <span class="body" class:marker={!m.text}>{m.text || t("memo.marker")}</span>
            </button>
          </li>
        {/each}
      </ul>
    {/if}
  </aside>
{:else}
  <button class="memo-col-closed" title={t("memo.column.open")} aria-label={t("memo.column.open")} onclick={onToggle}>
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M3.5 14.5V2" />
      <path d="M3.5 2.5h8.2l-1.9 2.9 1.9 2.9H3.5" />
    </svg>
    {#if sorted.length > 0}<span class="n">{sorted.length}</span>{/if}
  </button>
{/if}

<style>
  .memo-col {
    box-sizing: border-box;
    width: 100%;
    max-height: calc(100vh - var(--memo-col-top, 0px) - 24px);
    overflow-y: auto;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-xl);
    padding: 8px;
  }
  .head {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.5rem;
    padding: 2px 4px 6px;
  }
  .title {
    font-size: 0.78rem;
    font-weight: 500;
    color: var(--ink-faint);
  }
  .toggle,
  .memo-col-closed {
    display: inline-flex;
    align-items: center;
    gap: 0.3rem;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-md);
    padding: 0.2rem;
    color: var(--ink-secondary);
    cursor: pointer;
    transition:
      background 120ms ease,
      color 120ms ease;
  }
  .toggle:hover,
  .memo-col-closed:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .memo-col-closed {
    flex-direction: column;
    padding: 0.45rem 0.35rem;
    border-color: var(--hairline);
    background: var(--surface);
  }
  .n {
    font-size: 0.72rem;
    font-variant-numeric: tabular-nums;
  }
  .empty {
    margin: 0;
    padding: 6px;
    font-size: 0.8rem;
    line-height: 1.5;
    color: var(--ink-faint);
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .item {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    width: 100%;
    text-align: left;
    background: transparent;
    border: 1px solid transparent;
    border-radius: var(--radius-md);
    padding: 6px 8px;
    font: inherit;
    font-size: 0.84rem;
    font-weight: 400;
    color: var(--ink);
    cursor: pointer;
    transition:
      background 120ms ease,
      border-color 120ms ease;
  }
  .item:hover {
    background: var(--warning-tint);
    border-color: var(--warning-line);
  }
  .row {
    display: flex;
    align-items: center;
    gap: 0.4rem;
  }
  .time {
    font-size: 0.74rem;
    font-weight: 500;
    font-variant-numeric: tabular-nums;
    color: var(--warning-ink);
  }
  .cut {
    font-size: 0.68rem;
    padding: 0 0.4em;
    color: var(--ink-secondary);
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-full);
    white-space: nowrap;
  }
  .body {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    line-height: 1.5;
  }
  .body.marker {
    color: var(--ink-secondary);
  }
</style>
