<script lang="ts">
  import { t } from "$lib/i18n/index.svelte";

  /** 按钮上显示的动作名。默认 "AI" 保持组件可复用;笔记页传「AI 整理」——
      按钮该说它**做什么**,光写 "AI" 和「重新分析」是同一个词不达意的毛病。
      running 态例外,仍是会跳的 "Aing"(本仓的招牌动效,用户点名保留)。 */
  let { state, label = "AI" }: {
    state: "idle" | "running" | "complete" | "failed";
    label?: string;
  } = $props();
</script>

<span class="ai-state" class:running={state === "running"} aria-label={state === "idle" ? t("record.ai.idle") : state === "running" ? t("record.ai.running") : state === "complete" ? t("record.ai.complete") : t("record.ai.failed")}>
  {#if state === "running"}
    {#each ["A", "i", "n", "g"] as letter, i}
      <span class="letter" style={`--i:${i}`}>{letter}</span>
    {/each}
  {:else if state === "complete"}
    <span>{label}</span><span class="dot done" aria-hidden="true"></span>
  {:else if state === "failed"}
    <span>{label}</span><span class="dot failed" aria-hidden="true"></span>
  {:else}
    <span>{label}</span>
  {/if}
</span>

<style>
  /* 字重 500:DESIGN.md「全组件字重 600/700 处降至 500,层级靠 ink 亮度保持」。
     完成/失败不再用 ✓ / × 字形——它们与相邻文字基线对不齐、粗细也跳,并排看像贴纸;
     改成同色小圆点,状态照样一眼看得出,而按钮仍是一整块。 */
  .ai-state { display: inline-flex; align-items: center; gap: 0.3em; min-width: 2.1em; font-weight: 500; }
  .running .letter {
    display: inline-block;
    animation: letter-hop 1.1s cubic-bezier(0.45, 0, 0.55, 1) infinite;
    animation-delay: calc(var(--i) * 90ms);
  }
  .dot { width: 5px; height: 5px; border-radius: 50%; flex: none; }
  .done { background: var(--success); }
  .failed { background: var(--danger); }
  @keyframes letter-hop {
    0%, 52%, 100% { transform: translateY(0); }
    18% { transform: translateY(-0.28em); }
    34% { transform: translateY(0.04em); }
  }
  @media (prefers-reduced-motion: reduce) {
    .running .letter { animation: none; }
  }
</style>
