<script lang="ts">
  import { relationDetail, semanticEntityDetail, type RelationDetail } from "./knowledge";
  import { DEFAULT_KNOWLEDGE_FILTER, relationLabel } from "./knowledgeView";
  import {
    buildConfirmRelation,
    buildEditRelation,
    buildEndRelation,
    buildSuppressRelation,
    createGovernanceController,
    retainLastKnownRelation,
    task10GovernanceApi,
  } from "./knowledgeGovernance";
  import { t } from "$lib/i18n/index.svelte";

  let {
    relationId,
    onClose,
    onChanged,
    relationLoader = relationDetail,
    resolveEntityName,
    readOnly = false,
    simple = false,
  }: {
    relationId: string;
    onClose: () => void;
    onChanged: () => Promise<void>;
    relationLoader?: (relationId: string) => Promise<RelationDetail | null>;
    resolveEntityName?: (entityId: string) => string | undefined;
    readOnly?: boolean;
    /** Everyday graph detail shows meaning and evidence without governance controls. */
    simple?: boolean;
  } = $props();

  let detail = $state<RelationDetail | null>(null);
  let lastKnown = $state<RelationDetail | null>(null);
  let loading = $state(true);
  let loadError = $state("");
  let working = $state(false);
  let status = $state("");
  let subjectId = $state("");
  let objectId = $state("");
  let predicateType = $state("");
  let predicateLabel = $state("");
  let validFrom = $state("");
  let validTo = $state("");
  let relationNote = $state("");
  let endAt = $state("");
  let subjectName = $state("");
  let objectName = $state("");
  let suppressDialog = $state<HTMLDialogElement>();
  let generation = 0;
  let loadedRelationId = "";
  const entityFilter = { ...DEFAULT_KNOWLEDGE_FILTER, include_history: true };

  async function refreshAndReload() {
    await onChanged();
    await load(relationId);
  }
  const controller = createGovernanceController(task10GovernanceApi, refreshAndReload);

  async function load(id: string) {
    const current = ++generation;
    if (loadedRelationId !== id) {
      loadedRelationId = id;
      detail = null;
      lastKnown = null;
      subjectName = "";
      objectName = "";
    }
    loading = true;
    loadError = "";
    try {
      const value = await relationLoader(id);
      if (current !== generation) return;
      detail = value;
      lastKnown = retainLastKnownRelation(value, lastKnown);
      if (value) {
        subjectId = value.relation.subject_id;
        objectId = value.relation.object_id;
        predicateType = value.relation.predicate_type;
        predicateLabel = value.relation.predicate_label ?? "";
        validFrom = value.relation.valid_from ?? "";
        validTo = value.relation.valid_to ?? "";
        if (resolveEntityName) {
          subjectName = resolveEntityName(value.relation.subject_id) ?? "";
          objectName = resolveEntityName(value.relation.object_id) ?? "";
        } else {
          const [subject, object] = await Promise.all([
            semanticEntityDetail(value.relation.subject_id, entityFilter).catch(() => null),
            semanticEntityDetail(value.relation.object_id, entityFilter).catch(() => null),
          ]);
          if (current !== generation) return;
          subjectName = subject?.name ?? "";
          objectName = object?.name ?? "";
        }
      }
    } catch (cause) {
      if (current !== generation) return;
      detail = null;
      loadError = t("governance.relation.loadFailed", { e: cause instanceof Error ? cause.message : String(cause) });
    } finally {
      if (current === generation) loading = false;
    }
  }

  $effect(() => { void load(relationId); });

  async function runOperation(
    pendingText: string,
    successText: string,
    operation: Parameters<typeof controller.submit>[0],
  ) {
    if (working) return;
    working = true;
    status = pendingText;
    try {
      await controller.submit(operation);
      status = controller.refreshError || successText;
    } catch {
      status = controller.error;
    } finally {
      working = false;
    }
  }

  function swapDirection() {
    const previousSubject = subjectId;
    subjectId = objectId;
    objectId = previousSubject;
  }

  async function saveEdit() {
    if (!detail || !subjectId.trim() || !objectId.trim() || !predicateType.trim()) return;
    const predicate = {
      type: predicateType.trim(),
      label: predicateType.trim() === "custom" ? predicateLabel.trim() || null : null,
    };
    if (predicate.type === "custom" && !predicate.label) {
      status = t("governance.relation.customLabelRequired");
      return;
    }
    await runOperation(
      t("governance.relation.saving"),
      t("governance.relation.saved"),
      buildEditRelation(
        detail.relation.id,
        subjectId.trim(),
        predicate,
        objectId.trim(),
        validFrom.trim() || null,
        validTo.trim() || null,
        relationNote.trim() || null,
      ),
    );
  }

  async function suppressRelation() {
    if (!detail) return;
    suppressDialog?.close();
    await runOperation(
      t("governance.relation.suppressing"),
      t("governance.relation.suppressed"),
      buildSuppressRelation(
        detail.relation.subject_id,
        { type: detail.relation.predicate_type, label: detail.relation.predicate_label },
        detail.relation.object_id,
      ),
    );
  }

  async function undoLast() {
    if (!controller.lastOperationId || working) return;
    working = true;
    status = t("governance.relation.undoing");
    try {
      await controller.undo(controller.lastOperationId);
      status = controller.refreshError || t("governance.relation.undone");
    } catch {
      status = controller.error;
    } finally {
      working = false;
    }
  }

  function handleEscape(event: KeyboardEvent) {
    if (event.key === "Escape" && !document.querySelector("dialog[open]")) onClose();
  }
