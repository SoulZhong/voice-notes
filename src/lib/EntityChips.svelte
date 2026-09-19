<script lang="ts">
  import { t } from "$lib/i18n/index.svelte";
  import type { Entity } from "$lib/notes";
  import { ENTITY_KINDS, entityKind, entityKindLabel } from "$lib/entityKind";

  /* 关键实体行(2026-09-17 设计:docs/superpowers/specs/2026-09-17-note-entity-list-design.md):
     说话人条下方同形态 chips。chip 本体点击 = 定位正文提及(再点跳下一处);
     chip 上的 ⌄ 开编辑浮层(改名/改类型/删除)。行尾「＋」新增(批量分隔符)。
     只收 人/组织/产品项目/术语 四类;按提及数降序;超行收「+N」。 */
  let {
    entities,
    counts,
    editable = false,
    onLocate,
    onRename,
    onDelete,
    onSetKind,
    onAdd,
  }: {
    entities: Entity[];
    /** 实体 id → 本篇提及数(排序与「提及 n 次」展示用)。 */
    counts: Record<string, number>;
    editable?: boolean;
    onLocate?: (id: string) => void;
    onRename?: (id: string, name: string) => Promise<void>;
    onDelete?: (id: string) => Promise<void>;
    onSetKind?: (id: string, kind: string) => Promise<void>;
    /** 批量新增:[name, kind][]。 */
    onAdd?: (entries: [string, string][]) => Promise<void>;
  } = $props();

  /** 展示类型白名单与配色/标签:统一取自 $lib/entityKind(唯一真值源)。
      正文里的实体提及用的是同一份表——chip 与正文同色才看得出是同一个东西。 */
  const KINDS = ENTITY_KINDS;
  const kindOf = (k: string) => entityKind(k);

  const COLLAPSED_MAX = 12;
  let showAll = $state(false);
  const visible = $derived.by(() => {
    const list = entities
      .filter((e) => e.name.trim() && kindOf(e.kind))
      .sort((a, b) => (counts[b.id] ?? 0) - (counts[a.id] ?? 0) || a.name.localeCompare(b.name));
    return showAll ? list : list.slice(0, COLLAPSED_MAX);
  });
  const hiddenN = $derived(
    Math.max(0, entities.filter((e) => e.name.trim() && kindOf(e.kind)).length - COLLAPSED_MAX),
  );

  // ── 编辑浮层(每次只开一个;点外/Esc 关由宿主页的全局 onclick 负责,这里 stopPropagation)──
  let editingId = $state<string | null>(null);
  let editingName = $state("");
  let addOpen = $state(false);
  let addInput = $state("");
  let addKind = $state("person");
  let busyErr = $state<string | null>(null);

  export function closeAll() {
    editingId = null;
    addOpen = false;
    busyErr = null;
  }

  function openEdit(e: Entity) {
    addOpen = false;
    editingId = e.id;
    editingName = e.name;
    busyErr = null;
  }

  async function act(fn: () => Promise<void>) {
    busyErr = null;
    try {
      await fn();
    } catch (err) {
      busyErr = String(err);
      return;
    }
    closeAll();
  }

  function commitRename(e: Entity) {
    const name = editingName.trim();
    if (!name || name === e.name) {
      closeAll();
      return;
    }
    if (onRename) void act(() => onRename(e.id, name));
  }

  function commitAdd() {
    // 与与会人员同款批量分隔:中英文逗号/分号/顿号;空格保留(英文名)。
    const parts = addInput
      .split(/[,，;；、]+/)
      .map((s) => s.trim())
      .filter(Boolean);
    addInput = "";
    if (parts.length === 0 || !onAdd) return;
    const entries: [string, string][] = parts.map((n) => [n, addKind]);
    void act(() => onAdd(entries));
  }
</script>

