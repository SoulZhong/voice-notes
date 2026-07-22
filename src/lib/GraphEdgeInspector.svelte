<script lang="ts">
  import { kindLabel, type GraphEdgeDetailItem } from "$lib/graph";
  import { formatDate, formatDuration } from "$lib/notes";

  let {
    perspective,
    leftName,
    rightName,
    items,
    loading,
    error,
    onClose,
    onRetry,
    onPick,
  }: {
    perspective: "note" | "entity";
    leftName: string;
    rightName: string;
    items: GraphEdgeDetailItem[];
    loading: boolean;
    error: string;
    onClose: () => void;
    onRetry: () => void;
    onPick: (item: GraphEdgeDetailItem) => void;
  } = $props();

  const title = $derived(perspective === "note" ? "共用实体" : "共同出现的笔记");
  const countLabel = $derived(
    perspective === "note" ? `${items.length} 个实体` : `${items.length} 篇笔记`,
  );
</script>

<header class="header">
  <div>
    <p class="eyebrow">连接详情</p>
    <h2>{title}</h2>
  </div>
  <button class="close" type="button" aria-label="关闭连接详情" onclick={onClose}>×</button>
</header>

<div class="endpoints" aria-label="连接两端">
  <span>{leftName}</span>
  <i aria-hidden="true"></i>
  <span>{rightName}</span>
</div>

{#if loading}
  <p class="state" aria-live="polite">正在读取连接内容</p>
{:else if error}
  <div class="state" aria-live="polite">
    <p>{error}</p>
    <button type="button" onclick={onRetry}>重新读取</button>
  </div>
{:else}
  <div class="list-heading">
    <p>{perspective === "note" ? "两篇笔记都提到了" : "两个实体都出现在"}</p>
    <span>{countLabel}</span>
  </div>
  {#if items.length > 0}
    <ul>
      {#each items as item (item.id)}
        <li>
          <button type="button" onclick={() => onPick(item)}>
            <span class="item-name">{item.name}</span>
            {#if item.kind}
              <small>{kindLabel(item.kind)}</small>
            {:else}
              <small>{formatDate(item.started_at ?? "")} · {formatDuration(item.duration_secs)}</small>
            {/if}
          </button>
        </li>
      {/each}
    </ul>
  {:else}
    <p class="state">这条连接已变化，请刷新图谱后再试。</p>
  {/if}
{/if}

<style>
  .header { display: flex; align-items: flex-start; justify-content: space-between; gap: 16px; }
  .eyebrow { margin: 0 0 5px; color: var(--ink-faint); font-size: 0.7rem; letter-spacing: 0.08em; }
  h2 { margin: 0; color: var(--ink); font-size: 1.15rem; font-weight: 550; letter-spacing: -0.02em; }
  .close {
    width: 36px;
    height: 36px;
    padding: 0;
    border: 0;
    border-radius: var(--radius-full);
    background: transparent;
    color: var(--ink-secondary);
    font: inherit;
    font-size: 1.3rem;
    cursor: pointer;
  }
  .close:hover { background: var(--surface-soft); color: var(--ink); }
  .endpoints {
    display: grid;
    grid-template-columns: minmax(0, 1fr) 28px minmax(0, 1fr);
    align-items: center;
    gap: 8px;
    margin: 24px 0 28px;
    color: var(--ink);
    font-size: 0.86rem;
    line-height: 1.45;
  }
  .endpoints span { overflow-wrap: anywhere; }
  .endpoints span:last-child { text-align: right; }
  .endpoints i { height: 1px; background: var(--accent); opacity: 0.75; }
  .list-heading { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; padding-bottom: 9px; border-bottom: 1px solid var(--hairline); }
  .list-heading p { margin: 0; color: var(--ink-secondary); font-size: 0.78rem; }
  .list-heading span { flex: none; color: var(--ink-faint); font-size: 0.72rem; }
  ul { margin: 0; padding: 0; list-style: none; }
  li { border-bottom: 1px solid var(--hairline); }
  li button {
    display: flex;
    width: 100%;
    min-height: 48px;
    align-items: center;
    justify-content: space-between;
    gap: 12px;
    padding: 11px 2px;
    border: 0;
    background: transparent;
    color: var(--ink);
    font: inherit;
    font-size: 0.86rem;
    line-height: 1.45;
    text-align: left;
    cursor: pointer;
  }
  li button:hover { color: var(--accent); }
  .item-name { min-width: 0; overflow-wrap: anywhere; }
  li small { flex: none; color: var(--ink-faint); font-size: 0.7rem; white-space: nowrap; }
  .state { margin: 40px 0 0; color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  .state p { margin: 0 0 14px; }
  .state button { min-height: 36px; padding: 7px 12px; border: 1px solid var(--hairline-strong); border-radius: var(--radius-md); background: transparent; color: var(--ink-secondary); font: inherit; cursor: pointer; }
  button:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
</style>
