<script lang="ts">
  import { relationDetail, type RelationDetail } from "./knowledge";
  import { relationLabel } from "./knowledgeView";
  import {
    buildConfirmRelation,
    buildEditRelation,
    buildEndRelation,
    buildRestoreRelation,
    buildSuppressRelation,
    createGovernanceController,
    task10GovernanceApi,
  } from "./knowledgeGovernance";

  let {
    relationId,
    onClose,
    onChanged,
  }: {
    relationId: string;
    onClose: () => void;
    onChanged: () => Promise<void>;
  } = $props();

  let detail = $state<RelationDetail | null>(null);
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
  let restoreOperationId = $state("");
  let suppressDialog = $state<HTMLDialogElement>();
  let generation = 0;

  async function refreshAndReload() {
    await onChanged();
    await load(relationId);
  }
  const controller = createGovernanceController(task10GovernanceApi, refreshAndReload);

  async function load(id: string) {
    const current = ++generation;
    loading = true;
    loadError = "";
    try {
      const value = await relationDetail(id);
      if (current !== generation) return;
      detail = value;
      if (value) {
        subjectId = value.relation.subject_id;
        objectId = value.relation.object_id;
        predicateType = value.relation.predicate_type;
        predicateLabel = value.relation.predicate_label ?? "";
        validFrom = value.relation.valid_from ?? "";
        validTo = value.relation.valid_to ?? "";
      }
    } catch (cause) {
      if (current !== generation) return;
      detail = null;
      loadError = `关系详情读取失败：${cause instanceof Error ? cause.message : String(cause)}`;
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
      status = "自定义关系需要填写完整关系名称。";
      return;
    }
    await runOperation(
      "正在保存关系修改",
      "关系方向、类型、时间与说明已更新",
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
      "正在永久抑制这条模型关系",
      "关系已否决并持久抑制，模型重跑不会自动恢复",
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
    status = "正在撤销上次操作";
    try {
      await controller.undo(controller.lastOperationId);
      status = controller.refreshError || "已写入补偿操作并刷新关系";
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
      <p class="eyebrow">关系证据</p>
      <h2 id="relation-drawer-title">关系详情</h2>
    </div>
    <button class="close" type="button" aria-label="关闭关系详情" onclick={onClose}>×</button>
  </header>

  {#if loading}
    <p class="state" role="status">正在读取完整关系与证据</p>
  {:else if loadError}
    <p class="state error" role="alert">{loadError}</p>
  {:else if !detail}
    <p class="state">这条关系已不在当前索引中；若刚完成治理操作，可关闭后从图谱重新选择。</p>
  {:else}
    {@const relation = detail.relation}
    <section class="direction" aria-label="完整关系方向">
      <span>{relation.subject_id}</span>
      <strong>→ {relationLabel(relation)} →</strong>
      <span>{relation.object_id}</span>
    </section>

    <section class="section" aria-labelledby="relation-overview">
      <h3 id="relation-overview">概览</h3>
      <dl>
        <div><dt>状态</dt><dd>{relation.status === "current" ? "当前" : "历史"}</dd></div>
        <div><dt>置信度</dt><dd>{Math.round(relation.confidence * 100)}%</dd></div>
        <div><dt>来源</dt><dd>{relation.origin}</dd></div>
        <div><dt>证据</dt><dd>{relation.evidence_count} 条</dd></div>
        <div><dt>提供方</dt><dd>{detail.provider || "人工治理"}</dd></div>
        <div><dt>模型</dt><dd>{detail.model || "未记录模型"}</dd></div>
        <div><dt>有效起点</dt><dd>{relation.valid_from || "未限定"}</dd></div>
        <div><dt>有效终点</dt><dd>{relation.valid_to || "持续有效"}</dd></div>
      </dl>
    </section>

    <section class="section versions" aria-labelledby="relation-versions">
      <h3 id="relation-versions">当前与历史版本</h3>
      <h4>当前版本</h4>
      {#if relation.status === "current"}
        <p>{relation.subject_id} → {relationLabel(relation)} → {relation.object_id}</p>
      {:else}<p class="muted">当前没有生效版本</p>{/if}
      <h4>历史版本</h4>
      {#if relation.status === "historical"}
        <p>{relation.subject_id} → {relationLabel(relation)} → {relation.object_id}</p>
      {:else}<p class="muted">当前索引没有这条关系的历史版本</p>{/if}
    </section>

    <section class="section evidence" aria-labelledby="relation-evidence">
      <h3 id="relation-evidence">全部证据</h3>
      {#each detail.evidence as item (item.id)}
        <blockquote>
          <p>{item.quote}</p>
          <footer>
            <a href={'/notes/' + encodeURIComponent(item.note_id) + '#paragraph-' + item.paragraph_index}>打开笔记 {item.note_id}</a>
            <span>第 {item.paragraph_index + 1} 段 · 字符 {item.start_offset}–{item.end_offset}</span>
            {#if item.source_seqs.length > 0}<span>时间片段序号 {item.source_seqs.join("、")}</span>{/if}
          </footer>
        </blockquote>
      {/each}
      {#if detail.evidence.length === 0}
        <p class="assertion">用户直接声明</p>
      {/if}
    </section>

    <section class="section actions" aria-labelledby="relation-actions">
      <h3 id="relation-actions">治理操作</h3>
      <div class="primary-actions">
        <button type="button" disabled={working} onclick={() => runOperation("正在确认关系", "关系已确认", buildConfirmRelation(relation.id))}>确认关系</button>
        <button class="danger" type="button" disabled={working} onclick={() => suppressDialog?.showModal()}>否决并抑制</button>
      </div>

      <details>
        <summary>编辑方向、类型、时间与说明</summary>
        <form onsubmit={(event) => { event.preventDefault(); void saveEdit(); }}>
          <label for="relation-subject">主体实体 ID</label>
          <input id="relation-subject" bind:value={subjectId} disabled={working} aria-describedby="relation-feedback" />
          <button class="swap" type="button" disabled={working} onclick={swapDirection}>交换关系方向</button>
          <label for="relation-object">客体实体 ID</label>
          <input id="relation-object" bind:value={objectId} disabled={working} aria-describedby="relation-feedback" />
          <label for="relation-predicate">关系类型</label>
          <select id="relation-predicate" bind:value={predicateType} disabled={working}>
            <option value="participates_in">参与</option><option value="responsible_for">负责</option>
            <option value="belongs_to">属于</option><option value="uses">使用</option>
            <option value="depends_on">依赖</option><option value="produces">产生</option>
            <option value="assigned_to">指派给</option><option value="occurs_at">发生于</option>
            <option value="custom">自定义关系</option>
          </select>
          {#if predicateType === "custom"}
            <label for="relation-custom-label">完整关系名称</label>
            <input id="relation-custom-label" bind:value={predicateLabel} disabled={working} aria-describedby="relation-feedback" />
          {/if}
          <label for="relation-valid-from">有效起点（RFC 3339，可留空）</label>
          <input id="relation-valid-from" bind:value={validFrom} disabled={working} aria-describedby="relation-feedback" />
          <label for="relation-valid-to">有效终点（RFC 3339，可留空）</label>
          <input id="relation-valid-to" bind:value={validTo} disabled={working} aria-describedby="relation-feedback" />
          <label for="relation-note">关系说明</label>
          <textarea id="relation-note" bind:value={relationNote} disabled={working}></textarea>
          <button type="submit" disabled={working || !subjectId.trim() || !objectId.trim() || !predicateType.trim() || (predicateType === "custom" && !predicateLabel.trim())}>保存关系修改</button>
        </form>
      </details>

      <details>
        <summary>结束或恢复关系</summary>
        <form onsubmit={(event) => {
          event.preventDefault();
          if (endAt.trim()) void runOperation("正在结束关系", "关系已转入历史", buildEndRelation(relation.id, endAt.trim()));
        }}>
          <label for="relation-end">关系结束时间（RFC 3339）</label>
          <input id="relation-end" bind:value={endAt} disabled={working} aria-describedby="relation-feedback" />
          <button type="submit" disabled={working || !endAt.trim()}>结束关系</button>
        </form>
        <form onsubmit={(event) => {
          event.preventDefault();
          if (restoreOperationId.trim()) void runOperation("正在恢复关系", "关系裁决已恢复", buildRestoreRelation(restoreOperationId.trim()));
        }}>
          <label for="restore-operation">要恢复的抑制或结束操作 ID</label>
          <input id="restore-operation" bind:value={restoreOperationId} disabled={working} aria-describedby="relation-feedback" />
          <button type="submit" disabled={working || !restoreOperationId.trim()}>恢复关系</button>
        </form>
      </details>

      <div class="undo-row">
        <button type="button" disabled={working || !controller.lastOperationId} onclick={undoLast}>撤销上次操作</button>
      </div>
    </section>

    <div class="feedback-row">
      <p id="relation-feedback" class:error={Boolean(controller.error)} aria-live="polite">{status}</p>
      {#if controller.refreshError}<button type="button" disabled={working} onclick={() => controller.retryRefresh().then(() => { status = "图谱已刷新"; }).catch(() => { status = controller.refreshError; })}>重试刷新图谱</button>{/if}
    </div>
  {/if}
</article>

<dialog bind:this={suppressDialog} class="confirm-dialog" aria-labelledby="suppress-title" aria-describedby="suppress-description">
  <h2 id="suppress-title">否决并永久抑制这条关系？</h2>
  <p id="suppress-description">该主体、关系类型与客体组合会写入持久裁决；模型更换证据或重新抽取也不会自动恢复。</p>
  <div>
    <button type="button" onclick={() => suppressDialog?.close()}>保留这条关系</button>
    <button class="danger" type="button" onclick={suppressRelation}>否决并抑制</button>
  </div>
</dialog>

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
  .direction span { font-size: 0.9rem; }
  .direction strong { color: var(--accent); font-size: 0.82rem; font-weight: 500; }
  .section { padding: 18px 0; border-top: 1px solid var(--hairline); }
  h3 { margin: 0 0 12px; color: var(--ink-secondary); font-size: 0.78rem; font-weight: 550; letter-spacing: 0.06em; }
  h4 { margin: 12px 0 5px; color: var(--ink-faint); font-size: 0.72rem; font-weight: 500; }
  dl { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; margin: 0; }
  dl div { min-width: 0; }
  dt { color: var(--ink-faint); font-size: 0.7rem; }
  dd { margin: 3px 0 0; color: var(--ink); font-size: 0.82rem; overflow-wrap: anywhere; font-variant-numeric: tabular-nums; }
  .versions p { margin: 0; color: var(--ink); font-size: 0.82rem; line-height: 1.55; overflow-wrap: anywhere; }
  .muted { color: var(--ink-faint) !important; }
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
  .undo-row { padding-top: 12px; border-top: 1px solid var(--hairline); }
  .feedback-row { position: sticky; bottom: 0; padding: 11px 0; border-top: 1px solid var(--hairline); background: var(--surface); }
  .feedback-row p { min-height: 1.3em; margin: 0 0 6px; color: var(--success); font-size: 0.78rem; line-height: 1.45; }
  .feedback-row p.error { color: var(--danger-ink); }
  .confirm-dialog { width: min(420px, calc(100vw - 32px)); padding: 20px; border: 1px solid var(--danger-line); border-radius: var(--radius-xl); background: var(--surface); color: var(--ink); box-shadow: var(--shadow-popover); }
  .confirm-dialog::backdrop { background: light-dark(rgba(20, 21, 22, 0.28), rgba(0, 0, 0, 0.64)); }
  .confirm-dialog h2 { font-size: 1.05rem; }
  .confirm-dialog p { color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  .confirm-dialog div { display: flex; justify-content: flex-end; gap: 8px; }
  @media (pointer: coarse) { button, summary { min-height: 44px; } .close { width: 44px; } }
  @media (prefers-reduced-motion: reduce) { *, *::before, *::after { transition-duration: 0.01ms !important; animation-duration: 0.01ms !important; } }
</style>
