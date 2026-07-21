<script lang="ts">
  import { entityMentions, relationDetail, type SemanticEntityDetail } from "./knowledge";
  import { kindInk, kindLabel, kindSoft } from "./graph";
  import { relationLabel } from "./knowledgeView";
  import { listNotes } from "./notes";
  import {
    buildAddAlias,
    buildBindPerson,
    buildCreateRelation,
    buildRemoveAlias,
    buildRenameEntity,
    createGovernanceController,
    task10GovernanceApi,
    type GovernanceMention,
  } from "./knowledgeGovernance";
  import EntitySplitDialog from "./EntitySplitDialog.svelte";

  let {
    detail,
    onChanged,
    onOpenRelation,
    simple = false,
    resolveEntityName,
  }: {
    detail: SemanticEntityDetail;
    onChanged: () => Promise<void>;
    onOpenRelation: (id: string) => void;
    /** Everyday graph detail hides database-governance operations. */
    simple?: boolean;
    resolveEntityName?: (id: string) => string | undefined;
  } = $props();

  let renameValue = $state("");
  let aliasValue = $state("");
  let mergeTarget = $state("");
  let relationTarget = $state("");
  let relationType = $state("custom");
  let relationCustomLabel = $state("");
  let relationNote = $state("");
  let personMentionId = $state("");
  let personEntityId = $state("");
  let mentions = $state<GovernanceMention[]>([]);
  let noteTitles = $state<Map<string, string>>(new Map());
  let mentionsLoading = $state(false);
  let splitOpen = $state(false);
  let working = $state(false);
  let status = $state("");
  let loadedEntityId = "";
  let evidenceGeneration = 0;
  const controller = createGovernanceController(task10GovernanceApi, () => onChanged());

  $effect(() => {
    if (detail.id !== loadedEntityId) {
      loadedEntityId = detail.id;
      renameValue = detail.name;
      aliasValue = "";
      mergeTarget = "";
      relationTarget = "";
      personMentionId = "";
      personEntityId = "";
      status = "";
    }
  });

  $effect(() => {
    const entityId = detail.id;
    const relations = detail.relations.map((relation) => relation.id);
    const generation = ++evidenceGeneration;
    mentionsLoading = true;
    Promise.all([
      entityMentions(entityId),
      Promise.all(relations.map((id) => relationDetail(id).catch(() => null))),
      listNotes().catch(() => []),
    ]).then(([items, relationDetails, notes]) => {
      if (generation !== evidenceGeneration) return;
      const relationIdsByMention = new Map<string, Set<string>>();
      for (const item of relationDetails) {
        if (!item) continue;
        for (const evidence of item.evidence) {
          for (const mentionId of [...evidence.subject_mentions, ...evidence.object_mentions]) {
            const ids = relationIdsByMention.get(mentionId) ?? new Set<string>();
            ids.add(item.relation.id);
            relationIdsByMention.set(mentionId, ids);
          }
        }
      }
      mentions = items.map((mention) => ({
        ...mention,
        relation_ids: [...(relationIdsByMention.get(mention.id) ?? [])].sort(),
      }));
      noteTitles = new Map(notes.map((note) => [note.id, note.title]));
      mentionsLoading = false;
    }).catch(() => {
      if (generation !== evidenceGeneration) return;
      mentions = [];
      noteTitles = new Map();
      mentionsLoading = false;
    });
  });

  const currentRelations = $derived(detail.relations.filter((relation) => relation.status === "current"));
  const historicalRelations = $derived(detail.relations.filter((relation) => relation.status === "historical"));
  const mentionGroups = $derived.by(() => {
    const groups = new Map<string, GovernanceMention[]>();
    for (const mention of [...mentions].sort((a, b) =>
      a.note_id.localeCompare(b.note_id) ||
      a.paragraph_index - b.paragraph_index ||
      a.start_offset - b.start_offset ||
      a.id.localeCompare(b.id)
    )) groups.set(mention.note_id, [...(groups.get(mention.note_id) ?? []), mention]);
    return [...groups.entries()].map(([noteId, items]) => ({
      noteId,
      title: noteTitles.get(noteId)?.trim() || `笔记 ${noteId}`,
      items,
    }));
  });
  const displayEntityName = (id: string) =>
    id === detail.id ? detail.name : resolveEntityName?.(id) ?? "相关实体";

  async function runOperation(
    pendingText: string,
    successText: string,
    operation: Parameters<typeof controller.submit>[0],
    afterSuccess?: () => void,
  ) {
    if (working) return;
    working = true;
    status = pendingText;
    try {
      await controller.submit(operation);
      afterSuccess?.();
      status = controller.refreshError || successText;
    } catch {
      status = controller.error;
    } finally {
      working = false;
    }
  }

  async function mergeEntity() {
    const target = mergeTarget.trim();
    if (!target || target === detail.id || working) return;
    working = true;
    status = "正在合并实体";
    try {
      await controller.merge(detail.id, target);
      status = controller.refreshError || "实体已合并，旧身份会稳定重定向到目标实体";
    } catch {
      status = controller.error;
    } finally {
      working = false;
    }
  }

  async function undoLastOperation() {
    if (!controller.lastOperationId || working) return;
    working = true;
    status = "正在撤销上次实体操作";
    try {
      await controller.undo(controller.lastOperationId);
      status = controller.refreshError || "已写入补偿操作并刷新图谱";
    } catch {
      status = controller.error;
    } finally {
      working = false;
    }
  }
