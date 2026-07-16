<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { ask } from "@tauri-apps/plugin-dialog";
  import { onNoteRenamed } from "$lib/events";
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
  import { tidy } from "$lib/tidy.svelte";
  import { listHooks, hooks as hooksStore, type HookCfg, HOOK_EVENTS } from "$lib/hooks.svelte";

  let notes = $state<NoteSummary[]>([]);
  let query = $state("");
  let error = $state("");

  // 页签完全由路由派生(点击=导航,零独立状态):/speakers 域=会议搭子,/hooks 域=钩子,
  // /ai 域=AI,/settings=设置,其余(笔记/录制)=录音记录。
  const tab = $derived(
    $page.url.pathname.startsWith("/speakers")
      ? "people"
      : $page.url.pathname.startsWith("/hooks")
        ? "hooks"
        : $page.url.pathname.startsWith("/ai")
          ? "ai"
          : $page.url.pathname === "/settings"
            ? "settings"
            : "notes",
  );

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

  let hookList = $state<HookCfg[]>([]);
  let hooksError = $state("");

  async function refreshHooks() {
    try {
      hookList = await listHooks();
      hooksError = "";
    } catch (e) {
      hooksError = `加载失败: ${e}`;
    }
  }

  // 切到钩子页签时拉取;编辑页保存/删除后经 version 触发重拉(与 peopleVersion 同套路)。
  $effect(() => {
    void hooksStore.version;
    if (tab === "hooks") refreshHooks();
  });

  /** 按事件分组(有配置的事件才出组,组序=白名单序)。 */
  const hookGroups = $derived(
    HOOK_EVENTS.map((e) => ({ ...e, items: hookList.filter((h) => h.event === e.value) })).filter(
      (g) => g.items.length > 0,
    ),
  );

  // 切到声纹库页签时拉取;详情页改名/合并/删除后经 peopleVersion 触发重拉,索引不滞留旧名。
  // 整理建议同步重算:「概览与整理」徽标要跟库同步,不能挂着旧数。
  $effect(() => {
    void recording.peopleVersion;
    if (tab === "people") {
      refreshPeople();
      tidy.refresh();
    }
  });

  // 与详情页同一套排序/分组语义:最近出现在前;待命名是待处理项排上面。
  const peopleSorted = $derived(
    [...people].sort((a, b) => (b.last_seen || "").localeCompare(a.last_seen || "")),
  );
  const peopleUnnamed = $derived(peopleSorted.filter((p) => !p.name));
  const peopleNamed = $derived(peopleSorted.filter((p) => p.name));

  /** 同名分组数(疑似重复):与概览页同一判定,计入徽标。 */
  const dupGroupCount = $derived.by(() => {
    const seen = new Set<string>();
    const dup = new Set<string>();
    for (const p of people) {
      if (!p.name) continue;
      if (seen.has(p.name)) dup.add(p.name);
      seen.add(p.name);
    }
    return dup.size;
  });
  /** 「概览与整理」徽标:待办数=可归属建议 + 疑似重复组。0 不显示。 */
  const tidyBadge = $derived(tidy.visible.length + dupGroupCount);
  let editingId = $state<string | null>(null);
  let editingTitle = $state("");
  // 右键菜单(冒烟反馈:改名/删除从行内挪进 context menu,列表不再有常驻操作行)
  let menuForId = $state<string | null>(null);
  let menuX = $state(0);
  let menuY = $state(0);
  let menuEl = $state<HTMLElement | null>(null);
  // 视口钳制:菜单在光标处展开,靠近右/下缘时整体收回视口内(原生菜单惯例),
  // 渲染后按实测尺寸修正一次。
  $effect(() => {
    if (!menuEl) return;
    const r = menuEl.getBoundingClientRect();
    if (r.right > window.innerWidth - 8) menuX = Math.max(8, window.innerWidth - 8 - r.width);
    if (r.bottom > window.innerHeight - 8) menuY = Math.max(8, window.innerHeight - 8 - r.height);
  });

  function openMenu(e: MouseEvent, id: string) {
    e.preventDefault();
    menuForId = id;
    menuX = e.clientX;
    menuY = e.clientY;
  }

  function closeMenu() {
    menuForId = null;
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

  // 后端自动改名(LLM 主题标题)发生在精修后台线程,前端版本号不会变,靠事件刷新。
  $effect(() => {
    let un: (() => void) | null = null;
    let disposed = false;
    onNoteRenamed(() => refresh()).then((u) => {
      if (disposed) u();
      else un = u;
    });
    return () => {
      disposed = true;
      un?.();
    };
  });

  async function toggleRecording() {
    if (recording.isLive) {
      await recording.stop(); // 跳详情由全局 status 监听驱动
    } else {
      await recording.start();
      // 无论成败都进录制页:失败时错误状态与模型下载卡只在录制页渲染,
      // 留在原地会表现为"点了没反应"(模型缺失场景实测踩坑)。
      goto("/record");
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

  /// 删除走系统原生确认对话框(plugin-dialog):平台惯例体验,替代旧的
  /// 「菜单原地变形成确认项」自造交互(冒烟反馈:不符合正常预期)。
  async function confirmDelete(id: string, title: string) {
    const yes = await ask(`「${title}」的转写与录音将一并删除，此操作不可恢复。`, {
      title: "删除笔记",
      kind: "warning",
      okLabel: "删除",
      cancelLabel: "取消",
    });
    if (!yes) return;
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
      <a class="title" class:unnamed={!p.name} href="/speakers/{p.id}">{p.name || `说话人 ${p.id.replace(/^P/, "")}`}</a>
      <span class="meta">最近出现 {formatDate(p.last_seen)}</span>
    </div>
  </li>
{/snippet}

<aside class="sidebar">
  <!-- 立体竖排页签(冒烟反馈):贴侧栏左缘,文件夹式——选中页签与内容面板同底、
       交界边线断开融为一体(凸起),未选中退后;点击即导航,选中由路由派生。 -->
  <nav class="tab-rail">
    <!-- 页签点击=导航到该页签的根:已在页签内(如笔记/人物详情页)再点一次回根,
         iOS/macOS 通用的"点当前 tab 回根"模式——概览页不再是只有第一跳能到的死角。 -->
    <button
      class="vtab"
      class:active={tab === "notes"}
      onclick={() => { if ($page.url.pathname !== "/") goto("/"); }}>录音</button
    >
    <button
      class="vtab"
      class:active={tab === "people"}
      onclick={() => { if ($page.url.pathname !== "/speakers") goto("/speakers"); }}>会议搭子</button
    >
    <button
      class="vtab"
      class:active={tab === "hooks"}
      onclick={() => { if ($page.url.pathname !== "/hooks") goto("/hooks"); }}>钩子</button
    >
    <button
      class="vtab vtab-upright"
      class:active={tab === "ai"}
      onclick={() => { if (!$page.url.pathname.startsWith("/ai")) goto("/ai"); }}>AI</button
    >
    <button
      class="vtab"
      class:active={tab === "settings"}
      onclick={() => { if ($page.url.pathname !== "/settings") goto("/settings"); }}>设置</button
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

  {#if tab === "hooks"}
    {#if hooksError}
      <div class="banner">{hooksError}</div>
    {/if}
    <ul class="list">
      <!-- 固定入口:新建钩子——虚线「添加」按钮,与下方钩子列表行明确区分 -->
      <li class="new-hook-row">
        <a class="new-hook" class:current={$page.url.pathname === "/hooks/new"} href="/hooks/new">
          <svg class="new-hook-icon" width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true">
            <path d="M8 3.5v9M3.5 8h9" />
          </svg>
          新建钩子
        </a>
      </li>
      {#if hookList.length === 0 && !hooksError}
        <p class="empty-hint">事件发生时自动执行命令或调用接口<br />先新建一条试试</p>
      {/if}
      {#each hookGroups as g (g.value)}
        <li class="group-label">{g.label}</li>
        {#each g.items as h (h.id)}
          <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
          <li
            class="item hook"
            class:off={!h.enabled}
            class:current={$page.url.pathname === `/hooks/${h.id}`}
            onclick={(e) => {
              if ((e.target as HTMLElement).closest("a")) return;
              goto(`/hooks/${h.id}`);
            }}
          >
            <div class="main-line">
              <a class="title" href="/hooks/{h.id}">{h.name || "未命名钩子"}</a>
              <span class="meta">{h.kind === "webhook" ? "Webhook" : "Shell 命令"}{h.enabled ? "" : " · 已停用"}</span>
            </div>
          </li>
        {/each}
      {/each}
    </ul>
  {:else if tab === "people"}
    {#if peopleError}
      <div class="banner">{peopleError}</div>
    {/if}
    {#if people.length === 0 && !peopleError}
      <p class="hint">录一场会议,停止后本场说话人会自动出现在这里</p>
    {/if}
    <!-- 人物索引(主从结构的"主"):点击进主区详情页;待命名是待处理项排上面,
         与旧管理页分区语义一致。行内无操作,管理动作全在详情页。 -->
    <ul class="list">
      <!-- 固定行:概览与整理(库级功能的常驻入口,不随人物列表滚没)。徽标=可归属
           建议+疑似重复组的待办数,像收件箱未读——有活儿要干时主动提示。 -->
      {#if people.length > 0}
        <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
        <li
          class="item overview"
          class:current={$page.url.pathname === "/speakers"}
          onclick={(e) => {
            if ((e.target as HTMLElement).closest("a")) return;
            goto("/speakers");
          }}
        >
          <svg class="overview-icon" width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <rect x="2" y="2.5" width="12" height="4" rx="1.2" />
            <rect x="2" y="9.5" width="7" height="4" rx="1.2" />
            <path d="M11.5 11.5h2.5M12.75 10.25v2.5" />
          </svg>
          <div class="main-line">
            <a class="title" href="/speakers">概览与整理</a>
          </div>
          {#if tidyBadge > 0}
            <span class="tidy-badge" title="{tidy.visible.length} 条归属建议 · {dupGroupCount} 组同名">{tidyBadge}</span>
          {/if}
        </li>
      {/if}
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

  </div>
</aside>

{#if menuForId}
  {@const menuNote = notes.find((n) => n.id === menuForId)}
  <!-- 点击任意处关闭;键盘路径由 svelte:window 的 Esc 承担,遮罩是纯指针便利层 -->
  <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
  <div class="menu-overlay" onclick={closeMenu} oncontextmenu={(e) => { e.preventDefault(); closeMenu(); }}></div>
  <div class="ctx-menu" bind:this={menuEl} style="left: {menuX}px; top: {menuY}px">
    <button
      class="ctx-item"
      onclick={() => {
        if (menuNote) beginRename(menuNote);
        closeMenu();
      }}>改名</button
    >
    <button
      class="ctx-item danger"
      onclick={() => {
        const id = menuForId!;
        const title = menuNote?.title ?? "";
        closeMenu();
        confirmDelete(id, title);
      }}>删除</button
    >
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
  /* 拉丁标签(AI):竖排会把字母放倒或上下堆叠,改横排让「AI」两字母同行并排 */
  .vtab-upright {
    writing-mode: horizontal-tb;
    letter-spacing: 0.04em;
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
  /* 人物行:小色点(与详情页头像同色源)+ 名字/最近出现;点击进主区详情(主从结构),
     hover/选中与笔记行同语义 */
  .item.person {
    display: flex;
    align-items: center;
    gap: 0.55em;
  }
  /* 概览与整理固定行:与人物行同形态,图标代色点;徽标=待办数(warning 色药丸) */
  /* 新建钩子:本页的主操作入口——实心 accent 按钮,与上方录制药丸拉开间距、
     并以蓝色区分红点录制;虚线/幽灵态表达太弱,看不出这是入口。 */
  .new-hook-row { list-style: none; }
  .new-hook {
    display: flex;
    align-items: center;
    justify-content: center;
    gap: 0.45em;
    margin-top: 0.9rem;
    padding: 0.55em 1em;
    border-radius: var(--radius-full);
    background: var(--accent);
    color: var(--on-accent);
    text-decoration: none;
    font-size: 0.9rem;
    font-weight: 600;
    box-shadow: var(--shadow-btn);
    transition: background 0.12s;
  }
  .new-hook-icon { flex: none; }
  .new-hook:hover { background: var(--accent-pressed); }
  .new-hook.current { background: var(--accent-pressed); }
  .empty-hint {
    color: var(--ink-faint);
    font-size: 0.82em;
    line-height: 1.5;
    text-align: center;
    padding: 0.9rem 0.5rem 0;
    margin: 0;
  }
  .item.overview {
    display: flex;
    align-items: center;
    gap: 0.55em;
  }
  .overview-icon {
    color: var(--ink-faint);
    flex: none;
  }
  .item.overview.current .overview-icon,
  .item.overview:hover .overview-icon {
    color: var(--ink-secondary);
  }
  .tidy-badge {
    margin-left: auto;
    flex: none;
    min-width: 1.3em;
    text-align: center;
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    font-size: 0.72rem;
    font-weight: 500;
    border-radius: var(--radius-full);
    padding: 0.05em 0.45em;
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

  /* 钩子行:禁用的整行淡显,一眼分辨在岗/停用 */
  .item.hook.off .title,
  .item.hook.off .meta {
    color: var(--ink-faint);
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
