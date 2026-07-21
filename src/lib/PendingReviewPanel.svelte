<script lang="ts">
  import { relationDetail, type PendingReviewItem, type RelationDetail } from "./knowledge";
  import {
    buildConfirmRelation,
    buildSuppressRelation,
    createGovernanceController,
    groupPending,
    pendingAfterLater,
    pendingReviewModel,
    task10GovernanceApi,
    type PendingReviewModel,
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
  let relationDetails = $state<Record<string, RelationDetail | null>>({});
  let relationLoadGeneration = 0;
  const controller = createGovernanceController(task10GovernanceApi, () => onChanged());
  const visibleItems = $derived(pendingAfterLater(items, hiddenIds));
  const groups = $derived(groupPending(visibleItems));

  async function loadRelationDetails(ids: string[]) {
    const generation = ++relationLoadGeneration;
    const entries = await Promise.all(ids.map(async (id) => {
      try {
        return [id, await relationDetail(id)] as const;
      } catch {
        return [id, null] as const;
      }
    }));
    if (generation === relationLoadGeneration) relationDetails = Object.fromEntries(entries);
  }

  $effect(() => {
    const relationIds = [...new Set(items.flatMap((item) => pendingReviewModel(item).relationIds))].sort();
    void loadRelationDetails(relationIds);
  });

  function itemTitle(item: PendingReviewItem, model: PendingReviewModel): string {
    if (model.message) return model.message;
    if (model.reason) return model.reason;
    if (model.evidenceId) return `证据 ${model.evidenceId}`;
    if (model.relationIds.length === 1) return `关系 ${model.relationIds[0]}`;
    return `待整理项 ${item.id}`;
  }

  async function confirmItem(item: PendingReviewItem, model: PendingReviewModel) {
    const relationId = model.relationIds[0];
    if (!model.canConfirm || !relationId || workingId) return;
    workingId = item.id;
    status = "正在确认关系";
    try {
      await controller.submit(buildConfirmRelation(relationId));
      status = controller.refreshError || "关系已确认并移出待整理";
    } catch {
      status = controller.error;
    } finally {
      workingId = null;
    }
  }

  async function suppressItem(item: PendingReviewItem, relation: RelationDetail) {
    if (workingId) return;
    const model = pendingReviewModel(item);
    const approved = window.confirm(`否决「${itemTitle(item, model)}」并永久抑制这条关系？模型重新抽取也不会自动恢复。`);
    if (!approved) return;
    workingId = item.id;
    status = "正在写入持久抑制裁决";
    try {
      await controller.submit(buildSuppressRelation(
        relation.relation.subject_id,
        { type: relation.relation.predicate_type, label: relation.relation.predicate_label },
        relation.relation.object_id,
      ));
      status = controller.refreshError || "候选关系已否决并持久抑制";
    } catch {
      status = controller.error || "候选关系读取失败，请稍后重试。";
    } finally {
      workingId = null;
    }
  }

  function later(item: PendingReviewItem) {
    hiddenIds = new Set([...hiddenIds, item.id]);
    status = `已在本次会话稍后处理「${itemTitle(item, pendingReviewModel(item))}」；没有写入后端。`;
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
          {@const model = pendingReviewModel(item)}
          <li>
            <div class="item-heading">
              <strong>{itemTitle(item, model)}</strong>
              <span>{item.kind}</span>
            </div>
            {#if model.noteId}<a class="note-link" href={'/notes/' + encodeURIComponent(model.noteId)}>打开原笔记 {model.noteId}</a>{/if}

            {#if model.kind === "identity_conflict"}
              <p class="detail">本地实体 {model.localEntityId || "未提供"}。{model.reason || "后端未提供冲突原因。"}</p>
              <div class="entity-links" aria-label="候选实体">
                {#each model.candidateEntityIds as candidateId (candidateId)}
                  <a href={'/graph?e=' + encodeURIComponent(candidateId)}>查看候选实体 {candidateId}</a>
                {/each}
              </div>
              <p class="unavailable">当前后端尚未提供按该候选集合绑定身份的命令；请从原笔记或候选实体继续核对。</p>
            {:else if model.kind === "invalid_document"}
              <p class="detail">{model.message || "后端未提供文档错误详情。"}</p>
              <p class="unavailable">请先修复原笔记的结构或 frontmatter；当前没有可安全执行的关系治理命令。</p>
            {:else if model.kind === "stale_evidence" || model.kind === "split_conflict"}
              <p class="detail">需修复的证据 ID：{model.evidenceId || "未提供"}</p>
              <p class="unavailable">请回到原笔记修复证据锚点或拆分归属；当前后端未提供直接修复命令。</p>
            {:else if model.kind === "time_conflict"}
              <p class="detail">以下关系的时间范围相互冲突：</p>
            {:else if model.kind === "relation_review"}
              <p class="detail">这是后端明确标记的待确认关系。</p>
            {:else}
              <pre>{JSON.stringify(item.payload, null, 2)}</pre>
              <p class="unavailable">未识别的待整理类型；当前不提供会发送不完整 payload 的通用动作。</p>
            {/if}

            {#if model.kind === "time_conflict"}
              <div class="relation-links" aria-label="时间冲突关系">
                {#each model.relationIds as relationId (relationId)}
                  <button type="button" disabled={workingId !== null} onclick={() => onOpenRelation(relationId)}>查看关系 {relationId}</button>
                {/each}
              </div>
            {:else if model.relationIds[0]}
              {@const relationId = model.relationIds[0]}
              {@const loadedRelation = relationDetails[relationId]}
              <div class="relation-links">
                <button type="button" disabled={workingId !== null} onclick={() => onOpenRelation(relationId)}>查看关系</button>
                {#if loadedRelation}
                  <button type="button" disabled={workingId !== null} onclick={() => onOpenRelation(relationId)}>编辑关系</button>
                  <button class="danger" type="button" disabled={workingId !== null} onclick={() => suppressItem(item, loadedRelation)}>否决并抑制</button>
                {:else if loadedRelation === null}
                  <span class="unavailable">当前无法读取完整关系 triple，已隐藏编辑与抑制动作。</span>
                {/if}
              </div>
            {/if}

            <div class="row-actions" aria-label={`处理 ${itemTitle(item, model)}`}>
              {#if model.canConfirm}<button type="button" disabled={workingId !== null} onclick={() => confirmItem(item, model)}>确认关系</button>{/if}
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
  .detail, .unavailable { margin: 7px 0; color: var(--ink-secondary); font-size: 0.76rem; line-height: 1.55; overflow-wrap: anywhere; }
  .unavailable { color: var(--ink-faint); }
  .entity-links, .relation-links, .row-actions { display: flex; flex-wrap: wrap; gap: 6px; margin-top: 8px; }
  .entity-links a { padding: 6px 8px; border: 1px solid var(--hairline); border-radius: var(--radius-md); color: var(--accent); font-size: 0.74rem; text-decoration: none; overflow-wrap: anywhere; }
  .relation-links .unavailable { flex: 1 1 100%; margin: 0; }
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