</script>

<svelte:window onkeydown={handleEscape} />

<article class="drawer" aria-labelledby="relation-drawer-title">
  <header>
    <div>
      <p class="eyebrow">{t("governance.relation.eyebrow")}</p>
      <h2 id="relation-drawer-title">{t("governance.relation.title")}</h2>
    </div>
    <button class="close" type="button" aria-label={t("governance.relation.closeAria")} onclick={onClose}>×</button>
  </header>

  {#if loading}
    <p class="state" role="status">{t("governance.relation.loading")}</p>
  {:else if loadError}
    <p class="state error" role="alert">{loadError}</p>
  {:else if !detail}
    <p class="state">{t("governance.relation.gone")}</p>
    {#if lastKnown}
      {@const relation = lastKnown.relation}
      <section class="direction tombstone" aria-label={t("governance.relation.tombstoneAria")}>
        <span><b>{subjectName || t("governance.relation.unresolvedEntity")}</b><small>{relation.subject_id}</small></span>
        <strong>→ {relationLabel(relation)} →</strong>
        <span><b>{objectName || t("governance.relation.unresolvedEntity")}</b><small>{relation.object_id}</small></span>
      </section>
      <p class="muted">{t("governance.relation.tombstoneNote")}</p>
    {/if}
  {:else}
    {@const relation = detail.relation}
    <section class="direction" aria-label={t("governance.relation.directionAria")}>
      <span><b>{subjectName || t("governance.relation.unresolvedEntity")}</b>{#if !simple}<small>{relation.subject_id}</small>{/if}</span>
      <strong>→ {relationLabel(relation)} →</strong>
      <span><b>{objectName || t("governance.relation.unresolvedEntity")}</b>{#if !simple}<small>{relation.object_id}</small>{/if}</span>
    </section>

    <section class="section" aria-labelledby="relation-overview">
      <h3 id="relation-overview">{t("governance.overview")}</h3>
      <dl>
        <div><dt>{t("governance.statusLabel")}</dt><dd>{relation.status === "current" ? t("governance.relation.statusCurrent") : t("governance.relation.statusHistorical")}</dd></div>
        <div><dt>{t("governance.relation.confidence")}</dt><dd>{Math.round(relation.confidence * 100)}%</dd></div>
        <div><dt>{t("governance.evidence")}</dt><dd>{t("governance.countItems", { n: relation.evidence_count })}</dd></div>
        {#if !simple}
          <div><dt>{t("governance.relation.origin")}</dt><dd>{relation.origin}</dd></div>
          <div><dt>{t("governance.relation.provider")}</dt><dd>{detail.provider || t("governance.relation.manualGovernance")}</dd></div>
          <div><dt>{t("governance.relation.model")}</dt><dd>{detail.model || t("governance.relation.noModel")}</dd></div>
          <div><dt>{t("governance.relation.validFrom")}</dt><dd>{relation.valid_from || t("governance.relation.unbounded")}</dd></div>
          <div><dt>{t("governance.relation.validToLabel")}</dt><dd>{relation.valid_to || t("governance.relation.ongoing")}</dd></div>
        {/if}
      </dl>
    </section>

    {#if !simple}<section class="section versions" aria-labelledby="relation-index-state">
      <h3 id="relation-index-state">{t("governance.relation.indexStateTitle")}</h3>
      <p>{t("governance.relation.indexStateBody", { status: relation.status === "current" ? t("governance.relation.statusCurrent") : t("governance.relation.statusHistorical") })}</p>
      <p class="muted">{t("governance.relation.singleRelationNote")}</p>
    </section>{/if}

    <section class="section evidence" aria-labelledby="relation-evidence">
      <h3 id="relation-evidence">{t("governance.relation.allEvidence")}</h3>
      {#each detail.evidence as item (item.id)}
        <blockquote>
          <p>{item.quote}</p>
          <footer>
            {#if readOnly}
              <span>{t("governance.relation.fixtureNote", { id: item.note_id })}</span>
            {:else}
              <a href={'/notes/' + encodeURIComponent(item.note_id) + '#paragraph-' + item.paragraph_index}>{t("governance.relation.openNote", { id: item.note_id })}</a>
            {/if}
            <span>{simple
              ? t("governance.mention.paragraph", { p: item.paragraph_index + 1 })
              : t("governance.mention.position", { p: item.paragraph_index + 1, start: item.start_offset, end: item.end_offset })}</span>
            {#if !simple && item.source_seqs.length > 0}<span>{t("governance.relation.sourceSeqs", { seqs: item.source_seqs.join("、") })}</span>{/if}
          </footer>
        </blockquote>
      {/each}
      {#if detail.evidence.length === 0}
        <p class="assertion">{t("governance.relation.userAssertion")}</p>
      {/if}
    </section>

    {#if readOnly}
      <section class="section actions" aria-labelledby="relation-actions">
        <h3 id="relation-actions">{t("governance.relation.isolationTitle")}</h3>
        <p class="unavailable">{t("governance.relation.isolationBody")}</p>
      </section>
    {:else if !simple}
      <section class="section actions" aria-labelledby="relation-actions">
      <h3 id="relation-actions">{t("governance.relation.actionsTitle")}</h3>
      <div class="primary-actions">
        <button type="button" disabled={working} onclick={() => runOperation(t("governance.relation.confirming"), t("governance.relation.confirmed"), buildConfirmRelation(relation.id))}>{t("governance.relation.confirm")}</button>
        <button class="danger" type="button" disabled={working} onclick={() => suppressDialog?.showModal()}>{t("governance.suppressReject")}</button>
      </div>

      <details>
        <summary>{t("governance.relation.editSummary")}</summary>
        <form onsubmit={(event) => { event.preventDefault(); void saveEdit(); }}>
          <label for="relation-subject">{t("governance.relation.subjectId")}</label>
          <input id="relation-subject" bind:value={subjectId} disabled={working} aria-describedby="relation-feedback" />
          <button class="swap" type="button" disabled={working} onclick={swapDirection}>{t("governance.relation.swap")}</button>
          <label for="relation-object">{t("governance.relation.objectId")}</label>
          <input id="relation-object" bind:value={objectId} disabled={working} aria-describedby="relation-feedback" />
          <label for="relation-predicate">{t("governance.relation.type")}</label>
          <select id="relation-predicate" bind:value={predicateType} disabled={working}>
            <option value="participates_in">{t("governance.predicate.participatesIn")}</option><option value="responsible_for">{t("governance.predicate.responsibleFor")}</option>
            <option value="belongs_to">{t("governance.predicate.belongsTo")}</option><option value="uses">{t("governance.predicate.uses")}</option>
            <option value="depends_on">{t("governance.predicate.dependsOn")}</option><option value="produces">{t("governance.predicate.produces")}</option>
            <option value="assigned_to">{t("governance.predicate.assignedTo")}</option><option value="occurs_at">{t("governance.predicate.occursAt")}</option>
            <option value="custom">{t("governance.predicate.custom")}</option>
          </select>
          {#if predicateType === "custom"}
            <label for="relation-custom-label">{t("governance.relation.customLabel")}</label>
            <input id="relation-custom-label" bind:value={predicateLabel} disabled={working} aria-describedby="relation-feedback" />
          {/if}
          <label for="relation-valid-from">{t("governance.relation.validFromInput")}</label>
          <input id="relation-valid-from" bind:value={validFrom} disabled={working} aria-describedby="relation-feedback" />
          <label for="relation-valid-to">{t("governance.relation.validToInput")}</label>
          <input id="relation-valid-to" bind:value={validTo} disabled={working} aria-describedby="relation-feedback" />
          <label for="relation-note">{t("governance.relation.note")}</label>
          <textarea id="relation-note" bind:value={relationNote} disabled={working}></textarea>
          <button type="submit" disabled={working || !subjectId.trim() || !objectId.trim() || !predicateType.trim() || (predicateType === "custom" && !predicateLabel.trim())}>{t("governance.relation.save")}</button>
        </form>
      </details>

      <details>
        <summary>{t("governance.relation.end")}</summary>
        <form onsubmit={(event) => {
          event.preventDefault();
          if (endAt.trim()) void runOperation(t("governance.relation.ending"), t("governance.relation.ended"), buildEndRelation(relation.id, endAt.trim()));
        }}>
          <label for="relation-end">{t("governance.relation.endAt")}</label>
          <input id="relation-end" bind:value={endAt} disabled={working} aria-describedby="relation-feedback" />
          <button type="submit" disabled={working || !endAt.trim()}>{t("governance.relation.end")}</button>
        </form>
        <p class="unavailable">{t("governance.relation.noRestoreNote")}</p>
      </details>
      </section>
    {/if}
  {/if}

  {#if !readOnly && !simple}
    <div class="feedback-row">
      <p id="relation-feedback" class:error={Boolean(controller.error)} aria-live="polite">{status}</p>
      <div>
        {#if controller.refreshError}<button type="button" disabled={working} onclick={() => controller.retryRefresh().then(() => { status = t("governance.refreshed"); }).catch(() => { status = controller.refreshError; })}>{t("governance.retryRefresh")}</button>{/if}
        {#if controller.lastOperationId}<button type="button" disabled={working} onclick={undoLast}>{t("governance.relation.undoLast")}</button>{/if}
      </div>
    </div>
  {/if}
</article>

{#if !readOnly}
  <dialog bind:this={suppressDialog} class="confirm-dialog" aria-labelledby="suppress-title" aria-describedby="suppress-description">
    <h2 id="suppress-title">{t("governance.relation.suppressTitle")}</h2>
    <p id="suppress-description">{t("governance.relation.suppressBody")}</p>
    <div>
      <button type="button" onclick={() => suppressDialog?.close()}>{t("governance.relation.suppressKeep")}</button>
      <button class="danger" type="button" onclick={suppressRelation}>{t("governance.suppressReject")}</button>
    </div>
  </dialog>
{/if}

<style>
  .drawer { color: var(--ink); }
  header { display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; padding-bottom: 18px; }
  .eyebrow { margin: 0 0 4px; color: var(--ink-faint); font-size: 0.72rem; letter-spacing: 0.08em; }
  h2 { margin: 0; font-size: 1.3rem; font-weight: 550; }
  .close { width: 36px; height: 36px; border: 0; border-radius: var(--radius-full); background: transparent; color: var(--ink-secondary); font-size: 1.3rem; cursor: pointer; }
  .close:hover { background: var(--surface-soft); color: var(--ink); }
  .state { color: var(--ink-secondary); font-size: 0.86rem; line-height: 1.55; }
  .state.error { color: var(--danger-ink); }
  .direction { display: grid; gap: 5px; padding: 14px 0 20px; border-top: 1px solid var(--hairline); overflow-wrap: anywhere; }
  .direction span { display: grid; gap: 2px; font-size: 0.9rem; }
  .direction span b { font-weight: 550; }
  .direction span small { color: var(--ink-faint); font: 0.68rem/1.4 ui-monospace, SFMono-Regular, Menlo, monospace; }
  .direction strong { color: var(--accent); font-size: 0.82rem; font-weight: 500; }
  .direction.tombstone { opacity: 0.78; }
  .section { padding: 18px 0; border-top: 1px solid var(--hairline); }
  h3 { margin: 0 0 12px; color: var(--ink-secondary); font-size: 0.78rem; font-weight: 550; letter-spacing: 0.06em; }
  dl { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; margin: 0; }
  dl div { min-width: 0; }
  dt { color: var(--ink-faint); font-size: 0.7rem; }
  dd { margin: 3px 0 0; color: var(--ink); font-size: 0.82rem; overflow-wrap: anywhere; font-variant-numeric: tabular-nums; }
  .versions p { margin: 0; color: var(--ink); font-size: 0.82rem; line-height: 1.55; overflow-wrap: anywhere; }
  .muted { color: var(--ink-faint) !important; }
  .unavailable { margin: 0; padding: 0 2px 14px; color: var(--ink-faint); font-size: 0.76rem; line-height: 1.55; }
  blockquote { margin: 0; padding: 12px 0 12px 12px; border-top: 1px solid var(--hairline); border-left: 2px solid var(--hairline-strong); }
  blockquote p { margin: 0; color: var(--ink); font-size: 0.86rem; line-height: 1.65; overflow-wrap: anywhere; }
  blockquote footer { display: grid; gap: 3px; margin-top: 7px; color: var(--ink-faint); font-size: 0.7rem; overflow-wrap: anywhere; }
  blockquote a { color: var(--accent); text-decoration: none; }
  .assertion { margin: 0; padding: 12px; border: 1px solid var(--hairline); border-radius: var(--radius-md); color: var(--ink-secondary); font-size: 0.86rem; }
  .primary-actions { display: flex; flex-wrap: wrap; gap: 8px; padding-bottom: 12px; }
  button { min-height: 34px; padding: 7px 10px; border: 1px solid var(--hairline-strong); border-radius: var(--radius-md); background: transparent; color: var(--ink-secondary); font: inherit; font-size: 0.8rem; cursor: pointer; }
  button:hover:not(:disabled) { background: var(--surface-soft); color: var(--ink); }
  button.danger { border-color: var(--danger-line); color: var(--danger-ink); }
  button.danger:hover { background: var(--danger-tint); }
  button:disabled, input:disabled, select:disabled, textarea:disabled { opacity: 0.5; cursor: default; }
  details { border-top: 1px solid var(--hairline); }
  summary { padding: 12px 2px; color: var(--ink); font-size: 0.84rem; cursor: pointer; }
  form { display: grid; gap: 7px; padding: 0 2px 14px; }
  label { color: var(--ink-secondary); font-size: 0.76rem; }
  input, select, textarea { box-sizing: border-box; width: 100%; padding: 8px 9px; border: 1px solid var(--hairline); border-radius: var(--radius-md); background: var(--surface-press); color: var(--ink); font: inherit; font-size: 0.82rem; }
  textarea { min-height: 68px; resize: vertical; }
  input:focus-visible, select:focus-visible, textarea:focus-visible, button:focus-visible, summary:focus-visible, a:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  .swap { justify-self: start; }
  .feedback-row { position: sticky; bottom: 0; padding: 11px 0; border-top: 1px solid var(--hairline); background: var(--surface); }
  .feedback-row p { min-height: 1.3em; margin: 0 0 6px; color: var(--success); font-size: 0.78rem; line-height: 1.45; }
  .feedback-row p.error { color: var(--danger-ink); }
  .feedback-row > div { display: flex; flex-wrap: wrap; gap: 7px; }
  .confirm-dialog { width: min(420px, calc(100vw - 32px)); padding: 20px; border: 1px solid var(--danger-line); border-radius: var(--radius-xl); background: var(--surface); color: var(--ink); box-shadow: var(--shadow-popover); }
  .confirm-dialog::backdrop { background: light-dark(rgba(20, 21, 22, 0.28), rgba(0, 0, 0, 0.64)); }
  .confirm-dialog h2 { font-size: 1.05rem; }
  .confirm-dialog p { color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  .confirm-dialog div { display: flex; justify-content: flex-end; gap: 8px; }
  @media (pointer: coarse) { button, summary { min-height: 44px; } .close { width: 44px; } }
  @media (prefers-reduced-motion: reduce) { *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; } }
</style>
