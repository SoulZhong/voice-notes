<script lang="ts">
  // 两段式文本输入(2026-08-11 冒烟反馈:裸 input 常驻可编辑太易误触):
  // 确认态只读展示值 + 「编辑」;编辑态 input + 确认/取消(Enter/Esc 同义)。
  // 失焦不做任何事——点「确认」按钮时 blur 先于 click 触发,任何 blur 语义
  // 都会吃掉这次点击,这是显式两按钮设计的前提。
  import { t } from "$lib/i18n/index.svelte";

  let {
    value = "",
    placeholder = "",
    masked = false,
    wide = false,
    disabled = false,
    onSave,
    onEditStart,
  }: {
    value?: string;
    placeholder?: string;
    /** 密钥类:确认态只显示掩码圆点,永不回显明文。 */
    masked?: boolean;
    wide?: boolean;
    disabled?: boolean;
    /** 确认时回调提交值;返回 Promise 则等待完成后才退出编辑态。 */
    onSave: (v: string) => unknown;
    /** 进入编辑态时回调(调用方清"测试通过"这类会给旧值背书的状态)。 */
    onEditStart?: () => void;
  } = $props();

  let editing = $state(false);
  let draft = $state("");
  let saving = $state(false);
  let inputEl = $state<HTMLInputElement | null>(null);

  function beginEdit() {
    if (disabled) return;
    draft = value;
    editing = true;
    onEditStart?.();
    // autofocus 属性对动态挂载不可靠,下一拍手动聚焦。
    setTimeout(() => inputEl?.focus(), 0);
  }
  async function confirm() {
    if (saving) return;
    saving = true;
    try {
      await onSave(draft.trim());
      editing = false;
    } finally {
      saving = false;
    }
  }
  function cancel() {
    editing = false; // 丢弃草稿,值以确认态(外部真值)为准
  }
  function onKey(e: KeyboardEvent) {
    if (e.key === "Enter") {
      e.preventDefault();
      void confirm();
    } else if (e.key === "Escape") {
      e.preventDefault();
      cancel();
    }
  }
</script>

{#if editing}
  <div class="ef editing" class:wide>
    <input
      bind:this={inputEl}
      class="ef-input"
      type={masked ? "password" : "text"}
      {placeholder}
      bind:value={draft}
      onkeydown={onKey}
      disabled={saving}
    />
    <button class="ef-btn primary" onclick={confirm} disabled={saving}>{t("common.confirm")}</button>
    <button class="ef-btn" onclick={cancel} disabled={saving}>{t("settings.cancel")}</button>
  </div>
{:else}
  <div class="ef" class:wide>
    <span class="ef-value" class:faint={!value} title={masked || !value ? undefined : value}>
      {#if !value}{placeholder || t("common.notSet")}{:else if masked}••••••••••{:else}{value}{/if}
    </span>
    <button class="ef-btn" onclick={beginEdit} {disabled}>{t("common.edit")}</button>
  </div>
{/if}

<style>
  .ef {
    display: inline-flex;
    align-items: center;
    gap: 0.45rem;
    margin-left: auto;
    max-width: 24rem;
    min-width: 0;
  }
  .ef.wide {
    max-width: 30rem;
  }
  .ef-value {
    font-size: 0.85rem;
    color: var(--ink);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    min-width: 0;
  }
  .ef-value.faint {
    color: var(--ink-faint, var(--ink-secondary));
  }
  .ef-input {
    flex: 1;
    min-width: 9rem;
    box-sizing: border-box;
    padding: 0.32em 0.6em;
    border: none;
    border-radius: var(--radius-md);
    background: var(--surface-press);
    color: var(--ink);
    font-size: 0.85rem;
  }
  .ef-input:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .ef-btn {
    flex: none;
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    padding: 0.28em 0.8em;
    font-size: 0.8rem;
    font-weight: 500;
    cursor: pointer;
    background: transparent;
    color: var(--ink-secondary);
  }
  .ef-btn:hover:not(:disabled) {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .ef-btn.primary {
    border-color: var(--accent);
    color: var(--accent);
  }
  .ef-btn:disabled {
    opacity: 0.5;
    cursor: default;
  }
</style>
