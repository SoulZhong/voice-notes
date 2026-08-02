<script lang="ts">
  import { onMount, untrack } from "svelte";
  import type { MentionEvidence, SemanticEntityDetail } from "./knowledge";
  import {
    buildSplitEntity,
    canSubmitSplit,
    createGovernanceController,
    splitPreview,
    task10GovernanceApi,
  } from "./knowledgeGovernance";
  import { t } from "$lib/i18n/index.svelte";

  let {
    entity,
    mentions,
    onClose,
    onCommitted,
  }: {
    entity: SemanticEntityDetail;
    mentions: MentionEvidence[];
    onClose: () => void;
    onCommitted: () => Promise<void>;
  } = $props();

  let dialog = $state<HTMLDialogElement>();
  let returnFocus: HTMLElement | null = null;
  let closed = false;
  let selectedIds = $state(new Set<string>());
  let name = $state(untrack(() => t("governance.split.defaultName", { name: entity.name })));
  let kind = $state(untrack(() => entity.kind));
  let aliases = $state("");
  let working = $state(false);
  let status = $state("");
  let splitOperationId = $state<string | null>(null);
  let undone = $state(false);
  const controller = createGovernanceController(task10GovernanceApi, () => onCommitted());

  const groups = $derived.by(() => {
    const byNote = new Map<string, MentionEvidence[]>();
    for (const mention of [...mentions].sort((a, b) =>
      a.note_id.localeCompare(b.note_id) ||
      a.paragraph_index - b.paragraph_index ||
      a.start_offset - b.start_offset ||
      a.id.localeCompare(b.id)
    )) {
      byNote.set(mention.note_id, [...(byNote.get(mention.note_id) ?? []), mention]);
    }
    return [...byNote.entries()].map(([noteId, items]) => ({ noteId, items }));
  });
  const selectedMentions = $derived(mentions.filter((mention) => selectedIds.has(mention.id)));
  const preview = $derived(splitPreview(selectedMentions, mentions));

  onMount(() => {
    returnFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
    dialog?.showModal();
  });

  function finishClose() {
    if (closed) return;
    closed = true;
    onClose();
    returnFocus?.focus();
  }

  function dismiss() {
    dialog?.close();
  }

  function toggleMention(id: string, checked: boolean) {
    const next = new Set(selectedIds);
    if (checked) next.add(id);
    else next.delete(id);
    selectedIds = next;
  }

  function toggleNote(items: MentionEvidence[], checked: boolean) {
    const next = new Set(selectedIds);
    for (const item of items) {
      if (checked) next.add(item.id);
      else next.delete(item.id);
    }
    selectedIds = next;
  }

  async function commitSplit() {
    if (working || !canSubmitSplit(preview) || !name.trim()) return;
    working = true;
    status = t("governance.split.working");
    try {
      const result = await controller.split(buildSplitEntity(
        entity.id,
        name.trim(),
        kind.trim() || null,
        aliases.split("、").map((alias) => alias.trim()).filter(Boolean),
        preview.selectedMentionIds,
      ));
      splitOperationId = result.operation_id;
      status = controller.refreshError || t("governance.split.done", { n: preview.mentionCount });
    } catch {
      status = controller.error;
    } finally {
      working = false;
    }
  }

  async function undoSplit() {
    if (!splitOperationId || working || undone) return;
    working = true;
    status = t("governance.split.undoing");
    try {
      await controller.undo(splitOperationId);
      undone = true;
      status = controller.refreshError || t("governance.split.undone");
    } catch {
      status = controller.error;
    } finally {
      working = false;
    }
  }
</script>

<dialog
  bind:this={dialog}
  class="split-dialog"
  aria-labelledby="split-title"
  aria-describedby="split-description split-feedback"
  onclose={finishClose}
  oncancel={() => { status = ""; }}
