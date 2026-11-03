<script lang="ts">
  // 录制页右侧备注栏:按时间列出本场全部备注。点一条 → 页面跳到对应书签并暂停跟随
  // (不跳播放——会中本来就不回放)。栏可收起;展开/收起由页面按容器宽度与用户偏好决定。
  import { t } from "$lib/i18n/index.svelte";
  import { fmtClock, type Memo } from "$lib/memos";

  let {
    memos,
    open,
    shortcutLabel,
    onToggle,
    onPick,
  }: {
    memos: readonly Memo[];
    open: boolean;
    shortcutLabel: string;
    onToggle: () => void;
    onPick: (m: Memo) => void;
  } = $props();
</script>

<aside class="memo-col" class:closed={!open}>
  <header>
    {#if open}
      <span class="title">{t("memo.column.title")}</span>
      <span class="count">{t("memo.column.count", { n: memos.length })}</span>
    {/if}
    <button
      class="toggle"
      title={open ? t("memo.column.close") : t("memo.column.open")}
      aria-label={open ? t("memo.column.close") : t("memo.column.open")}
      aria-expanded={open}
      onclick={onToggle}
    >
      <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
        {#if open}<path d="M6 4l4 4-4 4" />{:else}<path d="M10 4L6 8l4 4" />{/if}
      </svg>
      {#if !open && memos.length > 0}<span class="badge">{memos.length}</span>{/if}
    </button>
  </header>
  {#if open}
    {#if memos.length === 0}
      <p class="empty">{t("memo.column.empty", { key: shortcutLabel })}</p>
    {:else}
      <ol>
        {#each memos as m (m.id)}
          <li>
            <button class="item" onclick={() => onPick(m)}>
              <span class="clock">{fmtClock(m.at_ms)}</span>
              {#if m.text}
                <span class="text">{m.text}</span>
              {:else}
                <span class="marker">{t("memo.marker")}</span>
              {/if}
            </button>
          </li>
        {/each}
      </ol>
    {/if}
  {/if}
</aside>

<style>
  .memo-col {
    position: sticky;
    top: var(--memo-col-top, 4.5rem);
    align-self: start;
    max-height: calc(100vh - var(--memo-col-top, 4.5rem) - 6rem);
    overflow-y: auto;
    background: var(--surface);
    border-radius: var(--radius-xl);
    padding: 0.6rem 0.4rem 0.6rem 0.8rem;
    font-size: 0.88rem;
  }
  .memo-col.closed {
    padding: 0.4rem;
    background: transparent;
  }
  header {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    margin-bottom: 0.3rem;
  }
  .title {
    font-weight: 600;
  }
  .count {
    color: var(--ink-faint);
    font-size: 0.8rem;
  }
  .toggle {
    margin-left: auto;
    position: relative;
    border: none;
    background: none;
    color: var(--ink-secondary);
    cursor: pointer;
    padding: 0.2rem;
    border-radius: var(--radius-sm);
  }
  .toggle:hover {
    background: var(--surface-soft);
  }
  .badge {
    position: absolute;
    top: -0.3rem;
    right: -0.4rem;
    font-size: 0.68rem;
    background: var(--accent);
    color: var(--on-primary);
    border-radius: var(--radius-full);
    padding: 0 0.3rem;
  }
  ol {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .item {
    display: flex;
    gap: 0.45rem;
    width: 100%;
    text-align: left;
    border: none;
    background: none;
    color: var(--ink);
    font: inherit;
    padding: 0.35rem 0.4rem;
    border-radius: var(--radius-md);
    cursor: pointer;
  }
  .item:hover {
    background: var(--surface-soft);
  }
  .clock {
    flex: none;
    font-variant-numeric: tabular-nums;
    color: var(--ink-secondary);
    font-size: 0.8rem;
    margin-top: 0.05rem;
  }
  .text {
    white-space: pre-wrap;
    word-break: break-word;
  }
  .marker {
    color: var(--ink-secondary);
  }
  .empty {
    color: var(--ink-secondary);
    margin: 0.3rem 0.2rem;
    line-height: 1.5;
  }
</style>
