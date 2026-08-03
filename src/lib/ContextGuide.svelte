<script lang="ts">
  import { page } from "$app/stores";
  import { getSettings, setSettings } from "$lib/models";
  import { PRODUCT_GUIDES, type ProductGuide, type ProductGuideStep } from "$lib/onboarding";
  import { t } from "$lib/i18n/index.svelte";

  let guide = $state<ProductGuide | null>(null);
  let steps = $state<ProductGuideStep[]>([]);
  let step = $state(0);
  let active = $state(false);
  let position = $state("left:12px;top:12px;width:360px");
  let placement = $state<"right" | "below">("right");
  let loadGeneration = 0;
  const current = $derived(steps[step]);

  function targetFor(s = current) {
    return s ? document.querySelector<HTMLElement>(s.selector) : null;
  }

  function positionBubble() {
    const el = targetFor();
    if (!el) return;
    const rect = el.getBoundingClientRect();
    const width = Math.min(370, window.innerWidth - 24);
    let left: number;
    let top: number;
    if (window.innerWidth - rect.right >= width + 24) {
      placement = "right";
      left = rect.right + 14;
      top = Math.max(12, Math.min(rect.top + 18, window.innerHeight - 250));
    } else {
      placement = "below";
      left = Math.max(12, Math.min(rect.left + 18, window.innerWidth - width - 12));
      top = Math.max(12, Math.min(rect.top + 62, window.innerHeight - 250));
    }
    position = `left:${left}px;top:${top}px;width:${width}px`;
  }

  function focusCurrent() {
    const el = targetFor();
    if (!el) return;
    document.querySelectorAll(".context-guide-target").forEach((n) => n.classList.remove("context-guide-target"));
    el.classList.add("context-guide-target");
    el.scrollIntoView({ behavior: "smooth", block: "center" });
    setTimeout(positionBubble, 260);
  }

  async function complete() {
    document.querySelectorAll(".context-guide-target").forEach((n) => n.classList.remove("context-guide-target"));
    active = false;
    if (!guide) return;
    try {
      const s = await getSettings();
      const completed = new Set(s.completed_guides);
      completed.add(guide.id);
      await setSettings({ ...s, completed_guides: [...completed] });
    } catch {
      /* 保存失败则下次进入重播 */
    }
  }

  function next() {
    if (step >= steps.length - 1) void complete();
    else {
      step += 1;
      setTimeout(focusCurrent);
    }
  }

  $effect(() => {
    const pathname = $page.url.pathname;
    const generation = ++loadGeneration;
    const matched = PRODUCT_GUIDES.find((g) => g.matches(pathname)) ?? null;
    document.querySelectorAll(".context-guide-target").forEach((n) => n.classList.remove("context-guide-target"));
    active = false;
    guide = matched;
    // 必须使用本轮的局部快照。若在 effect 内重新读取刚赋值的 $state guide，
    // guide 会成为依赖；对象赋值随后再次触发 effect，形成无限响应式循环，
    // 饿死笔记页的 invoke Promise，使界面永久停在「加载中」。
    if (!matched) return;
    setTimeout(async () => {
      if (generation !== loadGeneration) return;
      const available = matched.steps.filter((s) => document.querySelector(s.selector));
      if (!available.length) return;
      try {
        const settings = await getSettings();
        if (generation !== loadGeneration || settings.completed_guides.includes(matched.id)) return;
      } catch {
        return;
      }
      steps = available;
      step = 0;
      active = true;
      setTimeout(focusCurrent);
    }, 180);
  });

  $effect(() => {
    if (!active) return;
    const reposition = () => positionBubble();
    window.addEventListener("resize", reposition);
    window.addEventListener("scroll", reposition, true);
    return () => {
      window.removeEventListener("resize", reposition);
      window.removeEventListener("scroll", reposition, true);
    };
  });
</script>

{#if active && current}
  <aside class="bubble" class:right={placement === "right"} class:below={placement === "below"} style={position}>
    <!-- 步骤文案字段存 i18n 键(见 onboarding.ts),渲染时 t() 取当前语言 -->
    <span>{t(current.eyebrow)}</span>
    <strong>{t(current.title)}</strong>
    <p>{t(current.body)}</p>
    <div class="actions">
      <button class="skip" onclick={complete}>{t("shell.guide.skip")}</button>
      <button class="next" onclick={next}>{step === steps.length - 1 ? t("shell.guide.done") : t("shell.guide.next")}</button>
    </div>
  </aside>
{/if}

<style>
  :global(.context-guide-target) {
    position: relative;
    z-index: 21;
    border-radius: var(--radius-xl);
    outline: 2px solid var(--accent);
    outline-offset: 7px;
  }
  .bubble {
    position: fixed;
    z-index: 40;
    box-sizing: border-box;
    padding: 0.9rem 1rem 0.8rem;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-xl);
    background: var(--canvas);
    box-shadow: var(--shadow-popover);
  }
  .bubble::before { content: ""; position: absolute; width: 0; height: 0; border-style: solid; }
  .bubble.right::before {
    left: -9px; top: 24px; border-width: 8px 9px 8px 0;
    border-color: transparent var(--hairline-strong) transparent transparent;
  }
  .bubble.below::before {
    left: 24px; top: -9px; border-width: 0 8px 9px;
    border-color: transparent transparent var(--hairline-strong);
  }
  span { color: var(--accent); font-size: 0.72rem; }
  strong { display: block; margin-top: 0.12rem; font-size: 0.92rem; }
  p { margin: 0.25rem 0 0; color: var(--ink-secondary); font-size: 0.8rem; line-height: 1.5; }
  .actions { display: flex; justify-content: flex-end; align-items: center; gap: 0.65rem; margin-top: 0.75rem; }
  button { cursor: pointer; font-size: 0.8rem; }
  .skip { border: none; background: none; color: var(--ink-secondary); }
  .next {
    border: 1px solid transparent; border-radius: var(--radius-full);
    background: var(--primary); color: var(--on-primary);
    padding: 0.48em 1.15em; box-shadow: var(--shadow-btn);
  }
</style>
