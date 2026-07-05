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
    speakerColor,
    type NoteSummary,
  } from "$lib/notes";
  import { listPeople, type PersonSummary } from "$lib/people";

  let notes = $state<NoteSummary[]>([]);
  let query = $state("");
  let error = $state("");

  // 页签完全由路由派生(点击=导航,零独立状态):/speakers 域=声纹库,其余(笔记/录制/设置)=录音记录。
  const tab = $derived($page.url.pathname.startsWith("/speakers") ? "people" : "notes");

  let people = $state<PersonSummary[]>([]);
  let peopleError = $state("");

  async function refreshPeople() {
    try {
      people = await listPeople();
      peopleError = "";
    } catch (e) {
      peopleError = `加载失败: ${e}`;
    }
  }

  // 切到声纹库页签时拉取;详情页改名/合并/删除后经 peopleVersion 触发重拉,索引不滞留旧名。
  $effect(() => {
    void recording.peopleVersion;
    if (tab === "people") refreshPeople();
  });

  // 与详情页同一套排序/分组语义:最近出现在前;待命名是待处理项排上面。
  const peopleSorted = $derived(
    [...people].sort((a, b) => (b.last_seen || "").localeCompare(a.last_seen || "")),
  );
  const peopleUnnamed = $derived(peopleSorted.filter((p) => !p.name));
  const peopleNamed = $derived(peopleSorted.filter((p) => p.name));
  let editingId = $state<string | null>(null);
  let editingTitle = $state("");
  let confirmingDeleteId = $state<string | null>(null);

  // 右键菜单(冒烟反馈:改名/删除从行内挪进 context menu,列表不再有常驻操作行)
  let menuForId = $state<string | null>(null);
  let menuX = $state(0);
  let menuY = $state(0);

  function openMenu(e: MouseEvent, id: string) {
    e.preventDefault();
    menuForId = id;
    confirmingDeleteId = null;
    menuX = e.clientX;
    menuY = e.clientY;
  }

  function closeMenu() {
    menuForId = null;
    confirmingDeleteId = null;
  }

  /** 整行可点跳转;行内的按钮/输入框/链接各有己任,不劫持。 */
  function rowClick(e: MouseEvent, n: NoteSummary) {
    if ((e.target as HTMLElement).closest("button, input, a")) return;
    goto(n.state === "active" ? "/record" : `/notes/${n.id}`);
  }

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

