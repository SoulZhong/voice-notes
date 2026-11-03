<script lang="ts">
  // 笔记页备注栏:按时间列出全部备注。点一条 = 滚到正文里对应的书签并闪一下,
  // **不跳播**(2026-11-03 拍板:看备注和听录音是两件事)。收起时只剩一枚竖条按钮。
  import { t } from "$lib/i18n/index.svelte";
  import { fmtClock, inCut, sortMemos, type Memo } from "$lib/memos";
  import { Button } from "$lib/ui/components/button";
  import { Badge } from "$lib/ui/components/badge";

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
      <Button variant="ghost" size="icon-xs" class="text-muted-foreground" title={t("memo.column.close")} aria-label={t("memo.column.close")} onclick={onToggle}>
        <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M6 4l4 4-4 4" />
        </svg>
      </Button>
    </div>
    {#if sorted.length === 0}
      <p class="empty">{t("memo.column.emptyNote")}</p>
    {:else}
      <ul class="list">
        {#each sorted as m (m.id)}
          <li>
            <Button
              variant="ghost"
              class="item h-auto w-full flex-col items-stretch justify-start gap-0.5 px-2 py-1.5 text-left font-normal whitespace-normal"
              onclick={() => onPick(m)}
            >
              <span class="row">
                <span class="time">{fmtClock(m.at_ms)}</span>
                {#if inCut(m.at_ms, cuts)}<Badge variant="outline" class="h-4 px-1.5 text-[0.68rem] font-normal text-muted-foreground">{t("memo.cut")}</Badge>{/if}
              </span>
              <span class="body" class:marker={!m.text}>{m.text || t("memo.marker")}</span>
            </Button>
          </li>
        {/each}
      </ul>
    {/if}
  </aside>
{:else}
  <Button
    variant="outline"
    class="h-auto flex-col gap-1 bg-card px-1.5 py-2 text-muted-foreground"
    title={t("memo.column.open")}
    aria-label={t("memo.column.open")}
    onclick={onToggle}
  >
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M3.5 14.5V2" />
      <path d="M3.5 2.5h8.2l-1.9 2.9 1.9 2.9H3.5" />
    </svg>
    {#if sorted.length > 0}<span class="n">{sorted.length}</span>{/if}
  </Button>
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
  /* 备注条悬停用警示黄(与正文书签同色系),覆盖 ghost 的中性悬停底。 */
  .list :global(.item) {
    font-size: 0.84rem;
    color: var(--ink);
  }
  .list :global(.item:hover) {
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
  .body {
    white-space: pre-wrap;
    overflow-wrap: anywhere;
    line-height: 1.5;
  }
  .body.marker {
    color: var(--ink-secondary);
  }
</style>
