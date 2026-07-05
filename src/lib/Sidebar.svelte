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
    <span class="rec-dot" class:square={recording.isLive}></span>
    {recording.isLive ? (recording.paused ? "已暂停 · 停止" : "停止录制") : "开始录制"}
  </button>

  <a class="nav-link" class:current={$page.url.pathname === "/speakers"} href="/speakers">
    <svg class="nav-icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round">
      <circle cx="8" cy="5.2" r="2.7" />
      <path d="M2.8 13.4c1-2.3 3-3.4 5.2-3.4s4.2 1.1 5.2 3.4" />
    </svg>
    说话人
  </a>

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
                <span
                  class="state"
                  class:interrupted={n.state === "recording"}
                  class:active={n.state === "active"}
                >
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
  /* sidebar 组件规范：surface 底 + 右侧发丝线，条目 rounded-md、hover surface-soft、
     当前页 surface-press + ink 加粗。 */
  .sidebar {
    width: 280px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    border-right: 1px solid var(--hairline);
    background: var(--surface);
    padding: 0.75rem;
    box-sizing: border-box;
    overflow-y: auto;
  }
  /* 录制按钮:白底 + 红点(语音备忘录式)。大面积强调蓝在侧栏太吵,主 CTA 的
     "彩色"由红点承担——红是本产品唯一常驻彩色信号,识别度反而更高。 */
  .record-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 0.5em;
    border: none;
    border-radius: var(--radius-md);
    padding: 0.55em 1em;
    font-size: 0.9rem;
    font-weight: 500;
    cursor: pointer;
    color: var(--ink);
    background: var(--canvas);
    box-shadow: var(--shadow-btn);
  }
  .record-btn:hover {
    background: var(--surface-soft);
  }
  .rec-dot {
    width: 9px;
    height: 9px;
    border-radius: var(--radius-full);
    background: var(--record);
    flex-shrink: 0;
  }
  /* 录制中红点变方块 = 通用"停止"符号,文字不再需要 Unicode 符号凑数 */
  .rec-dot.square {
    border-radius: 2px;
  }
  .record-btn.recording {
    color: var(--record);
    font-weight: 600;
  }
  .record-btn:disabled {
    opacity: 0.6;
    cursor: default;
  }
  .nav-link {
    display: flex;
    align-items: center;
    gap: 0.45em;
    box-sizing: border-box;
    margin-top: 0.6rem;
    padding: 0.45em 0.6em;
    border-radius: var(--radius-md);
    color: var(--ink-secondary);
    text-decoration: none;
    font-size: 0.9em;
    font-weight: 500;
  }
  .nav-icon {
    width: 15px;
    height: 15px;
    color: var(--ink-faint);
  }
  .nav-link.current .nav-icon,
  .nav-link:hover .nav-icon {
    color: var(--ink-secondary);
  }
  .nav-link:hover {
    background: var(--surface-soft);
  }
  .nav-link.current {
    background: var(--surface-press);
    color: var(--ink);
    font-weight: 700;
  }
  /* 过滤框:内嵌式(surface-press 底、无边)——侧栏里带边框的输入框比正文还抢眼,
     Notion 侧栏过滤即此形态;聚焦才浮出 canvas 底 + accent 环。 */
  .search {
    box-sizing: border-box;
    width: 100%;
    margin: 0.75rem 0;
    padding: 0.4em 0.7em;
    border-radius: var(--radius-md);
    border: 1px solid transparent;
    background: var(--surface-press);
    color: var(--ink);
    font-size: 0.9em;
  }
  .search::placeholder {
    color: var(--ink-faint);
  }
  .search:focus {
    outline: none;
    background: var(--canvas);
    border-color: var(--accent);
    box-shadow: 0 0 0 1px var(--accent);
  }
  .list {
    list-style: none;
    margin: 0;
    padding: 0;
  }
  .item {
    padding: 0.55rem 0.5rem;
    border-radius: var(--radius-md);
  }
  .item:hover {
    background: var(--surface-soft);
  }
  .item.current {
    background: var(--surface-press);
  }
  /* 悬停显影:行级操作默认隐身,列表保持安静(DESIGN.md 原则 5) */
  .item .actions {
    visibility: hidden;
  }
  .item:hover .actions,
  .item.current .actions {
    visibility: visible;
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
    color: var(--accent);
  }
  .rename {
    font-size: 0.92em;
    padding: 0.15em 0.3em;
    border-radius: var(--radius-md);
    border: 1px solid var(--accent);
    background: var(--canvas);
    color: var(--ink);
  }
  .meta {
    color: var(--ink-faint);
    font-size: 0.75em;
  }
  .state {
    font-size: 0.72em;
    font-weight: 600;
    border-radius: var(--radius-md);
    padding: 0.05em 0.4em;
    margin-left: 0.35em;
    vertical-align: middle;
  }
  /* 已中断：沿用 warning 色系（浅色调+深文字），亮/暗色下都可读。 */
  .state.interrupted {
    background: var(--warning-line);
    color: var(--warning-ink);
  }
  /* 录制中：record 是双主题一致的常驻彩色信号，白字在两种主题下都清晰。 */
  .state.active {
    background: var(--record);
    color: var(--on-accent);
  }
  .actions {
    display: flex;
    gap: 0.25rem;
    margin-top: 0.15rem;
  }
  /* button-link：无底无边，accent 字，悬停加下划线 */
  .link {
    background: none;
    border: none;
    color: var(--accent);
    cursor: pointer;
    padding: 0.1em 0.25em;
    font-size: 0.78em;
  }
  .link:hover {
    text-decoration: underline;
  }
  .link.danger {
    color: var(--danger);
    font-weight: 600;
  }
  /* 此处 banner 只用于加载失败，用 danger 色系（DESIGN.md：错误横幅换 danger） */
  .banner {
    background: var(--danger-tint);
    border: 1px solid var(--danger-line);
    color: var(--danger-ink);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.6rem;
    margin-bottom: 0.5rem;
    font-size: 0.85rem;
  }
  .hint {
    color: var(--ink-faint);
    font-size: 0.85em;
  }
</style>