>
  <div class="dialog-frame">
    <header>
      <div>
        <p class="eyebrow">{t("governance.split.eyebrow")}</p>
        <h2 id="split-title">{t("governance.split.title", { name: entity.name })}</h2>
      </div>
      <button class="icon-button" type="button" aria-label={t("governance.split.closeAria")} onclick={dismiss}>×</button>
    </header>

    <p id="split-description" class="description">
      {t("governance.split.description")}
    </p>

    <div class="identity-fields">
      <label>
        <span>{t("governance.split.name")}</span>
        <input bind:value={name} disabled={working || splitOperationId !== null} aria-describedby="split-feedback" />
      </label>
      <label>
        <span>{t("governance.split.kind")}</span>
        <input bind:value={kind} disabled={working || splitOperationId !== null} aria-describedby="split-feedback" />
      </label>
      <label class="wide-field">
        <span>{t("governance.split.aliases")}</span>
        <input bind:value={aliases} disabled={working || splitOperationId !== null} aria-describedby="split-feedback" />
      </label>
    </div>

    <div class="selection-heading">
      <h3>{t("governance.split.selectTitle")}</h3>
      <p>{t("governance.split.preview", { notes: preview.noteCount, mentions: preview.mentionCount, relations: preview.affectedRelationCount })}</p>
    </div>

    <div class="evidence-list">
      {#each groups as group (group.noteId)}
        {@const allSelected = group.items.every((item) => selectedIds.has(item.id))}
        <section class="note-group">
          <div class="note-heading">
            <a href={'/notes/' + encodeURIComponent(group.noteId)}>{t("governance.noteFallback", { id: group.noteId })}</a>
            <label class="select-note">
              <input
                type="checkbox"
                checked={allSelected}
                disabled={working || splitOperationId !== null}
                onchange={(event) => toggleNote(group.items, event.currentTarget.checked)}
              />
              {t("governance.split.selectNote")}
            </label>
          </div>
          <ul>
            {#each group.items as mention (mention.id)}
              <li>
                <label class="mention-choice">
                  <input
                    type="checkbox"
                    checked={selectedIds.has(mention.id)}
                    disabled={working || splitOperationId !== null}
                    onchange={(event) => toggleMention(mention.id, event.currentTarget.checked)}
                  />
                  <span>
                    <q>{mention.quote}</q>
                    <small>{t("governance.mention.position", { p: mention.paragraph_index + 1, start: mention.start_offset, end: mention.end_offset })}</small>
                  </span>
                </label>
              </li>
            {/each}
          </ul>
        </section>
      {/each}
      {#if mentions.length === 0}
        <p class="empty">{t("governance.split.empty")}</p>
      {/if}
    </div>

    <p id="split-feedback" class:error={Boolean(controller.error)} class="feedback" aria-live="polite">
      {status}
    </p>

    <footer>
      <button class="secondary" type="button" onclick={dismiss}>{t("governance.split.keep")}</button>
      {#if splitOperationId && !undone}
        <button class="secondary" type="button" disabled={working} onclick={undoSplit}>{t("governance.split.undo")}</button>
      {:else if !splitOperationId}
        <button
          class="primary"
          type="button"
          disabled={working || !canSubmitSplit(preview) || !name.trim()}
          onclick={commitSplit}
        >{t("governance.split.commit")}</button>
      {/if}
    </footer>
  </div>
</dialog>

<style>
  .split-dialog {
    width: min(720px, calc(100vw - 32px));
    max-height: min(780px, calc(100dvh - 32px));
    padding: 0;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-xl);
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--shadow-popover);
  }
  .split-dialog::backdrop { background: light-dark(rgba(20, 21, 22, 0.28), rgba(0, 0, 0, 0.64)); }
  .dialog-frame { display: flex; flex-direction: column; max-height: inherit; padding: 24px; box-sizing: border-box; }
  header, footer, .note-heading, .selection-heading { display: flex; align-items: center; justify-content: space-between; gap: 16px; }
  .eyebrow { margin: 0 0 4px; color: var(--ink-faint); font-size: 0.75rem; letter-spacing: 0.08em; }
  h2 { margin: 0; font-size: 1.25rem; font-weight: 550; }
  h3 { margin: 0; font-size: 0.9rem; font-weight: 550; }
  .description { margin: 12px 0 20px; color: var(--ink-secondary); font-size: 0.9rem; line-height: 1.6; }
  .icon-button { width: 36px; height: 36px; border: 0; border-radius: var(--radius-full); background: transparent; color: var(--ink-secondary); font-size: 1.3rem; cursor: pointer; }
  .icon-button:hover { background: var(--surface-soft); color: var(--ink); }
  .identity-fields { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; padding-bottom: 20px; border-bottom: 1px solid var(--hairline); }
  .wide-field { grid-column: 1 / -1; }
  label > span:first-child, .identity-fields label { display: grid; gap: 6px; color: var(--ink-secondary); font-size: 0.8rem; }
  input:not([type="checkbox"]) { box-sizing: border-box; width: 100%; padding: 9px 10px; border: 1px solid var(--hairline); border-radius: var(--radius-md); background: var(--surface-press); color: var(--ink); font: inherit; }
  input:not([type="checkbox"]):focus-visible, button:focus-visible, a:focus-visible, input[type="checkbox"]:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  .selection-heading { align-items: baseline; margin: 20px 0 8px; }
  .selection-heading p { margin: 0; color: var(--ink-faint); font-size: 0.78rem; font-variant-numeric: tabular-nums; }
  .evidence-list { min-height: 160px; overflow-y: auto; border-top: 1px solid var(--hairline); }
  .note-group { padding: 14px 0; border-bottom: 1px solid var(--hairline); }
  .note-heading a { color: var(--accent); font-size: 0.82rem; text-decoration: none; overflow-wrap: anywhere; }
  .select-note { display: flex; align-items: center; gap: 7px; color: var(--ink-secondary); font-size: 0.78rem; cursor: pointer; }
  ul { list-style: none; margin: 8px 0 0; padding: 0; display: grid; gap: 4px; }
  .mention-choice { display: grid; grid-template-columns: 24px 1fr; gap: 8px; padding: 8px 4px; cursor: pointer; }
  .mention-choice:hover { background: var(--surface-soft); }
  .mention-choice input { margin-top: 3px; }
  q { display: block; color: var(--ink); font-size: 0.9rem; line-height: 1.65; overflow-wrap: anywhere; }
  small { display: block; margin-top: 4px; color: var(--ink-faint); font-size: 0.72rem; }
  .empty { color: var(--ink-secondary); font-size: 0.88rem; }
  .feedback { min-height: 1.4em; margin: 12px 0 0; color: var(--success); font-size: 0.8rem; }
  .feedback.error { color: var(--danger-ink); }
  footer { justify-content: flex-end; margin-top: 8px; padding-top: 12px; border-top: 1px solid var(--hairline); }
  footer button { min-height: 36px; padding: 7px 14px; border-radius: var(--radius-md); font: inherit; font-size: 0.84rem; cursor: pointer; }
  .secondary { border: 1px solid var(--hairline-strong); background: transparent; color: var(--ink-secondary); }
  .secondary:hover { background: var(--surface-soft); color: var(--ink); }
  .primary { border: 0; background: var(--primary); color: var(--on-primary); font-weight: 550; }
  button:disabled, input:disabled { opacity: 0.5; cursor: default; }
  @media (pointer: coarse) {
    button, .select-note, .mention-choice { min-height: 44px; }
    .icon-button { width: 44px; }
  }
  @media (max-width: 560px) {
    .dialog-frame { padding: 18px; }
    .identity-fields { grid-template-columns: 1fr; }
    .wide-field { grid-column: auto; }
    .selection-heading, .note-heading { align-items: flex-start; flex-direction: column; gap: 8px; }
  }
  @media (prefers-reduced-motion: reduce) {
    *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; }
  }
</style>
