<script lang="ts">
  // 备注浮窗(⌥⌘M / 托盘「加备注…」):按下那一刻后端已写好一条时间标记,这里只负责补文字。
  // 回车 = 保存并关闭(焦点还给原应用);Esc = 关闭(时间标记保留);失焦 = 有字先存再关。
  // 浮窗开着又按一次快捷键:后端再钉一条新的并发 memo_panel_open——先把当前输入存下再切过去。
  import { onMount } from "svelte";
  import { t } from "$lib/i18n/index.svelte";
  import {
    editMemo,
    fmtClock,
    memoPanelHide,
    memoPanelState,
    onMemoPanelOpen,
    MEMO_MAX_CHARS,
    type MemoPanelOpen,
  } from "$lib/memos";
  import { Textarea } from "$lib/ui/components/textarea";

  let cur = $state<MemoPanelOpen | null>(null);
  let text = $state("");
  let error = $state("");
  let busy = false;
  let input = $state<HTMLTextAreaElement | null>(null);

  async function persist(target: MemoPanelOpen | null, value: string) {
    const v = value.trim();
    if (!target || !v || v.length > MEMO_MAX_CHARS) return;
    await editMemo(target.note_id, target.memo, { text: v });
  }

  async function open(next: MemoPanelOpen) {
    const prev = cur;
    const pending = text;
    cur = next;
    text = "";
    error = "";
    if (prev && prev.memo.id !== next.memo.id) {
      await persist(prev, pending).catch((e) => (error = t("memo.failed", { e: String(e) })));
    }
    queueMicrotask(() => input?.focus());
  }

  async function saveAndClose() {
    if (busy) return;
    busy = true;
    try {
      await persist(cur, text);
      text = "";
      cur = null;
      await memoPanelHide();
    } catch (e) {
      error = t("memo.failed", { e: String(e) });
    } finally {
      busy = false;
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.isComposing) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void saveAndClose();
    } else if (e.key === "Escape") {
      e.preventDefault();
      text = "";
      cur = null;
      void memoPanelHide();
    }
  }

  onMount(() => {
    void memoPanelState().then((s) => {
      if (s) void open(s);
    });
    const un = onMemoPanelOpen((p) => void open(p));
    // 失焦(用户点回会议软件):有字先存,时间标记无论如何保留。
    const onBlur = () => {
      if (cur) void saveAndClose();
    };
    window.addEventListener("blur", onBlur);
    return () => {
      window.removeEventListener("blur", onBlur);
      void un.then((f) => f());
    };
  });
</script>

<div class="panel">
  <div class="head">
    <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M3.5 14V2.5M3.5 3h8l-1.8 2.8L11.5 8.5h-8" />
    </svg>
    {#if cur}
      <span>{t("memo.panel.pinned", { t: fmtClock(cur.memo.at_ms) })}</span>
    {/if}
    <span class="hint">{t("memo.panel.hint")}</span>
  </div>
  <!-- svelte-ignore a11y_autofocus -->
  <!-- 浮窗本身就是输入语境(外框即面板),组件去边去底去聚焦环,只留文字区。 -->
  <Textarea
    bind:ref={input}
    bind:value={text}
    autofocus
    rows={2}
    class="min-h-0 flex-1 resize-none rounded-none border-0 bg-transparent p-0 text-[0.95rem] leading-normal shadow-none field-sizing-fixed focus-visible:ring-0 md:text-[0.95rem] dark:bg-transparent"
    placeholder={t("memo.panel.placeholder")}
    onkeydown={onKeydown}
  />
  {#if error}<div class="err">{error}</div>{/if}
</div>

<style>
  :global(html),
  :global(body) {
    margin: 0;
    height: 100%;
    background: var(--canvas);
    color: var(--ink);
    overflow: hidden;
  }
  .panel {
    box-sizing: border-box;
    height: 100vh;
    display: flex;
    flex-direction: column;
    gap: 0.4rem;
    padding: 0.7rem 0.9rem;
    border: 1px solid var(--hairline-strong);
    border-radius: var(--radius-lg);
    background: var(--surface);
  }
  .head {
    display: flex;
    align-items: center;
    gap: 0.4rem;
    font-size: 0.85rem;
    color: var(--accent);
    font-variant-numeric: tabular-nums;
  }
  .hint {
    margin-left: auto;
    color: var(--ink-faint);
    font-size: 0.75rem;
  }
  .err {
    color: var(--danger);
    font-size: 0.8rem;
  }
</style>
