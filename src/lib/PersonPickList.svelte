<script lang="ts">
  // 选人候选列表:录音页说话人面板与详情页「合并到…」菜单共用同一份渲染/过滤逻辑
  // (personPick.ts)。输入框语义两处不同(改名+过滤 vs 纯检索),留在宿主组件里。
  import { speakerInk } from "$lib/notes";
  import type { PersonSummary } from "$lib/people";
  import { dupNameSet, filterPeople, personLabel, recentLabel } from "$lib/personPick";
  import { t } from "$lib/i18n/index.svelte";

  let {
    people,
    query = "",
    excludeIds = [],
    onpick,
    emptyText = t("speakers.noMatch"),
    selectedId = null,
  }: {
    people: PersonSummary[];
    /** 检索词(空串=全量);宿主决定何时把这个传成非空(改名输入/纯检索输入)。 */
    query?: string;
    /** 排除的人物 id(如「合并到…」菜单要去掉当前人物自己)。 */
    excludeIds?: string[];
    onpick: (p: PersonSummary) => void;
    /** 候选为空时的提示文案;宿主按"库本就是空"还是"过滤后为空"传不同文案。 */
    emptyText?: string;
    /** 当前已关联/已选中的人物 id(可选):命中行末尾加勾选标记。 */
    selectedId?: string | null;
  } = $props();

  const candidates = $derived(filterPeople(people.filter((p) => !excludeIds.includes(p.id)), query));
  const dups = $derived(dupNameSet(people));
</script>

{#if candidates.length > 0}
  <div class="list">
    {#each candidates as p (p.id)}
      <button class="row" onclick={() => onpick(p)}>
        <!-- 色点用 ink 变体:soft 底(15% alpha)做 9px 点几乎不可见 -->
        <span class="dot" style="background: {speakerInk(p.id, 'mic')}"></span>
        <span class="row-label">{personLabel(p)}</span>
        {#if !p.name || dups.has(p.name)}
          <!-- 未命名/重名条目:补最近出现日期,两行不至于一模一样 -->
          <span class="row-sub">{recentLabel(p)}</span>
        {/if}
        {#if selectedId && p.id === selectedId}
          <svg class="tick" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M3 8.5l3.2 3.2L13 5" />
          </svg>
        {/if}
      </button>
    {/each}
  </div>
{:else}
  <div class="empty">{emptyText}</div>
{/if}

<style>
  .list {
    max-height: 13rem;
    overflow-y: auto;
  }
  /* 菜单行:全宽、radius-md、hover surface-soft */
  .row {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    width: 100%;
    padding: 0.38rem 0.55rem;
    background: none;
    border: none;
    border-radius: var(--radius-md);
    color: var(--ink);
    font: inherit;
    text-align: left;
    cursor: pointer;
  }
  .row:hover {
    background: var(--surface-soft);
  }
  /* 人物色点:与徽章同一调色板按 P 号取色(跨会议恒定) */
  .dot {
    width: 9px;
    height: 9px;
    border-radius: var(--radius-full);
    flex: none;
  }
  .row-label {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  /* 行内次要信息(最近出现日期):faint 小字,不与名字争 */
  .row-sub {
    color: var(--ink-faint);
    font-size: 0.72rem;
    flex: none;
  }
  .tick {
    color: var(--accent);
    flex: none;
  }
  .empty {
    padding: 0.38rem 0.55rem 0.45rem;
    color: var(--ink-faint);
  }
</style>
