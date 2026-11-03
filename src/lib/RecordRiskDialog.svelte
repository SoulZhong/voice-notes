<script lang="ts">
  // 开录前风险确认。挂在 layout 里而不是各页各挂一份:两个开录入口(录制页、侧边栏)
  // 共用同一个门,对话框也该只有一处。
  //
  // 为什么是确认而不是横幅:2026-08-17 那场会议,语音突显的告警横幅**正在显示**,
  // 用户照录了,丢了约 27% 的语音。被动横幅这个形式已被实证无效。
  // 也不是硬门禁——会议已经开始时,明知有损失也要马上录是合理的。
  import { t } from "$lib/i18n/index.svelte";
  import { recordRiskGate } from "$lib/recordRisk.svelte";
  import * as AlertDialog from "$lib/ui/components/alert-dialog";
  import { Button } from "$lib/ui/components/button";

  const open = $derived(recordRiskGate.risks.length > 0);
</script>

<!-- UI 改版:AlertDialog 外壳(原生 <dialog> showModal 的等价物:模态、焦点圈定、点遮罩不关)。
     打开/关闭仍完全由 recordRiskGate.risks 派生;组件自己发起的关闭只有 Esc 一条,
     等同「去改设置」(不开录是更安全的默认)——经 bind 的 setter 落到 cancel()。
     初始焦点沿用组件默认的「第一个可聚焦元素」,与原生 showModal 一致。 -->
<AlertDialog.Root
  bind:open={
    () => open,
    (v) => {
      if (!v && open) recordRiskGate.cancel();
    }
  }
>
  <AlertDialog.Content
    class="gap-0 rounded-xl border border-border bg-card p-5 text-foreground shadow-(--shadow-popover) ring-0 data-[size=default]:max-w-[min(30rem,calc(100vw-2rem))] data-[size=default]:sm:max-w-[min(30rem,calc(100vw-2rem))]"
    aria-describedby="record-risk-body"
  >
    <AlertDialog.Title class="m-0 text-[1.05rem] font-[550]">{t("record.risk.title")}</AlertDialog.Title>
    <AlertDialog.Description id="record-risk-body" class="mt-1.5 text-[0.84rem] leading-relaxed text-muted-foreground">
      {t("record.risk.body")}
    </AlertDialog.Description>
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
    <div class="actions">
      <Button variant="outline" class="rounded-full px-3.5" onclick={() => recordRiskGate.proceed()}>
        {t("record.risk.proceed")}
      </Button>
      <!-- 「去改设置」是主按钮:默认动作应当是别带着已知损失开录。 -->
      <Button class="rounded-full px-3.5 shadow-(--shadow-btn) hover:bg-(color:--primary-pressed)" onclick={() => recordRiskGate.cancel()}>
        {t("record.risk.cancel")}
      </Button>
    </div>
  </AlertDialog.Content>
</AlertDialog.Root>

<style>
  /* 不是破坏性操作,走中性描边(样式不复用 RelationDrawer 那份 danger-line 版)。 */
  .actions { display: flex; justify-content: flex-end; gap: 8px; margin-top: 18px; }
  ul { display: grid; gap: 14px; margin: 16px 0 0; padding: 0; list-style: none; }
  li { display: grid; gap: 4px; padding-left: 12px; border-left: 2px solid var(--hairline-strong); }
  li strong { color: var(--ink); font-size: 0.92rem; font-weight: 550; }
  li span { color: var(--ink-secondary); font-size: 0.84rem; line-height: 1.6; }
  li em { color: var(--accent); font-size: 0.82rem; font-style: normal; }
  li small { color: var(--ink-faint); font-size: 0.74rem; }
  @media (pointer: coarse) { .actions :global(button) { min-height: 44px; } }
</style>
