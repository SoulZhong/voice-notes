<script lang="ts">
  import type { KnowledgeFilter } from "$lib/knowledge";
  import { t } from "$lib/i18n/index.svelte";
  import { Button } from "$lib/ui/components/button";
  import { Checkbox } from "$lib/ui/components/checkbox";
  import { Input } from "$lib/ui/components/input";
  import * as Popover from "$lib/ui/components/popover";

  let {
    filter,
    kinds,
    predicates,
    visibleCount,
    totalCount,
    loading = false,
    onChange,
    onCollapse,
  }: {
    filter: KnowledgeFilter;
    kinds: { value: string; label: string }[];
    predicates: { value: string; label: string }[];
    visibleCount: number;
    totalCount: number;
    loading?: boolean;
    onChange: (filter: KnowledgeFilter) => void;
    onCollapse: () => void;
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

  // 筛选触发钮:胶囊外形与旧 summary 一致;弹层宽度与旧 fieldset 一致(min 180px)。
  const triggerClass = "rounded-full font-normal text-muted-foreground aria-expanded:text-foreground";
  const menuClass = "w-auto min-w-[180px] max-h-[min(360px,60vh)] overflow-auto p-2";
</script>

<div class="map-toolbar" aria-label={t("graph.toolbar.aria")}>
  <div class="filter-run">
    <Popover.Root>
      <Popover.Trigger>
        {#snippet child({ props })}
          <Button {...props} variant="outline" size="sm" class={triggerClass}>
            {t("graph.toolbar.entityKinds")}{filter.entity_kinds.length ? ` · ${filter.entity_kinds.length}` : ""}
            <span class="caret" aria-hidden="true">⌄</span>
          </Button>
        {/snippet}
      </Popover.Trigger>
      <Popover.Content align="start" class={menuClass}>
        <fieldset class="options">
          <legend class="sr-only">{t("graph.toolbar.entityKinds")}</legend>
          {#each kinds as kind (kind.value)}
            <label class="option">
              <Checkbox
                checked={filter.entity_kinds.includes(kind.value)}
                onCheckedChange={() => toggleList("entity_kinds", kind.value)}
              />
              <span>{kind.label}</span>
            </label>
          {/each}
        </fieldset>
      </Popover.Content>
    </Popover.Root>

    <Popover.Root>
      <Popover.Trigger>
        {#snippet child({ props })}
          <Button {...props} variant="outline" size="sm" class={triggerClass}>
            {t("graph.toolbar.predicates")}{filter.predicate_types.length ? ` · ${filter.predicate_types.length}` : ""}
            <span class="caret" aria-hidden="true">⌄</span>
          </Button>
        {/snippet}
      </Popover.Trigger>
      <Popover.Content align="start" class={menuClass}>
        <fieldset class="options">
          <legend class="sr-only">{t("graph.toolbar.predicates")}</legend>
          {#if predicates.length === 0}
            <p class="empty-options">{t("graph.toolbar.noPredicates")}</p>
          {:else}
            {#each predicates as predicate (predicate.value)}
              <label class="option">
                <Checkbox
                  checked={filter.predicate_types.includes(predicate.value)}
                  onCheckedChange={() => toggleList("predicate_types", predicate.value)}
                />
                <span>{predicate.label}</span>
              </label>
            {/each}
          {/if}
        </fieldset>
      </Popover.Content>
    </Popover.Root>

    <Popover.Root>
      <Popover.Trigger>
        {#snippet child({ props })}
          <Button {...props} variant="outline" size="sm" class={triggerClass}>
            {t("graph.toolbar.more")}{filter.from || filter.to || filter.include_history || filter.include_cooccurrence ? ` · ${t("graph.toolbar.enabled")}` : ""}
            <span class="caret" aria-hidden="true">⌄</span>
          </Button>
        {/snippet}
      </Popover.Trigger>
      <Popover.Content align="start" class="{menuClass} w-60 gap-2.5 p-3">
        <div class="date-fields">
          <label class="date-field">
            <span>{t("graph.toolbar.from")}</span>
            <Input
              type="date"
              value={filter.from ?? ""}
              onchange={(event) =>
                onChange({ ...filter, from: event.currentTarget.value || null })}
            />
          </label>
          <label class="date-field">
            <span>{t("graph.toolbar.to")}</span>
            <Input
              type="date"
              value={filter.to ?? ""}
              onchange={(event) => onChange({ ...filter, to: event.currentTarget.value || null })}
            />
          </label>
          <label class="option">
            <Checkbox
              checked={filter.include_history}
              onCheckedChange={(checked) => onChange({ ...filter, include_history: checked })}
            />
            <span>{t("graph.toolbar.includeHistory")}</span>
          </label>
          <label class="option">
            <Checkbox
              checked={filter.include_cooccurrence}
              onCheckedChange={(checked) => onChange({ ...filter, include_cooccurrence: checked })}
            />
            <span>{t("graph.toolbar.includeCooccur")}</span>
          </label>
        </div>
      </Popover.Content>
    </Popover.Root>
  </div>

  <div class="view-run">
    <span class="view-count" aria-live="polite">
      {loading ? t("graph.toolbar.updating") : t("graph.toolbar.count", { visible: visibleCount, total: totalCount })}
    </span>
    {#if activeCount > 0}
      <Button variant="ghost" size="sm" class="rounded-full" onclick={reset}>{t("graph.toolbar.reset", { n: activeCount })}</Button>
    {/if}
    <Button variant="ghost" size="sm" class="rounded-full" onclick={onCollapse}>{t("graph.toolbar.collapse")}</Button>
  </div>
</div>

<style>
  .map-toolbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    flex-wrap: nowrap;
    gap: 8px 16px;
    min-height: 44px;
    padding: 6px 10px;
    overflow-x: auto;
    overflow-y: hidden;
    border-bottom: 1px solid var(--hairline);
    background: var(--canvas);
    color: var(--ink-secondary);
    font-size: 0.76rem;
    white-space: nowrap;
    scrollbar-width: thin;
  }
  .filter-run, .view-run { display: flex; align-items: center; gap: 6px; min-width: 0; }
  .filter-run { flex: none; flex-wrap: nowrap; }
  .view-run { flex: none; justify-content: flex-end; white-space: nowrap; }
  .caret { margin-left: 2px; color: var(--ink-faint); }
  .view-count { color: var(--ink-faint); font-variant-numeric: tabular-nums; }
  /* 以下在弹层内(经 portal 挂到 body):只用 app.css 全局 token,不依赖本页局部变量。 */
  .options { display: grid; gap: 3px; margin: 0; padding: 0; border: 0; min-width: 0; }
  .option {
    display: flex;
    align-items: center;
    gap: 8px;
    min-height: 34px;
    padding: 2px 7px;
    border-radius: var(--radius-md);
    color: var(--ink);
    font-size: 0.8rem;
    white-space: nowrap;
    cursor: pointer;
  }
  .option:hover { background: var(--surface-soft); }
  .empty-options { max-width: 18rem; margin: 0; padding: 8px 7px; color: var(--ink-secondary); font-size: 0.8rem; line-height: 1.55; white-space: normal; }
  .date-fields { display: grid; gap: 10px; }
  .date-field { display: grid; gap: 5px; color: var(--ink-secondary); font-size: 0.8rem; }
  @media (pointer: coarse) {
    .option { min-height: 44px; }
  }
</style>
