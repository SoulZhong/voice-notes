<script lang="ts">
  import { entityMentions, relationDetail, type SemanticEntityDetail } from "./knowledge";
  import { kindInk, kindLabel, kindSoft } from "./graph";
  import { relationLabel } from "./knowledgeView";
  import { formatDate, formatDuration, listNotes } from "./notes";
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
  import { t } from "$lib/i18n/index.svelte";

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
  let noteStartedAt = $state<Map<string, string>>(new Map());
  let noteDurations = $state<Map<string, number | null>>(new Map());
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
      noteStartedAt = new Map(notes.map((note) => [note.id, note.started_at]));
      noteDurations = new Map(notes.map((note) => [note.id, note.duration_secs]));
      mentionsLoading = false;
    }).catch(() => {
      if (generation !== evidenceGeneration) return;
      mentions = [];
      noteTitles = new Map();
      noteStartedAt = new Map();
      noteDurations = new Map();
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
    return [...groups.entries()].map(([noteId, items]) => {
      const startedAt = noteStartedAt.get(noteId) ?? "";
      const formattedTime = formatDate(startedAt);
      const formattedDuration = formatDuration(noteDurations.get(noteId) ?? null);
      return {
        noteId,
        title: noteTitles.get(noteId)?.trim() || t("governance.noteFallback", { id: noteId }),
        startedAt,
        time: formattedTime === "—" ? "" : formattedTime,
        duration: formattedDuration === "—" ? "" : formattedDuration,
        items,
      };
    });
  });
  const displayEntityName = (id: string) =>
    id === detail.id ? detail.name : resolveEntityName?.(id) ?? t("governance.entity.relatedEntity");

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
    status = t("governance.entity.merging");
    try {
      await controller.merge(detail.id, target);
      status = controller.refreshError || t("governance.entity.merged");
    } catch {
      status = controller.error;
    } finally {
      working = false;
    }
  }

  async function undoLastOperation() {
    if (!controller.lastOperationId || working) return;
    working = true;
    status = t("governance.entity.undoing");
    try {
      await controller.undo(controller.lastOperationId);
      status = controller.refreshError || t("governance.entity.undone");
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
      <p class="eyebrow">{simple ? t("governance.entity.eyebrowSimple") : t("governance.entity.eyebrow")}</p>
      <h2 id="entity-governance-title">{detail.name}</h2>
    </div>
    <span class="kind" style:background={kindSoft(detail.kind)} style:color={kindInk(detail.kind)}>{kindLabel(detail.kind)}</span>
  </header>

  {#if detail.degraded}
    <p class="degraded" role="status">{detail.message || t("governance.entity.degraded")}</p>
  {/if}

  <section class="section overview" aria-labelledby="entity-overview">
    <h3 id="entity-overview">{t("governance.overview")}</h3>
    <dl class="facts">
      <div><dt>{t("governance.statusLabel")}</dt><dd>{detail.confirmed ? t("governance.entity.confirmed") : t("governance.entity.modelDetected")}</dd></div>
      <div><dt>{t("governance.entity.notesLabel")}</dt><dd>{t("governance.entity.noteCount", { n: detail.note_count })}</dd></div>
      <div><dt>{t("governance.entity.mentionsLabel")}</dt><dd>{t("governance.entity.mentionCount", { n: detail.mention_total })}</dd></div>
      <div><dt>{t("governance.entity.relationsLabel")}</dt><dd>{t("governance.countItems", { n: detail.relations.length })}</dd></div>
    </dl>

    {#if !simple}
    <form class="inline-form" onsubmit={(event) => {
      event.preventDefault();
      const nextName = renameValue.trim();
      if (nextName && nextName !== detail.name) void runOperation(
        t("governance.entity.renaming"),
        t("governance.entity.renamed", { name: nextName }),
        buildRenameEntity(detail.id, nextName),
      );
    }}>
      <label for="entity-rename">{t("governance.entity.canonicalName")}</label>
      <div class="field-action">
        <input id="entity-rename" bind:value={renameValue} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <button type="submit" disabled={working || detail.degraded || !renameValue.trim() || renameValue.trim() === detail.name}>{t("governance.entity.rename")}</button>
      </div>
    </form>

    <div class="aliases">
      <p class="field-label">{t("governance.entity.aliasesLabel")}</p>
      {#if detail.aliases.length > 0}
        <div class="alias-list">
          {#each detail.aliases as alias (alias)}
            <span class="alias-chip">
              <span>{alias}</span>
              <button
                type="button"
                aria-label={t("governance.entity.removeAliasAria", { alias })}
                disabled={working || detail.degraded}
                onclick={() => runOperation(t("governance.entity.removingAlias"), t("governance.entity.aliasRemoved", { alias }), buildRemoveAlias(detail.id, alias))}
              >×</button>
            </span>
          {/each}
        </div>
      {:else}
        <p class="empty-line">{t("governance.entity.noAliases")}</p>
      {/if}
      <label for="entity-alias">{t("governance.entity.addAlias")}</label>
      <form class="field-action" onsubmit={(event) => {
        event.preventDefault();
        const alias = aliasValue.trim();
        if (alias) void runOperation(t("governance.entity.addingAlias"), t("governance.entity.aliasAdded", { alias }), buildAddAlias(detail.id, alias), () => { aliasValue = ""; });
      }}>
        <input id="entity-alias" bind:value={aliasValue} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <button type="submit" disabled={working || detail.degraded || !aliasValue.trim()}>{t("governance.entity.addAlias")}</button>
      </form>
    </div>
    {/if}
  </section>

  {#if !simple}
  <section class="section actions" aria-labelledby="entity-actions">
    <h3 id="entity-actions">{t("governance.entity.actionsTitle")}</h3>
    <details>
      <summary>{t("governance.entity.merge")}</summary>
      <form class="disclosure-form" onsubmit={(event) => { event.preventDefault(); void mergeEntity(); }}>
        <label for="merge-target">{t("governance.entity.mergeTarget")}</label>
        <input id="merge-target" bind:value={mergeTarget} disabled={working || detail.degraded} aria-describedby="merge-help entity-feedback" />
        <small id="merge-help">{t("governance.entity.mergeHelp")}</small>
        <button type="submit" disabled={working || detail.degraded || !mergeTarget.trim() || mergeTarget.trim() === detail.id}>{t("governance.entity.merge")}</button>
      </form>
    </details>
    <div class="direct-action">
      <div><strong>{t("governance.entity.split")}</strong><small>{t("governance.entity.splitHelp")}</small></div>
      <button type="button" disabled={working || detail.degraded || mentionsLoading || mentions.length === 0} onclick={() => (splitOpen = true)}>{t("governance.entity.split")}</button>
    </div>
    <details>
      <summary>{t("governance.entity.createRelation")}</summary>
      <form class="disclosure-form" onsubmit={(event) => {
        event.preventDefault();
        const target = relationTarget.trim();
        const predicate = relationType === "custom"
          ? { type: "custom", label: relationCustomLabel.trim() || null }
          : { type: relationType.trim(), label: null };
        if (target && predicate.type && (predicate.type !== "custom" || predicate.label)) void runOperation(
          t("governance.entity.creatingRelation"),
          t("governance.entity.relationCreated"),
          buildCreateRelation(detail.id, predicate, target, null, null, relationNote.trim() || null, [], true),
          () => { relationTarget = ""; relationNote = ""; },
        );
      }}>
        <label for="relation-target">{t("governance.relation.objectId")}</label>
        <input id="relation-target" bind:value={relationTarget} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <label for="relation-type">{t("governance.relation.type")}</label>
        <select id="relation-type" bind:value={relationType} disabled={working || detail.degraded}>
          <option value="participates_in">{t("governance.predicate.participatesIn")}</option><option value="responsible_for">{t("governance.predicate.responsibleFor")}</option>
          <option value="belongs_to">{t("governance.predicate.belongsTo")}</option><option value="uses">{t("governance.predicate.uses")}</option>
          <option value="depends_on">{t("governance.predicate.dependsOn")}</option><option value="produces">{t("governance.predicate.produces")}</option>
          <option value="assigned_to">{t("governance.predicate.assignedTo")}</option><option value="occurs_at">{t("governance.predicate.occursAt")}</option>
          <option value="custom">{t("governance.predicate.custom")}</option>
        </select>
        {#if relationType === "custom"}
          <label for="relation-label">{t("governance.relation.customLabel")}</label>
          <input id="relation-label" bind:value={relationCustomLabel} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        {/if}
        <label for="relation-note">{t("governance.relation.note")}</label>
        <textarea id="relation-note" bind:value={relationNote} disabled={working || detail.degraded}></textarea>
        <button type="submit" disabled={working || detail.degraded || !relationTarget.trim() || (relationType === "custom" && !relationCustomLabel.trim())}>{t("governance.entity.createRelation")}</button>
      </form>
    </details>
    <details>
      <summary>{t("governance.entity.bindPerson")}</summary>
      <form class="disclosure-form" onsubmit={(event) => {
        event.preventDefault();
        if (personMentionId.trim() && personEntityId.trim()) void runOperation(
          t("governance.entity.bindingPerson"),
          t("governance.entity.personBound"),
          buildBindPerson(personMentionId.trim(), personEntityId.trim()),
          () => { personMentionId = ""; },
        );
      }}>
        <label for="person-mention">{t("governance.entity.mentionId")}</label>
        <input id="person-mention" bind:value={personMentionId} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <label for="person-id">{t("governance.entity.personEntityId")}</label>
        <input id="person-id" bind:value={personEntityId} disabled={working || detail.degraded} aria-describedby="entity-feedback" />
        <button type="submit" disabled={working || detail.degraded || !personMentionId.trim() || !personEntityId.trim()}>{t("governance.entity.bindPerson")}</button>
      </form>
    </details>
  </section>
  {/if}

  <section class="section" aria-labelledby="entity-relations">
    <h3 id="entity-relations">{t("governance.entity.relationsLabel")}</h3>
    <h4>{t("governance.entity.currentRelations")} <span>{currentRelations.length}</span></h4>
    <ul class="relation-list">
      {#each currentRelations as relation (relation.id)}
        <li>
          <button type="button" onclick={() => onOpenRelation(relation.id)}>
            <span>{`${displayEntityName(relation.subject_id)} → ${relationLabel(relation)} → ${displayEntityName(relation.object_id)}`}</span>
            <small>{t("governance.entity.relationMeta", { confidence: Math.round(relation.confidence * 100), count: relation.evidence_count })}</small>
          </button>
        </li>
      {/each}
      {#if currentRelations.length === 0}<li class="empty-line">{t("governance.entity.noCurrentRelations")}</li>{/if}
    </ul>
    {#if !simple}
      <h4>{t("governance.entity.historicalRelations")} <span>{historicalRelations.length}</span></h4>
      <ul class="relation-list">
        {#each historicalRelations as relation (relation.id)}
          <li>
            <button type="button" onclick={() => onOpenRelation(relation.id)}>
              <span>{relation.subject_id === detail.id ? `${detail.name} → ${relationLabel(relation)} → ${relation.object_id}` : `${relation.subject_id} → ${relationLabel(relation)} → ${detail.name}`}</span>
              <small>{relation.valid_to ? t("governance.entity.validTo", { date: relation.valid_to }) : t("governance.entity.historicalVersion")}</small>
            </button>
          </li>
        {/each}
        {#if historicalRelations.length === 0}<li class="empty-line">{t("governance.entity.noHistoricalRelations")}</li>{/if}
      </ul>
    {/if}
  </section>

  <section class="section evidence" aria-labelledby="entity-evidence">
    <h3 id="entity-evidence">{simple ? t("governance.entity.linkedNotes") : t("governance.evidence")}</h3>
    {#if mentionsLoading}
      <p class="empty-line">{simple ? t("governance.entity.loadingLinkedNotes") : t("governance.entity.loadingMentions")}</p>
    {:else}
      {#if simple}
        <ul class="source-notes">
          {#each mentionGroups as group (group.noteId)}
            <li>
              <a class="note-link" href={'/notes/' + encodeURIComponent(group.noteId)}>
                <span>{group.title}</span>
                {#if group.time || group.duration}
                  <small class="note-meta">
                    {#if group.time}<time datetime={group.startedAt}>{group.time}</time>{/if}
                    {#if group.time && group.duration}<span aria-hidden="true">·</span>{/if}
                    {#if group.duration}<span>{group.duration}</span>{/if}
                  </small>
                {/if}
              </a>
            </li>
          {/each}
        </ul>
        {#if mentionGroups.length === 0}<p class="empty-line">{t("governance.entity.noLinkedNotes")}</p>{/if}
      {:else}
        {#each mentionGroups as group (group.noteId)}
          <div class="note-group">
            <a class="note-link" href={'/notes/' + encodeURIComponent(group.noteId)}>
              <span>{group.title}</span>
              {#if group.time || group.duration}
                <small class="note-meta">
                  {#if group.time}<time datetime={group.startedAt}>{group.time}</time>{/if}
                  {#if group.time && group.duration}<span aria-hidden="true">·</span>{/if}
                  {#if group.duration}<span>{group.duration}</span>{/if}
                </small>
              {/if}
            </a>
            {#each group.items as mention (mention.id)}
              <blockquote>
                <p>{mention.quote}</p>
                <footer>{t("governance.mention.position", { p: mention.paragraph_index + 1, start: mention.start_offset, end: mention.end_offset })}</footer>
              </blockquote>
            {/each}
          </div>
        {/each}
        {#if mentions.length === 0}<p class="empty-line">{t("governance.entity.noMentions")}</p>{/if}
      {/if}
    {/if}
  </section>

  {#if !simple}<div class="feedback-actions">
    <p id="entity-feedback" class:error={Boolean(controller.error)} aria-live="polite">{status}</p>
    {#if controller.refreshError}<button type="button" disabled={working} onclick={() => controller.retryRefresh().then(() => { status = t("governance.refreshed"); }).catch(() => { status = controller.refreshError; })}>{t("governance.retryRefresh")}</button>{/if}
    {#if controller.lastOperationId}<button type="button" disabled={working} onclick={undoLastOperation}>{t("governance.entity.undoLast")}</button>{/if}
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
  .note-link { color: var(--accent); text-decoration: none; overflow-wrap: anywhere; }
  .note-link > span:first-child { min-width: 0; font-size: 0.82rem; }
  .note-meta { display: flex; flex: none; gap: 5px; color: var(--ink-faint); font-size: 0.7rem; font-variant-numeric: tabular-nums; font-weight: 400; white-space: nowrap; }
  .note-meta span { font-size: inherit; }
  .note-group > .note-link { display: flex; align-items: baseline; justify-content: space-between; gap: 12px; }
  .source-notes { display: grid; gap: 0; margin: 0; padding: 0; list-style: none; }
  .source-notes li { border-top: 1px solid var(--hairline); }
  .source-notes .note-link { display: flex; align-items: baseline; justify-content: space-between; gap: 10px; padding: 10px 0; }
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
