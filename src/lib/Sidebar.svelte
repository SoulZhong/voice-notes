<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { invoke } from "@tauri-apps/api/core";
  import { ask } from "@tauri-apps/plugin-dialog";
  import { onNoteRenamed } from "$lib/events";
  import { recording } from "$lib/recording.svelte";
  import { recordRiskGate } from "$lib/recordRisk.svelte";
  import { playback } from "$lib/playback.svelte";
  import { noteBadgeKind, type NoteBadgeKind } from "$lib/noteBadge";
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
  import { filterPeople, sortPeopleAlpha, sortPeopleBySamples } from "$lib/personPick";
  import { tidy } from "$lib/tidy.svelte";
  import { buildTidyQueue, splitArchive } from "$lib/tidyQueue";
  import { listHooks, hooks as hooksStore, type HookCfg, HOOK_EVENTS } from "$lib/hooks.svelte";
  import { graphEntities, kindLabel, kindInk, type EntitySummary } from "$lib/graph";
  import { graphFilter } from "$lib/graphFilter.svelte";
  import { noteGraphState } from "$lib/noteGraph.svelte";
  import { t } from "$lib/i18n/index.svelte";

  let notes = $state<NoteSummary[]>([]);
  let query = $state("");
  let error = $state("");

  // 页签完全由路由派生(点击=导航,零独立状态):/speakers 域=会议搭子,/hooks 域=钩子,
  // /ai 域=AI,/settings=设置,其余(笔记/录制)=录音记录。
  const tab = $derived(
    $page.url.pathname.startsWith("/speakers")
      ? "people"
      : $page.url.pathname.startsWith("/graph")
        ? "graph"
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
      peopleError = t("common.loadFailed", { e });
    }
  }

  let hookList = $state<HookCfg[]>([]);
  let hooksError = $state("");

  async function refreshHooks() {
    try {
      hookList = await listHooks();
      hooksError = "";
    } catch (e) {
      hooksError = t("common.loadFailed", { e });
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
  // 整理收件箱由 layout 全局驱动(见 +layout.svelte),这里不再重复 refresh。
  $effect(() => {
    void recording.peopleVersion;
    if (tab === "people") {
      refreshPeople();
    }
  });

  // ── 图谱:实体与文章两种视角共享搜索；治理和关系筛选留在画布渐进展开。──
  let graphEnts = $state<EntitySummary[]>([]);
  let graphDrawerOpen = $state(false);
  const graphNotes = $derived(noteGraphState.data.nodes);

  $effect(() => {
    if (tab !== "graph") graphDrawerOpen = false;
  });

  async function refreshGraphEntities() {
    try {
      graphEnts = await graphEntities();
    } catch {
      graphEnts = [];
    }
  }
  $effect(() => {
    if (tab !== "graph") return;
    if (graphFilter.mode === "note") {
      if (noteGraphState.status === "idle") void noteGraphState.load();
    } else {
      void refreshGraphEntities();
    }
  });

  const graphKinds = $derived.by(() => {
    const c = new Map<string, number>();
    for (const e of graphEnts) c.set(e.kind, (c.get(e.kind) ?? 0) + 1);
    return [...c.entries()].sort((a, b) => b[1] - a[1]).map(([k]) => k);
  });
  const graphShown = $derived(
    graphEnts.filter((e) => {
      if (graphFilter.kind !== "all" && e.kind !== graphFilter.kind) return false;
      const q = graphFilter.query.trim().toLowerCase();
      if (!q) return true;
      return e.name.toLowerCase().includes(q) || e.aliases.some((a) => a.toLowerCase().includes(q));
    }),
  );
  const graphNotesShown = $derived(
    graphNotes.filter((note) => {
      const q = graphFilter.query.trim().toLowerCase();
      return !q || note.name.toLowerCase().includes(q);
    }),
  );
  const graphSelected = $derived($page.url.searchParams.get("e"));
  // 图谱实体统一留在关系地图的附着式检查器，不因人物类型把探索上下文切走。
  function entityHref(e: EntitySummary): string {
    return "/graph?e=" + encodeURIComponent(e.id);
  }

  // 人物字母序(拼音/编号数值序);待命名是待处理项排上面,组内各自有序。
  const peopleSorted = $derived(sortPeopleAlpha(people));
  let peopleQuery = $state("");
  // 切出人物页签时清空搜索词,回来不留上次的过滤态。
  $effect(() => {
    if (tab !== "people") peopleQuery = "";
  });
  const peopleFiltered = $derived(filterPeople(peopleSorted, peopleQuery));
  const peopleUnnamed = $derived(peopleFiltered.filter((p) => !p.name));
  // 已命名组:样本份数降序、最近样本收录时间降序(2026-08-30 用户要求);未命名组仍按编号。
  const peopleNamed = $derived(sortPeopleBySamples(peopleFiltered.filter((p) => p.name)));

  /** 「概览与整理」徽标:待拍板的活(有效回执+建议+同名组+无样本);失效存档不计。 */
  const tidyBadge = $derived(
    splitArchive(buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed)).pending.length,
  );
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
      error = t("common.loadFailed", { e });
    }
  }

  // 挂载时 + 录制状态翻转时 + 笔记改名/删除时刷新列表（新笔记出现/徽章变化/标题变化）。
  $effect(() => {
    void recording.statusVersion;
    void recording.notesVersion;
    refresh();
  });

  // 后端自动改名(LLM 主题标题)发生在 Aing 后台线程,前端版本号不会变,靠事件刷新。
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
      try {
        await recording.stop(); // 跳详情由全局 status 监听驱动
      } catch (err) {
        console.error("停止录制失败", err); // stop() 失败已回滚状态,这里只记日志防未处理拒绝
      }
    } else {
      // 与录制页同一个门:确认不通过就不开录,也不跳页。
      if (!(await recordRiskGate.guard())) return;
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
      error = t("shell.renameFailed", { e });
    }
  }

  /// 删除走系统原生确认对话框(plugin-dialog):平台惯例体验,替代旧的
  /// 「菜单原地变形成确认项」自造交互(冒烟反馈:不符合正常预期)。
  async function confirmDelete(id: string, title: string) {
    const yes = await ask(t("shell.deleteConfirm.message", { title }), {
      title: t("shell.deleteConfirm.title"),
      kind: "warning",
      okLabel: t("shell.deleteConfirm.ok"),
      cancelLabel: t("shell.deleteConfirm.cancel"),
    });
    if (!yes) return;
    // 删除前必须先停:音轨是 mmap,Unix 上已删文件会继续播,Windows 上活动映射
    // 还可能让删除本身失败。用 noteId 比对而非当前路由——可能正在设置页听着 A,
    // 同时从侧栏删掉 A,这条路径必须覆盖。
    if (playback.session?.noteId === id) {
      await invoke("player_stop", {}).catch(() => {});
      playback.clear();
    }
    try {
      await deleteNote(id);
      recording.bumpNotes();
      // 删的是当前正在看的笔记 → 回首页
      if ($page.url.pathname === `/notes/${id}`) {
        goto("/");
      }
    } catch (e) {
      error = t("common.deleteFailed", { e });
    }
  }

  // 暂停是运行时标志(不落盘),active 笔记叠加 recording.paused 显示「已暂停」,
  // 与录制按钮(下方 365 行)/实时转写页头同口径。合成逻辑抽 noteBadgeKind(带单测)。
  const badgeLabel: Record<NoteBadgeKind, () => string> = {
    active: () => t("shell.note.stateActive"),
    paused: () => t("shell.note.statePaused"),
    interrupted: () => t("shell.note.stateInterrupted"),
  };
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
      <a class="title" class:unnamed={!p.name} href="/speakers/{p.id}">{p.name || t("shell.person.unnamed", { id: p.id.replace(/^P/, "") })}</a>
      <span class="meta">{t("shell.person.lastSeen", { date: formatDate(p.last_seen) })}</span>
    </div>
  </li>
{/snippet}

