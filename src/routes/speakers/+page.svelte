<script lang="ts">
  import { onMount } from "svelte";
  import { listPeople, type PersonSummary } from "$lib/people";
  import { recording } from "$lib/recording.svelte";

  // 主从结构的落地页:人物索引在侧栏,本页只做概览引导——不再重复列一遍名单。
  let people = $state<PersonSummary[]>([]);
  let error = $state("");

  const named = $derived(people.filter((p) => p.name).length);
  const unnamed = $derived(people.length - named);

  async function refresh() {
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  onMount(refresh);
  // 详情页改名/合并/删除后统计同步。
  $effect(() => {
    void recording.peopleVersion;
    refresh();
  });
</script>

<main class="container">
  <h1>声纹库</h1>
  <p class="desc">
    录到的说话人会自动登记。给"未命名"的人<strong>命名</strong>后,之后的录制会自动认出他并直接显示名字;
    认错拆重了就用<strong>合并</strong>归到同一个人。从左侧选择一个人查看详情、试听原声或管理。
  </p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if people.length === 0}
    <div class="empty">
      <p>还没有说话人。</p>
      <p class="hint">录一场会议(单人说话累计满 10 秒),停止后会自动出现在左侧。</p>
    </div>
  {:else}
    <div class="stats">
      <div class="stat">
        <span class="num">{people.length}</span>
        <span class="label">位说话人</span>
      </div>
      <div class="stat">
        <span class="num">{named}</span>
        <span class="label">已命名</span>
      </div>
      {#if unnamed > 0}
        <div class="stat todo">
          <span class="num">{unnamed}</span>
          <span class="label">待命名</span>
        </div>
      {/if}
    </div>
    <p class="pick-hint">
      从左侧列表选择一个人查看详情。
      {#if unnamed > 0}「待命名」的人命名后,之后的录制会自动显示名字。{/if}
    </p>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 44rem;
  }
  h1 {
    margin: 0 0 0.75rem;
  }
  .desc {
    color: var(--ink-secondary);
    font-size: 0.85rem;
    line-height: 1.5;
    margin: 0 0 1.25rem;
    max-width: 40rem;
  }
  /* 统计卡:surface 底并排三块,数字大字 500 权重(层级靠亮度不靠重字重) */
  .stats {
    display: flex;
    gap: 0.75rem;
    margin-bottom: 1rem;
  }
  .stat {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.9rem 1.3rem;
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 6.5rem;
  }
  .num {
    font-size: 1.5rem;
    font-weight: 500;
    color: var(--ink);
    line-height: 1.2;
  }
  .label {
    font-size: 0.8rem;
    color: var(--ink-secondary);
  }
  /* 待命名是待处理项:warning 色系点亮数字提示还有活没干 */
  .stat.todo .num {
    color: var(--warning-ink);
  }
  .pick-hint {
    color: var(--ink-faint);
    font-size: 0.85rem;
  }
  .empty {
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 2rem 1.5rem;
    text-align: center;
  }
  .empty p {
    margin: 0 0 0.4rem;
    font-weight: 500;
  }
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: var(--ink-faint);
    font-weight: 400;
  }
</style>
