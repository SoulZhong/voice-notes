<script lang="ts">
  // 两段式文本输入(2026-08-11 冒烟反馈:裸 input 常驻可编辑太易误触):
  // 确认态只读展示值 + 「编辑」;编辑态 input + 确认/取消(Enter/Esc 同义)。
  // 失焦不做任何事——点「确认」按钮时 blur 先于 click 触发,任何 blur 语义
  // 都会吃掉这次点击,这是显式两按钮设计的前提。
  import { t } from "$lib/i18n/index.svelte";
  import { Button } from "$lib/ui/components/button";
  import { Input } from "$lib/ui/components/input";
  import { cn } from "$lib/ui/utils";

  // 控件外观(UI 改版):小号描边钮;「确认」用 accent 描边字区分主次。
  const btn = (primary = false) =>
    cn(
      "h-6 px-3 text-[0.8rem] text-muted-foreground hover:text-foreground",
      primary && "border-ring text-ring hover:text-ring dark:border-ring",
    );

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
    // 中文输入法选词的 Enter 不是提交。
    if (e.key === "Enter" && !e.isComposing) {
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
    <Input
      bind:ref={inputEl}
      class="h-7 min-w-36 flex-1 border-transparent bg-accent px-2.5 text-[0.85rem] md:text-[0.85rem] dark:bg-accent"
      type={masked ? "password" : "text"}
      {placeholder}
      bind:value={draft}
      onkeydown={onKey}
      disabled={saving}
    />
    <Button variant="outline" size="xs" class={btn(true)} onclick={confirm} disabled={saving}>{t("common.confirm")}</Button>
    <Button variant="outline" size="xs" class={btn()} onclick={cancel} disabled={saving}>{t("settings.cancel")}</Button>
  </div>
{:else}
  <div class="ef" class:wide>
    <span class="ef-value" class:faint={!value} title={masked || !value ? undefined : value}>
      {#if !value}{placeholder || t("common.notSet")}{:else if masked}••••••••••{:else}{value}{/if}
    </span>
    <Button variant="outline" size="xs" class={btn()} onclick={beginEdit} {disabled}>{t("common.edit")}</Button>
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
</style>