</script>

<article class="governance" aria-labelledby="entity-governance-title">
  <header class="entity-heading">
    <div>
      <p class="eyebrow">{simple ? "实体详情" : "实体治理"}</p>
      <h2 id="entity-governance-title">{detail.name}</h2>
    </div>
    <span class="kind" style:background={kindSoft(detail.kind)} style:color={kindInk(detail.kind)}>{kindLabel(detail.kind)}</span>
  </header>

  {#if detail.degraded}
    <p class="degraded" role="status">{detail.message || "图谱处于只读降级状态，当前信息仍可查看。"}</p>
  {/if}

  <section class="section overview" aria-labelledby="entity-overview">
    <h3 id="entity-overview">概览</h3>
    <dl class="facts">
      <div><dt>状态</dt><dd>{detail.confirmed ? "已确认" : "模型识别"}</dd></div>
      <div><dt>笔记</dt><dd>{detail.note_count} 篇</dd></div>
      <div><dt>提及</dt><dd>{detail.mention_total} 次</dd></div>
      <div><dt>关系</dt><dd>{detail.relations.length} 条</dd></div>
    </dl>

    {#if !simple}
    <form class="inline-form" onsubmit={(event) => {
      event.preventDefault();
      const nextName = renameValue.trim();
      if (nextName && nextName !== detail.name) void runOperation(
        "正在重命名实体",
        `实体已重命名为「${nextName}」`,
        buildRenameEntity(detail.id, nextName),
      );
    }}>
      <label for="entity-rename">规范名称</label>
      <div class="field-action">
        <input id="entity-rename" bind:value={renameValue} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <button type="submit" disabled={working || detail.degraded || !renameValue.trim() || renameValue.trim() === detail.name}>重命名实体</button>
      </div>
    </form>

    <div class="aliases">
      <p class="field-label">别名</p>
      {#if detail.aliases.length > 0}
        <div class="alias-list">
          {#each detail.aliases as alias (alias)}
            <span class="alias-chip">
              <span>{alias}</span>
              <button
                type="button"
                aria-label={`移除别名 ${alias}`}
                disabled={working || detail.degraded}
                onclick={() => runOperation("正在移除别名", `已移除别名「${alias}」`, buildRemoveAlias(detail.id, alias))}
              >×</button>
            </span>
          {/each}
        </div>
      {:else}
        <p class="empty-line">尚未添加别名</p>
      {/if}
      <label for="entity-alias">添加别名</label>
      <form class="field-action" onsubmit={(event) => {
        event.preventDefault();
        const alias = aliasValue.trim();
        if (alias) void runOperation("正在添加别名", `已添加别名「${alias}」`, buildAddAlias(detail.id, alias), () => { aliasValue = ""; });
      }}>
        <input id="entity-alias" bind:value={aliasValue} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <button type="submit" disabled={working || detail.degraded || !aliasValue.trim()}>添加别名</button>
      </form>
    </div>
    {/if}
  </section>

  {#if !simple}
  <section class="section actions" aria-labelledby="entity-actions">
    <h3 id="entity-actions">身份与关系操作</h3>
    <details>
      <summary>合并实体</summary>
      <form class="disclosure-form" onsubmit={(event) => { event.preventDefault(); void mergeEntity(); }}>
        <label for="merge-target">目标实体 ID</label>
        <input id="merge-target" bind:value={mergeTarget} disabled={working || detail.degraded} aria-describedby="merge-help entity-feedback" />
        <small id="merge-help">当前实体会稳定重定向到目标实体，提及与关系在解析时汇入目标。</small>
        <button type="submit" disabled={working || detail.degraded || !mergeTarget.trim() || mergeTarget.trim() === detail.id}>合并实体</button>
      </form>
    </details>
    <div class="direct-action">
      <div><strong>拆分证据</strong><small>按完整原文提及建立新的稳定身份。</small></div>
      <button type="button" disabled={working || detail.degraded || mentionsLoading || mentions.length === 0} onclick={() => (splitOpen = true)}>拆分证据</button>
    </div>
    <details>
      <summary>新建关系</summary>
      <form class="disclosure-form" onsubmit={(event) => {
        event.preventDefault();
        const target = relationTarget.trim();
        const predicate = relationType === "custom"
          ? { type: "custom", label: relationCustomLabel.trim() || null }
          : { type: relationType.trim(), label: null };
        if (target && predicate.type && (predicate.type !== "custom" || predicate.label)) void runOperation(
          "正在新建关系",
          "已新建用户直接声明的关系",
          buildCreateRelation(detail.id, predicate, target, null, null, relationNote.trim() || null, [], true),
          () => { relationTarget = ""; relationNote = ""; },
        );
      }}>
        <label for="relation-target">客体实体 ID</label>
        <input id="relation-target" bind:value={relationTarget} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <label for="relation-type">关系类型</label>
        <select id="relation-type" bind:value={relationType} disabled={working || detail.degraded}>
          <option value="participates_in">参与</option><option value="responsible_for">负责</option>
          <option value="belongs_to">属于</option><option value="uses">使用</option>
          <option value="depends_on">依赖</option><option value="produces">产生</option>
          <option value="assigned_to">指派给</option><option value="occurs_at">发生于</option>
          <option value="custom">自定义关系</option>
        </select>
        {#if relationType === "custom"}
          <label for="relation-label">完整关系名称</label>
          <input id="relation-label" bind:value={relationCustomLabel} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        {/if}
        <label for="relation-note">关系说明</label>
        <textarea id="relation-note" bind:value={relationNote} disabled={working || detail.degraded}></textarea>
        <button type="submit" disabled={working || detail.degraded || !relationTarget.trim() || (relationType === "custom" && !relationCustomLabel.trim())}>新建关系</button>
      </form>
    </details>
    <details>
      <summary>关联会议搭子</summary>
      <form class="disclosure-form" onsubmit={(event) => {
        event.preventDefault();
        if (personMentionId.trim() && personEntityId.trim()) void runOperation(
          "正在关联会议搭子",
          "提及证据已关联到会议搭子",
          buildBindPerson(personMentionId.trim(), personEntityId.trim()),
          () => { personMentionId = ""; },
        );
      }}>
        <label for="person-mention">提及证据 ID</label>
        <input id="person-mention" bind:value={personMentionId} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <label for="person-id">会议搭子实体 ID</label>
        <input id="person-id" bind:value={personEntityId} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <button type="submit" disabled={working || detail.degraded || !personMentionId.trim() || !personEntityId.trim()}>关联会议搭子</button>
      </form>
    </details>
  </section>
  {/if}

  <section class="section" aria-labelledby="entity-relations">
    <h3 id="entity-relations">关系</h3>
    <h4>当前关系 <span>{currentRelations.length}</span></h4>
    <ul class="relation-list">
      {#each currentRelations as relation (relation.id)}
        <li>
          <button type="button" onclick={() => onOpenRelation(relation.id)}>
            <span>{`${displayEntityName(relation.subject_id)} → ${relationLabel(relation)} → ${displayEntityName(relation.object_id)}`}</span>
            <small>{Math.round(relation.confidence * 100)}% 置信度 · {relation.evidence_count} 条证据</small>
          </button>
        </li>
      {/each}
      {#if currentRelations.length === 0}<li class="empty-line">没有当前关系</li>{/if}
    </ul>
    {#if !simple}
      <h4>历史关系 <span>{historicalRelations.length}</span></h4>
      <ul class="relation-list">
        {#each historicalRelations as relation (relation.id)}
          <li>
            <button type="button" onclick={() => onOpenRelation(relation.id)}>
              <span>{relation.subject_id === detail.id ? `${detail.name} → ${relationLabel(relation)} → ${relation.object_id}` : `${relation.subject_id} → ${relationLabel(relation)} → ${detail.name}`}</span>
              <small>{relation.valid_to ? `有效至 ${relation.valid_to}` : "历史版本"}</small>
            </button>
          </li>
        {/each}
        {#if historicalRelations.length === 0}<li class="empty-line">没有历史关系</li>{/if}
      </ul>
    {/if}
  </section>

  <section class="section evidence" aria-labelledby="entity-evidence">
    <h3 id="entity-evidence">{simple ? "关联笔记" : "证据"}</h3>
    {#if mentionsLoading}
      <p class="empty-line">{simple ? "正在读取关联笔记" : "正在读取完整提及证据"}</p>
    {:else}
      {#if simple}
        <ul class="source-notes">
          {#each mentionGroups as group (group.noteId)}
            <li><a href={'/notes/' + encodeURIComponent(group.noteId)}>{group.title}</a></li>
          {/each}
        </ul>
        {#if mentionGroups.length === 0}<p class="empty-line">没有关联笔记</p>{/if}
      {:else}
        {#each mentionGroups as group (group.noteId)}
          <div class="note-group">
            <a href={'/notes/' + encodeURIComponent(group.noteId)}>{group.title}</a>
            {#each group.items as mention (mention.id)}
              <blockquote>
                <p>{mention.quote}</p>
                <footer>第 {mention.paragraph_index + 1} 段 · 字符 {mention.start_offset}–{mention.end_offset}</footer>
              </blockquote>
            {/each}
          </div>
        {/each}
        {#if mentions.length === 0}<p class="empty-line">没有可显示的提及证据</p>{/if}
      {/if}
    {/if}
  </section>

  {#if !simple}<div class="feedback-actions">
    <p id="entity-feedback" class:error={Boolean(controller.error)} aria-live="polite">{status}</p>
    {#if controller.refreshError}<button type="button" disabled={working} onclick={() => controller.retryRefresh().then(() => { status = "图谱已刷新"; }).catch(() => { status = controller.refreshError; })}>重试刷新图谱</button>{/if}
    {#if controller.lastOperationId}<button type="button" disabled={working} onclick={undoLastOperation}>撤销上次实体操作</button>{/if}
  </div>{/if}
</article>

{#if splitOpen && !simple}
  <EntitySplitDialog entity={detail} {mentions} onClose={() => (splitOpen = false)} onCommitted={onChanged} />
{/if}

<style>
  .governance { container-type: inline-size; color: var(--ink); }
  .entity-heading { display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; padding-bottom: 20px; }
  .eyebrow { margin: 0 0 4px; color: var(--ink-faint); font-size: 0.72rem; letter-spacing: 0.08em; }
  h2 { margin: 0; font-size: 1.3rem; line-height: 1.3; font-weight: 550; overflow-wrap: anywhere; }
  .kind { flex: none; margin-top: 18px; padding: 2px 8px; border-radius: var(--radius-md); font-size: 0.72rem; }
  .degraded { margin: 0 0 16px; padding: 10px; border: 1px solid var(--warning-line); border-radius: var(--radius-md); background: var(--warning-tint); color: var(--warning-ink); font-size: 0.82rem; line-height: 1.5; }
  .section { padding: 20px 0; border-top: 1px solid var(--hairline); }
  h3 { margin: 0 0 14px; color: var(--ink-secondary); font-size: 0.78rem; font-weight: 550; letter-spacing: 0.06em; }
  h4 { display: flex; gap: 6px; margin: 18px 0 6px; color: var(--ink-secondary); font-size: 0.8rem; font-weight: 500; }
  h4 span { color: var(--ink-faint); font-variant-numeric: tabular-nums; }
  .facts { display: grid; grid-template-columns: repeat(4, minmax(0, 1fr)); margin: 0 0 20px; }
  .facts div { padding-right: 8px; }
  dt { color: var(--ink-faint); font-size: 0.7rem; }
  dd { margin: 3px 0 0; color: var(--ink); font-size: 0.86rem; font-variant-numeric: tabular-nums; }
  .inline-form, .aliases { display: grid; gap: 7px; margin-top: 14px; }
  label, .field-label { margin: 0; color: var(--ink-secondary); font-size: 0.78rem; }
  .field-action { display: grid; grid-template-columns: minmax(0, 1fr) auto; gap: 8px; }
  input, select, textarea { box-sizing: border-box; width: 100%; min-width: 0; padding: 8px 9px; border: 1px solid var(--hairline); border-radius: var(--radius-md); background: var(--surface-press); color: var(--ink); font: inherit; font-size: 0.84rem; }
  textarea { min-height: 72px; resize: vertical; }
  input:focus-visible, select:focus-visible, textarea:focus-visible, button:focus-visible, summary:focus-visible, a:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  button { font-family: inherit; }
  .field-action button, .disclosure-form button, .direct-action button { min-height: 34px; padding: 7px 10px; border: 1px solid var(--hairline-strong); border-radius: var(--radius-md); background: transparent; color: var(--ink-secondary); font-size: 0.8rem; cursor: pointer; }
  button:hover:not(:disabled) { background: var(--surface-soft); color: var(--ink); }
  button:disabled, input:disabled, select:disabled, textarea:disabled { opacity: 0.5; cursor: default; }
  .alias-list { display: flex; flex-wrap: wrap; gap: 6px; }
  .alias-chip { display: inline-flex; align-items: center; gap: 2px; min-width: 0; padding: 3px 4px 3px 8px; border: 1px solid var(--hairline); border-radius: var(--radius-full); color: var(--ink-secondary); font-size: 0.78rem; overflow-wrap: anywhere; }
  .alias-chip button { width: 24px; height: 24px; padding: 0; border: 0; border-radius: var(--radius-full); background: transparent; color: var(--ink-faint); cursor: pointer; }
  details { border-top: 1px solid var(--hairline); }
  details:last-child { border-bottom: 1px solid var(--hairline); }
  summary { padding: 12px 2px; color: var(--ink); font-size: 0.86rem; cursor: pointer; }
  .disclosure-form { display: grid; gap: 7px; padding: 0 2px 14px; }
  .disclosure-form small, .direct-action small { color: var(--ink-faint); font-size: 0.72rem; line-height: 1.5; }
  .direct-action { display: flex; align-items: center; justify-content: space-between; gap: 12px; padding: 12px 2px; border-top: 1px solid var(--hairline); }
  .direct-action div { display: grid; gap: 3px; }
  .direct-action strong { font-size: 0.86rem; font-weight: 400; }
  .relation-list { list-style: none; margin: 0; padding: 0; }
  .relation-list li { border-top: 1px solid var(--hairline); }
  .relation-list button { width: 100%; padding: 10px 2px; border: 0; background: transparent; color: var(--ink); text-align: left; cursor: pointer; }
  .relation-list button span, .relation-list button small { display: block; overflow-wrap: anywhere; }
  .relation-list button span { font-size: 0.84rem; line-height: 1.5; }
  .relation-list button small { margin-top: 3px; color: var(--ink-faint); font-size: 0.7rem; }
  .empty-line { margin: 6px 0; color: var(--ink-faint); font-size: 0.78rem; }
  .note-group { padding: 12px 0; border-top: 1px solid var(--hairline); }
  .note-group > a { color: var(--accent); font-size: 0.78rem; text-decoration: none; overflow-wrap: anywhere; }
  .source-notes { display: grid; gap: 0; margin: 0; padding: 0; list-style: none; }
  .source-notes li { border-top: 1px solid var(--hairline); }
  .source-notes a { display: block; padding: 12px 0; color: var(--accent); font-size: 0.82rem; text-decoration: none; overflow-wrap: anywhere; }
  .source-notes a:hover { color: var(--ink); }
  blockquote { margin: 10px 0 0; padding: 0 0 0 12px; border-left: 2px solid var(--hairline-strong); }
  blockquote p { margin: 0; color: var(--ink); font-size: 0.86rem; line-height: 1.65; overflow-wrap: anywhere; }
  blockquote footer { margin-top: 4px; color: var(--ink-faint); font-size: 0.7rem; }
  .feedback-actions { position: sticky; bottom: 0; display: flex; flex-wrap: wrap; align-items: center; gap: 8px; padding: 12px 0; border-top: 1px solid var(--hairline); background: var(--surface); }
  .feedback-actions p { flex: 1 1 100%; min-height: 1.3em; margin: 0; color: var(--success); font-size: 0.78rem; line-height: 1.45; }
  .feedback-actions p.error { color: var(--danger-ink); }
  .feedback-actions button { padding: 6px 9px; border: 1px solid var(--hairline-strong); border-radius: var(--radius-md); background: transparent; color: var(--ink-secondary); font-size: 0.76rem; cursor: pointer; }
  @container (max-width: 340px) { .facts { grid-template-columns: repeat(2, 1fr); gap: 10px; } .field-action { grid-template-columns: 1fr; } }
  @media (pointer: coarse) { button, summary { min-height: 44px; } .alias-chip button { width: 44px; } }
  @media (prefers-reduced-motion: reduce) { *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; } }
</style>