{#snippet personRow(p: PersonSummary)}
  <!-- 与笔记行同构:行内锚点提供键盘路径,li onclick 是指针便利层 -->
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
  <li
    class="item person"
    class:current={$page.url.pathname === `/speakers/${p.id}`}
    onclick={(e) => {
      if ((e.target as HTMLElement).closest("a")) return;
      goto(`/speakers/${p.id}`);
    }}
  >
    <span class="dot" style="background: {speakerColor(p.id, 'mic')}"></span>
    <div class="main-line">
      <a class="title" class:unnamed={!p.name} href="/speakers/{p.id}">{p.name || "未命名"}</a>
      <span class="meta">最近出现 {formatDate(p.last_seen)}</span>
    </div>
  </li>
{/snippet}

<aside class="sidebar">
  <!-- 立体竖排页签(冒烟反馈):贴侧栏左缘,文件夹式——选中页签与内容面板同底、
       交界边线断开融为一体(凸起),未选中退后;点击即导航,选中由路由派生。 -->
  <nav class="tab-rail">
    <button
      class="vtab"
      class:active={tab === "notes"}
      onclick={() => { if (tab !== "notes") goto("/"); }}>录音记录</button
    >
    <button
      class="vtab"
      class:active={tab === "people"}
      onclick={() => { if (tab !== "people") goto("/speakers"); }}>声纹库</button
    >
  </nav>

  <div class="panel">
  <button
    class="record-btn"
    class:recording={recording.isLive}
    onclick={toggleRecording}
    disabled={recording.pending}
  >
    <span class="rec-dot" class:square={recording.isLive}></span>
    {recording.isLive ? (recording.paused ? "已暂停 · 停止" : "停止录制") : "开始录制"}
  </button>

  {#if tab === "people"}
    {#if peopleError}
      <div class="banner">{peopleError}</div>
    {/if}
    {#if people.length === 0 && !peopleError}
      <p class="hint">录一场会议,停止后本场说话人会自动出现在这里</p>
    {/if}
    <!-- 人物索引(主从结构的"主"):点击进主区详情页;待命名是待处理项排上面,
         与旧管理页分区语义一致。行内无操作,管理动作全在详情页。 -->
    <ul class="list">
      {#if peopleUnnamed.length > 0}
        <li class="group-label">待命名</li>
        {#each peopleUnnamed as p (p.id)}
          {@render personRow(p)}
        {/each}
      {/if}
      {#if peopleNamed.length > 0}
        <li class="group-label">已命名</li>
        {#each peopleNamed as p (p.id)}
          {@render personRow(p)}
        {/each}
      {/if}
    </ul>
  {:else}
  <input class="search" type="search" placeholder="按标题过滤…" bind:value={query} />

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if filtered.length === 0}
    <p class="hint">{notes.length === 0 ? "还没有笔记" : "没有匹配的笔记"}</p>
  {/if}

  <ul class="list">
    {#each filtered as n (n.id)}
      <!-- 行内 .title 锚点已提供键盘路径(Tab+Enter),li 的 onclick 是指针便利层 -->
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
      <li
        class="item"
        class:current={$page.url.pathname === `/notes/${n.id}`}
        onclick={(e) => rowClick(e, n)}
        oncontextmenu={(e) => openMenu(e, n.id)}
      >
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
      </li>
    {/each}
  </ul>
  {/if}

  <!-- 设置沉底常驻(冒烟确认位置);声纹库已升级为页签,footer 只剩工具入口。 -->
  <nav class="nav-footer">
    <a class="nav-link" class:current={$page.url.pathname === "/settings"} href="/settings">
      <svg class="nav-icon" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round">
        <circle cx="8" cy="8" r="2.2" />
        <path d="M8 2.2V4.4 M8 11.6V13.8 M2.2 8H4.4 M11.6 8H13.8 M3.9 3.9L5.5 5.5 M10.5 10.5L12.1 12.1 M3.9 12.1L5.5 10.5 M10.5 5.5L12.1 3.9" />
      </svg>
      设置
    </a>
  </nav>
  </div>
</aside>

{#if menuForId}
  {@const menuNote = notes.find((n) => n.id === menuForId)}
  <!-- 点击任意处关闭;键盘路径由 svelte:window 的 Esc 承担,遮罩是纯指针便利层 -->
  <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
  <div class="menu-overlay" onclick={closeMenu} oncontextmenu={(e) => { e.preventDefault(); closeMenu(); }}></div>
  <div class="ctx-menu" style="left: {menuX}px; top: {menuY}px">
    {#if confirmingDeleteId === menuForId}
      <button
        class="ctx-item danger"
        onclick={() => {
          const id = menuForId!;
          closeMenu();
          confirmDelete(id);
        }}>确认删除「{menuNote?.title ?? ""}」</button
      >
      <button class="ctx-item" onclick={closeMenu}>取消</button>
    {:else}
      <button
        class="ctx-item"
        onclick={() => {
          if (menuNote) beginRename(menuNote);
          menuForId = null;
        }}>改名</button
      >
      <button class="ctx-item danger" onclick={() => (confirmingDeleteId = menuForId)}>删除</button>
    {/if}
  </div>
{/if}

<svelte:window onkeydown={(e) => { if (e.key === "Escape" && menuForId) closeMenu(); }} />

<style>
  /* sidebar 组件规范：surface 底 + 右侧发丝线，条目 rounded-md、hover surface-soft、
     当前页 surface-press + ink 主色（层级靠亮度对比，不靠加粗）。 */
  /* 侧栏 = 页签轨道(canvas 底) + 内容面板(surface 底)双列:面板比轨道亮一档,
     选中页签借面板底色"长"在轨道上,立体感来自表面阶梯而非投影。 */
  .sidebar {
    width: 300px;
    flex-shrink: 0;
    display: flex;
    flex-direction: row;
    border-right: 1px solid var(--hairline);
    background: var(--canvas);
    box-sizing: border-box;
    overflow-y: hidden;
  }
  .tab-rail {
    width: 34px;
    flex-shrink: 0;
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding-top: 0.75rem;
  }
  /* 竖排文件夹页签:选中态与面板同底且右边线断开(margin-right 盖住面板左边线),
     页签与面板融为一体=凸起;未选中透明退后,hover 半显影。 */
  .vtab {
    writing-mode: vertical-rl;
    letter-spacing: 0.12em;
    padding: 0.8em 0.3em;
    font-size: 0.8rem;
    font-weight: 500;
    color: var(--ink-faint);
    background: transparent;
    border: 1px solid transparent;
    border-right: none;
    border-radius: var(--radius-md) 0 0 var(--radius-md);
    cursor: pointer;
  }
  .vtab:hover {
    background: var(--surface-soft);
    color: var(--ink-secondary);
  }
  .vtab.active {
    background: var(--surface);
    color: var(--ink);
    border-color: var(--hairline);
    margin-right: -1px;
    position: relative;
    z-index: 1;
  }
  .panel {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    background: var(--surface);
    border-left: 1px solid var(--hairline);
    padding: 0.75rem;
    box-sizing: border-box;
    /* 滚动收敛到 .list:footer 沉底常驻,长列表不会把设置推出视口 */
    overflow-y: hidden;
  }
  /* 录制按钮:主 CTA 药丸(primary 底 + on-primary 字 + radius-full,dark 下即白药丸)+ 红点。
     大面积强调蓝在侧栏太吵,"彩色"由红点承担——红是本产品唯一常驻彩色信号,识别度反而更高。 */
  .record-btn {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 0.5em;
    border: none;
    border-radius: var(--radius-full);
    padding: 0.55em 1em;
    font-size: 0.9rem;
    font-weight: 500;
    cursor: pointer;
    color: var(--on-primary);
    background: var(--primary);
    box-shadow: var(--shadow-btn);
  }
  .record-btn:hover {
    background: var(--primary-pressed);
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
  /* 录制中红字于药丸:dark 下白药丸上 #ff6161 实测 2.94:1 偏低,由旁侧红色方块符号
     独立承担停止语义兜底,冒烟观察;light 下黑药丸上同色 5.98:1 无虞。两主题均保留
     record 字色。 */
  .record-btn.recording {
    color: var(--record);
    font-weight: 500;
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
    font-weight: 500;
  }
  /* 人物行:小色点(与详情页头像同色源)+ 名字/最近出现;点击进主区详情(主从结构),
     hover/选中与笔记行同语义 */
  .item.person {
    display: flex;
    align-items: center;
    gap: 0.55em;
  }
  /* 分组标签:待命名/已命名,与详情域分区语义一致;非交互,安静小字 */
  .group-label {
    list-style: none;
    color: var(--ink-faint);
    font-size: 0.75rem;
    font-weight: 500;
    padding: 0.55rem 0.5rem 0.2rem;
  }
  .dot {
    width: 10px;
    height: 10px;
    border-radius: var(--radius-full);
    flex-shrink: 0;
  }
  .title.unnamed {
    color: var(--ink-faint);
    font-weight: 400;
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
    flex: 1;
    min-height: 0; /* flex 子项默认 min-height:auto 会撑破容器,收掉才滚得起来 */
    overflow-y: auto;
  }
  /* 底部工具区:单行两列工具条(Raycast 式 status bar)。竖排两行显松散零碎,
     与顶部紧凑药丸不成体系;并排居中让两个次级入口成组且只占一行高。 */
  .nav-footer {
    margin-top: 0.5rem;
    padding-top: 0.5rem;
    border-top: 1px solid var(--hairline);
    display: flex;
    flex-direction: row;
    gap: 2px;
    flex-shrink: 0;
  }
  .nav-footer .nav-link {
    flex: 1;
    justify-content: center;
  }
  /* 整行可点(冒烟反馈):cursor 表意,操作走右键菜单,行内无常驻按钮 */
  .item {
    padding: 0.55rem 0.5rem;
    border-radius: var(--radius-md);
    cursor: pointer;
  }
  .item:hover {
    background: var(--surface-soft);
  }
  .item.current {
    background: var(--surface-press);
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
    font-weight: 500;
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
    font-weight: 500;
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
  /* 录制中：record 是双主题一致的常驻彩色信号，白字于红底（暗色同值同白）。 */
  .state.active {
    background: var(--record);
    color: var(--on-record);
  }
  /* 右键菜单:popover 规范(surface-press 底 + hairline + shadow-popover);
     暗色下 canvas 比承载面更黑,浮层若用 canvas 会成"洞",故底走 surface-press。
     透明遮罩承接"点击别处关闭",fixed 定位跟随鼠标坐标。 */
  .menu-overlay {
    position: fixed;
    inset: 0;
    z-index: 40;
  }
  .ctx-menu {
    position: fixed;
    z-index: 41;
    min-width: 9rem;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 4px;
    display: flex;
    flex-direction: column;
  }
  .ctx-item {
    background: none;
    border: none;
    text-align: left;
    color: var(--ink);
    cursor: pointer;
    padding: 0.4em 0.7em;
    border-radius: var(--radius-md);
    font-size: 0.88rem;
  }
  .ctx-item:hover {
    background: var(--surface-soft);
  }
  .ctx-item.danger {
    color: var(--danger);
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
