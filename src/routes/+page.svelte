<script lang="ts">
  import { onMount } from "svelte";
  import {
    listNotes,
    renameNote,
    deleteNote,
    formatDate,
    formatDuration,
    type NoteSummary,
  } from "$lib/notes";

  let notes = $state<NoteSummary[]>([]);
  let query = $state("");
  let error = $state("");
  let editingId = $state<string | null>(null);
  let editingTitle = $state("");
  let confirmingDeleteId = $state<string | null>(null);

  const filtered = $derived(
    query.trim() ? notes.filter((n) => n.title.toLowerCase().includes(query.trim().toLowerCase())) : notes,
  );

  async function refresh() {
    try {
      notes = await listNotes();
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
  }

  onMount(refresh);

  function beginRename(n: NoteSummary) {
    editingId = n.id;
    editingTitle = n.title;
  }

  async function commitRename() {
    if (!editingId) return;
    const id = editingId;
    editingId = null;
    try {
      await renameNote(id, editingTitle);
    } catch (e) {
      error = `改名失败: ${e}`;
    }
    await refresh();
  }

  async function confirmDelete(id: string) {
    confirmingDeleteId = null;
    try {
      await deleteNote(id);
    } catch (e) {
      error = `删除失败: ${e}`;
    }
    await refresh();
  }

  const stateBadge = (s: NoteSummary["state"]) =>
    s === "active" ? "录制中" : s === "recording" ? "已中断" : "";
</script>

<main class="container">
  <div class="row header">
    <h1>会议笔记</h1>
    <a class="primary" href="/record">开始录制</a>
  </div>

  <input class="search" type="search" placeholder="按标题过滤…" bind:value={query} />

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if filtered.length === 0}
    <p class="hint">{notes.length === 0 ? "还没有笔记，点「开始录制」来第一场。" : "没有匹配的笔记。"}</p>
  {/if}

  <ul class="list">
    {#each filtered as n (n.id)}
      <li class="item">
        <div class="main">
          {#if editingId === n.id}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="rename"
              autofocus
              bind:value={editingTitle}
              onkeydown={(e) => {
                if (e.key === "Enter") commitRename();
                if (e.key === "Escape") editingId = null;
              }}
              onblur={commitRename}
            />
          {:else}
            <a class="title" href={n.state === "active" ? "/record" : `/notes/${n.id}`}>
              {n.title}
              {#if stateBadge(n.state)}
                <span class="state" class:interrupted={n.state === "recording"} class:active={n.state === "active"}>
                  {stateBadge(n.state)}
                </span>
              {/if}
            </a>
          {/if}
          <span class="meta">{formatDate(n.started_at)} · {formatDuration(n.duration_secs)}</span>
        </div>
        <div class="actions">
          <button class="link" onclick={() => beginRename(n)}>改名</button>
          {#if confirmingDeleteId === n.id}
            <button class="link danger" onclick={() => confirmDelete(n.id)}>确认删除</button>
            <button class="link" onclick={() => (confirmingDeleteId = null)}>取消</button>
          {:else}
            <button class="link" onclick={() => (confirmingDeleteId = n.id)}>删除</button>
          {/if}
        </div>
      </li>
    {/each}
  </ul>
</main>

<style>
  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
    max-width: 42rem;
  }
  .row.header {
    display: flex;
    justify-content: space-between;
    align-items: center;
  }
  h1 {
    margin: 0 0 0.5rem;
  }
  a.primary {
    background: #396cd8;
    color: #fff;
    border-radius: 8px;
    padding: 0.5em 1.2em;
    text-decoration: none;
    font-weight: 500;
  }
  .search {
    width: 100%;
    box-sizing: border-box;
    margin: 0.75rem 0 1rem;
    padding: 0.5em 0.8em;
    border-radius: 8px;
    border: 1px solid #ccc;
    font-size: 1em;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .item {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 0.5rem;
    padding: 0.7rem 0.4rem;
    border-bottom: 1px solid #e5e5e7;
  }
  .main {
    display: flex;
    flex-direction: column;
    gap: 0.15rem;
    min-width: 0;
  }
  .title {
    color: inherit;
    text-decoration: none;
    font-weight: 600;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .title:hover {
    color: #396cd8;
  }
  .rename {
    font-size: 1em;
    padding: 0.2em 0.4em;
    border-radius: 6px;
    border: 1px solid #396cd8;
  }
  .meta {
    color: #888;
    font-size: 0.85em;
  }
  .state {
    font-size: 0.7em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.1em 0.45em;
    margin-left: 0.4em;
    vertical-align: middle;
    color: #fff;
  }
  .state.interrupted {
    background: #d88a39;
  }
  .state.active {
    background: #c0392b;
  }
  .actions {
    display: flex;
    gap: 0.3rem;
    flex-shrink: 0;
  }
  .link {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.2em 0.3em;
    font-size: 0.9em;
    box-shadow: none;
  }
  .link.danger {
    color: #c0392b;
    font-weight: 600;
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
    .item {
      border-color: #3a3a3a;
    }
    .search {
      background: #2a2a2a;
      border-color: #444;
      color: #f0f0f0;
    }
    .rename {
      background: #2a2a2a;
      color: #f0f0f0;
    }
    .banner {
      background: #3a2e18;
      border-color: #6b5426;
      color: #e8c88a;
    }
    .hint {
      color: #555;
    }
  }
</style>
