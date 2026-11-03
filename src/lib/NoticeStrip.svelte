<script lang="ts">
  // 通知条(2026-08-22 提示系统重设计,方案甲):一次只打扰一条。
  // 错误各自成红行;非错误只显示最高优先级一条,行尾「还有 N 条」展开抽屉;
  // 「知道了」按 (笔记, key, epoch) 记在 localStorage,数据变了自动回来。
  import { splitNotices, noticeEpoch, type Notice } from "$lib/notices";
  import { t } from "$lib/i18n/index.svelte";
  import { Button } from "$lib/ui/components/button";
  import { cn } from "$lib/ui/utils";

  // 行内文字按钮:accent 链接式,字号随通知行;quiet = 「知道了 / 还有 N 条」这类次要动作。
  const linkBtn = (quiet = false) =>
    cn("h-auto px-0 py-0 text-[length:inherit] font-normal text-ring", quiet && "opacity-60 hover:opacity-100");

  let {
    notices,
    storageKey,
  }: {
    /** 数组序即优先级(高在前)。 */
    notices: Notice[];
    /** 记忆「知道了」的 localStorage 键(建议含笔记 id)。 */
    storageKey: string;
  } = $props();

  const load = (): Record<string, string> => {
    try {
      return JSON.parse(localStorage.getItem(storageKey) ?? "{}");
    } catch {
      return {};
    }
  };
  let dismissed = $state<Record<string, string>>({});
  $effect(() => {
    void storageKey;
    dismissed = load();
  });
  let expanded = $state(false);

  const view = $derived(splitNotices(notices, dismissed));

  function dismiss(n: Notice) {
    dismissed = { ...dismissed, [n.key]: noticeEpoch(n) };
    try {
      localStorage.setItem(storageKey, JSON.stringify(dismissed));
    } catch {
      /* 私隐模式等存不了就存不了:本次会话内仍生效 */
    }
  }
</script>

{#snippet row(n: Notice, sub: boolean)}
  <div class="n-row n-{n.level}" class:n-sub={sub}>
    <div class="n-main">
      <span class="n-text">{n.text}</span>
      {#each n.actions ?? [] as a (a.label)}
        <Button variant="link" class={linkBtn()} disabled={a.disabled} onclick={a.run}>{a.label}</Button>
      {/each}
      {#if n.level !== "error" && n.dismissible !== false}
        <Button variant="link" class={linkBtn(true)} onclick={() => dismiss(n)}>{t("notes.notice.gotIt")}</Button>
      {/if}
      {#if !sub && view.others.length > 0}
        <span class="n-spacer"></span>
        <Button variant="link" class={linkBtn(true)} aria-expanded={expanded} onclick={() => (expanded = !expanded)}>
          {expanded ? t("notes.notice.collapse") : t("notes.notice.more", { n: view.others.length })}
        </Button>
      {/if}
    </div>
    {#if sub && n.detail}
      <div class="n-detail">{n.detail}</div>
    {/if}
  </div>
{/snippet}

{#if view.errors.length > 0 || view.head}
  <div class="n-strip">
    {#each view.errors as n (n.key)}
      {@render row(n, false)}
    {/each}
    {#if view.head}
      {@render row(view.head, false)}
      {#if expanded}
        {#each view.others as n (n.key)}
          {@render row(n, true)}
        {/each}
      {/if}
    {/if}
  </div>
{/if}

<style>
  .n-strip {
    display: flex;
    flex-direction: column;
    gap: 4px;
    margin: 6px 0;
  }
  /* 色值全部走 app.css 语义 token(UI 改版:不再硬编码),双主题与各皮肤自动跟随。 */
  .n-row {
    border-left: 3px solid var(--n-bar, var(--hairline-strong));
    background: var(--n-bg, var(--surface-soft));
    border-radius: var(--radius-md);
    padding: 5px 10px;
    font-size: 13px;
    line-height: 1.5;
  }
  .n-main {
    display: flex;
    align-items: baseline;
    gap: 10px;
    flex-wrap: wrap;
  }
  .n-spacer {
    flex: 1;
  }
  .n-detail {
    opacity: 0.6;
    font-size: 12px;
    margin-top: 2px;
  }
  .n-sub {
    margin-left: 14px;
  }
  .n-error {
    --n-bar: var(--danger);
    --n-bg: var(--danger-tint);
  }
  .n-action {
    --n-bar: var(--warning-ink);
    --n-bg: var(--warning-tint);
  }
  .n-suggest {
    --n-bar: var(--warning-line);
    --n-bg: color-mix(in srgb, var(--warning-tint) 60%, transparent);
  }
  .n-info {
    --n-bar: var(--hairline-strong);
    --n-bg: var(--surface-soft);
  }
</style>
