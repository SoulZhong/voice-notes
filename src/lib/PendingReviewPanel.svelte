<script lang="ts">
  import { relationDetail, type JsonValue, type PendingReviewItem } from "./knowledge";
  import {
    buildConfirmRelation,
    buildSuppressRelation,
    createGovernanceController,
    groupPending,
    pendingAfterLater,
    task10GovernanceApi,
  } from "./knowledgeGovernance";

  let {
    items,
    onClose,
    onChanged,
    onOpenRelation,
  }: {
    items: PendingReviewItem[];
    onClose: () => void;
    onChanged: () => Promise<void>;
    onOpenRelation: (id: string) => void;
  } = $props();

  let hiddenIds = $state(new Set<string>());
  let workingId = $state<string | null>(null);
  let status = $state("");
  const controller = createGovernanceController(task10GovernanceApi, () => onChanged());
  const visibleItems = $derived(pendingAfterLater(items, hiddenIds));
  const groups = $derived(groupPending(visibleItems));

  function record(value: JsonValue): Record<string, JsonValue> | null {
    return value !== null && typeof value === "object" && !Array.isArray(value)
      ? value as Record<string, JsonValue>
      : null;
  }

  function itemTitle(item: PendingReviewItem): string {
    const payload = record(item.payload);
    for (const key of ["label", "name", "reason", "message"] as const) {
      const value = payload?.[key];
      if (typeof value === "string" && value.trim()) return value;
    }
    return item.relation_id ? `关系 ${item.relation_id}` : `待整理项 ${item.id}`;
  }

  function payloadText(item: PendingReviewItem): string {
    return JSON.stringify(item.payload, null, 2);
  }

  async function confirmItem(item: PendingReviewItem) {
    if (!item.relation_id || workingId) {
      if (!item.relation_id) status = "这条待整理项没有可确认的关系 ID，可先打开编辑查看完整内容。";
      return;
    }
    workingId = item.id;
    status = "正在确认关系";
    try {
      await controller.submit(buildConfirmRelation(item.relation_id));
      status = controller.refreshError || "关系已确认并移出待整理";
    } catch {
      status = controller.error;
    } finally {
      workingId = null;
    }
  }

  async function suppressItem(item: PendingReviewItem) {
    if (workingId) return;
    const approved = window.confirm(`否决「${itemTitle(item)}」并永久抑制这条关系？模型重新抽取也不会自动恢复。`);
    if (!approved) return;
    workingId = item.id;
    status = "正在写入持久抑制裁决";
    try {
      const relation = item.relation_id ? await relationDetail(item.relation_id) : null;
      const payload = record(item.payload);
      const subjectId = relation?.relation.subject_id ?? (typeof payload?.subject_id === "string" ? payload.subject_id : null);
      const objectId = relation?.relation.object_id ?? (typeof payload?.object_id === "string" ? payload.object_id : null);
      const predicateType = relation?.relation.predicate_type ?? (typeof payload?.predicate_type === "string" ? payload.predicate_type : null);
      const predicateLabel = relation?.relation.predicate_label ?? (typeof payload?.predicate_label === "string" ? payload.predicate_label : null);
      if (!subjectId || !objectId || !predicateType) {
        status = "无法读取这条候选关系的主体、类型和客体；请先打开编辑并补全关系。";
        return;
      }
      await controller.submit(buildSuppressRelation(subjectId, { type: predicateType, label: predicateLabel }, objectId));
      status = controller.refreshError || "候选关系已否决并持久抑制";
    } catch {
      status = controller.error || "候选关系读取失败，请稍后重试。";
    } finally {
      workingId = null;
    }
  }

  function later(item: PendingReviewItem) {
    hiddenIds = new Set([...hiddenIds, item.id]);
    status = `已在本次会话稍后处理「${itemTitle(item)}」；没有写入后端。`;
  }

  async function undoLast() {
    if (!controller.lastOperationId || workingId) return;
    workingId = "undo";
    status = "正在撤销上次待整理操作";
    try {
      await controller.undo(controller.lastOperationId);
      status = controller.refreshError || "已撤销上次待整理操作";
    } catch {
      status = controller.error;
    } finally {
      workingId = null;
    }
  }
</script>

<svelte:window onkeydown={(event) => { if (event.key === "Escape") onClose(); }} />

