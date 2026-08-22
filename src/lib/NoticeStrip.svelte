<script lang="ts">
  // 通知条(2026-08-22 提示系统重设计,方案甲):一次只打扰一条。
  // 错误各自成红行;非错误只显示最高优先级一条,行尾「还有 N 条」展开抽屉;
  // 「知道了」按 (笔记, key, epoch) 记在 localStorage,数据变了自动回来。
  import { splitNotices, noticeEpoch, type Notice } from "$lib/notices";
  import { t } from "$lib/i18n/index.svelte";

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
        <button class="link" disabled={a.disabled} onclick={a.run}>{a.label}</button>
      {/each}
      {#if n.level !== "error" && n.dismissible !== false}
        <button class="link n-quiet" onclick={() => dismiss(n)}>{t("notes.notice.gotIt")}</button>
      {/if}
      {#if !sub && view.others.length > 0}
        <span class="n-spacer"></span>
        <button class="link n-quiet" onclick={() => (expanded = !expanded)}>
          {expanded ? t("notes.notice.collapse") : t("notes.notice.more", { n: view.others.length })}
        </button>
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
  .n-row {
    border-left: 3px solid var(--n-bar, #8a8a8a);
    background: var(--n-bg, rgba(255, 255, 255, 0.04));
    border-radius: 6px;
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
  .n-quiet {
    opacity: 0.6;
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
    --n-bar: #e5484d;
    --n-bg: rgba(229, 72, 77, 0.09);
  }
  .n-action {
    --n-bar: #d9a521;
    --n-bg: rgba(217, 165, 33, 0.08);
  }
  .n-suggest {
    --n-bar: #9a8a4a;
    --n-bg: rgba(154, 138, 74, 0.06);
  }
  .n-info {
    --n-bar: #6b6f76;
    --n-bg: rgba(255, 255, 255, 0.03);
  }
</style>
