<script lang="ts">
  import type { KnowledgeFilter } from "$lib/knowledge";

  let {
    filter,
    kinds,
    predicates,
    visibleCount,
    totalCount,
    loading = false,
    showingAll = false,
    onChange,
    onCollapse,
    onShowAll,
  }: {
    filter: KnowledgeFilter;
    kinds: { value: string; label: string }[];
    predicates: { value: string; label: string }[];
    visibleCount: number;
    totalCount: number;
    loading?: boolean;
    showingAll?: boolean;
    onChange: (filter: KnowledgeFilter) => void;
    onCollapse: () => void;
    onShowAll: () => void;
  } = $props();

  const activeCount = $derived(
    filter.entity_kinds.length +
      filter.predicate_types.length +
      (filter.from ? 1 : 0) +
      (filter.to ? 1 : 0) +
      (filter.include_history ? 1 : 0) +
      (filter.include_cooccurrence ? 1 : 0),
  );

  function toggleList(key: "entity_kinds" | "predicate_types", value: string) {
    const current = filter[key];
    onChange({
      ...filter,
      [key]: current.includes(value)
        ? current.filter((item) => item !== value)
        : [...current, value].sort(),
    });
  }

  function reset() {
    onChange({
      entity_kinds: [],
      predicate_types: [],
      from: null,
      to: null,
      include_history: false,
      include_cooccurrence: false,
    });
  }
</script>

<div class="map-toolbar" aria-label="知识图谱筛选与视图控制">
  <div class="filter-run">
    <details class="filter-menu">
      <summary>实体类型{filter.entity_kinds.length ? ` · ${filter.entity_kinds.length}` : ""}</summary>
      <fieldset>
        <legend>实体类型</legend>
        {#each kinds as kind (kind.value)}
          <label>
            <input
              type="checkbox"
              checked={filter.entity_kinds.includes(kind.value)}
              onchange={() => toggleList("entity_kinds", kind.value)}
            />
            <span>{kind.label}</span>
          </label>
        {/each}
      </fieldset>
    </details>

    <details class="filter-menu">
      <summary>关系类型{filter.predicate_types.length ? ` · ${filter.predicate_types.length}` : ""}</summary>
      <fieldset>
        <legend>关系类型</legend>
        {#each predicates as predicate (predicate.value)}
          <label>
            <input
              type="checkbox"
              checked={filter.predicate_types.includes(predicate.value)}
              onchange={() => toggleList("predicate_types", predicate.value)}
            />
            <span>{predicate.label}</span>
          </label>
        {/each}
      </fieldset>
    </details>

    <details class="filter-menu date-menu">
      <summary>有效时间{filter.from || filter.to ? " · 已设定" : ""}</summary>
      <div class="date-fields">
        <label>
          <span>开始日期</span>
          <input
            type="date"
            value={filter.from ?? ""}
            onchange={(event) =>
              onChange({ ...filter, from: event.currentTarget.value || null })}
          />
        </label>
        <label>
          <span>结束日期</span>
          <input
            type="date"
            value={filter.to ?? ""}
            onchange={(event) => onChange({ ...filter, to: event.currentTarget.value || null })}
          />
        </label>
      </div>
    </details>

    <label class="inline-toggle">
      <input
        type="checkbox"
        checked={filter.include_history}
        onchange={(event) =>
          onChange({ ...filter, include_history: event.currentTarget.checked })}
      />
      <span>{filter.include_history ? "包含历史关系" : "仅当前关系"}</span>
    </label>

    <label class="inline-toggle weak">
      <input
        type="checkbox"
        checked={filter.include_cooccurrence}
        onchange={(event) =>
          onChange({ ...filter, include_cooccurrence: event.currentTarget.checked })}
      />
      <span>显示共现弱连接</span>
    </label>
  </div>

  <div class="view-run">
    <span class="view-count" aria-live="polite">
      {loading ? "正在更新关系" : `${visibleCount} / ${totalCount} 个实体`}
    </span>
    {#if activeCount > 0}
      <button type="button" class="text-action" onclick={reset}>重置 {activeCount} 项筛选</button>
    {/if}
    <button type="button" class="text-action" onclick={onCollapse}>收起到主干</button>
    <button type="button" class:active={showingAll} class="show-all" onclick={onShowAll}>
      {showingAll ? "已显示全部" : "显示全部"}
    </button>
  </div>
</div>

<style>
  .map-toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 8px 16px;
    min-height: 44px;
    padding: 6px 10px;
    border-bottom: 1px solid var(--hairline);
    background: var(--canvas);
    color: var(--ink-secondary);
    font-size: 0.76rem;
  }
  .filter-run, .view-run { display: flex; align-items: center; gap: 6px; min-width: 0; }
  .filter-run { flex-wrap: wrap; }
  .view-run { justify-content: flex-end; white-space: nowrap; }
  details { position: relative; }
  summary, button, .inline-toggle {
    min-height: 32px;
    box-sizing: border-box;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-full);
    background: transparent;
    color: inherit;
    font: inherit;
  }
  summary {
    display: flex;
    align-items: center;
    padding: 6px 11px;
    cursor: pointer;
    list-style: none;
  }
  summary::-webkit-details-marker { display: none; }
  summary::after { content: "⌄"; margin-left: 6px; color: var(--ink-faint); }
  details[open] summary, summary:hover, button:hover, .inline-toggle:hover {
    border-color: var(--hairline-strong);
    background: var(--surface-soft);
    color: var(--ink);
  }
  fieldset, .date-fields {
    position: absolute;
    z-index: 20;
    top: calc(100% + 6px);
    left: 0;
    display: grid;
    gap: 3px;
    min-width: 180px;
    max-height: min(360px, 60vh);
    margin: 0;
    padding: 8px;
    overflow: auto;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-lg);
    background: var(--surface-press);
    box-shadow: var(--shadow-popover);
  }
  legend { position: absolute; width: 1px; height: 1px; overflow: hidden; clip-path: inset(50%); }
  fieldset label {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 34px;
    padding: 2px 7px;
    border-radius: var(--radius-md);
    color: var(--ink);
    white-space: nowrap;
  }
  fieldset label:hover { background: var(--surface-soft); }
  input[type="checkbox"] { accent-color: var(--accent); }
  .date-fields { min-width: 220px; gap: 10px; padding: 12px; }
  .date-fields label { display: grid; gap: 5px; color: var(--ink-secondary); }
  .date-fields input {
    min-height: 36px;
    box-sizing: border-box;
    padding: 5px 8px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-md);
    background: var(--canvas);
    color: var(--ink);
    font: inherit;
  }
  .inline-toggle { display: inline-flex; align-items: center; gap: 6px; padding: 5px 10px; cursor: pointer; }
  .weak { border-style: dashed; }
  button { padding: 5px 10px; cursor: pointer; }
  .text-action { border-color: transparent; }
  .show-all { color: var(--accent); border-color: var(--accent-tint); }
  .show-all.active { background: var(--accent-tint); }
  .view-count { color: var(--ink-faint); font-variant-numeric: tabular-nums; }
  summary:focus-visible, button:focus-visible, label:has(input:focus-visible), input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  @media (max-width: 980px) {
    .map-toolbar { align-items: flex-start; flex-direction: column; }
    .view-run { width: 100%; justify-content: flex-start; overflow-x: auto; }
  }
  @media (pointer: coarse) {
    summary, button, .inline-toggle, fieldset label, .date-fields input { min-height: 44px; }
  }
</style>