<article class="pending" aria-labelledby="pending-title">
  <header>
    <div>
      <p class="eyebrow">知识治理队列</p>
      <h2 id="pending-title">待整理 {visibleItems.length}</h2>
    </div>
    <button class="close" type="button" aria-label="关闭待整理面板" onclick={onClose}>×</button>
  </header>

  <p class="intro">按证据风险排序处理。稍后处理只隐藏本次会话，否决会写入持久抑制。</p>

  {#each groups as group (group.key)}
    <section class="group" aria-labelledby={'pending-group-' + group.key}>
      <h3 id={'pending-group-' + group.key}>{group.label} <span>{group.items.length}</span></h3>
      <ul>
        {#each group.items as item (item.id)}
          <li>
            <div class="item-heading">
              <strong>{itemTitle(item)}</strong>
              <span>{item.kind}</span>
            </div>
            {#if item.note_id}<a class="note-link" href={'/notes/' + encodeURIComponent(item.note_id)}>打开笔记 {item.note_id}</a>{/if}
            <pre>{payloadText(item)}</pre>
            <div class="row-actions" aria-label={`处理 ${itemTitle(item)}`}>
              <button type="button" disabled={workingId !== null || !item.relation_id} onclick={() => confirmItem(item)}>确认关系</button>
              <button type="button" disabled={workingId !== null || !item.relation_id} onclick={() => item.relation_id && onOpenRelation(item.relation_id)}>编辑关系</button>
              <button class="danger" type="button" disabled={workingId !== null} onclick={() => suppressItem(item)}>否决并抑制</button>
              <button type="button" disabled={workingId !== null} onclick={() => later(item)}>稍后处理</button>
            </div>
          </li>
        {/each}
      </ul>
    </section>
  {/each}

  {#if groups.length === 0}
    <section class="empty">
      <h3>当前没有待整理项</h3>
      <p>新的低置信关系、身份或时间冲突出现后会汇入这里。</p>
    </section>
  {/if}

  <div class="feedback">
    <p class:error={Boolean(controller.error)} aria-live="polite">{status}</p>
    {#if controller.refreshError}<button type="button" disabled={workingId !== null} onclick={() => controller.retryRefresh().then(() => { status = "图谱已刷新"; }).catch(() => { status = controller.refreshError; })}>重试刷新图谱</button>{/if}
    {#if controller.lastOperationId}<button type="button" disabled={workingId !== null} onclick={undoLast}>撤销上次待整理操作</button>{/if}
  </div>
</article>

<style>
  .pending { color: var(--ink); }
  header { display: flex; align-items: flex-start; justify-content: space-between; gap: 12px; }
  .eyebrow { margin: 0 0 4px; color: var(--ink-faint); font-size: 0.72rem; letter-spacing: 0.08em; }
  h2 { margin: 0; font-size: 1.3rem; font-weight: 550; }
  .close { width: 36px; height: 36px; padding: 0; border: 0; border-radius: var(--radius-full); background: transparent; color: var(--ink-secondary); font-size: 1.3rem; cursor: pointer; }
  .close:hover { background: var(--surface-soft); color: var(--ink); }
  .intro { margin: 10px 0 20px; color: var(--ink-secondary); font-size: 0.82rem; line-height: 1.6; }
  .group { padding: 18px 0; border-top: 1px solid var(--hairline); }
  h3 { margin: 0 0 8px; color: var(--ink-secondary); font-size: 0.78rem; font-weight: 550; letter-spacing: 0.04em; }
  h3 span { color: var(--ink-faint); font-variant-numeric: tabular-nums; }
  ul { list-style: none; margin: 0; padding: 0; }
  li { padding: 13px 0; border-top: 1px solid var(--hairline); }
  button:focus-visible, a:focus-visible { outline: 2px solid var(--accent); outline-offset: 2px; }
  .item-heading { display: flex; align-items: flex-start; justify-content: space-between; gap: 8px; }
  .item-heading strong { font-size: 0.86rem; font-weight: 500; line-height: 1.5; overflow-wrap: anywhere; }
  .item-heading span { flex: none; color: var(--ink-faint); font-size: 0.68rem; overflow-wrap: anywhere; }
  .note-link { display: inline-block; margin-top: 5px; color: var(--accent); font-size: 0.74rem; text-decoration: none; overflow-wrap: anywhere; }
  pre { margin: 8px 0; padding: 8px 0; color: var(--ink-secondary); font: 0.72rem/1.55 ui-monospace, SFMono-Regular, Menlo, monospace; white-space: pre-wrap; overflow-wrap: anywhere; }
  .row-actions { display: flex; flex-wrap: wrap; gap: 6px; }
  button { min-height: 32px; padding: 6px 9px; border: 1px solid var(--hairline-strong); border-radius: var(--radius-md); background: transparent; color: var(--ink-secondary); font: inherit; font-size: 0.76rem; cursor: pointer; }
  button:hover:not(:disabled) { background: var(--surface-soft); color: var(--ink); }
  button.danger { border-color: var(--danger-line); color: var(--danger-ink); }
  button.danger:hover:not(:disabled) { background: var(--danger-tint); }
  button:disabled { opacity: 0.5; cursor: default; }
  .empty { padding: 28px 0; border-top: 1px solid var(--hairline); }
  .empty p { margin: 0; color: var(--ink-faint); font-size: 0.82rem; line-height: 1.55; }
  .feedback { position: sticky; bottom: 0; display: flex; flex-wrap: wrap; gap: 7px; padding: 11px 0; border-top: 1px solid var(--hairline); background: var(--surface); }
  .feedback p { flex: 1 1 100%; min-height: 1.3em; margin: 0; color: var(--success); font-size: 0.76rem; line-height: 1.45; }
  .feedback p.error { color: var(--danger-ink); }
  @media (pointer: coarse) { button, li { min-height: 44px; } .close { width: 44px; } }
  @media (prefers-reduced-motion: reduce) { *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; } }
</style>
