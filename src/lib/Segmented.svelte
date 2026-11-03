<script lang="ts">
  import { untrack } from "svelte";
  import * as ToggleGroup from "$lib/ui/components/toggle-group";
  import { toggleVariants } from "$lib/ui/components/toggle";
  import { cn } from "$lib/ui/utils";
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

  let trackEl: HTMLElement | null = $state(null);
  let btns: (HTMLElement | null | undefined)[] = $state([]);
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

  // 选中态完全受控于 value 属性:ToggleGroup 的写入只转成 onSelect,不在组件内存一份
  // (父组件可能拒绝/延后切换,如声纹模型要先确认)。单选组再点已选项会写入 "",忽略。
  function pick(v: string) {
    if (!v || v === value) return;
    const it = items.find((x) => x.id === v);
    if (it && !it.momentary && !it.disabled) onSelect(v);
  }

  function handleKey(e: KeyboardEvent) {
    if (e.key !== "ArrowLeft" && e.key !== "ArrowRight") return;
    const next = nextEnabledIndex(items, activeIdx, e.key === "ArrowRight" ? 1 : -1);
    if (next === activeIdx || next < 0) return;
    e.preventDefault();
    onSelect(items[next]!.id);
    btns[next]?.focus();
  }

  const itemClass = $derived(
    cn(
      "relative z-[1] rounded-[calc(var(--radius-md)_-_2px)] font-medium tracking-[0.2px] text-muted-foreground",
      "hover:bg-transparent hover:text-foreground data-[state=on]:bg-transparent data-[state=on]:text-foreground",
      "disabled:pointer-events-auto disabled:cursor-default disabled:opacity-45 disabled:hover:text-muted-foreground",
      size === "sm" ? "h-6 min-w-0 px-2 text-[0.8rem]" : "h-7 min-w-0 px-2.5 text-[0.85rem]",
    ),
  );
</script>

<ToggleGroup.Root
  type="single"
  bind:ref={trackEl}
  bind:value={() => value, pick}
  rovingFocus={false}
  spacing={0.5}
  class="relative flex-none rounded-md border border-border bg-muted p-0.5"
>
  <span
    class="thumb"
    class:ready={ready}
    class:gone={thumb.width === 0}
    style={`transform: translateX(${thumb.left}px); width: ${thumb.width}px;`}
    aria-hidden="true"
  ></span>
  {#each items as it, i (it.id)}
    {#if it.momentary}
      <!-- 动作段:不参与单选,点了触发 onAction,滑块不落位 -->
      <button
        bind:this={btns[i]}
        type="button"
        class={cn(toggleVariants({ size: size === "sm" ? "sm" : "default" }), itemClass)}
        disabled={it.disabled}
        title={it.title}
        onclick={() => onAction?.(it.id)}
        onkeydown={handleKey}
      >{it.label}</button>
    {:else}
      <ToggleGroup.Item
        value={it.id}
        disabled={it.disabled}
        title={it.title}
        size={size === "sm" ? "sm" : "default"}
        class={itemClass}
        onkeydown={handleKey}
      >
        {#snippet child({ props })}
          <!-- 漫游 tabindex 自己管:Tab 只停在选中项,方向键切换(原 tablist 语义) -->
          <button {...props} bind:this={btns[i]} tabindex={it.id === value ? 0 : -1}>{it.label}</button>
        {/snippet}
      </ToggleGroup.Item>
    {/if}
  {/each}
</ToggleGroup.Root>

<style>
  /* 凹槽式 segmented(DESIGN.md 命令面板语言):muted 凹槽轨道 + 发丝线边(ToggleGroup 根),
     选中项是抬升滑块(surface-press + hairline-strong + shadow-btn),120ms 滑动。 */
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
  @media (prefers-reduced-motion: reduce) {
    .thumb.ready {
      transition: none;
    }
  }
</style>
