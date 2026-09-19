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
    onMerge,
    onSetAliases,
    onAdd,
    graphHref,
  }: {
    entities: Entity[];
    /** 实体 id → 本篇提及数(排序与「提及 n 次」展示用)。 */
    counts: Record<string, number>;
    editable?: boolean;
    onLocate?: (id: string) => void;
    onRename?: (id: string, name: string) => Promise<void>;
    onDelete?: (id: string) => Promise<void>;
    onSetKind?: (id: string, kind: string) => Promise<void>;
    /** 合并(二期):把 id 并进 targetId。本篇立即合,全局账本由后端同步。 */
    onMerge?: (id: string, targetId: string) => Promise<void>;
    /** 别名整表替换。别名决定正文里哪些写法算这个实体,改完后端重算提及。 */
    onSetAliases?: (id: string, aliases: string[]) => Promise<void>;
    /** 批量新增:[name, kind][]。 */
    onAdd?: (entries: [string, string][]) => Promise<void>;
    /** 实体 id → 知识图谱链接(解析不到全局 id 的返回 null,不出这个入口)。 */
    graphHref?: (id: string) => string | null;
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

  // 合并浮层:展开后在本篇其它实体里挑一个当"并进去"的目标(胜方)。
  let mergeOpen = $state(false);
  let mergeQuery = $state("");
  // 别名新增输入(展示与删除直接在 chip 上,不另开态)。
  let aliasInput = $state("");

  export function closeAll() {
    editingId = null;
    addOpen = false;
    mergeOpen = false;
    mergeQuery = "";
    aliasInput = "";
    busyErr = null;
  }

  function openEdit(e: Entity) {
    addOpen = false;
    mergeOpen = false;
    mergeQuery = "";
    aliasInput = "";
    editingId = e.id;
    editingName = e.name;
    busyErr = null;
  }

  /** 加别名:与新增实体同款批量分隔(中英文逗号/分号/顿号),空格保留(英文名)。
      整表替换——把现有别名连同新输入一起发下去,后端做归一去重与撞名校验。 */
  function commitAlias(e: Entity) {
    const parts = aliasInput
      .split(/[,，;；、]+/)
      .map((x) => x.trim())
      .filter(Boolean);
    aliasInput = "";
    if (parts.length === 0 || !onSetAliases) return;
    void act(() => onSetAliases(e.id, [...(e.aliases ?? []), ...parts]));
  }

  function removeAlias(e: Entity, alias: string) {
    if (!onSetAliases) return;
    void act(() => onSetAliases(e.id, (e.aliases ?? []).filter((a) => a !== alias)));
  }

  /** 合并候选:本篇其它全部实体(**不受 chip 行的 12 个折叠上限约束**——要并的那个
      很可能正躲在「+N」里),按名字过滤;同名的排前面,那是最常见的合并动机。 */
  function mergeTargets(self: Entity): Entity[] {
    const q = mergeQuery.trim().toLowerCase();
    const same = (e: Entity) => e.name.trim().toLowerCase() === self.name.trim().toLowerCase();
    return entities
      .filter((e) => e.id !== self.id && e.name.trim())
      .filter((e) => !q || e.name.toLowerCase().includes(q) || (e.aliases ?? []).some((a) => a.toLowerCase().includes(q)))
      .sort((a, b) => Number(same(b)) - Number(same(a)) || (counts[b.id] ?? 0) - (counts[a.id] ?? 0))
      .slice(0, 8);
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
            {#if onSetAliases}
              <!-- 别名:正文里这个实体的其它写法。加了立刻多认出提及,删了对应高亮
                   一并消失——所以每条都带 ×,而不是只读展示。合并/改名塞进来的别名
                   也在这里,用户第一次有地方看见并纠正它们。 -->
              <div class="ent-aliases">
                {#each e.aliases ?? [] as al (al)}
                  <span class="ent-alias">
                    {al}
                    <button class="ent-alias-x" aria-label={t("notes.entities.aliasRemove", { name: al })} onclick={() => removeAlias(e, al)}>×</button>
                  </span>
                {/each}
                <input
                  class="ent-alias-input"
                  placeholder={t("notes.entities.aliasPlaceholder")}
                  bind:value={aliasInput}
                  onkeydown={(ev) => {
                    if (ev.key === "Enter") commitAlias(e);
                    if (ev.key === "Escape") closeAll();
                  }}
                />
              </div>
            {/if}
            <div class="ent-actions">
              {#if onMerge}
                <button class="ent-act" class:on={mergeOpen} onclick={() => { mergeOpen = !mergeOpen; mergeQuery = ""; }}>
                  {t("notes.entities.merge")}
                </button>
              {/if}
              {#if graphHref?.(e.id)}
                <a class="ent-act" href={graphHref(e.id)}>{t("notes.entities.openGraph")}</a>
              {/if}
              <button class="ent-del" onclick={() => onDelete && void act(() => onDelete(e.id))}>
                {t("notes.entities.delete")}
              </button>
            </div>
            {#if mergeOpen}
              <!-- 合并目标选择:并进谁。方向刻意是"本条并进目标"(本条消失),
                   与浮层标题就是本条这件事一致,不做反向,免得点错把留下的那个删了。 -->
              <div class="ent-merge">
                <p class="ent-hint">{t("notes.entities.mergeHint", { name: e.name })}</p>
                <input
                  class="ent-input"
                  placeholder={t("notes.entities.mergeSearch")}
                  bind:value={mergeQuery}
                  onkeydown={(ev) => { if (ev.key === "Escape") { mergeOpen = false; } }}
                />
                {#each mergeTargets(e) as tgt (tgt.id)}
                  {@const tk = kindOf(tgt.kind)}
                  <button
                    class="ent-merge-item"
                    onclick={() => onMerge && void act(() => onMerge(e.id, tgt.id))}
                  >
                    <span class="ent-chip-mini" style="background: {tk?.tint}; color: {tk?.ink}">{tgt.name}</span>
                    {#if (counts[tgt.id] ?? 0) > 0}<span class="ent-n">{counts[tgt.id]}</span>{/if}
                  </button>
                {:else}
                  <p class="ent-hint">{t("notes.entities.mergeNone")}</p>
                {/each}
              </div>
            {:else}
              <span class="ent-hint">{onSetAliases ? t("notes.entities.aliasHint") : t("notes.entities.renameHint")}</span>
            {/if}
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
  /* 动作行:合并 / 打开图谱 / 删除。flex-wrap 让窄浮层里换行而不是把药丸压扁——
     此前 space-between + 不可换行的提示文字把「从本篇删除」挤成了两行(实测截图)。 */
  .ent-actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin-top: 0.5rem;
  }
  /* 中性动作药丸(合并 / 打开图谱):与危险色的删除拉开,同尺寸同形状。 */
  .ent-act {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.12em 0.6em;
    font-size: 0.75rem;
    line-height: 1.5;
    white-space: nowrap;
    text-decoration: none;
    cursor: pointer;
  }
  .ent-act:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .ent-act.on {
    border-color: var(--accent);
    color: var(--accent);
  }
  /* 别名区:已有别名做小药丸(各带 ×),末尾跟一个"加别名"输入框,同一行流式排布。 */
  .ent-aliases {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 0.3rem;
    margin-top: 0.5rem;
  }
  .ent-alias {
    display: inline-flex;
    align-items: center;
    gap: 0.2em;
    background: var(--surface-soft);
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.1em 0.2em 0.1em 0.55em;
    font-size: 0.75rem;
    white-space: nowrap;
  }
  .ent-alias-x {
    border: none;
    background: transparent;
    color: var(--ink-faint);
    font-size: 0.95em;
    line-height: 1;
    padding: 0 0.25em;
    cursor: pointer;
  }
  .ent-alias-x:hover {
    color: var(--danger);
  }
  .ent-alias-input {
    flex: 1 1 6rem;
    min-width: 5rem;
    border: 1px dashed var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: var(--radius-full);
    padding: 0.1em 0.55em;
    font-size: 0.75rem;
  }
  .ent-alias-input:focus {
    outline: none;
    border-style: solid;
    border-color: var(--accent);
  }
  /* 合并目标选择区 */
  .ent-merge {
    margin-top: 0.5rem;
    display: flex;
    flex-direction: column;
    gap: 0.3rem;
  }
  .ent-merge-item {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    border: none;
    background: transparent;
    border-radius: var(--radius-sm);
    padding: 0.2em 0.3em;
    cursor: pointer;
    text-align: left;
  }
  .ent-merge-item:hover {
    background: var(--surface-soft);
  }
  .ent-chip-mini {
    border-radius: var(--radius-full);
    padding: 0.1em 0.55em;
    font-size: 0.78rem;
    white-space: nowrap;
  }
  .ent-del {
    border: 1px solid var(--danger-line);
    background: transparent;
    color: var(--danger);
    border-radius: var(--radius-full);
    padding: 0.12em 0.6em;
    font-size: 0.75rem;
    line-height: 1.5;
    /* 「从本篇删除」六个字不得被挤断行(截图实证) */
    white-space: nowrap;
    margin-left: auto;
    cursor: pointer;
  }
  .ent-del:hover {
    background: var(--danger-tint);
    color: var(--danger-ink);
  }
  .ent-hint {
    display: block;
    margin-top: 0.4rem;
    color: var(--ink-faint);
    font-size: 0.72rem;
    line-height: 1.5;
  }
  .ent-err {
    margin-top: 0.4rem;
    color: var(--danger);
    font-size: 0.75rem;
  }
</style>
