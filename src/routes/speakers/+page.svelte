<script lang="ts">
  import { onMount } from "svelte";
  import {
    listPeople,
    renamePerson,
    mergePerson,
    deletePerson,
    type PersonSummary,
  } from "$lib/people";
  import { formatDate, formatDuration } from "$lib/notes";

  let people = $state<PersonSummary[]>([]);
  let error = $state("");

  // 开始改名时顺带收起其它行内菜单/确认态，避免同屏多个操作态互相冲突。
  let mergeMenuId = $state<string | null>(null);
  let pendingMerge = $state<{ loser: string; winner: string } | null>(null);
  let confirmDeleteId = $state<string | null>(null);

  const sourceLabel = (s: string) => (s === "mic" ? "麦克风" : s === "system" ? "系统声音" : s);

  /** 合并菜单里的展示名(未命名人也要能被当成合并目标指认)。 */
  function displayName(p: PersonSummary): string {
    return p.name || `未命名 · 最近 ${formatDate(p.last_seen)}`;
  }

  async function refresh() {
    try {
      people = await listPeople();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  onMount(refresh);

  // 显式编辑态(冒烟反馈:占位文本 contenteditable 让"能改名"完全不可发现)——
  // 未命名人给显眼的「命名」按钮,已命名人名字带 ✎ 角标;点击换成真输入框。
  let editingId = $state<string | null>(null);
  let editingName = $state("");

  function beginRename(p: PersonSummary) {
    editingId = p.id;
    editingName = p.name;
    mergeMenuId = null;
    pendingMerge = null;
    confirmDeleteId = null;
  }

  async function commitRename(p: PersonSummary) {
    if (editingId !== p.id) return;
    const text = editingName.trim();
    editingId = null;
    if (!text || text === p.name) return; // 空/未变:静默还原,不当真改名
    try {
      await renamePerson(p.id, text);
      await refresh();
    } catch (err) {
      error = `改名失败: ${err}`;
      await refresh();
    }
  }

  async function doMerge() {
    if (!pendingMerge) return;
    const { loser, winner } = pendingMerge;
    pendingMerge = null;
    try {
      await mergePerson(loser, winner);
      await refresh();
    } catch (e) {
      // 录制中后端拒绝等错误文案原样展示。
      error = `${e}`;
    }
  }

  async function doDelete(id: string) {
    confirmDeleteId = null;
    try {
      await deletePerson(id);
      await refresh();
    } catch (e) {
      error = `删除失败: ${e}`;
    }
  }
</script>

<main class="container">
  <h1>说话人</h1>
  <p class="desc">
    这里是声纹库:录到的说话人会自动登记。给"未命名"的人<strong>命名</strong>后,
    之后的录制会自动认出他并直接显示名字;认错拆重了就用<strong>合并</strong>归到同一个人。
  </p>

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if people.length === 0}
    <p class="hint">还没有说话人。录一场会议(单人说话累计满 10 秒),停止后会自动出现在这里。</p>
  {:else}
    <ul class="list">
      {#each people as p (p.id)}
        <li class="item">
          <div class="main-line">
            {#if editingId === p.id}
              <!-- svelte-ignore a11y_autofocus -->
              <input
                class="name-input"
                autofocus
                placeholder="输入名字,如 张三"
                bind:value={editingName}
                onkeydown={(e) => {
                  if (e.key === "Enter") commitRename(p);
                  if (e.key === "Escape") editingId = null;
                }}
                onblur={() => commitRename(p)}
              />
            {:else if p.name}
              <button class="name-btn" title="点击改名" onclick={() => beginRename(p)}>
                {p.name}<span class="pencil">✎</span>
              </button>
            {:else}
              <span class="unnamed">未命名</span>
              <button class="name-cta" onclick={() => beginRename(p)}>命名</button>
            {/if}
            <span class="meta">
              {#if !p.name}最近 {formatDate(p.last_seen)} · {/if}累计发声 {formatDuration(Math.floor(p.total_ms / 1000))}
              {#each p.sources as s (s)}
                <span class="badge">{sourceLabel(s)}</span>
              {/each}
            </span>
          </div>
          <div class="actions">
            {#if pendingMerge && pendingMerge.loser === p.id}
              {@const target = people.find((o) => o.id === pendingMerge?.winner)}
              <span class="confirm-text">
                确认合并到「{target ? displayName(target) : "?"}」？合并后该人历史笔记显示目标人的名字。
              </span>
              <button class="link danger" onclick={doMerge}>确认合并</button>
              <button class="link" onclick={() => (pendingMerge = null)}>取消</button>
            {:else if mergeMenuId === p.id}
              <span class="menu">
                {#each people.filter((o) => o.id !== p.id) as o (o.id)}
                  <button class="menu-item" onclick={() => (pendingMerge = { loser: p.id, winner: o.id })}>
                    {displayName(o)}
                  </button>
                {/each}
                <button class="menu-item" onclick={() => (mergeMenuId = null)}>取消</button>
              </span>
            {:else}
              <button class="link" disabled={people.length < 2} onclick={() => (mergeMenuId = p.id)}>
                合并到…
              </button>
            {/if}
            {#if confirmDeleteId === p.id}
              <button class="link danger" onclick={() => doDelete(p.id)}>确认删除</button>
              <button class="link" onclick={() => (confirmDeleteId = null)}>取消</button>
            {:else}
              <button class="link" onclick={() => (confirmDeleteId = p.id)}>删除</button>
            {/if}
          </div>
        </li>
      {/each}
    </ul>
  {/if}
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
  }
  h1 {
    margin: 0 0 1rem;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
    background: #f5f5f7;
    border-radius: 8px;
  }
  .item {
    padding: 0.75rem 1rem;
    border-bottom: 1px solid #e5e5e7;
  }
  .item:last-child {
    border-bottom: none;
  }
  .main-line {
    display: flex;
    align-items: baseline;
    justify-content: space-between;
    gap: 1rem;
    flex-wrap: wrap;
  }
  .desc {
    color: #666;
    font-size: 0.92em;
    margin: -0.5rem 0 1rem;
    max-width: 46rem;
  }
  .name-btn {
    background: none;
    border: none;
    box-shadow: none;
    font: inherit;
    font-weight: 600;
    font-size: 1.05em;
    color: inherit;
    cursor: pointer;
    border-radius: 4px;
    padding: 0.1em 0.3em;
    margin: -0.1em -0.3em;
  }
  .name-btn:hover {
    background: rgba(57, 108, 216, 0.08);
  }
  .pencil {
    color: #aaa;
    font-size: 0.8em;
    margin-left: 0.35em;
  }
  .name-btn:hover .pencil {
    color: #396cd8;
  }
  .unnamed {
    font-weight: 600;
    font-size: 1.05em;
    font-style: italic;
    color: #999;
  }
  .name-cta {
    background: #396cd8;
    color: #fff;
    border: none;
    border-radius: 6px;
    padding: 0.15em 0.8em;
    font-size: 0.85em;
    font-weight: 600;
    cursor: pointer;
    margin-left: 0.5em;
    box-shadow: none;
  }
  .name-cta:hover {
    background: #2f5ec4;
  }
  .name-input {
    font: inherit;
    font-weight: 600;
    font-size: 1.05em;
    border: 1px solid #396cd8;
    border-radius: 6px;
    padding: 0.1em 0.4em;
    min-width: 12rem;
  }
  .meta {
    color: #888;
    font-size: 0.85em;
    white-space: nowrap;
  }
  .badge {
    display: inline-block;
    font-size: 0.85em;
    border-radius: 6px;
    padding: 0.05em 0.5em;
    margin-left: 0.4em;
    background: #e5e5e7;
    color: #555;
  }
  .actions {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.4rem;
    margin-top: 0.4rem;
  }
  .link {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.15em 0.3em;
    font-size: 0.85em;
    box-shadow: none;
  }
  .link:disabled {
    color: #aaa;
    cursor: default;
  }
  .link.danger {
    color: #c0392b;
    font-weight: 600;
  }
  .menu {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 0.25em;
    background: #fff;
    border: 1px solid #ccc;
    border-radius: 8px;
    padding: 0.2em 0.4em;
  }
  .menu-item {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    font-size: 0.85em;
    padding: 0.15em 0.4em;
  }
  .confirm-text {
    font-size: 0.85em;
    color: #8a5a00;
  }
  .banner {
    background: #fff4e5;
    border: 1px solid #f0c98a;
    color: #8a5a00;
    border-radius: 8px;
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .hint {
    color: #aaa;
  }
  @media (prefers-color-scheme: dark) {
    .list {
      background: #2a2a2a;
    }
    .item {
      border-color: #3a3a3a;
    }
    .desc {
      color: #999;
    }
    .name-input {
      background: #2a2a2a;
      color: #f0f0f0;
    }
    .unnamed {
      color: #777;
    }
    .badge {
      background: #3a3a3a;
      color: #ccc;
    }
    .menu {
      background: #2a2a2a;
      border-color: #555;
    }
    .banner {
      background: #3a2e18;
      border-color: #6b5426;
      color: #e8c88a;
    }
    .hint {
      color: #555;
    }
    .confirm-text {
      color: #e8c88a;
    }
  }
</style>
