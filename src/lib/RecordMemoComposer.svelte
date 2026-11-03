<script lang="ts">
  // 录制页底部的备注输入框(会中)。时间点 = **开始打字**那一刻的录音时间,由后端取
  // (memo_anchor_now),不信前端走表;草稿被清空(删光或 Esc)就丢掉锚点,下一次按键重取。
  import { t } from "$lib/i18n/index.svelte";
  import { addMemo, fmtClock, memoAnchorNow, MEMO_MAX_CHARS, type Memo } from "$lib/memos";

  let { noteId, onSaved }: { noteId: string; onSaved?: (m: Memo) => void } = $props();

  let text = $state("");
  let anchor = $state<number | null>(null);
  let anchoring = false;
  let saving = $state(false);
  let error = $state("");
  let el = $state<HTMLTextAreaElement | null>(null);

  const tooLong = $derived(text.length > MEMO_MAX_CHARS);

  // 换笔记(新开一场)时丢掉旧草稿的锚点。
  $effect(() => {
    void noteId;
    anchor = null;
  });

  async function ensureAnchor() {
    if (anchor !== null || anchoring) return;
    anchoring = true;
    try {
      anchor = await memoAnchorNow(noteId);
    } catch (e) {
      error = t("memo.failed", { e: String(e) });
    } finally {
      anchoring = false;
    }
  }

  function onInput() {
    error = "";
    if (text === "") anchor = null;
    else void ensureAnchor();
  }

  async function save() {
    if (saving || tooLong || !text.trim()) return;
    saving = true;
    try {
      // 极快的回车可能抢在锚点返回之前:此时就地取一次。
      const at = anchor ?? (await memoAnchorNow(noteId));
      const m = await addMemo(noteId, at, text.trim(), "composer");
      text = "";
      anchor = null;
      onSaved?.(m);
    } catch (e) {
      error = t("memo.failed", { e: String(e) });
    } finally {
      saving = false;
    }
  }

  function onKeydown(e: KeyboardEvent) {
    if (e.isComposing) return;
    if (e.key === "Enter" && !e.shiftKey) {
      e.preventDefault();
      void save();
    } else if (e.key === "Escape") {
      e.preventDefault();
      text = "";
      anchor = null;
      el?.blur();
    }
  }

  // 自适应高度:最多约 5 行,超出内部滚动。
  $effect(() => {
    void text;
    if (!el) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 120)}px`;
  });
</script>

<div class="composer-anchor">
  <div class="composer">
    <svg class="flag" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M3.5 14V2.5M3.5 3h8l-1.8 2.8L11.5 8.5h-8" />
    </svg>
    <textarea
      bind:this={el}
      bind:value={text}
      rows="1"
      placeholder={t("memo.composer.placeholder")}
      oninput={onInput}
      onkeydown={onKeydown}
      disabled={saving}
    ></textarea>
  </div>
  {#if anchor !== null || error || tooLong}
    <div class="meta">
      {#if error}
        <span class="err">{error}</span>
      {:else if tooLong}
        <span class="err">{t("memo.composer.tooLong", { n: MEMO_MAX_CHARS })}</span>
      {:else if anchor !== null}
        <span>{t("memo.composer.pinned", { t: fmtClock(anchor) })}</span>
      {/if}
    </div>
  {/if}
</div>

<style>
  /* 吸底:与「回到最新」药丸(.jump-anchor,bottom 1rem 零高锚点)错开——
     药丸由页面在录制中抬到输入框上方。 */
  .composer-anchor {
    position: sticky;
    bottom: 0;
    z-index: 5;
    background: var(--canvas);
    padding: 0.6rem 0 0.8rem;
  }
  .composer {
    display: flex;
    align-items: flex-start;
    gap: 0.5rem;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.7rem;
  }
  .composer:focus-within {
    border-color: var(--accent);
  }
  .flag {
    flex: none;
    margin-top: 0.25rem;
    color: var(--ink-faint);
  }
  textarea {
    flex: 1;
    resize: none;
    border: none;
    outline: none;
    background: transparent;
    color: var(--ink);
    font: inherit;
    font-size: 0.95rem;
    line-height: 1.5;
    padding: 0;
    max-height: 120px;
  }
  .meta {
    margin-top: 0.3rem;
    font-size: 0.8rem;
    color: var(--ink-secondary);
    padding-left: 0.2rem;
  }
  .err {
    color: var(--danger);
  }
</style>
