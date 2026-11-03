<script lang="ts">
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { invoke } from "@tauri-apps/api/core";
  import { ask, open } from "@tauri-apps/plugin-dialog";
  import { onNoteRenamed } from "$lib/events";
  import { recording } from "$lib/recording.svelte";
  import { recordRiskGate } from "$lib/recordRisk.svelte";
  import { playback } from "$lib/playback.svelte";
  import { noteBadgeKind, type NoteBadgeKind } from "$lib/noteBadge";
  import {
    listNotes,
    renameNote,
    deleteNote,
    importAudio,
    IMPORT_EXTS,
    formatDate,
    formatDuration,
    speakerColor,
    type NoteSummary,
  } from "$lib/notes";
  import { listPeople, type PersonSummary } from "$lib/people";
  import { filterPeople, sortPeopleAlpha, sortPeopleBySamples } from "$lib/personPick";
  import { fmtClock, searchMemos, type MemoHit } from "$lib/memos";
  import { tidy } from "$lib/tidy.svelte";
  import { buildTidyQueue, splitArchive } from "$lib/tidyQueue";
  import { listHooks, hooks as hooksStore, type HookCfg, HOOK_EVENTS } from "$lib/hooks.svelte";
  import { graphEntities, kindLabel, kindInk, type EntitySummary } from "$lib/graph";
  import { graphFilter } from "$lib/graphFilter.svelte";
  import { noteGraphState } from "$lib/noteGraph.svelte";
  import { i18n, t } from "$lib/i18n/index.svelte";
  import { loadUiMode, showDeviceTab, showHooksTab } from "$lib/uiMode.svelte";
  import {
    listDictationNotes,
    deleteDictationNote,
    onDictationNotesChanged,
    type DictationSummary,
  } from "$lib/device";
  import XIcon from "@lucide/svelte/icons/x";
  import { Button } from "$lib/ui/components/button";
  import { Input } from "$lib/ui/components/input";
  import * as ContextMenu from "$lib/ui/components/context-menu";
  import { cn } from "$lib/ui/utils";

  let notes = $state<NoteSummary[]>([]);
  /** 听写笔记(设备听写按会话归档):与会议笔记同列,按最后一次听写的时间插入。 */
  let dictations = $state<DictationSummary[]>([]);
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
            : $page.url.pathname === "/device" || $page.url.pathname.startsWith("/dictations")
              ? "device"
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

  /** 「概览与整理」徽标:待拍板的活(有效回执+身份线索+建议+同名组+无样本);失效存档不计。
      身份线索与 /speakers 页同一队列(此前漏计:页面上有卡、徽标却是 0)。 */
  const tidyBadge = $derived(
    splitArchive(
      buildTidyQueue(people, tidy.visible, tidy.receipts, tidy.dismissed, tidy.visibleIdentify),
    ).pending.length,
  );
  let editingId = $state<string | null>(null);
  let editingTitle = $state("");
  // 右键菜单(冒烟反馈:改名/删除从行内挪进 context menu,列表不再有常驻操作行)。
  // UI 改版:换成 ContextMenu 组件——整张列表共用一个触发器(不给每行各挂一个菜单实例),
  // 右键时按事件目标的 data-note-id 认行;光标处展开与视口钳制、Esc/点别处关闭由组件承担。
  let menuForId = $state<string | null>(null);
  let menuOpen = $state(false);
  /** 菜单里点了「重命名」:等菜单收起后再进改名态——否则改名框刚拿到焦点就可能被
      菜单的关闭/焦点归还抢走,blur 立即提交,看上去像「点了重命名没反应」。 */
  let renameAfterClose: NoteSummary | null = null;
  /** 关菜单时要不要把焦点还给原行:Esc 关 → 还;选「重命名」或点菜单外关 → 不还(改名框/被点处要拿焦点)。 */
  let skipRestoreFocus = false;

  function onListContextMenu(e: MouseEvent, open: unknown) {
    // 听写行自己处理右键(先 preventDefault,组件据此不开菜单);空白处不开菜单。
    const row = (e.target as HTMLElement).closest<HTMLElement>("[data-note-id]");
    if (!row) return;
    menuForId = row.dataset.noteId ?? null;
    (open as (e: MouseEvent) => void)(e);
  }

  function closeMenu() {
    menuOpen = false;
    menuForId = null;
  }

  /** 整行可点跳转;行内的按钮/输入框/链接各有己任,不劫持。 */
  function rowClick(e: MouseEvent, n: NoteSummary) {
    if ((e.target as HTMLElement).closest("button, input, a")) return;
    goto(n.state === "active" ? "/record" : `/notes/${n.id}`);
  }

  // 笔记搜索也搜会议备注(「我记得写过一句『封版』,是哪场会?」正是备注最常见的找法)。
  // 备注不在笔记列表里,另走一个只读 memos.json 的命令;输入停 250ms 再查,过期结果丢弃。
  let memoHits = $state<MemoHit[]>([]);
  let memoSeq = 0;
  $effect(() => {
    const q = query.trim();
    const seq = ++memoSeq;
    if (!q) {
      memoHits = [];
      return;
    }
    const timer = setTimeout(() => {
      searchMemos(q)
        .then((hits) => {
          if (seq === memoSeq) memoHits = hits;
        })
        .catch(() => {});
    }, 250);
    return () => clearTimeout(timer);
  });
  /** 每篇笔记的第一条命中备注(行下方露一句,说明为什么这篇被搜出来)。 */
  const memoHitByNote = $derived.by(() => {
    const m = new Map<string, MemoHit>();
    for (const h of memoHits) if (!m.has(h.note_id)) m.set(h.note_id, h);
    return m;
  });
  const filtered = $derived(
    query.trim()
      ? notes.filter(
          (n) => n.title.toLowerCase().includes(query.trim().toLowerCase()) || memoHitByNote.has(n.id),
        )
      : notes,
  );
  // 听写笔记按标签与句子文字过滤(笔记搜索搜得到听写内容)。
  const filteredDictations = $derived(
    query.trim()
      ? dictations.filter((d) => d.search_text.toLowerCase().includes(query.trim().toLowerCase()))
      : dictations,
  );
  type Row = { kind: "note"; at: string; n: NoteSummary } | { kind: "dictation"; at: string; d: DictationSummary };
  /** 两种笔记合成一列:按时间倒序(会议看开始时间,听写看最后一次听写)。 */
  const rows = $derived.by<Row[]>(() => {
    const all: Row[] = [
      // 设备页签只列听写笔记;录音页签两类同列(按时间穿插)。
      ...(tab === "device" ? [] : filtered.map((n) => ({ kind: "note" as const, at: n.started_at, n }))),
      ...filteredDictations.map((d) => ({ kind: "dictation" as const, at: d.updated_at, d })),
    ];
    return all.sort((a, b) => new Date(b.at).getTime() - new Date(a.at).getTime());
  });

  async function refresh() {
    try {
      const [n, d] = await Promise.all([listNotes(), listDictationNotes().catch(() => [])]);
      notes = n;
      dictations = d;
      error = "";
    } catch (e) {
      error = t("common.loadFailed", { e });
    }
  }

  // 「设备」菜单是否显示:挂载时按设置对齐一次(之后由设置页开关直接改)。
  $effect(() => {
    loadUiMode();
  });

  // 设备听写写下新句子/撤销时后端发事件:列表上的计数与排序随之更新。
  $effect(() => {
    let un: (() => void) | null = null;
    let disposed = false;
    onDictationNotesChanged(() => refresh()).then((u) => {
      if (disposed) u();
      else un = u;
    });
    return () => {
      disposed = true;
      un?.();
    };
  });

  /** 听写笔记右键只有删除(标签跟随会话标题,不在这里改名)。 */
  async function confirmDeleteDictation(d: DictationSummary) {
    const yes = await ask(t("device.note.deleteConfirm.message", { label: d.label }), {
      title: t("device.note.deleteConfirm.title"),
      kind: "warning",
      okLabel: t("device.note.deleteConfirm.ok"),
      cancelLabel: t("shell.deleteConfirm.cancel"),
    });
    if (!yes) return;
    try {
      await deleteDictationNote(d.id);
      await refresh();
      if ($page.url.pathname === `/dictations/${d.id}`) goto("/");
    } catch (e) {
      error = t("common.deleteFailed", { e });
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

  // 导入音频:选文件 → 后端解码建档(数秒,期间按钮转忙态)→ 跳进新笔记。
  // 转写不等——它在后台继续,新笔记页会按既有的「分析中」逻辑显示进度并在完成时自刷新
  // (导入的后半程复用重转写的槽与事件,见后端 do_import_audio 注释)。
  let importing = $state(false);

  async function importAudioFile() {
    if (importing || recording.isLive) return;
    const picked = await open({
      multiple: false,
      directory: false,
      filters: [{ name: t("shell.import.filterName"), extensions: IMPORT_EXTS }],
    });
    if (typeof picked !== "string") return; // 取消
    importing = true;
    error = "";
    try {
      const id = await importAudio(picked);
      recording.bumpNotes(); // 侧栏立刻出现这篇(此刻已有音频,还没有正文)
      goto(`/notes/${id}`);
    } catch (e) {
      error = t("shell.import.failed", { e });
    } finally {
      importing = false;
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
  // ── 控件外观(UI 改版):侧栏过滤框沿用「内嵌式」——surface-press 底、无边,聚焦才浮出
  // canvas 底 + accent 边;Notion 侧栏过滤即此形态,带边框的输入框在侧栏里比正文还抢眼。
  const searchClass =
    "my-3 h-8 border-transparent bg-accent dark:bg-accent focus-visible:bg-background dark:focus-visible:bg-background";
  /** 图谱视角分段(实体/文章):选中段抬成 surface 底 + 微投影,与原分段控件同观感。 */
  function segClass(on: boolean) {
    return cn(
      "h-7 flex-1 rounded-[calc(var(--radius-md)-2px)] text-[0.8em]",
      on
        ? "bg-card text-foreground shadow-(--shadow-btn) hover:bg-card dark:hover:bg-card"
        : "text-muted-foreground",
    );
  }
  /** 图谱 kind 过滤药丸:选中走 accent 弱底 + accent 字。 */
  function chipClass(on: boolean) {
    return cn(
      "gchip h-auto gap-1 rounded-full px-2.5 py-0.5 text-[0.72em]",
      on
        ? "bg-(color:--accent-tint) text-ring hover:bg-(color:--accent-tint) hover:text-ring dark:hover:bg-(color:--accent-tint)"
        : "bg-accent text-muted-foreground",
    );
  }

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
    {#if showHooksTab()}
      <a
        class="vtab"
        class:active={tab === "hooks"}
        href="/hooks"
        data-sveltekit-preload-code="eager">{t("shell.tab.hooks")}</a
      >
    {/if}
    <!-- AI 页签按语言分流(2026-09-18 用户点名:中英各守各的排版习惯):
         中文竖排里拉丁缩略语用縦中横正立;英文界面与其余页签一致按书脊惯例旋转 -->
    <a
      class="vtab"
      class:vtab-upright={i18n.locale === "zh"}
      class:active={tab === "ai"}
      href="/ai">AI</a
    >
    {#if showDeviceTab()}
      <button
        class="vtab"
        class:active={tab === "device"}
        onclick={() => { if ($page.url.pathname !== "/device") goto("/device"); }}>{t("shell.tab.device")}</button
      >
    {/if}
    <button
      class="vtab"
      class:active={tab === "settings"}
      onclick={() => { if ($page.url.pathname !== "/settings") goto("/settings"); }}>{t("shell.tab.settings")}</button
    >
  </nav>

  <div id="graph-sidebar-panel" class="panel" class:drawer-open={graphDrawerOpen}>
  {#if tab === "graph"}
    <Button
      variant="ghost"
      size="icon"
      class="graph-drawer-close"
      aria-label={t("shell.graph.closeSidebar")}
      onclick={() => (graphDrawerOpen = false)}
    ><XIcon /></Button>
  {/if}
  <Button
    class={cn(
      "h-9 w-full gap-2 rounded-full text-[0.9rem] shadow-(--shadow-btn) hover:bg-(color:--primary-pressed) disabled:opacity-60",
      recording.isLive && "text-(color:--record)",
    )}
    onclick={toggleRecording}
    disabled={recording.pending || recording.stopping}
    title={recording.stopping ? t("shell.record.finishingHint") : undefined}
  >
    <span class="rec-dot" class:square={recording.isLive}></span>
    {recording.stopping ? t("shell.record.start") : recording.isLive ? (recording.paused ? t("shell.record.paused") : t("shell.record.stop")) : t("shell.record.start")}
  </Button>

  <!-- 导入已有录音:录制的次级入口,常驻在录制药丸下方。刻意带文字而非纯图标——
       同样的幽灵图标钮在重转写入口上被实测"没人找到"(见笔记页 retrans-btn 注释)。
       录制中禁用:后端同样会拒(录制与转写全局互斥),这里先一步说明原因。 -->
  <Button
    variant="ghost"
    size="sm"
    class="mt-1.5 w-full rounded-full text-[0.82rem] text-muted-foreground hover:bg-muted hover:text-foreground dark:hover:bg-muted"
    onclick={importAudioFile}
    disabled={importing || recording.isLive || recording.pending}
    title={recording.isLive ? t("shell.import.busyRecording") : t("shell.import.hint")}
  >
    <svg width="14" height="14" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M8 10.5V2.5M5 5.5L8 2.5l3 3" />
      <path d="M2.5 10.5v2a1 1 0 0 0 1 1h9a1 1 0 0 0 1-1v-2" />
    </svg>
    {importing ? t("shell.import.running") : t("shell.import.action")}
  </Button>

  {#if tab === "hooks"}
    {#if hooksError}
      <div class="banner">{hooksError}</div>
    {/if}
    <ul class="list">
      <!-- 固定入口:新建钩子——虚线「添加」按钮,与下方钩子列表行明确区分 -->
      <li class="new-hook-row">
        <Button
          href="/hooks/new"
          class={cn(
            "mt-3.5 h-9 w-full gap-2 rounded-full bg-ring text-[0.9rem] font-semibold text-(color:--on-accent) shadow-(--shadow-btn) hover:bg-(color:--accent-pressed)",
            $page.url.pathname === "/hooks/new" && "bg-(color:--accent-pressed)",
          )}
        >
          <svg class="new-hook-icon" width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true">
            <path d="M8 3.5v9M3.5 8h9" />
          </svg>
          {t("shell.hooks.new")}
        </Button>
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
          <Input class="h-7 bg-card text-[0.82rem] md:text-[0.82rem]" placeholder={t("shell.people.searchPlaceholder")} bind:value={peopleQuery} />
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
      <Button
        variant="ghost"
        size="sm"
        class={segClass(graphFilter.mode === "entity")}
        aria-pressed={graphFilter.mode === "entity"}
        onclick={() => { graphFilter.mode = "entity"; if ($page.url.search) goto("/graph"); }}
      >{t("shell.graph.modeEntity")}</Button>
      <Button
        variant="ghost"
        size="sm"
        class={segClass(graphFilter.mode === "note")}
        aria-pressed={graphFilter.mode === "note"}
        onclick={() => { graphFilter.mode = "note"; if ($page.url.search) goto("/graph"); }}
      >{t("shell.graph.modeNote")}</Button>
    </div>
    <Input
      class={searchClass}
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
        <Button variant="ghost" size="xs" class={chipClass(graphFilter.kind === "all")} aria-pressed={graphFilter.kind === "all"} onclick={() => (graphFilter.kind = "all")}>{t("shell.graph.kindAll")}</Button>
        {#each graphKinds as k (k)}
          <Button variant="ghost" size="xs" class={chipClass(graphFilter.kind === k)} aria-pressed={graphFilter.kind === k} onclick={() => (graphFilter.kind = k)}>
            <span class="gchip-dot" style="background: {kindInk(k)}"></span>{kindLabel(k)}
          </Button>
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
  <Input class={searchClass} type="search" placeholder={t("shell.notes.filterPlaceholder")} bind:value={query} />

  {#if error}
    <div class="banner">{error}</div>
  {/if}

  {#if rows.length === 0}
    <p class="hint">
      {tab === "device"
        ? dictations.length === 0 ? t("device.notes.empty") : t("shell.notes.noMatch")
        : notes.length + dictations.length === 0 ? t("shell.notes.empty") : t("shell.notes.noMatch")}
    </p>
  {/if}

  <ContextMenu.Root
    bind:open={menuOpen}
    onOpenChange={(o) => {
      if (o) return;
      menuForId = null;
      const n = renameAfterClose;
      renameAfterClose = null;
      if (n) setTimeout(() => beginRename(n), 0);
    }}
  >
  <ContextMenu.Trigger>
    {#snippet child({ props })}
  <!-- 触发器 props 自带 tabindex=-1,这里覆盖回 undefined(列表本身不进 Tab 序) -->
  <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
  <ul
    {...props}
    class="list"
    tabindex={undefined}
    onpointerdown={undefined}
    oncontextmenu={(e) => onListContextMenu(e, props.oncontextmenu)}
  >
    {#each rows as row (row.kind === "note" ? row.n.id : row.d.id)}
      {#if row.kind === "dictation"}
        {@const d = row.d}
        <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
        <li
          class="item dictation"
          class:current={$page.url.pathname === `/dictations/${d.id}`}
          onclick={(e) => {
            if ((e.target as HTMLElement).closest("button, input, a")) return;
            goto(`/dictations/${d.id}`);
          }}
          oncontextmenu={(e) => {
            e.preventDefault();
            confirmDeleteDictation(d);
          }}
        >
          <div class="main-line">
            <a class="title" href="/dictations/{d.id}">
              {d.label}
              <span class="state dictation-tag">{t("device.note.kind")}</span>
            </a>
            <span class="meta">{formatDate(d.updated_at)} · {d.app} · {t("device.note.count", { n: d.count })}</span>
          </div>
        </li>
      {:else}
      {@const n = row.n}
      <!-- 行内 .title 锚点已提供键盘路径(Tab+Enter),li 的 onclick 是指针便利层 -->
      <!-- svelte-ignore a11y_no_noninteractive_element_interactions, a11y_click_events_have_key_events -->
      <li
        class="item"
        class:current={$page.url.pathname === `/notes/${n.id}`}
        data-note-id={n.id}
        onclick={(e) => rowClick(e, n)}
      >
        <div class="main-line">
          {#if editingId === n.id}
            <!-- svelte-ignore a11y_autofocus -->
            <Input
              class="h-6 border-ring bg-background px-1.5 text-[0.92em] md:text-[0.92em]"
              autofocus
              bind:value={editingTitle}
              onkeydown={(e) => {
                if (e.key === "Enter" && !e.isComposing) commitRename();
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
          {#if memoHitByNote.get(n.id)}
            {@const h = memoHitByNote.get(n.id)!}
            <span class="memo-hit">{t("memo.search.label")} {fmtClock(h.at_ms)} · {h.text}</span>
          {/if}
        </div>
      </li>
      {/if}
    {/each}
  </ul>
    {/snippet}
  </ContextMenu.Trigger>
  <!-- 关闭时默认把焦点还给原行;只有选了「重命名」才不还(改名框此刻要拿焦点,见 renameAfterClose)。 -->
  <ContextMenu.Content
    class="min-w-36"
    onInteractOutside={() => {
      // 点菜单外关闭:焦点该留在用户刚点的地方(如搜索框),不拽回原行(Codex 二轮 P2)。
      skipRestoreFocus = true;
    }}
    onCloseAutoFocus={(e) => {
      if (skipRestoreFocus) e.preventDefault();
      skipRestoreFocus = false;
    }}
  >
    {@const menuNote = notes.find((n) => n.id === menuForId)}
    <ContextMenu.Item
      onSelect={() => {
        renameAfterClose = menuNote ?? null;
        skipRestoreFocus = true;
      }}>{t("shell.menu.rename")}</ContextMenu.Item
    >
    <ContextMenu.Item
      variant="destructive"
      onSelect={() => {
        const id = menuForId!;
        const title = menuNote?.title ?? "";
        closeMenu();
        confirmDelete(id, title);
      }}>{t("shell.menu.delete")}</ContextMenu.Item
    >
  </ContextMenu.Content>
  </ContextMenu.Root>
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

<svelte:window onkeydown={(e) => {
  if (e.key !== "Escape") return;
  // 右键菜单的 Esc 由 ContextMenu 组件自己收;这里只管图谱抽屉(两者不会同时出现)。
  if (menuOpen) return;
  if (graphDrawerOpen) graphDrawerOpen = false;
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
  .panel > :global(.graph-drawer-close),
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
    font-family: inherit;
    transition: background 0.12s, color 0.12s;
  }
  /* 焦点态与新组件同口径:accent 焦点环(内收,不被轨道裁掉)。 */
  .vtab:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: -2px;
  }
  /* 拉丁标签(AI,仅中文界面):中文竖排里拉丁缩略语用縦中横正立横排;
     英文界面不加本类,与其余页签一致旋转 */
  .vtab-upright {
    writing-mode: horizontal-tb;
    letter-spacing: 0.04em;
  }
  /* 英文界面:0.12em 字距是给汉字设计的,拉丁单词按英文习惯收紧 */
  :global(html[lang="en"]) .vtab {
    letter-spacing: 0.05em;
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
  /* 录制按钮:主 CTA 药丸(Button 默认变体 = primary 底 + on-primary 字,补 radius-full 与
     shadow-btn)+ 红点。大面积强调蓝在侧栏太吵,"彩色"由红点承担——红是本产品唯一常驻
     彩色信号,识别度反而更高。录制中字色走 record(dark 下白药丸上 2.94:1 偏低,由红色方块
     符号独立承担停止语义兜底;light 下 5.98:1 无虞)。导入是幽灵态次级入口,同宽对齐、不抢重心。 */
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
  .new-hook-icon { flex: none; }
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
  .memo-hit {
    display: block;
    font-size: 0.75rem;
    color: var(--ink-secondary);
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
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
  /* 图谱 kind 过滤药丸(侧栏窄,紧凑换行) */
  .gchips {
    display: flex;
    flex-wrap: wrap;
    gap: 4px;
    margin-bottom: 0.5rem;
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
  /* 听写笔记:天蓝浅色调标签,与会议笔记一眼区分(不是状态,是种类)。 */
  .state.dictation-tag {
    background: var(--tint-sky);
    color: var(--tint-sky-ink);
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
    .panel > :global(.graph-drawer-close) {
      display: grid;
      place-items: center;
      align-self: flex-end;
      width: 44px;
      min-height: 44px;
      margin: -4px -4px 2px 0;
      border-radius: var(--radius-full);
      color: var(--ink-secondary);
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
    .sidebar.graph-mode .panel :global(button),
    .sidebar.graph-mode .panel :global(input) { min-height: 44px; }
    .sidebar.graph-mode .panel :global(button),
    .sidebar.graph-mode .panel :global(.gchip) { min-inline-size: 44px; }
  }
  @media (prefers-reduced-motion: reduce) {
    .sidebar.graph-mode .panel { transition: none; }
  }
</style>