{#if visible.length > 0 || editable}
  <div class="ent-row">
    {#each visible as e (e.id)}
      {@const k = kindOf(e.kind)}
      <span class="ent-chip" style="background: {k?.tint}; color: {k?.ink}">
        <button
          class="ent-name"
          title={t("notes.entities.locate", { n: counts[e.id] ?? 0 })}
          onclick={() => onLocate?.(e.id)}
        >
          {e.name}{#if (counts[e.id] ?? 0) > 1}<span class="ent-n">{counts[e.id]}</span>{/if}
        </button>
        {#if editable}
          <button
            class="ent-edit"
            aria-label={t("notes.entities.edit")}
            onclick={(ev) => {
              ev.stopPropagation();
              editingId === e.id ? closeAll() : openEdit(e);
            }}
          >
            <svg width="9" height="9" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M4 6l4 4 4-4" /></svg>
          </button>
        {/if}
        {#if editingId === e.id}
          <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
          <div class="ent-pop" onclick={(ev) => ev.stopPropagation()}>
            <input
              class="ent-input"
              placeholder={t("notes.entities.renamePlaceholder")}
              bind:value={editingName}
              onkeydown={(ev) => {
                if (ev.key === "Enter") commitRename(e);
                if (ev.key === "Escape") closeAll();
              }}
            />
            <div class="ent-kinds">
              {#each KINDS as kk (kk.key)}
                <button
                  class="ent-kind"
                  class:on={kindOf(e.kind)?.key === kk.key}
                  onclick={() => onSetKind && void act(() => onSetKind(e.id, kk.key))}
                >
                  {entityKindLabel(kk.key)}
                </button>
              {/each}
            </div>
            <div class="ent-actions">
              <button class="ent-del" onclick={() => onDelete && void act(() => onDelete(e.id))}>
                {t("notes.entities.delete")}
              </button>
              <span class="ent-hint">{t("notes.entities.renameHint")}</span>
            </div>
            {#if busyErr}<div class="ent-err">{busyErr}</div>{/if}
          </div>
        {/if}
      </span>
    {/each}
    {#if hiddenN > 0 && !showAll}
      <button class="ent-more" onclick={() => (showAll = true)}>+{hiddenN}</button>
    {:else if showAll && hiddenN > 0}
      <button class="ent-more" onclick={() => (showAll = false)}>{t("notes.entities.collapse")}</button>
    {/if}
    {#if editable}
      <span class="ent-chip ent-add-wrap">
        <button
          class="ent-more ent-add"
          title={t("notes.entities.addTitle")}
          onclick={(ev) => {
            ev.stopPropagation();
            addOpen = !addOpen;
            editingId = null;
            busyErr = null;
          }}
        >
          ＋ {t("notes.entities.add")}
        </button>
        {#if addOpen}
          <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
          <div class="ent-pop" onclick={(ev) => ev.stopPropagation()}>
            <input
              class="ent-input"
              placeholder={t("notes.entities.addPlaceholder")}
              bind:value={addInput}
              onkeydown={(ev) => {
                if (ev.key === "Enter") commitAdd();
                if (ev.key === "Escape") closeAll();
              }}
            />
            <div class="ent-kinds">
              {#each KINDS as kk (kk.key)}
                <button class="ent-kind" class:on={addKind === kk.key} onclick={() => (addKind = kk.key)}>
                  {entityKindLabel(kk.key)}
                </button>
              {/each}
            </div>
            {#if busyErr}<div class="ent-err">{busyErr}</div>{/if}
          </div>
        {/if}
      </span>
    {/if}
  </div>
{/if}

<style>
  .ent-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.35rem;
    margin: 0.5rem 0 0;
  }
  .ent-chip {
    position: relative;
    display: inline-flex;
    align-items: center;
    border-radius: var(--radius-full);
    font-size: 0.78rem;
  }
  .ent-name {
    display: inline-flex;
    align-items: center;
    gap: 0.25em;
    border: none;
    background: none;
    color: inherit;
    padding: 0.18em 0.3em 0.18em 0.7em;
    border-radius: var(--radius-full) 0 0 var(--radius-full);
    cursor: pointer;
    font-size: inherit;
  }
  .ent-chip:not(:has(.ent-edit)) .ent-name {
    padding-right: 0.7em;
    border-radius: var(--radius-full);
  }
  .ent-n {
    font-size: 0.85em;
    opacity: 0.65;
    font-variant-numeric: tabular-nums;
  }
  .ent-edit {
    border: none;
    background: none;
    color: inherit;
    opacity: 0.55;
    padding: 0.18em 0.5em 0.18em 0.1em;
    cursor: pointer;
    border-radius: 0 var(--radius-full) var(--radius-full) 0;
  }
  .ent-edit:hover {
    opacity: 1;
  }
  .ent-more {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.15em 0.6em;
    font-size: 0.78rem;
    cursor: pointer;
  }
  .ent-more:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .ent-add-wrap {
    background: transparent;
  }
  /* 浮层:与说话人/音轨菜单同语言 */
  .ent-pop {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    z-index: 25;
    min-width: 15rem;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 0.5rem;
    color: var(--ink);
  }
  .ent-input {
    width: 100%;
    box-sizing: border-box;
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-md);
    padding: 0.35em 0.55em;
    font-size: 0.85rem;
  }
  .ent-input:focus {
    outline: 2px solid var(--accent);
  }
  .ent-kinds {
    display: flex;
    gap: 0.3rem;
    margin-top: 0.45rem;
    flex-wrap: wrap;
  }
  .ent-kind {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.12em 0.6em;
    font-size: 0.75rem;
    cursor: pointer;
  }
  .ent-kind.on {
    color: var(--accent);
    border-color: var(--accent);
    background: var(--accent-tint);
  }
  .ent-actions {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.6rem;
    margin-top: 0.5rem;
  }
  .ent-del {
    border: 1px solid var(--danger-line);
    background: transparent;
    color: var(--danger);
    border-radius: var(--radius-full);
    padding: 0.12em 0.6em;
    font-size: 0.75rem;
    cursor: pointer;
  }
  .ent-del:hover {
    background: var(--danger-tint);
    color: var(--danger-ink);
  }
  .ent-hint {
    color: var(--ink-faint);
    font-size: 0.72rem;
  }
  .ent-err {
    margin-top: 0.4rem;
    color: var(--danger);
    font-size: 0.75rem;
  }
</style>
