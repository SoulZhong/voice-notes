<script lang="ts">
  import { t } from "$lib/i18n/index.svelte";

  let { state }: { state: "idle" | "running" | "complete" | "failed" } = $props();
</script>

<span class="ai-state" class:running={state === "running"} aria-label={state === "idle" ? t("record.ai.idle") : state === "running" ? t("record.ai.running") : state === "complete" ? t("record.ai.complete") : t("record.ai.failed")}>
  {#if state === "running"}
    {#each ["A", "i", "n", "g"] as letter, i}
      <span class="letter" style={`--i:${i}`}>{letter}</span>
    {/each}
  {:else if state === "complete"}
    <span>AI</span><span class="done" aria-hidden="true">✓</span>
  {:else if state === "failed"}
    <span>AI</span><span class="failed" aria-hidden="true">×</span>
  {:else}
    <span>AI</span>
  {/if}
</span>

<style>
  .ai-state { display: inline-flex; align-items: baseline; min-width: 2.1em; font-weight: 600; }
  .running .letter {
    display: inline-block;
    animation: letter-hop 1.1s cubic-bezier(0.45, 0, 0.55, 1) infinite;
    animation-delay: calc(var(--i) * 90ms);
  }
  .done { color: var(--success); margin-left: 0.28em; font-weight: 700; }
  .failed { color: var(--danger); margin-left: 0.28em; font-weight: 700; }
  @keyframes letter-hop {
    0%, 52%, 100% { transform: translateY(0); }
    18% { transform: translateY(-0.28em); }
    34% { transform: translateY(0.04em); }
  }
  @media (prefers-reduced-motion: reduce) {
    .running .letter { animation: none; }
  }
</style>
