<script lang="ts">
  import { untrack } from "svelte";
  import { nextEnabledIndex, type SegmentedItem } from "./segmented";

  let {
    items,
    value,
    onSelect,
    onAction,
    size = "md",
  }: {
    items: SegmentedItem[];
    value: string;
    onSelect: (id: string) => void;
    onAction?: (id: string) => void;
    size?: "sm" | "md";
  } = $props();

  let trackEl: HTMLDivElement | undefined = $state();
  let btns: (HTMLButtonElement | undefined)[] = $state([]);
  // 滑块几何与过渡开关分开存,measure 只写不读它们:在 $effect 里读写同一 state
  // 会无限重跑,effect_update_depth_exceeded 冻死整页反应性(2026-08-09 实锤:
  // 首帧渲染正常,之后全应用点击无响应)。ready=false 时不播过渡:首帧直接落位。
  let thumb = $state({ left: 0, width: 0 });
  let ready = $state(false);

  const activeIdx = $derived(items.findIndex((it) => !it.momentary && it.id === value));

  function measure() {
    const btn = activeIdx >= 0 ? btns[activeIdx] : undefined;
    if (!btn || !trackEl) {
      thumb = { left: 0, width: 0 };
      return;
    }
    // offsetLeft 与绝对定位 left:0 同以轨道 padding 盒为参考系,无需再减边框
    thumb = { left: btn.offsetLeft, width: btn.offsetWidth };
    if (!ready) requestAnimationFrame(() => (ready = true));
  }

  // 选中变化/文案变化(i18n 切换、「生成中(阶段)」计数)→ 重测;ResizeObserver 兜底
  // 容器与字体变化。measure 用 untrack 包住:它写 thumb/读 ready,任其入依赖都会
  // 形成写后重跑环;依赖显式收敛为 activeIdx 与 label 串。
  $effect(() => {
    void activeIdx;
    void items.map((it) => it.label).join("\0");
    untrack(measure);
  });
  $effect(() => {
    if (!trackEl) return;
    const ro = new ResizeObserver(measure);
    ro.observe(trackEl);
    return () => ro.disconnect();
  });

  function handleKey(e: KeyboardEvent) {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    const next = nextEnabledIndex(items, activeIdx, e.key === "ArrowRight" ? 1 : -1);
    if (next === activeIdx || next < 0) return;
    e.preventDefault();
    onSelect(items[next]!.id);
    btns[next]?.focus();
  }
</script>

<div class="seg" class:sm={size === "sm"} role="tablist" bind:this={trackEl}>
  <span
    class="thumb"
    class:ready={ready}
    class:gone={thumb.width === 0}
    style={`transform: translateX(${thumb.left}px); width: ${thumb.width}px;`}
    aria-hidden="true"
  ></span>
  {#each items as it, i (it.id)}
    <button
      bind:this={btns[i]}
      type="button"
      role={it.momentary ? undefined : "tab"}
      aria-selected={it.momentary ? undefined : it.id === value}
      class="seg-btn"
      class:active={!it.momentary && it.id === value}
      disabled={it.disabled}
      title={it.title}
      tabindex={it.momentary ? 0 : it.id === value ? 0 : -1}
      onclick={() => (it.momentary ? onAction?.(it.id) : onSelect(it.id))}
      onkeydown={handleKey}
    >{it.label}</button>
  {/each}
</div>

<style>
  /* 凹槽式 segmented(DESIGN.md 命令面板语言):surface-soft 凹槽轨道 + 发丝线边,
     选中项是抬升滑块(surface-press + hairline-strong + shadow-btn),120ms 滑动。 */
  .seg {
    position: relative;
    display: inline-flex;
    align-items: center;
    padding: 2px;
    background: var(--surface-soft);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-md);
  }
  .thumb {
    position: absolute;
    top: 2px;
    bottom: 2px;
    left: 0;
    background: var(--surface-press);
    border: 1px solid var(--hairline-strong);
    border-radius: calc(var(--radius-md) - 2px);
    box-shadow: var(--shadow-btn);
    box-sizing: border-box;
    pointer-events: none;
  }
  .thumb.ready {
    transition:
      transform 120ms ease,
      width 120ms ease;
  }
  .thumb.gone {
    opacity: 0;
  }
  .seg-btn {
    position: relative;
    border: none;
    background: none;
    box-shadow: none;
    padding: 0.28em 0.7em;
    font-size: 0.85rem;
    font-weight: 500;
    line-height: 1.3;
    letter-spacing: 0.2px;
    color: var(--ink-secondary);
    border-radius: calc(var(--radius-md) - 2px);
    cursor: pointer;
    transition: color 120ms ease;
  }
  .sm .seg-btn {
    font-size: 0.8rem;
    padding: 0.2em 0.55em;
  }
  .seg-btn:hover:not(:disabled),
  .seg-btn.active {
    color: var(--ink);
  }
  .seg-btn:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .seg-btn:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -1px;
  }
  @media (prefers-reduced-motion: reduce) {
    .thumb.ready {
      transition: none;
    }
  }
</style>
