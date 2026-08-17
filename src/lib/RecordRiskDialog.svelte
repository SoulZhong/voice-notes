<script lang="ts">
  // 开录前风险确认。挂在 layout 里而不是各页各挂一份:两个开录入口(录制页、侧边栏)
  // 共用同一个门,对话框也该只有一处。
  //
  // 为什么是确认而不是横幅:2026-08-17 那场会议,语音突显的告警横幅**正在显示**,
  // 用户照录了,丢了约 27% 的语音。被动横幅这个形式已被实证无效。
  // 也不是硬门禁——会议已经开始时,明知有损失也要马上录是合理的。
  import { t } from "$lib/i18n/index.svelte";
  import { recordRiskGate } from "$lib/recordRisk.svelte";

  let dialog = $state<HTMLDialogElement | null>(null);
  const open = $derived(recordRiskGate.risks.length > 0);

  $effect(() => {
    if (!dialog) return;
    if (open && !dialog.open) dialog.showModal();
    else if (!open && dialog.open) dialog.close();
  });
</script>

<dialog
  bind:this={dialog}
  class="confirm-dialog"
  aria-labelledby="record-risk-title"
  aria-describedby="record-risk-body"
  oncancel={(e) => {
    // Esc 等同「去改设置」:不开录是更安全的默认。
    e.preventDefault();
    recordRiskGate.cancel();
  }}
>
  <h2 id="record-risk-title">{t("record.risk.title")}</h2>
  <p id="record-risk-body">{t("record.risk.body")}</p>
  <ul>
    {#each recordRiskGate.risks as risk (risk.kind)}
      <li>
        <strong>{t(`record.risk.${risk.kind}.title`)}</strong>
        <span>{t(`record.risk.${risk.kind}.impact`)}</span>
        <em>{t(`record.risk.${risk.kind}.how`)}</em>
        {#if risk.detail}<small>{risk.detail}</small>{/if}
      </li>
    {/each}
  </ul>
  <div>
    <button type="button" onclick={() => recordRiskGate.proceed()}>
      {t("record.risk.proceed")}
    </button>
    <button class="primary" type="button" onclick={() => recordRiskGate.cancel()}>
      {t("record.risk.cancel")}
    </button>
  </div>
</dialog>

<style>
  /* 样式自带一份而不是复用 RelationDrawer 的:那份是组件作用域的,且用 danger-line
     描边(压制关系是破坏性操作)。这里不是破坏性操作,走中性描边。 */
  .confirm-dialog {
    width: min(30rem, calc(100vw - 2rem));
    padding: 20px;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-xl);
    background: var(--surface);
    color: var(--ink);
    box-shadow: var(--shadow-popover);
  }
  .confirm-dialog::backdrop { background: light-dark(rgba(20, 21, 22, 0.28), rgba(0, 0, 0, 0.64)); }
  .confirm-dialog h2 { margin: 0; font-size: 1.05rem; font-weight: 550; }
  .confirm-dialog > p { margin: 6px 0 0; color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  .confirm-dialog > div { display: flex; justify-content: flex-end; gap: 8px; margin-top: 18px; }
  ul { display: grid; gap: 14px; margin: 16px 0 0; padding: 0; list-style: none; }
  li { display: grid; gap: 4px; padding-left: 12px; border-left: 2px solid var(--hairline-strong); }
  li strong { color: var(--ink); font-size: 0.92rem; font-weight: 550; }
  li span { color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  li em { color: var(--accent); font-size: 0.82rem; font-style: normal; }
  li small { color: var(--ink-faint); font-size: 0.74rem; }
  button { padding: 7px 14px; border: 1px solid var(--hairline); border-radius: var(--radius-full); background: var(--surface); color: var(--ink); font-size: 0.86rem; cursor: pointer; }
  button:hover { background: var(--surface-soft); }
  /* 「去改设置」是主按钮:默认动作应当是别带着已知损失开录。 */
  .primary { border-color: var(--primary); background: var(--primary); color: var(--on-primary); box-shadow: var(--shadow-btn); }
  .primary:hover { background: var(--primary-pressed); }
  @media (pointer: coarse) { button { min-height: 44px; } }
</style>
