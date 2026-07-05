<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { recording } from "$lib/recording.svelte";
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

  // 挂载时 + 录制状态翻转时 + 笔记改名/删除时刷新列表（新笔记出现/徽章变化/标题变化）。
  $effect(() => {
    void recording.statusVersion;
    void recording.notesVersion;
    refresh();
  });

  async function toggleRecording() {
    if (recording.isLive) {
      await recording.stop(); // 跳详情由全局 status 监听驱动
    } else {
      const started = await recording.start();
      if (started) goto("/record");
    }
  }

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
      recording.bumpNotes();
    } catch (e) {
      error = `改名失败: ${e}`;
    }
  }

  async function confirmDelete(id: string) {
    confirmingDeleteId = null;
    try {
      await deleteNote(id);
      recording.bumpNotes();
      // 删的是当前正在看的笔记 → 回首页
      if ($page.url.pathname === `/notes/${id}`) {
        goto("/");
      }
    } catch (e) {
      error = `删除失败: ${e}`;
    }
  }

  const stateBadge = (s: NoteSummary["state"]) =>
    s === "active" ? "录制中" : s === "recording" ? "已中断" : "";
</script>

<aside class="sidebar">
  <button
    class="record-btn"
    class:recording={recording.isLive}
    onclick={toggleRecording}
    disabled={recording.pending}
  >
    {recording.isLive ? (recording.paused ? "⏸ 已暂停 · 停止" : "■ 停止") : "● 开始录制"}
  </button>

  <a class="nav-link" class:current={$page.url.pathname === "/speakers"} href="/speakers">👤 说话人</a>

  <input class="search" type="search" placeholder="按标题过滤…" bind:value={query} />

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if filtered.length === 0}
    <p class="hint">{notes.length === 0 ? "还没有笔记" : "没有匹配的笔记"}</p>
  {/if}

  <ul class="list">
    {#each filtered as n (n.id)}
      <li class="item" class:current={$page.url.pathname === `/notes/${n.id}`}>
        <div class="main-line">
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
</aside>

<style>
  .sidebar {
    width: 280px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    border-right: 1px solid #e5e5e7;
    background: #fafafa;
    padding: 0.75rem;
    box-sizing: border-box;
    overflow-y: auto;
  }
  .record-btn {
    border: none;
    border-radius: 8px;
    padding: 0.6em 1em;
    font-size: 1em;
    font-weight: 600;
    cursor: pointer;
    color: #fff;
    background: #396cd8;
  }
  .record-btn.recording {
    background: #c0392b;
  }
  .nav-link {
    display: block;
    box-sizing: border-box;
    margin-top: 0.6rem;
    padding: 0.45em 0.6em;
    border-radius: 8px;
    color: inherit;
    text-decoration: none;
    font-size: 0.9em;
    font-weight: 500;
  }
  .nav-link:hover {
    background: #eef2fb;
  }
  .nav-link.current {
    background: #eef2fb;
    color: #396cd8;
  }
  .search {
    box-sizing: border-box;
    width: 100%;
    margin: 0.75rem 0;
    padding: 0.4em 0.7em;
    border-radius: 8px;
    border: 1px solid #ccc;
    font-size: 0.9em;
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .item {
    padding: 0.55rem 0.4rem;
    border-bottom: 1px solid #e5e5e7;
  }
  .item.current {
    background: #eef2fb;
    border-radius: 6px;
  }
  .main-line {
    display: flex;
    flex-direction: column;
    gap: 0.1rem;
    min-width: 0;
  }
  .title {
    color: inherit;
    text-decoration: none;
    font-weight: 600;
    font-size: 0.92em;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .title:hover {
    color: #396cd8;
  }
  .rename {
    font-size: 0.92em;
    padding: 0.15em 0.3em;
    border-radius: 6px;
    border: 1px solid #396cd8;
  }
  .meta {
    color: #888;
    font-size: 0.75em;
  }
  .state {
    font-size: 0.72em;
    font-weight: 600;
    border-radius: 6px;
    padding: 0.05em 0.4em;
    margin-left: 0.35em;
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
    gap: 0.25rem;
    margin-top: 0.15rem;
  }
  .link {
    background: none;
    border: none;
    color: #396cd8;
    cursor: pointer;
    padding: 0.1em 0.25em;
    font-size: 0.78em;
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
    padding: 0.5rem 0.6rem;
    margin-bottom: 0.5rem;
    font-size: 0.85rem;
  }
  .hint {
    color: #aaa;
    font-size: 0.85em;
  }
  @media (prefers-color-scheme: dark) {
    .sidebar {
      background: #1e1e1e;
      border-color: #3a3a3a;
    }
    .item {
      border-color: #3a3a3a;
    }
    .item.current {
      background: #2a3348;
    }
    .nav-link:hover,
    .nav-link.current {
      background: #2a3348;
    }
    .nav-link.current {
      color: #7ea3f0;
    }
    .search,
    .rename {
      background: #2a2a2a;
      border-color: #444;
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
