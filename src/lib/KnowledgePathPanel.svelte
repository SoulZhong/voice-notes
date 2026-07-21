<script lang="ts">
  import type { KnowledgePath, KnowledgePathStep } from "$lib/knowledge";
  import { relationLabel } from "$lib/knowledgeView";

  let {
    startId,
    endId,
    names,
    path,
    status,
    error = "",
    includeCooccurrence = false,
    onToggleCooccurrence,
    onOpenEvidence,
    onClear,
  }: {
    startId: string | null;
    endId: string | null;
    names: ReadonlyMap<string, string>;
    path: KnowledgePath | null;
    status: "idle" | "choosing" | "loading" | "ready" | "empty" | "error";
    error?: string;
    includeCooccurrence?: boolean;
    onToggleCooccurrence: (value: boolean) => void;
    onOpenEvidence: (relationId: string) => void;
    onClear: () => void;
  } = $props();

  const nameFor = (id: string | null) => (id ? names.get(id) ?? id : "尚未选择");
  const isWeak = (step: KnowledgePathStep) => step.origin === "cooccurrence";
  const labelFor = (step: KnowledgePathStep) =>
    isWeak(step)
      ? `共同出现（${step.note_count} 篇）`
      : relationLabel(step);
  const originLabel = (origin: KnowledgePathStep["origin"]) =>
    ({
      model: "模型提取",
      confirmed: "人工确认",
      manual: "人工建立",
      user_assertion: "用户声明",
      cooccurrence: "共现弱连接",
    })[origin];
</script>

{#if startId}
  <section class="path-panel" aria-label="两点关系路径">
    <header>
      <div class="path-points">
        <span class="point-mark">起</span>
        <strong>{nameFor(startId)}</strong>
        <span class="path-arrow" aria-hidden="true">→</span>
        <span class="point-mark endpoint">终</span>
        <strong>{nameFor(endId)}</strong>
      </div>
      <button type="button" class="clear" onclick={onClear}>清除路径</button>
    </header>

    <label class="weak-toggle">
      <input
        type="checkbox"
        checked={includeCooccurrence}
        onchange={(event) => onToggleCooccurrence(event.currentTarget.checked)}
      />
      <span>包含共现弱连接</span>
    </label>

    {#if status === "choosing"}
      <p class="state">再选择一个实体作为终点，画布不会隐藏其他关系。</p>
    {:else if status === "loading"}
      <p class="state" aria-live="polite">正在沿当前筛选寻找可解释路径</p>
    {:else if status === "error"}
      <p class="state error" role="alert">{error || "路径读取失败。请检查筛选条件后重试。"}</p>
    {:else if status === "empty"}
      <p class="state">未找到可连接两点的路径。可以放宽关系类型或启用共现弱连接。</p>
    {:else if status === "ready" && path}
      <ol class="steps">
        {#each path.steps as step, index (step.id + ":" + index)}
          <li class:weak={isWeak(step)}>
            <div class="step-line" aria-hidden="true"></div>
            <div class="step-copy">
              <p class="step-route">
                <span>{nameFor(step.from_id)}</span>
                <span class="direction">{step.direction === "forward" ? "正向" : "逆向"}</span>
                <span>{nameFor(step.to_id)}</span>
              </p>
              <p class="relation-name">{labelFor(step)}</p>
              <p class="provenance">
                {originLabel(step.origin)} · 置信度 {Math.round(step.confidence * 100)}% · {step.evidence_count} 条证据
              </p>
              {#if !isWeak(step)}
                <button type="button" class="evidence" onclick={() => onOpenEvidence(step.id)}>
                  查看关系证据
                </button>
              {/if}
            </div>
          </li>
        {/each}
      </ol>
    {/if}
  </section>
{/if}

<style>
  .path-panel {
    position: absolute;
    right: 14px;
    top: 58px;
    z-index: 8;
    width: min(370px, calc(100% - 28px));
    max-height: calc(100% - 76px);
    box-sizing: border-box;
    padding: 13px 14px 14px;
    overflow-y: auto;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-lg);
    background: var(--surface-press);
    color: var(--ink);
  }
  header, .path-points, .weak-toggle, .step-route, .provenance {
    display: flex;
    align-items: center;
  }
  header { justify-content: space-between; gap: 12px; }
  .path-points { min-width: 0; gap: 7px; font-size: 0.78rem; }
  .path-points strong { min-width: 0; overflow-wrap: anywhere; font-weight: 620; }
  .point-mark {
    display: grid;
    place-items: center;
    flex: 0 0 20px;
    height: 20px;
    border-radius: var(--radius-full);
    background: var(--accent-tint);
    color: var(--accent);
    font-size: 0.65rem;
  }
  .point-mark.endpoint { background: var(--surface-soft); color: var(--ink-secondary); }
  .path-arrow { color: var(--ink-faint); }
  button {
    min-height: 32px;
    border: 0;
    border-radius: var(--radius-md);
    background: transparent;
    color: var(--ink-secondary);
    font: inherit;
    font-size: 0.73rem;
    cursor: pointer;
  }
  button:hover { background: var(--surface-soft); color: var(--ink); }
  .clear { flex: none; padding: 4px 7px; }
  .weak-toggle {
    gap: 7px;
    min-height: 38px;
    margin-top: 8px;
    border-top: 1px solid var(--hairline);
    color: var(--ink-secondary);
    font-size: 0.74rem;
  }
  input { accent-color: var(--accent); }
  .state { margin: 10px 0 2px; color: var(--ink-secondary); font-size: 0.78rem; line-height: 1.55; }
  .state.error { color: var(--danger); }
  .steps { margin: 10px 0 0; padding: 0; list-style: none; }
  .steps li { position: relative; display: grid; grid-template-columns: 18px 1fr; gap: 8px; padding: 0 0 14px; }
  .step-line { width: 0; height: 100%; margin: 3px auto 0; border-left: 1px solid var(--accent); }
  .steps li.weak .step-line { border-left-style: dashed; border-left-color: var(--hairline-strong); }
  .step-copy { min-width: 0; }
  .step-route { flex-wrap: wrap; gap: 5px; margin: 0; color: var(--ink-secondary); font-size: 0.73rem; }
  .direction { color: var(--ink-faint); }
  .relation-name { margin: 4px 0 0; color: var(--ink); font-size: 0.86rem; line-height: 1.45; overflow-wrap: anywhere; }
  .provenance { flex-wrap: wrap; gap: 3px; margin: 3px 0 0; color: var(--ink-faint); font-size: 0.68rem; line-height: 1.45; }
  .evidence { margin-top: 4px; padding: 3px 0; color: var(--accent); }
  button:focus-visible, label:has(input:focus-visible) { outline: 2px solid var(--accent); outline-offset: 2px; }
  @media (pointer: coarse) {
    button, .weak-toggle { min-height: 44px; }
  }
  @media (max-width: 700px) {
    .path-panel { top: auto; bottom: 12px; max-height: 52%; }
  }
</style>