<aside class="sidebar" class:graph-mode={tab === "graph"}>
  <!-- 立体竖排页签(冒烟反馈):贴侧栏左缘,文件夹式——选中页签与内容面板同底、
       交界边线断开融为一体(凸起),未选中退后;点击即导航,选中由路由派生。 -->
  <nav class="tab-rail">
    <!-- 页签点击=导航到该页签的根:已在页签内(如笔记/人物详情页)再点一次回根,
         iOS/macOS 通用的"点当前 tab 回根"模式——概览页不再是只有第一跳能到的死角。 -->
    <button
      class="vtab"
      class:active={tab === "notes"}
      onclick={() => { if ($page.url.pathname !== "/") goto("/"); }}>{t("shell.tab.notes")}</button
    >
    <button
      class="vtab"
      class:active={tab === "people"}
      onclick={() => { if ($page.url.pathname !== "/speakers") goto("/speakers"); }}>{t("shell.tab.people")}</button
    >
    <button
      class="vtab"
      class:active={tab === "graph"}
      onclick={() => { if ($page.url.pathname !== "/graph" || $page.url.search !== "") goto("/graph"); }}>{t("shell.tab.graph")}</button
    >
    {#if tab === "graph"}
      <button
        type="button"
        class="graph-drawer-toggle"
        aria-expanded={graphDrawerOpen}
        aria-controls="graph-sidebar-panel"
        onclick={() => (graphDrawerOpen = !graphDrawerOpen)}
      >{t("shell.graph.filterToggle")}</button>
    {/if}
    <a
      class="vtab"
      class:active={tab === "hooks"}
      href="/hooks"
      data-sveltekit-preload-code="eager">{t("shell.tab.hooks")}</a
    >
    <a
      class="vtab vtab-upright"
      class:active={tab === "ai"}
      href="/ai">AI</a
    >
    <button
      class="vtab"
      class:active={tab === "settings"}
      onclick={() => { if ($page.url.pathname !== "/settings") goto("/settings"); }}>{t("shell.tab.settings")}</button
    >
  </nav>

  <div id="graph-sidebar-panel" class="panel" class:drawer-open={graphDrawerOpen}>
  {#if tab === "graph"}
    <button
      type="button"
      class="graph-drawer-close"
      aria-label={t("shell.graph.closeSidebar")}
      onclick={() => (graphDrawerOpen = false)}
    >×</button>
  {/if}
  <button
    class="record-btn"
    class:recording={recording.isLive}
    onclick={toggleRecording}
    disabled={recording.pending || recording.stopping}
  >
    <span class="rec-dot" class:square={recording.isLive}></span>
    {recording.stopping ? t("shell.record.stopping") : recording.isLive ? (recording.paused ? t("shell.record.paused") : t("shell.record.stop")) : t("shell.record.start")}
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
          {t("shell.hooks.new")}
        </a>
      </li>
      {#if hookList.length === 0 && !hooksError}
        <p class="empty-hint">{t("shell.hooks.emptyHint1")}<br />{t("shell.hooks.emptyHint2")}</p>
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
              <a class="title" href="/hooks/{h.id}">{h.name || t("shell.hooks.unnamed")}</a>
              <span class="meta">{h.kind === "webhook" ? "Webhook" : t("shell.hooks.kindShell")}{h.enabled ? "" : t("shell.hooks.disabledSuffix")}</span>
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
      <p class="hint">{t("shell.people.hint")}</p>
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
            <a class="title" href="/speakers">{t("shell.people.overview")}</a>
          </div>
          {#if tidyBadge > 0}
            <span class="tidy-badge" title={t("shell.people.pendingCount", { n: tidyBadge })}>{tidyBadge}</span>
          {/if}
        </li>
        <li class="people-search-row">
          <input class="people-search" placeholder={t("shell.people.searchPlaceholder")} bind:value={peopleQuery} />
        </li>
      {/if}
      {#if peopleUnnamed.length > 0}
        <li class="group-label">{t("shell.people.groupUnnamed")}</li>
        {#each peopleUnnamed as p (p.id)}
          {@render personRow(p)}
        {/each}
      {/if}
      {#if peopleNamed.length > 0}
        <li class="group-label">{t("shell.people.groupNamed")}</li>
        {#each peopleNamed as p (p.id)}
          {@render personRow(p)}
        {/each}
      {/if}
      {#if peopleQuery.trim() && peopleUnnamed.length === 0 && peopleNamed.length === 0}
        <li class="hint people-empty">{t("shell.people.noMatch")}</li>
      {/if}
    </ul>
  {:else if tab === "graph"}
    <div class="gmode" aria-label={t("shell.graph.viewAria")}>
      <button
        class="gmode-seg"
        class:on={graphFilter.mode === "entity"}
        onclick={() => { graphFilter.mode = "entity"; if ($page.url.search) goto("/graph"); }}
      >{t("shell.graph.modeEntity")}</button>
      <button
        class="gmode-seg"
        class:on={graphFilter.mode === "note"}
        onclick={() => { graphFilter.mode = "note"; if ($page.url.search) goto("/graph"); }}
      >{t("shell.graph.modeNote")}</button>
    </div>
    <input
      class="search"
      type="search"
      name="graph-search"
      aria-label={graphFilter.mode === "note" ? t("shell.graph.searchNotes") : t("shell.graph.searchEntities")}
      placeholder={graphFilter.mode === "note" ? t("shell.graph.searchNotes") : t("shell.graph.searchEntities")}
      bind:value={graphFilter.query}
    />
    {#if graphFilter.mode === "entity" && graphFilter.query.trim()}
      <p class="search-behavior">{t("shell.graph.searchBehavior")}</p>
    {/if}
    {#if graphFilter.mode === "entity"}
      <div class="gchips">
        <button class="gchip" class:on={graphFilter.kind === "all"} onclick={() => (graphFilter.kind = "all")}>{t("shell.graph.kindAll")}</button>
        {#each graphKinds as k (k)}
          <button class="gchip" class:on={graphFilter.kind === k} onclick={() => (graphFilter.kind = k)}>
            <span class="gchip-dot" style="background: {kindInk(k)}"></span>{kindLabel(k)}
          </button>
        {/each}
      </div>
      {#if graphShown.length === 0}
        <p class="hint">{graphEnts.length === 0 ? t("shell.graph.emptyEntities") : t("shell.graph.noMatchEntities")}</p>
      {/if}
      <ul class="list">
        {#each graphShown as e (e.id)}
          <li class="entity-row">
            <a class="item entity" class:current={graphSelected === e.id} href={entityHref(e)}>
              <span class="dot" style="background: {e.is_person ? speakerColor(e.id, 'mic') : kindInk(e.kind)}"></span>
              <div class="main-line">
                <span class="title">{e.name}</span>
                <span class="meta">{kindLabel(e.kind)} · {t("shell.graph.noteCount", { n: e.note_count })}</span>
              </div>
            </a>
          </li>
        {/each}
      </ul>
    {:else}
      {#if graphNotesShown.length === 0}
        <p class="hint">{noteGraphState.status === "error" ? t("shell.graph.notesLoadFailed") : graphNotes.length === 0 ? t("shell.graph.emptyNotes") : t("shell.notes.noMatch")}</p>
      {/if}
      <ul class="list">
        {#each graphNotesShown as note (note.id)}
          <li>
            <a class="item entity" href={"/notes/" + encodeURIComponent(note.id)}>
              <span class="dot" style="background: {kindInk('note')}"></span>
              <div class="main-line">
                <span class="title">{note.name}</span>
                <span class="meta">{t("shell.graph.entityMentions", { n: note.note_count, m: note.mention_total })}</span>
              </div>
            </a>
          </li>
        {/each}
      </ul>
    {/if}
  {:else}
  <input class="search" type="search" placeholder={t("shell.notes.filterPlaceholder")} bind:value={query} />

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if filtered.length === 0}
    <p class="hint">{notes.length === 0 ? t("shell.notes.empty") : t("shell.notes.noMatch")}</p>
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
            {@const kind = noteBadgeKind(n.state, recording.paused)}
            <a class="title" href={n.state === "active" ? "/record" : `/notes/${n.id}`}>
              {n.title}
              {#if kind}
                <span
                  class="state"
                  class:interrupted={kind === "interrupted"}
                  class:active={kind === "active"}
                  class:paused={kind === "paused"}
                >
                  {badgeLabel[kind]()}
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

{#if tab === "graph" && graphDrawerOpen}
  <button
    type="button"
    class="graph-drawer-scrim"
    aria-label={t("shell.graph.closeSidebar")}
    onclick={() => (graphDrawerOpen = false)}
  ></button>
{/if}

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
      }}>{t("shell.menu.rename")}</button
    >
    <button
      class="ctx-item danger"
      onclick={() => {
        const id = menuForId!;
        const title = menuNote?.title ?? "";
        closeMenu();
        confirmDelete(id, title);
      }}>{t("shell.menu.delete")}</button
    >
  </div>
{/if}

<svelte:window onkeydown={(e) => {
  if (e.key !== "Escape") return;
  if (menuForId) closeMenu();
  else if (graphDrawerOpen) graphDrawerOpen = false;
}} />

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
  .graph-drawer-toggle,
  .graph-drawer-close,
  .graph-drawer-scrim { display: none; }
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
    box-sizing: border-box;
    display: flex;
    align-items: center;
    justify-content: center;
    width: 100%;
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
    text-decoration: none;
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
     hover/选中与笔记行同语义。图谱实体行(.entity)是同一形态——之前漏了这条 flex
     规则,色点(.dot)没有行内布局撑不开,视觉上完全不可见,现在一并补上。 */
  .item.person,
  .item.entity {
    display: flex;
    align-items: center;
    gap: 0.55em;
  }
  a.item.entity {
    color: inherit;
    text-decoration: none;
  }
  .entity-row { position: relative; list-style: none; }
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
  /* 搭子搜索行:非交互 li 壳,输入框本体照 SpeakerChips .panel-input 起个带边框态──
     发丝边 + surface 底 + radius-md,列表滚动区顶部,概览行之下。 */
  .people-search-row {
    list-style: none;
    padding: 0 0.1rem 0.4rem;
  }
  .people-search {
    box-sizing: border-box;
    width: 100%;
    padding: 0.4em 0.6em;
    border: 1px solid var(--hairline);
    border-radius: var(--radius-md);
    background: var(--surface);
    color: var(--ink);
    font: inherit;
    font-size: 0.82rem;
  }
  .people-search::placeholder {
    color: var(--ink-faint);
  }
  .people-search:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
  }
  .people-empty {
    list-style: none;
    padding: 0.5rem 0.5rem;
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
  .search:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    background: var(--canvas);
    border-color: var(--accent);
  }
  .search-behavior { margin: -0.45rem 0 0.6rem; color: var(--ink-faint); font-size: 0.7rem; line-height: 1.45; }
  .gmode {
    display: flex;
    gap: 2px;
    margin: 0.6rem 0 0.5rem;
    padding: 2px;
    border-radius: var(--radius-md);
    background: var(--surface-press);
  }
  .gmode-seg {
    flex: 1;
    min-height: 30px;
    border: 0;
    border-radius: calc(var(--radius-md) - 2px);
    background: transparent;
    color: var(--ink-secondary);
    font: inherit;
    font-size: 0.8em;
    font-weight: 500;
    cursor: pointer;
  }
  .gmode-seg.on { background: var(--surface); color: var(--ink); box-shadow: var(--shadow-btn); }
  /* 图谱 kind 过滤药丸(侧栏窄,紧凑换行) */
  .gchips {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-bottom: 0.5rem;
  }
  .gchip {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    padding: 2px 9px;
    border-radius: 999px;
    font-size: 0.72em;
    font-weight: 500;
    background: var(--surface-press);
    color: var(--ink-secondary);
    border: 1px solid transparent;
    cursor: pointer;
  }
  .gchip.on {
    background: var(--accent-tint);
    color: var(--accent);
  }
  /* kind 色点(kindInk):选中态仍统一走 accent 高亮(哪个药丸被选清清楚楚),
     色点只在未选中时充当"这是什么类别"的视觉索引,不跟 accent 抢语义。 */
  .gchip-dot {
    width: 6px;
    height: 6px;
    border-radius: 999px;
    flex: none;
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
    overflow-wrap: anywhere;
  }
  /* 图谱与治理内容始终保留完整名称；窄侧栏通过换行承担长度，而不是截断。 */
  .item.entity .title {
    white-space: normal;
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
  /* 已暂停：录制的中性挂起态，退为灰底次级墨——与实时转写页头的灰点「已暂停」
     同语义降调，红底只留给真正在录的信号。 */
  .state.paused {
    background: var(--surface-press);
    color: var(--ink-secondary);
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
  @media (max-width: 700px) {
    .sidebar.graph-mode {
      width: 44px;
      overflow: visible;
    }
    .sidebar.graph-mode .tab-rail { width: 44px; position: relative; z-index: 37; }
    .sidebar.graph-mode .panel {
      position: fixed;
      z-index: 36;
      top: 0;
      bottom: 0;
      left: 44px;
      width: min(280px, calc(100vw - 60px));
      transform: translateX(calc(-100% - 44px));
      visibility: hidden;
      pointer-events: none;
      border-right: 1px solid var(--hairline-strong);
      box-shadow: var(--shadow-popover);
      transition:
        transform 240ms cubic-bezier(0.16, 1, 0.3, 1),
        visibility 0s linear 240ms;
    }
    .sidebar.graph-mode .panel.drawer-open {
      transform: translateX(0);
      visibility: visible;
      pointer-events: auto;
      transition-delay: 0s;
    }
    .graph-drawer-toggle {
      display: grid;
      place-items: center;
      width: 44px;
      height: 44px;
      box-sizing: border-box;
      margin: 4px 0;
      padding: 7px 3px;
      border: 1px solid var(--hairline-strong);
      border-radius: var(--radius-md);
      background: var(--surface);
      color: var(--accent);
      font: inherit;
      font-size: 0.72rem;
      writing-mode: vertical-rl;
      cursor: pointer;
    }
    .graph-drawer-close {
      display: grid;
      place-items: center;
      align-self: flex-end;
      width: 44px;
      min-height: 44px;
      margin: -4px -4px 2px 0;
      padding: 0;
      border: 0;
      border-radius: var(--radius-full);
      background: transparent;
      color: var(--ink-secondary);
      font: inherit;
      font-size: 1.25rem;
      cursor: pointer;
    }
    .graph-drawer-scrim {
      position: fixed;
      z-index: 35;
      inset: 0 0 0 44px;
      display: block;
      padding: 0;
      border: 0;
      background: color-mix(in srgb, var(--canvas) 34%, transparent);
      cursor: default;
    }
  }
  @media (pointer: coarse) {
    .sidebar.graph-mode .panel button,
    .sidebar.graph-mode .panel input { min-height: 44px; }
    .sidebar.graph-mode .panel button,
    .sidebar.graph-mode .panel .gchip { min-inline-size: 44px; }
  }
  @media (prefers-reduced-motion: reduce) {
    .sidebar.graph-mode .panel { transition: none; }
  }
</style>
