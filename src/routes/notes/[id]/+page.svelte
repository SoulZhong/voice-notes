<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { onTranscodeDone , onAingProgress, onAutoSplitProgress } from "$lib/events";
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { save } from "@tauri-apps/plugin-dialog";
  import { recording } from "$lib/recording.svelte";
  import { recordRiskGate } from "$lib/recordRisk.svelte";
  import AiStateLabel from "$lib/AiStateLabel.svelte";
  import { onRefine, onRetranscribe, onMixedRegen, onNoteRenamed, onNoteRealigned } from "$lib/events";
  import {
    getNote,
    renameNote,
    exportNote,
    exportNoteAudio,
    openNoteDir,
    exportFileName,
    getRefined,
    refineNote,
    retryFailedRefine,
    noteRefining,
    retranscribeNote,
    retranscribeStatus,
    mixedInputStatus,
    deleteNoteSpeaker,
    mixedPlaybackInfo,
    regenerateMixed,
    mixedRegenStatus,
    type MixedPlaybackInfo,
    formatDate,
    formatDuration,
    speakerLabel,
    speakerColor,
    speakerInk,
    speakerIdCompare,
    editSegment,
    deleteSegment,
    setSegmentSpeaker,
    setSegmentsSpeaker,
    deleteSegments, restoreSuppressedSegments, foldSceneEcho,
    clearNoteSpeakerPerson,
    noteAudioInfo,
    assignNoteSpeakerPerson,
    type Note,
    type TrackInfo,
    type RefinedDoc,
    noteRelated,
    type RelatedNote,
    saveRefined,
    getScene,
    type SceneDoc,
    type ParagraphPayload,
    listCalendarCandidates,
    setNoteCalendarEvent,
    noteCalendarPermission,
    identifyNote,
    finalizeInterruptedNote,
    formatTs,
    getNoteEdits,
    addNoteCut,
    removeNoteCut,
    getClipRanks,
    type CutRange,
    type CalendarCandidate,
  } from "$lib/notes";
  import { noteEntityLinks, type EntityLink } from "$lib/graph";
  import { playback } from "$lib/playback.svelte";
  import { t } from "$lib/i18n/index.svelte";
  import { listPeople, type PersonSummary } from "$lib/people";
  import { schemeToDefaultPlayback, shouldFallbackToDual } from "$lib/audioScheme";
  import { getSettings, modelsStatus } from "$lib/models";
  import { refineReady } from "$lib/refineReady";
  import { lowDensityStat, shouldOfferBetterEngine } from "$lib/lowDensity";
  import { aiSkipHint } from "$lib/aiSkipHint";
  import SpeakerChips from "$lib/SpeakerChips.svelte";
  import MultiSpeakerPanel from "$lib/MultiSpeakerPanel.svelte";
  import { autoSplitSpeaker, undoAutoSplit, latestUndoableSplit, listSplitOps, type AutoSplitOut, type SplitOp } from "$lib/multiSpeaker";
  import NoticeStrip from "$lib/NoticeStrip.svelte";
  import SegSpeakerPop from "$lib/SegSpeakerPop.svelte";
  import { contiguousRun, overlappedMicSeqs, seqRange } from "$lib/segPick";
  import type { Notice } from "$lib/notices";
  import AudioPlayer from "$lib/AudioPlayer.svelte";
  import MarkdownEditor, { type BadgeAttrs } from "$lib/editor/MarkdownEditor.svelte";
  import { rebaseQueuedRefinedSave } from "$lib/editor/editorDoc";
  import Segmented from "$lib/Segmented.svelte";
  import type { SegmentedItem } from "$lib/segmented";

  let note = $state<Note | null>(null);
  let error = $state("");
  let editing = $state(false);
  let editingTitle = $state("");
  let exportMsg = $state("");

  // ── 原始稿 WYSIWYG(MarkdownEditor mode="segments"):段结构锁定编辑器 + 浮层菜单/
  //    删除确认。组件契约见 MarkdownEditor.svelte 顶部注释与 Task 8 移交文件——
  //    onEditSegment 成功后必须调 markSegmentSaved(seq, newText),否则同一焦点
  //    会话内的连续提交会拿旧 expectedText 撞后端 CAS。 ──
  let segEditor = $state<ReturnType<typeof MarkdownEditor> | null>(null);
  // 原始稿浮层:说话人菜单 / 删除确认(锚定 NodeView 内按钮的屏幕矩形,与精修稿的
  // entityPop/refinedBadgePop 同一套 position:fixed 套路)。
  let segMenuPop = $state<{ seq: number; rect: DOMRect } | null>(null);
  let segDeletePop = $state<{ seq: number; rect: DOMRect } | null>(null);
  /** setSegments 实际完成文档替换(含 onMount 补放路径)的信号:.md-seg DOM 由
      MarkdownEditor 异步产出,高亮/灰显 effect 若只依赖 activeSeqs/discardedSeqs,
      在渲染尚未落地时查询会拿空集(切视图/首次挂载的竞态)。递增即代表"至少已
      渲染一次新文档",配合下方 segmentsrendered 桥接(与 segescape 同一处
      addEventListener,capture 阶段)。 */
  let segRenderTick = $state(0);

  // 修订稿视图:refined 与 note 一样按 id 拉取、id 切换即复位(见下方 id-effect)。
  let refined = $state<RefinedDoc | null>(null);
  /** 修订稿的载入闸门。写 refined 的有四路:refresh()、Aing 终态事件、在跑复核、
      编辑器保存后的回读。它们各自 await,谁先发起不代表谁先落地——Codex 连着几轮
      抓到的一串竞态(旧稿盖新稿、忙态解除得比新稿早、提示在重取途中闪一下)本质
      是同一件事:多个写者无序。这里一次收口:
      ① 取稿一律走 loadRefined,末次请求赢(seq 比对),旧响应作废;
      ② 取稿在途(refinedLoading > 0)期间不出「这场没做 AI 整理」——眼前这份稿子
         随时可能被换掉,凭它下结论必然出错;
      ③ 编辑器回读那几处自己取稿,同样走 beginRefinedLoad()(占号 + 计入在途)/
         commitRefined(seq, doc)/endRefinedLoad(seq, ok) 三件套——占号必须在 await
         **之前**,否则一份早发出、慢回来的旧回读会拿到更大的号把新稿盖掉;计入在途
         则是为了让提示在任何一路取稿在途时都闭嘴,不只在 loadRefined 那一路;
      ④ 重取失败时不放行提示:那份留在手里的旧稿(stages.llm 仍是 "off")正是
         误报的来源,宁可不提示。 */
  let refinedSeq = 0;
  let refinedLoading = $state(0);
  /** 重取失败,手里这份可能是过期稿:提示一律不出,直到某次重取成功。 */
  let refinedStale = $state(false);
  /** 发起一次取稿:占号 + 计入在途。每个 begin 必须配一个 endRefinedLoad(放 finally)。
      untrack 不可省:`refinedLoading += 1` 是"读 + 写",而 refresh() 是在 $effect 里
      **同步**调到这里的——不 untrack 的话那个 effect 会把 refinedLoading 记成依赖、
      又亲手改它,自我失效成死循环,Svelte 抛 effect_update_depth_exceeded 中止整页
      渲染,笔记页永远停在"加载中"(2026-08-16 真机撞到)。 */
  function beginRefinedLoad(): number {
    untrack(() => {
      refinedLoading += 1;
    });
    refinedSeq += 1;
    return refinedSeq;
  }
  /** 结束一次取稿。committed=false 且自己仍是最新请求时,说明手里这份可能过期
      (取失败/取回来是 null/守卫没过),提示一律不出——被更新的请求顶掉则不算。 */
  function endRefinedLoad(seq: number, committed: boolean) {
    untrack(() => {
      refinedLoading -= 1;
    });
    if (!committed && seq === refinedSeq) refinedStale = true;
  }
  /** 只有仍是最新一次请求、且没切走笔记时才落地;返回是否真的落了。
      调用方要据此决定跟着落不落编辑器同步——被判过期的那份稿子绝不能推给编辑器,
      否则 refined 是新的、编辑器却被回滚成旧的。 */
  function commitRefined(seq: number, doc: RefinedDoc | null, forId: string): boolean {
    if (seq !== refinedSeq || forId !== id) return false;
    refined = doc;
    refinedStale = false;
    return true;
  }
  async function loadRefined(forId: string): Promise<void> {
    const seq = beginRefinedLoad();
    let ok = false;
    try {
      ok = commitRefined(seq, await getRefined(forId), forId);
    } catch {
      /* 增值层:取不到就维持现状,过期与否交给 endRefinedLoad 判 */
    } finally {
      endRefinedLoad(seq, ok);
    }
  }
  let refining = $state(false);
  /** Aing 逐块进度(行内「精修中 3/8 · 约剩 4 分」)。null=无进度可画。 */
  let aingProg = $state<{ stage: string; done: number; total: number; avgMs: number } | null>(null);
  $effect(() => {
    const forId = id;
    aingProg = null; // 换笔记清残留
    const un = onAingProgress((e) => {
      if (e.note_id !== forId) return;
      aingProg = { stage: e.stage, done: e.done, total: e.total, avgMs: e.avg_chunk_ms };
    });
    return () => {
      un.then((f) => f());
    };
  });
  $effect(() => {
    // 终态清行内进度:refining 结束(done/failed)后残留的 3/8 会误导。
    if (!refining) aingProg = null;
  });
  const aingEtaMin = $derived(
    aingProg && aingProg.done >= 2 && aingProg.avgMs > 0
      ? Math.max(1, Math.ceil(((aingProg.total - aingProg.done) * aingProg.avgMs) / 60000))
      : null, // 前两块没完成不显示时间:没数据不瞎估
  );
  /** 在跑状态是否已确定:进页那一刻 refining 还是默认 false,而 run_local 一开始就把
      stages.llm 落成 "off"——若在补问 note_refining 落地之前就渲染,横幅会闪一下,
      还给出一个后端必然拒绝的重跑按钮(Codex P2 四轮)。未确定期间一律不提示。 */
  let refineStatusKnown = $state(false);
  /** 快照说"在跑"之后的复核间隔。只在这条自愈路径上用,正常靠事件驱动。 */
  const REFINE_RECHECK_MS = 3000;
  /** 当前识别引擎与「更强引擎是否已下载」:决定要不要提示"这场疑似识别失败,换引擎重转写"
      (判据与由来见 $lib/lowDensity)。挂载取一次;换引擎会重转写,届时整页重来。 */
  let repairReady = $state(false);
  let repairing = $state(false);
  let refineRunFailed = $state(false);
  let refineErr = $state("");
  let viewMode = $state<"refined" | "raw">("refined");

  // 文件重转写(三期):离线用盘上音频重转全文,覆盖原始逐字稿(自动备份)。
  // retransErr 复用 refineErr 同款粘性 banner 展示套路(本页无 toast 机制)。
  let retranscribing = $state(false);
  let retransStage = $state("");
  let retransConfirm = $state(false);
  let retransErr = $state("");
  let mixedReason = $state<string | null>(null); // null = 成品轨可用
  // Fix 4:回填查询 retranscribeStatus() 在途时，若终态事件（onRetranscribe 的
  // ok/error）先到，迟到的回填快照会把已结束的任务重新标成"重转写中"且无人再纠正
  // （事件只在状态变化时触发一次，不会再来一条把它拨回去）。这个旗标记录"本次 id
  // 下是否已经见过至少一条 onRetranscribe 事件"——见过就说明事件通道已经接管了
  // retranscribing 的真相，回填快照必须让路。回填查询的发起时机见 Fix 3（codex 第
  // 三轮，已挪进订阅 effect，必须等 listen() resolve 之后再发）。
  let retransEventSeen = $state(false);
  // 会议搭子人物列表:修订稿说话人条的「选人」面板用。增值层,取失败静默按空处理。
  let people = $state<PersonSummary[]>([]);
  // 相关笔记(经知识图谱共享实体):增值层,取失败静默按空。
  let related = $state<RelatedNote[]>([]);

  const id = $derived($page.params.id as string);

  // 音频播放:轨道列表 + 播放器时钟(高亮跟随)。录制中(含暂停)不显示播放器,
  // 文件正在写,不做边写边播的半态。
  let tracks = $state<TrackInfo[]>([]);
  let player = $state<ReturnType<typeof AudioPlayer> | null>(null);
  let playerMs = $state(0);
  let playerPlaying = $state(false);

  // 回放方案 A/B(二期):双轨对齐+门控(A)/成品轨直放(B)。会话内状态,
  // 切笔记复位——对比场景本来就是当场切,不做持久化。
  let playbackScheme = $state<"dual" | "mixed">("dual");
  /** 设置页三档决定的默认回放(spec 2026-08-10)。增值层:取失败按 a(双轨)不打扰。
      挂载取一次即定,id 切换复位用它;会话内手动切换语义不变。 */
  let defaultPlayback = $state<"dual" | "mixed">("dual");
  /** 会后 AI 开关与执行体就绪:决定「这场没做 AI 整理」提示的口径(见 $lib/aiSkipHint)。
      与 defaultPlayback 同一次 getSettings 取,挂载取一次即定——去配置要离开本页,
      回来时组件重挂载,自然拿到新值。 */
  let refineOn = $state(false);
  let refineConfigured = $state(false);
  let mixedInfo = $state<MixedPlaybackInfo | null>(null);
  /** 成品轨读数进行中:true 期间回落判定不抢跑——mixedInfo=null 是"未知"不是"确无"。 */
  let mixedPending = $state(true);
  /** A/B 切换的续播现场(codex P2):换方案会整体重装原生播放器(核心恒从 0/
      paused 起),对比要的恰是同一时刻——切换前记下位置,onLoaded 回调里恢复。 */
  let pendingResume = $state<{ ms: number; playing: boolean } | null>(null);
  let regenStage = $state<string | null>(null); // 非 null = 补生成进行中(值为阶段名)
  let regenErr = $state("");

  // mix-switch 的 segmented 数据:有成品轨给 双轨/成品轨 切换(不可信置灰+tooltip 原因);
  // 没有则第二段是「生成」动作段(momentary,滑块不落位),沿用 startRegen/regenStage 原逻辑。
  const mixItems = $derived.by((): SegmentedItem[] => {
    const dual: SegmentedItem = { id: "dual", label: t("notes.mix.dual") };
    if (mixedInfo?.track) {
      return [
        dual,
        {
          id: "mixed",
          label: t("notes.mix.mixed"),
          disabled: mixedInfo.untrusted !== null,
          title: mixedInfo.untrusted ?? t("notes.mix.title"),
        },
      ];
    }
    return [
      dual,
      {
        id: "gen",
        label: regenStage ? t("notes.mix.generating", { stage: regenStage }) : t("notes.mix.generate"),
        momentary: true,
        disabled: regenStage !== null || recording.isLive,
        title: t("notes.mix.none"),
      },
    ];
  });

  /** 展示序:filter+sort 已下沉 NoteStore::load(单一真值源),后端保证无空白段、
      按 (start_ms, seq) 升序,前端直接消费。 */
  const displaySegments = $derived(note ? note.segments : []);
  /** 本笔记正在录制（含暂停）时禁用一切编辑入口（后端另有 guard 兜底）。 */
  const canEdit = $derived(!(recording.isLive && recording.noteId === id));
  const speakerIds = $derived(note ? Object.keys(note.speakers).sort(speakerIdCompare) : []);
  /** 改派菜单打开的那一段当前是谁。按 seq 回查 note.segments(权威值),
      不信浮层里带的快照——同 segBadge 的道理。 */
  const segMenuSpeaker = $derived(
    segMenuPop ? (note?.segments.find((s) => s.seq === segMenuPop!.seq)?.speaker ?? null) : null,
  );

  /** 修订稿是否可展示：无 Aing 结果、或笔记尚未 complete（例如中断续录中）一律强制原始稿。 */
  const refinedAvailable = $derived(!!refined && note?.meta.state === "complete");
  const refinedHasFailure = $derived(
    !!refined && [refined.stages.filter, refined.stages.recluster, refined.stages.llm].includes("failed"),
  );
  const aiState = $derived(
    refining ? "running" : refineRunFailed || refinedHasFailure ? "failed" : refinedAvailable ? "complete" : "idle",
  );
  /** 实际渲染的视图：viewMode 是用户意图，refinedAvailable=false 时无条件降级为 raw。 */
  const effectiveView = $derived(refinedAvailable ? viewMode : "raw");
  /** 视图切换 segmented:无修订稿时该段置灰 + tooltip 原因(行为与旧 .link 版完全一致) */
  const viewItems = $derived<SegmentedItem[]>([
    {
      id: "refined",
      label: t("notes.view.refined"),
      disabled: !refinedAvailable,
      title: refinedAvailable ? undefined : t("notes.view.noRefined"),
    },
    { id: "raw", label: t("notes.view.raw") },
  ]);
  /** 原始稿中被 Aing 过滤掉的段（灰显用）。 */
  const discardedSeqs = $derived(new Set(refined?.discarded_seqs ?? []));

  /** 修订稿段落标签/配色的解析表:原始稿说话人表 + 旧文档映射不回 S 的遗留
      段落 id(仅供段落文字显示兜底)。胸牌组件不吃这张表——它直接吃
      note.speakers,与原始稿视图同一个对象,列表不可能有差异(2026-08-21
      用户实测:遗留 R 分组会让两视图胸牌对不上)。 */
  const refinedSpeakers = $derived.by(() => {
    const m: Record<
      string,
      { name: string; sources: string[]; person_id?: string | null; multi_speaker?: boolean }
    > = { ...(note?.speakers ?? {}) };
    for (const p of refined?.paragraphs ?? []) {
      if (!p.speaker || m[p.speaker]) continue;
      m[p.speaker] = { name: p.name ?? "", sources: ["mic"], person_id: p.person_id ?? null };
    }
    return m;
  });

  // ── 精修稿 WYSIWYG 接线(MarkdownEditor):可编辑 + 失焦/停顿自动保存 + revision
  //    冲突重载 + 实体浮层。组件契约见 MarkdownEditor.svelte 顶部注释与移交文件——
  //    onSaveRefined 的每条失败路径必须调 markSaveFailed(),否则自动保存永久停摆。 ──
  let refinedEditor = $state<ReturnType<typeof MarkdownEditor> | null>(null);
  let refinedHostEl = $state<HTMLDivElement | null>(null);
  // 实体悬浮浮层:{ entityId, rect };离开/点击别处即收起——但不是立即清:鼠标从
  // 实体 span 移向浮层里的按钮途中会先经过两者之间的空隙,entityleave/宿主
  // mouseleave 若立即清空,浮层在按钮点到之前就没了(entity-pop 是 rect.bottom+4
  // 的兄弟节点,不在实体 span 内部)。改成排 ~180ms 收起,浮层自身 hover 能取消。
  let entityPop = $state<{ entityId: string; rect: DOMRect } | null>(null);
  let entityPopHideTimer: ReturnType<typeof setTimeout> | null = null;
  const ENTITY_POP_HIDE_MS = 180;
  function scheduleHideEntityPop() {
    if (entityPopHideTimer) clearTimeout(entityPopHideTimer);
    entityPopHideTimer = setTimeout(() => {
      entityPopHideTimer = null;
      entityPop = null;
    }, ENTITY_POP_HIDE_MS);
  }
  function cancelHideEntityPop() {
    if (entityPopHideTimer) {
      clearTimeout(entityPopHideTimer);
      entityPopHideTimer = null;
    }
  }
  // 精修稿说话人徽章点击浮层:沿用说话人条改名/选人入口,弹层只做身份提示。
  let refinedBadgePop = $state<{ attrs: BadgeAttrs; rect: DOMRect } | null>(null);
  // 保存错误粘性去重:markSaveFailed 无退避地按 2s 重试,持续性拒绝(Aing 中/录制中)
  // 不该每次都刷新一条 banner——同一条错误只设置一次,markSaved 成功后清空。
  let refinedSaveErr = $state("");
  // 当前编辑器里加载的是哪个笔记的精修稿(而非 route 的 id):id 切换时 flush 必须
  // 落到*旧*笔记,不能用已经翻新的 id——否则会把上一篇的编辑存进新笔记。
  let loadedRefinedId: string | null = null;
  type RefinedSaveSnapshot = { revision: number; paragraphs: ParagraphPayload[] };
  type ActiveRefinedSave = { payload: RefinedSaveSnapshot; done: Promise<number> };
  // lifecycle drain 活在页面层,不依赖即将销毁的 MarkdownEditor 实例。每篇笔记至多
  // 保留一份最新快照;active 完成后按其新 revision/段序重基并串行落盘。
  const activeRefinedSaves = new Map<string, ActiveRefinedSave>();
  const pendingRefinedDrains = new Map<string, RefinedSaveSnapshot>();
  const runningRefinedDrains = new Set<string>();
  // 身份闸门:标记"编辑器已同步到的那份 refined 对象"+"是哪个编辑器实例同步的"。
  // 保存成功后 refined 换新对象身份(revision 更新)必然触发下面的同步 effect;若
  // 不闸,失焦保存场景 hasFocus() 为 false,effect 会用*旧*的段落快照把编辑器里
  // 刚打上的内容重建一遍,吹掉用户紧接着的输入。凡是"页面主动把 doc 写成与编辑器
  // 一致"的地方(保存成功回写、冲突重载显式 setRefined),都要顺手把该 doc 记进
  // syncedRefined、当前编辑器实例记进 syncedEditor,让 effect 识别出"已经同步
  // 过,不用再来一次"。两者必须配对判定(Fix Round 2):只闸 doc 不闸 editor 实例,
  // 会在「修订稿→原始稿→修订稿」来回切时炸——MarkdownEditor 挂在
  // {#if effectiveView === "refined"} 里,视图切走会销毁旧实例、切回来挂全新实例;
  // 若 refined 对象没变(没有新保存/新精修),新实例挂载触发 effect 重跑时
  // doc === syncedRefined 命中就直接 return,新实例永远收不到 setRefined,渲染空白
  // 且其中打字因组件内部 loadedDoc 为 null 静默不落盘。
  let syncedRefined: RefinedDoc | null = null;
  let syncedEditor: unknown = null;

  function queueRefinedDrain(payload: RefinedSaveSnapshot) {
    const targetId = loadedRefinedId ?? id;
    pendingRefinedDrains.set(targetId, payload);
    if (runningRefinedDrains.has(targetId)) return;
    const active = activeRefinedSaves.get(targetId) ?? null;
    void drainRefinedAfterActive(targetId, active);
  }

  async function drainRefinedAfterActive(targetId: string, initial: ActiveRefinedSave | null) {
    runningRefinedDrains.add(targetId);
    let revision: number | null = null;
    let previous: ParagraphPayload[] | null = null;
    try {
      if (initial) {
        revision = await initial.done;
        previous = initial.payload.paragraphs;
      }
      while (pendingRefinedDrains.has(targetId)) {
        const queued = pendingRefinedDrains.get(targetId)!;
        pendingRefinedDrains.delete(targetId);
        const next =
          revision !== null && previous
            ? rebaseQueuedRefinedSave(revision, previous, queued.paragraphs)
            : queued;
        revision = await saveRefined(targetId, next.revision, next.paragraphs);
        previous = next.paragraphs;
      }
      // 仍停留在本篇且没有继续输入时,把 detached 保存的最终盘上状态同步回来。
      if (targetId === id && !refinedEditor?.hasFocus()) {
        const seq = beginRefinedLoad(); // 占号+计入在途必须在 await 之前(见闸门注释 ③)
        let ok = false;
        try {
          const latest = await getRefined(targetId);
          // 被更新的取稿顶掉时不推给编辑器(见 commitRefined 注释)。
          if (latest && targetId === id && !refinedEditor?.hasFocus()) {
            ok = commitRefined(seq, latest, targetId);
            if (ok) {
              syncedRefined = latest;
              syncedEditor = refinedEditor;
              refinedEditor?.setRefined(latest);
            }
          }
        } finally {
          endRefinedLoad(seq, ok);
        }
      }
    } catch (err) {
      const msg = t("notes.detail.drainFailed", { e: err });
      if (targetId === id && msg !== refinedSaveErr) refinedSaveErr = msg;
    } finally {
      runningRefinedDrains.delete(targetId);
      // 排空结束的极窄窗口里可能又收到一份更新快照,继续下一轮而不丢它。
      if (pendingRefinedDrains.has(targetId)) {
        void drainRefinedAfterActive(targetId, activeRefinedSaves.get(targetId) ?? null);
      }
    }
  }

  function refinedBadge(attrs: BadgeAttrs): { label: string; bg: string; ink: string } {
    const sid = attrs.speaker;
    return {
      label: speakerLabel(sid, "mic", refinedSpeakers),
      bg: speakerColor(sid, "mic", refinedSpeakers),
      ink: speakerInk(sid, "mic", refinedSpeakers),
    };
  }

  async function doSaveRefined(payload: { revision: number; paragraphs: ParagraphPayload[] }) {
    const targetId = loadedRefinedId ?? id;
    const done = saveRefined(targetId, payload.revision, payload.paragraphs);
    const active = { payload, done };
    activeRefinedSaves.set(targetId, active);
    try {
      const newRev = await done;
      // await 期间编辑器可能已经切到别的笔记(loadedRefinedId 变了):这份回执打在
      // *旧*笔记上,组件自己的 setRefined(切笔记时会重新整份载入)早就复位过
      // saveInFlight,不需要再补 markSaveFailed——直接丢弃回执即可。
      if (loadedRefinedId !== targetId) return;
      // markSaved 用后端刚返回的 newRev,在下面的 getRefined 之前调用:避免编辑器
      // revision 出现空窗期(空窗期内若触发下一次自动保存,会拿着旧 revision 去打
      // 乐观并发冲突)。
      refinedEditor?.markSaved(newRev);
      // 不再本地拼 { ...refined, revision: newRev }(Fix Round 2 之前的做法):那样
      // refined.paragraphs 仍是保存前的旧内容,与编辑器里实际内容(用户刚打的字)
      // 对不上——"空稿提示与输入并存"之类的问题正是源于此。保存成功后直接回读
      // 盘上最新精修稿,让 refined 说真话;这份内容和编辑器一致,配合下面 syncedRefined/
      // syncedEditor 闸门,视图来回切换重建新编辑器实例时会用这份正确内容渲染,
      // 而不是被闸门跳过导致空白。
      const seq = beginRefinedLoad(); // 占号+计入在途必须在 await 之前(见闸门注释 ③)
      let ok = false;
      try {
        const latest = await getRefined(targetId);
        // await 后重验守卫:回读期间用户可能已经切走了笔记。
        if (targetId === id && loadedRefinedId === targetId) {
          if (latest) {
            ok = commitRefined(seq, latest, targetId);
            if (ok) {
              syncedRefined = latest;
              syncedEditor = refinedEditor;
            }
          }
          // latest 为 null(极端情况,如笔记目录被清):保持 refined 原样不动,不因
          // 一次回读失败就把已展示的内容整篇清空。
        }
      } catch {
        /* 回读失败:refined 保持原状;本次保存本身已经成功(markSaved 已确认落定) */
      } finally {
        endRefinedLoad(seq, ok);
      }
      if (refinedSaveErr) refinedSaveErr = "";
    } catch (err) {
      // 同上:编辑器已经切走,回执作废,不需要 markSaveFailed。
      if (loadedRefinedId !== targetId) return;
      // markSaveFailed 必须无条件调用(即使随后走冲突重载分支):否则一次拒绝就让
      // 组件 saveInFlight 卡 true,自动保存永久停摆。
      refinedEditor?.markSaveFailed();
      // 精修稿保存错误走独立粘性 banner(refinedSaveErr),不写共享 error——refresh()
      // 成功会清 error,若复用它,持续性失败(Aing 中反复被拒)会被后台刷新悄悄抹掉。
      const msg = t("notes.detail.saveRefinedFailed", { e: err });
      if (msg !== refinedSaveErr) refinedSaveErr = msg;
      // revision 冲突(乐观并发):当前编辑已经落空,重载盘上最新内容重建文档。
      // 非冲突失败(Aing 中/录制中被拒):只保留错误提示,让编辑按 idle 定时器重试
      // (markSaveFailed 已排好下一次)。
      if (String(err).includes("已在别处更新")) { // i18n-exempt: 与后端错误原文判等
        const seq = beginRefinedLoad(); // 占号+计入在途必须在 await 之前(见闸门注释 ③)
        let ok = false;
        try {
          const latest = await getRefined(targetId);
          if (targetId === id && loadedRefinedId === targetId) {
            ok = commitRefined(seq, latest, targetId);
          }
          // 这里显式 setRefined 已经把编辑器文档重建到位;同时把 latest/当前编辑器
          // 实例记进 syncedRefined/syncedEditor,让下面的同步 effect 认出"已经
          // 同步过"直接跳过——否则 effect 会因 refined 换了身份再 setRefined 一次,
          // 属于重复重建。
          if (ok && latest) {
            refinedEditor?.setRefined(latest);
            syncedRefined = latest;
            syncedEditor = refinedEditor;
          }
        } catch {
          /* 重载失败保持错误横幅 */
        } finally {
          endRefinedLoad(seq, ok);
        }
      }
    } finally {
      if (activeRefinedSaves.get(targetId) === active) activeRefinedSaves.delete(targetId);
    }
  }

  // refined 变化(载入/精修完成/冲突重载)→ 重建编辑器文档;输入中不打断。
  // 身份闸门(syncedRefined + syncedEditor,见上方声明的注释):doSaveRefined 成功
  // 回写/显式冲突重载已经让*这个*编辑器实例和这个具体 doc 对象同步过,不必再来
  // 一次——否则失焦保存场景会用刚落盘的旧引用重建文档,吹掉用户紧接着的输入。
  // 两者必须配对判定:只闸 doc 会在视图来回切换、MarkdownEditor 被销毁重挂出新
  // 实例但 refined 对象没变时,把新实例挂载这次也跳过,渲染空白且输入静默不保存。
  $effect(() => {
    const doc = refined;
    const ed = refinedEditor;
    if (!ed || !doc || effectiveView !== "refined") return;
    if (doc === syncedRefined && ed === syncedEditor) return;
    if (ed.hasFocus()) return; // 常驻编辑态:正在打字时外部刷新不吹掉输入
    ed.setRefined(doc);
    syncedRefined = doc;
    syncedEditor = ed;
    loadedRefinedId = id;
  });

  // 实体悬浮浮层桥接:组件在编辑器根上派发 entityhover/entityleave,两者都不带
  // bubbles:true——只有 capture 阶段的祖先监听才能收到(非冒泡事件的标准套路,
  // 与旧式 focus/blur 在有 focusin/focusout 之前的处理方式同理)。span→span 快速
  // 移动会先 leave(A) 再 hover(B):leave 的 entityId 与当前展示不符就忽略,否则
  // 新浮层会被刚到达的旧 leave 事件立刻收起。
  $effect(() => {
    const el = refinedHostEl;
    if (!el) return;
    const onHover = (e: Event) => {
      const detail = (e as CustomEvent<{ entityId: string; rect: DOMRect }>).detail;
      cancelHideEntityPop(); // 新实体到达:取消 pending 收起,直接替换内容
      entityPop = detail;
    };
    const onLeave = (e: Event) => {
      const detail = (e as CustomEvent<{ entityId: string }>).detail;
      if (entityPop && entityPop.entityId === detail.entityId) scheduleHideEntityPop();
    };
    el.addEventListener("entityhover", onHover, true);
    el.addEventListener("entityleave", onLeave, true);
    return () => {
      el.removeEventListener("entityhover", onHover, true);
      el.removeEventListener("entityleave", onLeave, true);
      cancelHideEntityPop(); // host 元素换掉/组件销毁:pending 的收起 timer 不留着
    };
  });

  // 离开页面/组件销毁前把未保存的精修稿编辑冲出去(与段编辑失焦保存同哲学:
  // 不因为切走了就悄悄丢用户刚打的字)。
  onDestroy(() => {
    refinedEditor?.flushRefined(true);
    cancelHideEntityPop();
  });

  // ── 说话人试听:chips 面板「试听他的声音」——不听声音没法确认「说话人 N」是谁。
  //    播该说话人时长最长的一段(代表性最好),重复点击按时长降序换下一段(取前 5,
  //    循环);单段最多听 15s,段尾自动停;用户手动暂停/拖走即退出试听态。 ──
  const PREVIEW_MAX_MS = 15_000;
  /** 退出试听只认两个确定信号:播到段尾(下方效应)与用户亲手点暂停(onUserPause)。
      从 playing 翻转**推断**"用户暂停"两次修两次漏:play() 是乐观置位,Rust 心跳在
      seek 生效、play 未生效的间隙采到 playing=false,armed 拦不住——preview 被清,
      段尾边界消失,整篇一直放(2026-08-20/21 用户两次实测)。 */
  let preview = $state<{ sid: string; idx: number; seq: number; endMs: number } | null>(null);
  /** 试听强制停表:按样本时长走墙钟,到点仍在试听态就硬停。响应式段尾边界已三轮
      "修好又复发"(2026-08-20 两次、08-21 一次用户实测),失效环节始终未坐实
      (事件链/边界比较/暂停失败均有嫌疑);墙钟不依赖位置事件与响应式依赖追踪,
      是无条件兜底。正常情况下边界效应先到,这里只负责最后一道。 */
  let previewTimer: ReturnType<typeof setTimeout> | null = null;
  function armPreviewWatchdog(durMs: number) {
    if (previewTimer) clearTimeout(previewTimer);
    previewTimer = setTimeout(() => {
      previewTimer = null;
      if (preview) {
        // 走到这里=响应式边界失灵(真凶已定案一例:effect 依赖追踪误杀,见下方
        // untrack 注释);留痕以便再犯时定位。
        console.warn("[preview] watchdog fired; reactive end-boundary did not stop playback");
        player?.pause();
        endPreview("watchdog");
      }
    }, durMs + 1200);
  }

  /** seq -> 原始段。修订稿试听要靠它把段落还原成真实音频区间。 */
  const segBySeq = $derived(new Map(displaySegments.map((s) => [s.seq, s])));

  // ── 多人混杂处置(设计:2026-08-20-mixed-speaker-split-design.md) ──
  let multiPanel = $state<{
    candidates: string[];
    existingOp: SplitOp | null;
  } | null>(null);
  /** 本篇未完成的打标操作(启动/刷新时拉取,恢复入口)。 */
  let openMultiOps = $state<SplitOp[]>([]);
  async function refreshMultiOps() {
    openMultiOps = await listSplitOps(id).catch(() => []);
  }
  $effect(() => {
    void id;
    void refreshMultiOps();
  });
  // ── 一键拆分(2026-08-22-one-click-split-design.md):点击即后台全默认执行 ──
  let autoSplitRunning = $state(false);
  /** 逐段嵌入进度(done/total);null=尚无事件(装载/解码阶段)。 */
  let autoSplitProg = $state<{ done: number; total: number } | null>(null);
  $effect(() => {
    const forId = id;
    const un = onAutoSplitProgress((e) => {
      if (e.note_id !== forId) return;
      autoSplitProg = { done: e.done, total: e.total };
    });
    return () => {
      un.then((f) => f());
    };
  });
  let autoToast = $state<
    | { kind: "split"; out: AutoSplitOut; undone: boolean }
    | { kind: "nochange" }
    | { kind: "error"; msg: string }
    | { kind: "undone" }
    | null
  >(null);
  async function runAutoSplit(sid: string) {
    if (autoSplitRunning) return;
    autoSplitRunning = true;
    autoSplitProg = null;
    autoToast = null;
    try {
      const out = await autoSplitSpeaker(id, sid);
      autoToast = out.split ? { kind: "split", out, undone: false } : { kind: "nochange" };
    } catch (e) {
      autoToast = { kind: "error", msg: `${e}` };
    } finally {
      autoSplitRunning = false;
      refresh();
      recording.bumpNotes();
      void refreshMultiOps();
      void refreshUndoable();
    }
  }
  async function undoSplitToast() {
    if (autoToast?.kind !== "split" || autoToast.undone) return;
    try {
      await undoAutoSplit(autoToast.out.op_id);
      autoToast = { kind: "undone" };
    } catch (e) {
      autoToast = { kind: "error", msg: `${e}` };
    }
    refresh();
    recording.bumpNotes();
  }

  /** 场景判定(scene.json,一期只提示):final_scene 为 2/3/4 时给 info 通知。 */
  let sceneDoc = $state<SceneDoc | null>(null);
  $effect(() => {
    void id;
    sceneDoc = null;
    getScene(id).then((d) => (sceneDoc = d)).catch(() => {});
  });

  /** 各说话人最近试听过的段 seq(「确认才入库」:关联时把这一段作为确认样本)。 */
  let lastAuditioned = $state<Record<string, number>>({});

  /** 最近一次可撤销拆分(24h 内;结果横幅关掉后撤销仍有处可寻)。本页会话内可忽略。 */
  let undoableSplit = $state<SplitOp | null>(null);
  $effect(() => {
    void id;
    void refreshUndoable();
  });
  async function refreshUndoable() {
    const op = await latestUndoableSplit(id).catch(() => null);
    undoableSplit =
      op && Date.now() - new Date(op.created_at).getTime() < 24 * 3600_000 ? op : null;
  }
  async function retryPartial() {
    try {
      await retryFailedRefine(id);
      refining = true; // 事件随后接管;先置上避免按钮双击
    } catch (e) {
      error = `${e}`;
    }
  }
  function resumeOp(op: SplitOp) {
    // 单说话人 op 走一键续跑(带进度,人话);多说话人是旧面板时代的遗留,只有它回面板。
    if (op.speaker_ids.length === 1) runAutoSplit(op.speaker_ids[0]);
    else multiPanel = { candidates: op.speaker_ids, existingOp: op };
  }

  /** 提示系统(2026-08-22 方案甲):本页全部常驻提示统一进一条通知条。
      数组序即优先级:错误(组件内单列) > 数据一致性 > 可重试 > 恢复中断 >
      质量建议 > 可撤销 > 背景信息。 */
  const pageNotices = $derived.by((): Notice[] => {
    const out: Notice[] = [];
    if (regenErr) out.push({ key: "regenErr", level: "error", text: regenErr });
    if (refineErr) out.push({ key: "refineErr", level: "error", text: refineErr });
    if (retransErr) out.push({ key: "retransErr", level: "error", text: retransErr });
    if (effectiveView === "refined" && refined && refinedSaveErr)
      out.push({ key: "refinedSaveErr", level: "error", text: refinedSaveErr });
    if (effectiveView === "refined" && refined?.stages.llm === "failed")
      out.push({ key: "llmFailed", level: "error", text: t("notes.banner.llmFailed") });
    if (effectiveView === "refined" && refined?.stale)
      out.push({
        key: "stale",
        level: "action",
        text: t("notes.retrans.staleBanner"),
        epoch: refined.generated_at,
        actions: [{
          label: refining ? t("notes.banner.aiRunning") : t("notes.banner.aiRun"),
          run: rerunRefine,
          disabled: refining || note?.meta.state !== "complete",
        }],
      });
    if (effectiveView === "refined" && refined?.stages.llm === "partial") {
      const nFailed = refined.llm_failed_paragraphs?.length ?? 0;
      out.push({
        key: "llmPartial",
        level: "action",
        text: t("notes.banner.llmPartial"),
        epoch: `${refined.generated_at}:${nFailed}`,
        actions:
          nFailed > 0 && !refining
            ? [{ label: t("notes.banner.retryFailedSegs", { n: nFailed }), run: retryPartial }]
            : [],
      });
    }
    const openOp = openMultiOps.find((o) => o.phase !== "done");
    if (openOp && !multiPanel && !autoSplitRunning)
      out.push({
        key: "resume",
        level: "action",
        text: t("speakers.multi.resume"),
        dismissible: false,
        actions: [{ label: t("speakers.multi.resumeOpen"), run: () => resumeOp(openOp) }],
      });
    if (offerBetterEngine)
      out.push({
        key: "lowDensity",
        level: "suggest",
        text: t("notes.banner.lowDensity", { n: lowDensity.count, s: lowDensity.seconds }),
        detail: t("notes.banner.lowDensityHint"),
        actions: [{
          label: repairing ? t("notes.banner.lowDensitySwitching") : t("notes.banner.lowDensityFix"),
          run: repairWithFirered,
          disabled: repairing,
        }],
      });
    if (undoableSplit && !autoSplitRunning && !autoToast)
      out.push({
        key: "undoable",
        level: "suggest",
        text: t("speakers.autosplit.undoableNotice"),
        epoch: undoableSplit.op_id,
        actions: [{ label: t("speakers.autosplit.undo"), run: undoLatestSplit }],
      });
    if (aiSkipped)
      out.push({
        key: "aiSkipped",
        level: "info",
        text: aiSkipped === "unconfigured" ? t("notes.banner.aiUnconfigured") : t("notes.banner.aiNotRun"),
        actions:
          aiSkipped === "unconfigured"
            ? [{ label: t("notes.banner.aiConfigure"), run: () => goto("/ai") }]
            : [{
                label: refining ? t("notes.banner.aiRunning") : t("notes.banner.aiRun"),
                run: rerunRefine,
                disabled: refining,
              }],
      });
    if (note?.meta.state === "recording")
      out.push({
        key: "interrupted",
        level: "info",
        text: finalizeErr
          ? t("notes.banner.finalizeFailed", { e: finalizeErr })
          : t("notes.banner.interrupted"),
        epoch: finalizeErr || undefined,
        actions: [{
          label: finalizing ? t("notes.banner.finalizing") : t("notes.banner.finalizeNow"),
          run: finalizeNow,
          // 本篇正被续录时不给收尾(后端也会拒);别的笔记在录不挡。
          disabled: finalizing || (recording.isLive && recording.noteId === id),
        }],
      });
    if ((note?.skipped_lines ?? 0) > 0)
      out.push({
        key: "skipped",
        level: "info",
        text: t("notes.banner.skipped", { n: note?.skipped_lines ?? 0 }),
        epoch: String(note?.skipped_lines ?? 0),
      });
    const sc = sceneDoc?.final_scene;
    if (sc === "dual_path" || sc === "speaker_echo" || sc === "onsite") {
      // 修订稿视图也给按钮:点击先切原始逐字稿再选中(勾选模式只存在于原始稿)。
      const suspects = sc !== "onsite" ? overlappedMicSeqs(displaySegments) : [];
      out.push({
        key: "scene",
        level: "info",
        text: t(
          sc === "dual_path"
            ? "notes.scene.dualPath"
            : sc === "speaker_echo"
              ? "notes.scene.speakerEcho"
              : "notes.scene.onsite",
        ),
        epoch: sc,
        actions:
          suspects.length > 0 && canEdit
            ? [{
                label: t("notes.scene.selectSuspects", { n: suspects.length }),
                run: () => {
                  viewMode = "raw";
                  selMode = true;
                  selected = suspects;
                  lastSel = suspects[suspects.length - 1] ?? null;
                },
              }]
            : [],
      });
    }
    // 场景二期折叠横幅(issue #162):有被折叠段即显示——展开全部找回;
    // dual_path 场展开后还能一键重新折叠(与停录自动折叠同实现)。
    const foldedN = note?.suppressed_segments?.length ?? 0;
    if (foldedN > 0 && canEdit && !refining)
      out.push({
        key: "folded",
        level: "info",
        text: t("notes.scene.folded", { n: foldedN }),
        epoch: String(foldedN),
        actions: [{
          label: t("notes.scene.unfoldAll"),
          run: async () => {
            try {
              await restoreSuppressedSegments(id, (note?.suppressed_segments ?? []).map((s) => s.seq));
              await refresh();
            } catch (e) {
              error = t("common.loadFailed", { e });
            }
          },
        }],
      });
    else if (foldedN === 0 && sc === "dual_path" && canEdit && !refining && overlappedMicSeqs(displaySegments).length > 0)
      out.push({
        key: "refold",
        level: "info",
        text: t("notes.scene.refoldHint", { n: overlappedMicSeqs(displaySegments).length }),
        epoch: "refold",
        actions: [{
          label: t("notes.scene.refold"),
          run: async () => {
            try {
              await foldSceneEcho(id);
              await refresh();
            } catch (e) {
              error = t("common.loadFailed", { e });
            }
          },
        }],
      });
    if (playbackScheme === "mixed" && mixedInfo?.ab_caveat)
      out.push({ key: "abCaveat", level: "info", text: t("notes.mix.abCaveat") });
    return out;
  });

  async function undoLatestSplit() {
    if (!undoableSplit) return;
    try {
      await undoAutoSplit(undoableSplit.op_id);
      autoToast = { kind: "undone" };
      undoableSplit = null;
    } catch (e) {
      autoToast = { kind: "error", msg: `${e}` };
    }
    refresh();
    recording.bumpNotes();
  }
  /** 拆分面板的段试听:独奏该段所在轨、seek、播到段尾自动停(复用 preview 机制)。 */
  function auditionSegment(seq: number) {
    const seg = note?.segments.find((s) => s.seq === seq);
    if (!seg || !player) return;
    const segSource = segSourceAt(seg.start_ms);
    preview = { sid: "__split", idx: 0, seq, endMs: seekFix(seg.end_ms, segSource) };
    armPreviewWatchdog(seg.end_ms - seg.start_ms);
    // 独奏该段所在轨:多轨混音下另一条轨的同时段声音会一起响(fix 分支合入后接上)。
    player.soloTrack(seg.source ?? null);
    player.seek(seekFix(seg.start_ms, segSource));
    player.play();
  }

  function onMultiChanged() {
    refresh();
    recording.bumpNotes();
    void refreshMultiOps();
  }

  /** 试听候选排序表(说话人 → 声纹最像此人的 seq 降序;后端纯读嵌入缓存)。
      现场会教训(2026-09-05 用户实报):最长的段往往是多人来回对话,按"最像"排,
      混了多人的段声纹偏离质心自然沉底。 */
  let clipRanks = $state<Record<string, number[]>>({});
  $effect(() => {
    const forId = id;
    getClipRanks(forId)
      .then((r) => {
        if (forId === id) clipRanks = r;
      })
      .catch(() => {
        if (forId === id) clipRanks = {};
      });
  });

  /** 该说话人的试听候选:声纹最像此人的 5 段;无嵌入缓存回落最长 5 段
      (修订稿一律回落到源段,见下)。 */
  function previewClips(sid: string): { seq: number; start_ms: number; end_ms: number }[] {
    // **修订稿一律回落到源段**。修订稿的段落是「不连续源段的合并」,却只记一个
    // start_ms~end_ms 的大范围;照那个范围连续播,中间别人说的话会原样放出来。
    // 2026-08-20 在一篇真实笔记上实测:472 个段落里 466 个(99%)的时间范围内夹着
    // 其他说话人的段,最长的一个横跨 58.6 秒而自己只占 8 段。用户的观感就是
    // 「试听的这些样本不是同一个人」。源段(中位 4.5s)才是真实的单人音频区间。
    // 一波说话人:两个视图的 sid 都是原始稿说话人,直接取其原始段;修订稿旧文档
    // 映射不回 S 的遗留 id 才退回「段落 source_seqs 还原」。
    const direct = displaySegments.filter((s) => s.speaker === sid);
    const pool =
      direct.length > 0
        ? direct
        : (refined?.paragraphs ?? [])
            .filter((p) => p.speaker === sid)
            .flatMap((p) => p.source_seqs)
            .map((seq) => segBySeq.get(seq))
            .filter((s) => s !== undefined);
    // 优先按声纹相似度序(后端 note_clip_ranks;多人混杂段沉底);该人无排序数据
    // (无嵌入缓存/段太少)回落最长序。
    const rank = clipRanks[sid];
    if (rank && rank.length > 0) {
      const bySeq = new Map(pool.map((s) => [s.seq, s]));
      const ranked = rank.map((q) => bySeq.get(q)).filter((s) => s !== undefined);
      if (ranked.length > 0) {
        return ranked.slice(0, 5).map((s) => ({ seq: s.seq, start_ms: s.start_ms, end_ms: s.end_ms }));
      }
    }
    return pool
      .sort((a, b) => (b.end_ms - b.start_ms) - (a.end_ms - a.start_ms))
      .slice(0, 5)
      .map((s) => ({ seq: s.seq, start_ms: s.start_ms, end_ms: s.end_ms }));
  }
  /** 播放指定片段(面板片段列表点选;2026-08-30 起不再"再点一次换下一段")。 */
  function previewSpeakerClip(sid: string, seq: number) {
    const clips = previewClips(sid);
    const idx = clips.findIndex((c) => c.seq === seq);
    if (idx < 0 || !player) return;
    if (preview?.sid === sid && preview.seq === seq) {
      endPreview("toggle");
      return;
    }
    const seg = segBySeq.get(seq);
    if (!seg) return;
    // endMs 与 seek 同一套修正(codex P2):停止条件比较的是修正后的 playerMs,
    // 边界不修正会让试听提前一个首帧偏移量截停。
    const segSource = segSourceAt(seg.start_ms);
    lastAuditioned[sid] = seg.seq;
    preview = { sid, idx, seq, endMs: seekFix(Math.min(seg.end_ms, seg.start_ms + PREVIEW_MAX_MS), segSource) };
    armPreviewWatchdog(Math.min(seg.end_ms - seg.start_ms, PREVIEW_MAX_MS));
    // 只放这一段所在的那条轨。播放器是多轨混音的:不压另一条轨的话,同一时刻远端
    // (system)说的话会跟着一起响,听起来就是「试听的样本不是同一个人」。
    player.soloTrack(seg.source ?? null);
    player.seek(seekFix(seg.start_ms, segSource));
    player.play();
  }
  /** 「这段不是此人」:把该段单独拆成新说话人——段内混杂/别人插话时聚类拆不开,人来指。 */
  async function detachClip(_sid: string, seq: number) {
    const seg = segBySeq.get(seq);
    if (!seg) return;
    if (preview?.seq === seq) endPreview("detach");
    await setSegmentSpeaker(id, seq, seg.text, "new");
    refresh();
    recording.bumpNotes();
  }
  /** 单钮试听(兼容入口):播第一段;已在播则换下一段。 */
  function previewSpeaker(sid: string) {
    const clips = previewClips(sid);
    if (clips.length === 0) return;
    const idx = preview?.sid === sid ? (preview.idx + 1) % clips.length : 0;
    previewSpeakerClip(sid, clips[idx].seq);
  }
  /** 退出试听态:清状态并解除独奏(三条退出路径共用,漏一条就会把轨一直压着)。 */
  function endPreview(_reason = "unknown") {
    if (previewTimer) {
      clearTimeout(previewTimer);
      previewTimer = null;
    }
    preview = null;
    player?.soloTrack(null);
  }

  // 段尾自动停:只在试听态生效,停完清态(不影响用户随后正常播放)。
  $effect(() => {
    if (preview && playerPlaying && playerMs >= preview.endMs) {
      untrack(() => {
        player?.pause();
        endPreview("boundary");
      });
    }
  });
  // untrack 是本效应的命门(2026-08-22 定案的「试听不停」真凶):endPreview →
  // player.soloTrack(null) 会读播放器的 soloSource 等 $state,不隔离的话这些读取
  // 被追踪成本效应的依赖——试听一开始 soloTrack(源) 改了 soloSource,本效应立即
  // 误触发,把试听态+看门狗一并清掉(且不暂停),停止边界连根消失,整篇一直放。
  // 三轮边界修复(armed/onUserPause/墙钟看门狗)全被它一招团灭,埋点日志实锤:
  // preview 建立 1ms 后 endPreview reason=note-change。
  $effect(() => {
    void id;
    untrack(() => endPreview("note-change"));
  });

  /** 原始稿各说话人的段数：说话人条按此排序，并折叠只出现 1 段的碎片。 */
  const segCounts = $derived.by(() => {
    const c: Record<string, number> = {};
    for (const s of displaySegments) {
      if (s.speaker) c[s.speaker] = (c[s.speaker] ?? 0) + 1;
    }
    return c;
  });

  function durationSecs(n: Note): number | null {
    // 活跃时长优先：段落时间轴最大 end_ms（与转写时间戳/录制计时一致，不含暂停）；
    // 无段落回退墙钟时长。
    if (n.segments.length > 0) {
      return Math.floor(Math.max(...n.segments.map((s) => s.end_ms)) / 1000);
    }
    if (n.meta.ended_at && n.meta.started_at) {
      const d = (new Date(n.meta.ended_at).getTime() - new Date(n.meta.started_at).getTime()) / 1000;
      return isNaN(d) ? null : Math.max(0, Math.floor(d));
    }
    return null;
  }

  // ── P3 日历行:权限态 + 改选候选下拉。动作成功后重拉 note(meta.calendar 已变)。
  // 权限查询在组件初始化时发一次(本页无 onMount,顶层调用等价)。
  let calPerm = $state("unavailable");
  let calMenuOpen = $state(false);
  let calCandidates = $state<CalendarCandidate[]>([]);
  let calBusy = $state(false);
  void noteCalendarPermission()
    .then((p) => (calPerm = p))
    .catch(() => {});
  async function openCalMenu() {
    if (calBusy) return;
    calMenuOpen = !calMenuOpen;
    if (!calMenuOpen) return;
    try {
      calCandidates = await listCalendarCandidates(id);
    } catch {
      calCandidates = [];
    }
  }
  async function pickCalEvent(eventId: string | null) {
    if (calBusy) return;
    calBusy = true;
    try {
      await setNoteCalendarEvent(id, eventId);
      await refresh();
    } catch (e) {
      error = `${e}`;
    }
    calMenuOpen = false;
    calBusy = false;
  }
  // P2a 手动重推身份:后台跑,identify_done 事件会刷新收件箱;错误就地横幅。
  let identifying = $state(false);
  async function rerunIdentify() {
    if (identifying) return;
    identifying = true;
    try {
      await identifyNote(id);
    } catch (e) {
      error = `${e}`;
    }
    identifying = false;
  }

  function fmtCalTime(ms: number): string {
    const d = new Date(ms);
    const pad = (n: number) => String(n).padStart(2, "0");
    return `${pad(d.getHours())}:${pad(d.getMinutes())}`;
  }

  async function refresh() {
    // 并行发起，note 失败才是真正的加载失败；refined/people 是增值层，取不到静默降级。
    const notePromise = getNote(id);
    const refinedLoad = loadRefined(id);
    const peoplePromise = listPeople().catch(() => []);
    try {
      note = await notePromise;
      error = "";
    } catch (e) {
      error = t("common.loadFailed", { e });
    }
    await refinedLoad;
    people = await peoplePromise;
  }

  // 轨道获取独立于 refresh:canEdit 必须在 await 之前同步读到才会成为 effect 依赖
  // ——否则本页停录后(id/notesVersion 都没变)effect 不重跑,播放器永远不出现。
  // await 后校验 id 未变,防快速切换笔记时旧响应覆盖新页面的轨道(错音频)。
  // 音频是增值层:取失败(旧笔记无音频/后端异常)静默按无轨道处理,不打扰主内容。
  /** 转码完成计数:transcode_done 事件驱动音轨重拉(停录后立即点播放的竞态窗口:
      转码完成瞬间源 WAV 被删,播放器握着失效引用会无声播放,此处自动切到 m4a)。 */
  let tracksVersion = $state(0);
  $effect(() => {
    const un = onTranscodeDone((e) => {
      if (e.note_id === id) tracksVersion++;
    });
    return () => {
      un.then((f) => f());
    };
  });

  $effect(() => {
    const forId = id;
    void recording.notesVersion;
    void tracksVersion;
    if (!canEdit) {
      tracks = [];
      return;
    }
    noteAudioInfo(forId)
      .then((t) => {
        if (forId === id) tracks = t;
      })
      .catch(() => {
        if (forId === id) tracks = [];
      });
  });

  // 成品轨读数(二期):与源轨列表同依赖同节奏重拉(transcode_done/停录都会 bump
  // tracksVersion,补生成完成也借同一计数触发)。取失败按无成品轨处理,增值层不打扰。
  // pending 落定后回落判定才生效。
  $effect(() => {
    const forId = id;
    void recording.notesVersion;
    void tracksVersion;
    mixedPending = true;
    mixedPlaybackInfo(forId)
      .then((i) => {
        if (forId === id) {
          mixedInfo = i;
          mixedPending = false;
        }
      })
      .catch(() => {
        if (forId === id) {
          mixedInfo = null;
          mixedPending = false;
        }
      });
  });

  // mixed 不可用即强制回落 dual(轨被删/变不可信/切到无成品轨的笔记/读数失败),
  // 装载数组的二选一表达式因此永远拿不到失效的 mixed。读数进行中(mixedPending)
  // 不判:b 档默认成品轨时,复位刚清掉的 mixedInfo 是"未知"不是"确无",抢跑会把
  // 默认成品轨秒降回双轨且无人再升回(终审回归实锤)。判定逻辑抽为 shouldFallbackToDual 配单测;
  // mixedPending 先于复位置位依赖本文件 effect 声明序——fetch effect 在前,勿调换。
  $effect(() => {
    if (shouldFallbackToDual(mixedPending, playbackScheme, mixedInfo)) {
      switchScheme("dual");
    }
  });

  function switchScheme(s: "dual" | "mixed") {
    if (s === playbackScheme) return;
    pendingResume = { ms: playerMs, playing: playerPlaying };
    playbackScheme = s;
  }

  /** AudioPlayer 每次装载完成回调:有待恢复现场就 seek 回同一时刻(越界即放弃,
      如切到时长更短的轨);进页首次装载 pendingResume 为 null,自然 no-op。 */
  function onPlayerLoaded() {
    // 从迷你条点标题回到本页:后端新内核从 0/paused 起(player.rs 里 Core 写死
    // cursor=0),位置不会自动续上,必须显式 seek。复用 pendingResume 通路,
    // 不新造机制。只在会话确实是本篇笔记时才接管。
    if (!pendingResume && playback.session?.noteId === note?.meta.id) {
      pendingResume = { ms: playback.currentMs, playing: playback.playing };
    }
    const r = pendingResume;
    pendingResume = null;
    if (!r || !player) return;
    if (r.ms > 0 && r.ms < player.durationMs()) {
      player.seek(r.ms);
      if (r.playing) player.play();
    }
  }

  // 相关笔记:增值层,取失败静默按空。id 切换即重取。
  $effect(() => {
    const forId = id;
    noteRelated(forId)
      .then((r) => {
        if (forId === id) related = r;
      })
      .catch(() => {
        if (forId === id) related = [];
      });
  });

  // 实体高亮点击导航:局部 ent_N → 全局 id(人 person_id / 非人 e:名)。增值层,
  // 取失败/无映射静默按空——旧笔记或图谱未就绪时高亮退回 Phase2a 的纯文本不可点。
  let entityLinks = $state<Record<string, EntityLink>>({});
  $effect(() => {
    const forId = id;
    noteEntityLinks(forId)
      .then((links) => {
        if (forId !== id) return;
        const m: Record<string, EntityLink> = {};
        for (const l of links) m[l.local_id] = l;
        entityLinks = m;
      })
      .catch(() => {
        if (forId === id) entityLinks = {};
      });
  });
  function gotoEntity(eid: string) {
    const link = entityLinks[eid];
    if (!link) return;
    if (link.is_person) goto("/speakers/" + link.global_id);
    else goto("/graph?e=" + encodeURIComponent(link.global_id));
  }

  /** 段落内实体片段 → 实体名(tooltip 用):从本篇 refined.entities 按局部 id 查。 */
  const entityName = $derived.by(() => {
    const m: Record<string, string> = {};
    for (const e of refined?.entities ?? []) m[e.id] = e.name;
    return (eid: string) => m[eid] ?? "";
  });

  // id 切换：无条件复位一切编辑态 + Aing 视图态（否则会短暂展示上一篇笔记的修订稿/进度）。
  // 同时清空 note/error：切换到长会议时后端 load 可能耗时数百毫秒，不清空会一直挂着
  // 上一篇的正文直到新数据整页跳变（观感=点了没反应、卡一下），清空后立即出加载态。
  // 只在 id 变化时清；编辑后的 refresh() 不经此处，不会闪屏。
  $effect(() => {
    void id;
    // 落盘先于复位:flushRefined 内部用 loadedRefinedId(不是下面即将复位的
    // refined/id)找旧笔记,必须在清空编辑态之前调用。
    //
    // 必须 untrack:refinedEditor 是 bind:this 的 $state,裸读会让本 effect 除 id 外
    // 还依赖它,于是有修订稿的笔记进入自毁循环——编辑器挂载使 refinedEditor 由 null
    // 变为实例 → effect 重跑 → note/refined 被清空 → 编辑器卸载 → refinedEditor 回到
    // null → effect 再跑……页面永远停在「加载中…」,且全程不报错(2026-08-02 定位)。
    untrack(() => refinedEditor?.flushRefined(true));
    note = null;
    error = "";
    editing = false;
    segMenuPop = null;
    segDeletePop = null;
    refined = null;
    // 换了笔记,上一篇的"手里这份可能过期"作废;新一篇由自己的取稿重新判定。
    refinedStale = false;
    syncedRefined = null;
    syncedEditor = null;
    refining = false;
    refineRunFailed = false;
    refineErr = "";
    retranscribing = false;
    retransStage = "";
    retransConfirm = false;
    retransErr = "";
    retransEventSeen = false;
    mixedReason = null;
    playbackScheme = defaultPlayback;
    pendingResume = null;
    mixedInfo = null;
    regenStage = null;
    regenErr = "";
    viewMode = "refined";
    // 换笔记清空试听记录(Codex P2):各篇 seq 都从 0 起,不清会把上一篇听过的 seq 当成
    // 本篇的 audited_seq 传给关联,让没听过的段冒充"用户确认过"进样本。
    lastAuditioned = {};
    cancelHideEntityPop();
    entityPop = null;
    refinedBadgePop = null;
    refinedSaveErr = "";
    exportMenuOpen = false;
    exportMsg = "";
    // 圈选游标是会话态且以毫秒记在**本篇**时间轴上:不清的话切到别篇会沿用旧值,
    // 导出静默取错区间(Codex 审出)。
    rangeStartMs = null;
    rangeEndMs = null;
    rangeDragging = false;
    rangeHintSeq = null;
    rangeHintEdge = null;
    clearTimeout(rangeHintTimer);
    // 剪辑表/试听排序属于上一篇:先清空,新一篇由各自拉取 effect 填(id 快照防旧响应)。
    edits = [];
    editsManage = false;
    editsErr = "";
    lastCut = null;
    clearTimeout(undoTimer);
    clipRanks = {};
  });

  // Aing 进度事件：按 id 注册/解绑（切页时旧监听必须解绑，否则会用旧 note_id 的事件误刷当前页）。
  // running 置 refining=true；stage="all" 是整体完成信号，done/failed 都要重新拉取 refined 并复位。
  $effect(() => {
    const forId = id;
    let unlisten: (() => void) | null = null;
    let disposed = false;
    // 只有**终态**事件能作废补问的快照:补问可能比终态事件还晚落地(后端先发
    // all/done、再把 id 从在跑集合里摘掉),晚到的 true 会把页面永久钉在"整理中"。
    // 反过来,filter/recluster/llm 这些中间事件不能作废快照——它们都不置
    // refining=true,执行体没配全时后面也不会再有 running 事件,作废了横幅就会在
    // 整理还没结束时冒出来(Codex P2 两轮)。
    let sawTerminal = false;
    refineStatusKnown = false;
    onRefine((e) => {
      if (e.note_id !== forId) return;
      if (e.state === "running") {
        refining = true;
        refineRunFailed = false;
      }
      if (e.stage === "all" && (e.state === "done" || e.state === "failed")) {
        sawTerminal = true;
        refineRunFailed = e.state === "failed";
        // refining 要等新稿到手再落:进页时缓存的那份 refined 还是跑之前的
        // (stages.llm = "off"),先落 refining 会让「这场没做 AI 整理」闪一下、
        // 还能被点到重跑。loadRefined 自己吞异常,不会留下未处理的 rejection。
        void loadRefined(forId).finally(() => {
          if (forId === id) refining = false;
        });
      }
    })
      .then((u) => {
        if (disposed) u();
        else unlisten = u;
        // 补问在途状态:running 事件是易失的,进页晚了就再也收不到,只看事件会把
        // "正在整理"误判成"没在整理"——而整理途中 stages.llm 本就是 "off",误判会让
        // 「这场没做 AI 整理」的横幅在整理途中冒出来(Codex P2)。
        // 必须等监听挂到位之后再问,否则查询与订阅两头并发,状态可能在缝隙里漏掉。
        return noteRefining(forId);
      })
      .then(async (r) => {
        if (disposed || forId !== id || sawTerminal) return;
        if (!r) {
          // 快照说"没在跑":这一场可能恰好在初次 getRefined 之后、监听装好之前
          // 跑完,那条终态事件谁也没接到,缓存里还是 stages.llm="off" 的旧稿——
          // 直接放行会让整理成功的笔记显示「这场没做 AI 整理」(Codex P2 八轮)。
          // 重取一次再放行(refineStatusKnown 在下面的 finally 里置,会等这一步)。
          await loadRefined(forId);
          return;
        }
        refining = true;
        // 从快照取到的"在跑"必须能自愈:终态事件可能早于后端把 id 从在跑集合
        // 里摘掉而发出,恰好在那一瞬订阅的话,快照拿到 true 却再也等不到事件
        // (摘除本身不发前端事件),页面会永久卡在"整理中"(Codex P2 五轮)。
        // 因此隔几秒复核一次,直到后端说不在跑或终态事件到达。
        void (async () => {
          while (!disposed && forId === id && !sawTerminal) {
            await new Promise((done) => setTimeout(done, REFINE_RECHECK_MS));
            if (disposed || forId !== id || sawTerminal) return;
            try {
              if (await noteRefining(forId)) continue;
              // 走到这里说明终态事件被错过了,refined 还是跑之前那份
              // (stages.llm 仍是 "off")。不重取的话,整理明明成功了,页面
              // 却继续显示「这场没做 AI 整理」并邀请再跑一次(Codex P2 六轮)。
              // refining 等新稿到手再落,免得中间闪一下(七轮);await 在 try 内,
              // 失败由下面的 catch 收掉,不留未处理的 rejection(八轮)。
              await loadRefined(forId);
              if (!disposed && forId === id) refining = false;
              return;
            } catch {
              return; // 查不动就不再复核:事件通道仍在,不至于全无出路
            }
          }
        })();
      })
      .catch(() => {})
      // 无论成败都算"已确定":查询失败时按事件为准,总不能永远不提示。
      .finally(() => {
        if (!disposed && forId === id) refineStatusKnown = true;
      });
    return () => {
      disposed = true;
      unlisten?.();
    };
  });

  // 文件重转写(三期):成品轨入口可用性(取失败按"未知原因"置灰,不悄悄放行)。
  // 回填在途任务的查询(retranscribeStatus)已挪进下方订阅 effect——见 Fix 3
  // (codex 第三轮)注释:必须等 listen() 的 promise resolve 之后再发起,不能像
  // 这里一样与订阅 effect 各自独立跑,否则两个 effect 谁先谁后不确定,查询可能
  // 在监听器挂到位之前就已发出/返回。
  $effect(() => {
    const forId = id;
    mixedInputStatus(forId)
      .then((r) => {
        if (forId === id) mixedReason = r;
      })
      .catch(() => {
        if (forId === id) mixedReason = t("notes.retrans.mixedCheckFailed");
      });
  });

  // 重转写进度事件：按 id 注册/解绑（同款 onRefine 套路）。running 置 retranscribing=true；
  // 非 running（ok/error）都是任务终态，复位并按结果刷新（ok 才重拉，error 提示不改数据）。
  //
  // Fix 3(codex 第三轮):回填查询(退回 retranscribeStatus 判在途任务，事件只覆盖
  // 在页期间，若切走再切回需要靠这个补一次状态)从独立 effect 挪到这里，且必须在
  // `onRetranscribe(...).then((u) => ...)` 里 listen() 的 promise 真正 resolve
  // 之后才发起——旧版两个 effect 互相独立，挂载/切页瞬间任务恰好结束时可能撞上：
  // 快照查到 running=true，而终态事件在 listen() 异步注册完成前就已发出，
  // 监听器还没挂上，事件被漏收，没人再清 retranscribing，永久卡住（retransEventSeen
  // 旗当时只盖住了 startRetranscribe 的 invoke 路径，管不到这条独立回填）。
  // 现在把查询挪到 listen() resolve 之后：此后任何终态事件都必被 handler 收到并
  // 置 retransEventSeen=true，下面 `if (retransEventSeen) return` 守卫自动让路，
  // 不会再被这条迟到快照覆盖成"重转写中"后无人纠正。
  $effect(() => {
    const forId = id;
    let unlisten: (() => void) | null = null;
    let disposed = false;
    onRetranscribe((e) => {
      if (e.note_id !== forId) return;
      retransEventSeen = true;
      if (e.state === "running") {
        retranscribing = true;
        retransStage = e.stage;
        return;
      }
      retranscribing = false;
      retransConfirm = false;
      if (e.state === "ok") {
        refresh();
        recording.bumpNotes();
      } else if (e.message) {
        retransErr = t("notes.retrans.failed", { e: e.message });
      }
    }).then((u) => {
      if (disposed) {
        u();
        return;
      }
      unlisten = u;
      // 监听器已挂载完成，此后任何终态事件都保证被上面的 handler 收到——现在才
      // 发起回填查询，杜绝"快照说 running，终态事件却在监听器就位前漏发"的窗口。
      retranscribeStatus()
        .then((s) => {
          if (disposed || forId !== id) return;
          // 此刻已经收到过 onRetranscribe 事件（可能已经是终态 ok/error）——事件
          // 通道已经接管了 retranscribing 的真相，这条迟到的快照不再可信，让路。
          if (retransEventSeen) return;
          if (s && s.note_id === forId) {
            retranscribing = true;
            retransStage = s.stage;
          }
        })
        .catch(() => {});
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  });

  // 补生成进度事件(二期):订阅→回填的顺序纪律与上方重转写 effect 相同(Fix 3),
  // 回填查询必须等 listen() resolve 之后发起,终态事件才不会漏在监听器就位前。
  $effect(() => {
    const forId = id;
    let unlisten: (() => void) | null = null;
    let disposed = false;
    let regenEventSeen = false;
    onMixedRegen((e) => {
      if (e.note_id !== forId) return;
      regenEventSeen = true;
      if (e.state === "running") {
        regenStage = e.stage;
        return;
      }
      regenStage = null;
      if (e.state === "ok") {
        // 借 tracksVersion 触发 mixedPlaybackInfo 重拉(同一依赖组)。
        tracksVersion++;
      } else if (e.message) {
        regenErr = t("notes.mix.genFailed", { message: e.message });
      }
    }).then((u) => {
      if (disposed) {
        u();
        return;
      }
      unlisten = u;
      mixedRegenStatus()
        .then((s) => {
          if (disposed || forId !== id || regenEventSeen) return;
          if (s === forId) regenStage = "mix";
        })
        .catch(() => {});
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  });

  async function startRegen() {
    regenErr = "";
    regenStage = "mix"; // 立即置忙,快失败时在 catch 复位(同 startRetranscribe 竞态样板)
    try {
      await regenerateMixed(id);
    } catch (e) {
      regenStage = null;
      regenErr = t("notes.mix.genFailed", { message: String(e) });
    }
  }

  // 后端自动改名(LLM 主题标题):只改标题字段,不整页 refresh(编辑中也安全)。
  $effect(() => {
    const forId = id;
    let un: (() => void) | null = null;
    let disposed = false;
    onNoteRenamed((e) => {
      if (e.note_id === forId && note) note.meta.title = e.title;
    }).then((u) => {
      if (disposed) u();
      else un = u;
    });
    return () => {
      disposed = true;
      un?.();
    };
  });
  // 跨轨时基纠正完成:段的时间戳(以及 mic/system 的行序)在后端已换成新时基,整页重拉。
  // 装载音轨后才算得出,故前端不会主动重拉;编辑中跳过,等编辑结束的刷新 effect 带上。
  $effect(() => {
    const forId = id;
    let un: (() => void) | null = null;
    let disposed = false;
    onNoteRealigned((e) => {
      if (e.note_id === forId && !editing) refresh();
    }).then((u) => {
      if (disposed) u();
      else un = u;
    });
    return () => {
      disposed = true;
      un?.();
    };
  });

  // 刷新：标题重命名进行中跳过（编辑态是 effect 依赖，编辑结束会自动重跑并刷新）。
  // 原始稿/精修稿编辑器的常驻编辑态不在此处拦截——两者的数据同步 effect 各自
  // 内部靠 hasFocus() 守卫,外部刷新不会吹掉正在输入的段/段落(与精修稿同哲学)。
  $effect(() => {
    void id;
    void recording.notesVersion;
    if (editing) return;
    exportMsg = "";
    refresh();
  });

  // 挂载取一次设置：确定默认回放方案 + 会后 AI 就绪口径。增值层,取失败按 a(双轨)
  // 不打扰;AI 两项取失败按"开关关着"处理,即不提示——宁可不提示,不可误报。
  void getSettings()
    .then((s) => {
      defaultPlayback = schemeToDefaultPlayback(s.audio_scheme);
      playbackScheme = defaultPlayback;
      refineOn = s.refine_enabled;
      refineConfigured = refineReady(s);
    })
    .catch(() => {});
  // 重转写要的不止 FireRed:双轨重转写还要用 Silero 切段,少一个都跑不起来
  // (设置页允许单独删任一件,Codex P2)。
  void modelsStatus()
    .then((m) => {
      const have = (id: string) => m.artifacts.some((a) => a.id === id && a.present);
      repairReady = have("firered") && have("vad");
    })
    .catch(() => {});

  /** 这场里"说了好几秒却几乎没转出字"的段。它们的音频是好的(实测同段换引擎能解出
      连贯中文),所以这不是噪声而是内容丢失——看得见才修得了。 */
  // 必须连**被抑制的段**一起数(Codex P2):那个 14.6 秒只出一个句号的段,正是被
  // 「无内容过滤」挪进 suppressed_segments 的——只扫 segments 会一个都数不到,
  // 恰好把最典型的事故形态漏掉。判据本身(≥3s 且有效字符 ≤2)会把外语过滤、回声
  // 撤回这类"有内容只是被规则拿掉"的段自然排除在外。
  const lowDensity = $derived(
    lowDensityStat([...(note?.segments ?? []), ...(note?.suppressed_segments ?? [])]),
  );
  const offerBetterEngine = $derived(
    shouldOfferBetterEngine(lowDensity, {
      noteEngine: note?.meta.asr_engine ?? undefined,
      ready: repairReady,
      // 门禁与页面上那个重转写按钮完全一致:录制中/精修中/本篇已有重转写在跑/本篇
      // 未完成时后端都会拒,提供一个必被拒的按钮只是骗点击(Codex P2)。
      //
      // 已知局限(与既有重转写按钮同款,刻意不在此扩大改动面):页面只看得到本篇的
      // 状态,别的笔记正在重转写、本篇转码未完、成品轨在重生成、模型下载/迁移在跑
      // 这些全局态它不知道,按钮仍会亮。真点了后端会拒,错误经 retransErr 原样显示
      // (说明是哪一种占用)。要根治得让后端出一个"现在能不能开重转写"的查询,
      // 那是这两个入口共同的事,该单独做。
      actionable:
        !retranscribing &&
        !refining &&
        !recording.isLive &&
        note?.meta.state === "complete" &&
        // 音频被留存策略清掉的老笔记只剩文字:重转写没有源可读,点了必然失败
        // (Codex P2)。tracks 为空即"盘上没音频"。
        tracks.length > 0,
    }),
  );

  /** 用 FireRed 重转写本篇。**只作用于这一次**,不动全局设置(Codex P1):改设置既
      不保证这次生效(云端模式下重转写会走云端批式),又会清缓存并异步预载,与紧接着
      启动的重转写各建一份 1.2G 模型。想让以后录制也用它,去设置里改。 */
  /** 「就此结束」:中断笔记免续录收尾。成功后 refresh 拉新 meta(complete),横幅
      随 state 消失;Aing/转码在后端继续,进度走既有 refine 事件。失败进横幅重试。 */
  let finalizing = $state(false);
  let finalizeErr = $state("");
  async function finalizeNow() {
    if (finalizing) return;
    finalizing = true;
    finalizeErr = "";
    try {
      await finalizeInterruptedNote(id);
      await refresh();
      // 侧栏列表不订阅本页状态:bump notesVersion 让它的 $effect 重拉,
      // 「已中断」徽章才会跟着 meta 变化消失(与编辑页保存后同一套路)。
      recording.bumpNotes();
    } catch (e) {
      finalizeErr = String(e);
    } finally {
      finalizing = false;
    }
  }

  async function repairWithFirered() {
    if (repairing) return;
    repairing = true;
    try {
      await startRetranscribe("dual", "firered");
    } finally {
      repairing = false;
    }
  }

  /** 「这场没做 AI 整理」:后端未配全时是完全静默降级的,不提示就等于没发生
      (2026-08-13 起连断四天六场没被发现)。判定见 $lib/aiSkipHint。 */
  const aiSkipped = $derived(
    aiSkipHint({
      llmStage: refined?.stages.llm,
      noteComplete: note?.meta.state === "complete",
      // 未确定、或修订稿正在重取期间,一律按"可能在跑"处理:宁可晚一拍提示,
      // 不可拿一份随时会被换掉的旧稿下结论。
      running: refining || !refineStatusKnown || refinedLoading > 0 || refinedStale,
      refineEnabled: refineOn,
      ready: refineConfigured,
    }),
  );

  // ── 波形音轨:按音频总长等分 260 桶,取桶内段落 rms 峰值。观感三件套(首版全高
  //    平顶像方块阵,冒烟反馈"不像声音波形"):①按本条录音的 rms 峰值归一(AGC 后
  //    普遍 0.2+,固定封顶会齐刷刷顶格)+γ0.7 拉开动态;②确定性抖动纹理(段级 rms
  //    在段内是平台,乘 ±18% 伪随机破平顶,真实响度包络仍在);③桶多条细。 ──
  const WAVE_BARS = 260;
  // 装载数组二选一(mixPlayback.test.ts 锁死):mixed 本就是 mic+system 混出来的,
  // 与源轨同时装载 = 三重叠加(音量翻倍、听感像回声)。不可信轨(untrusted)不进装载。
  const playerTracks = $derived(
    playbackScheme === "mixed" && mixedInfo?.track && !mixedInfo.untrusted ? [mixedInfo.track] : tracks,
  );
  const audioTotalMs = $derived(playerTracks.reduce((m, t) => Math.max(m, t.offset_ms + t.duration_ms), 0));
  const waveform = $derived.by(() => {
    if (audioTotalMs <= 0) return [];
    // 真实音频波形优先(后端转码预计算/懒回填):有声音就有波形,与录音机直觉一致。
    // 多轨(mic/system)按全局时间轴对位取 max;真实数据自带起伏,不加抖动纹理。
    const real: number[] = new Array(WAVE_BARS).fill(0);
    let hasReal = false;
    for (const t of playerTracks) {
      if (!t.waveform?.length) continue;
      hasReal = true;
      const n = t.waveform.length;
      for (let j = 0; j < n; j++) {
        const ms = t.offset_ms + ((j + 0.5) / n) * t.duration_ms;
        const g = Math.max(0, Math.min(WAVE_BARS - 1, Math.floor((ms / audioTotalMs) * WAVE_BARS)));
        const v = t.waveform[j] / 255;
        if (v > real[g]) real[g] = v;
      }
    }
    if (hasReal) {
      const peak = Math.max(0.12, ...real);
      return real.map((v) => (v > 0 ? Math.min(1, Math.pow(v / peak, 0.7)) : 0));
    }
    // 回退:按转写段落 rms 聚合的包络(无波形数据的旧笔记)。
    const bars: number[] = new Array(WAVE_BARS).fill(0);
    for (const s of displaySegments) {
      const r = s.rms ?? 0.05;
      const b0 = Math.max(0, Math.min(WAVE_BARS - 1, Math.floor((s.start_ms / audioTotalMs) * WAVE_BARS)));
      const b1 = Math.max(b0, Math.min(WAVE_BARS - 1, Math.ceil((s.end_ms / audioTotalMs) * WAVE_BARS) - 1));
      for (let b = b0; b <= b1; b++) bars[b] = Math.max(bars[b], r);
    }
    const peak = Math.max(0.12, ...bars); // 下限防全场轻声被归一放大成满高噪声
    const jitter = (i: number) => {
      const x = Math.sin(i * 12.9898) * 43758.5453;
      return 0.82 + 0.36 * (x - Math.floor(x)); // 0.82..1.18,确定性(不随刷新跳变)
    };
    return bars.map((r, i) => (r > 0 ? Math.min(1, Math.pow(r / peak, 0.7) * jitter(i)) : 0));
  });

  /** 播放位置落在区间内的段(mic/system 可重叠,同帧可能多段)。 */
  const activeSeqs = $derived.by(() => {
    const s = new Set<number>();
    if (tracks.length === 0) return s;
    for (const seg of displaySegments) {
      // mixed 态段落边界与 seek 同一套修正(codex P2):live 成品轨里各源内容整体
      // 后移其首帧偏移,只修 seek 不修比较边界会让高亮/跟随提前一个偏移量。
      if (playerMs >= seekFix(seg.start_ms, seg.source) && playerMs < seekFix(seg.end_ms, seg.source)) {
        s.add(seg.seq);
      }
    }
    return s;
  });

  // ── 播放歌词式跟随(与录制页同一交互):当前段钉屏幕垂直中央、放大高亮;
  //    用户 wheel 上滑即暂停跟随,浮出「回到播放位置」;点击或重新播放恢复。 ──
  let transcriptEl = $state<HTMLElement | null>(null);
  let follow = $state(true);

  /** 最近的可滚动祖先(布局里的 .main);不硬编码布局选择器。 */
  function scrollParent(): HTMLElement | null {
    for (let p = transcriptEl?.parentElement; p; p = p.parentElement) {
      if (/(auto|scroll)/.test(getComputedStyle(p).overflowY)) return p;
    }
    return null;
  }

  /** 当前播放段(时间轴首个命中;mic/system 重叠时两段都高亮,居中锚定首个)。 */
  const currentSeq = $derived.by(() => {
    const first = displaySegments.find((s) => activeSeqs.has(s.seq));
    return first ? first.seq : null;
  });

  function centerCurrent() {
    if (currentSeq === null) return;
    document
      .querySelector(`[data-seq="${currentSeq}"]`)
      ?.scrollIntoView({ block: "center", behavior: "smooth" });
  }

  function resumeFollow() {
    follow = true;
    lastScrolledSeq = -1;
    centerCurrent();
  }

  let lastScrolledSeq = -1;
  // 按下播放 = 想跟着听:恢复跟随并立即居中一次。untrack 隔离 resumeFollow 内部
  // 读到的 currentSeq,否则播放推进会不断重跑本 effect,把用户的"暂停跟随"顶掉。
  $effect(() => {
    if (playerPlaying) untrack(resumeFollow);
  });
  $effect(() => {
    if (!playerPlaying || !follow || rangeDragging) return;
    if (currentSeq !== null && currentSeq !== lastScrolledSeq) {
      lastScrolledSeq = currentSeq;
      centerCurrent();
    }
  });

  // ── 圈选游标(导出范围):会话态,不落盘。拖动中与原始逐字稿联动——离游标
  //    最近的段滚到屏幕中央并高亮,松手后高亮片刻消散。 ──
  let rangeStartMs = $state<number | null>(null);
  let rangeEndMs = $state<number | null>(null);
  let rangeDragging = $state(false);
  let rangeHintSeq = $state<number | null>(null);
  /** 正拖的是哪枚游标:决定联动段的切线画在文字上方(start)还是下方(end)。 */
  let rangeHintEdge = $state<"start" | "end" | null>(null);
  let rangeHintTimer: ReturnType<typeof setTimeout> | undefined;
  function onRangeDrag(which: "start" | "end", ms: number) {
    rangeDragging = true;
    clearTimeout(rangeHintTimer);
    // 离游标最近的段:命中区间即距离 0;边界与播放高亮同一套 seekFix 修正。
    let best: number | null = null;
    let bestD = Infinity;
    for (const sg of displaySegments) {
      const s = seekFix(sg.start_ms, sg.source);
      const e = seekFix(sg.end_ms, sg.source);
      const d = ms < s ? s - ms : ms >= e ? ms - e + 1 : 0;
      if (d < bestD) {
        bestD = d;
        best = sg.seq;
        if (d === 0) break;
      }
    }
    if (best === null) return;
    // 联动的对象是原始逐字稿:正看修订稿就自动切过去(2026-09-04 用户点名)。
    // 切换后 .md-seg 由编辑器异步重建,此刻 querySelector 多半扑空——滚动交给
    // 下方高亮 effect 在 segmentsrendered 后补(scrollHintIntoView 幂等)。
    if (effectiveView === "refined") viewMode = "raw";
    rangeHintSeq = best;
    rangeHintEdge = which;
    scrollHintIntoView(best);
  }
  /** 把联动段滚到屏幕中央(每个 seq 只滚一次,拖动中同段的连续 move 不重复滚;
      behavior 用默认瞬时滚动——拖动是连续手势,平滑滚动追不上会一路抖)。 */
  let lastHintScrolled = -1;
  function scrollHintIntoView(seq: number) {
    if (lastHintScrolled === seq) return;
    const el = document.querySelector(`[data-seq="${seq}"]`);
    if (!el) return; // 还没渲染出来:等 segRenderTick 驱动高亮 effect 时重试
    lastHintScrolled = seq;
    el.scrollIntoView({ block: "center" });
  }
  function onRangeDragEnd() {
    rangeDragging = false;
    lastHintScrolled = -1;
    clearTimeout(rangeHintTimer);
    rangeHintTimer = setTimeout(() => {
      rangeHintSeq = null;
      rangeHintEdge = null;
    }, 1600);
  }
  /** 游标圈定的导出范围;未圈定或覆盖全程时为 null(导整篇)。 */
  function exportRange(): { start: number; end: number } | null {
    const total = player?.durationMs() ?? 0;
    if (total <= 0) return null;
    const s = Math.max(0, rangeStartMs ?? 0);
    const e = Math.min(total, rangeEndMs ?? total);
    if (e <= s || (s <= 0 && e >= total)) return null;
    return { start: Math.round(s), end: Math.round(e) };
  }

  // ── 音频删减(非破坏性剪辑表 edits.json):删掉圈内 → 播放跳过、导出剔除、
  //    逐字稿灰显;原始录音一个字节不动,随时可恢复。 ──
  let edits = $state<CutRange[]>([]);
  let editsManage = $state(false);
  let editsErr = $state("");
  /** 刚删的那段(可能已被后端吞并成更大的区间):一键撤销入口。
      瞬态:出现约 10s 后收走——管理浮层里永远能恢复,常驻的撤销只会把行越挤越长。 */
  let lastCut = $state<CutRange | null>(null);
  let undoTimer: ReturnType<typeof setTimeout> | undefined;
  /** 短时长(区别于 formatTs 的定长时间点):不足 1 小时 m:ss,否则 h:mm:ss。 */
  function fmtDur(ms: number): string {
    const s = Math.round(ms / 1000);
    const h = Math.floor(s / 3600);
    const m = Math.floor((s % 3600) / 60);
    const sec = String(s % 60).padStart(2, "0");
    return h > 0 ? `${h}:${String(m).padStart(2, "0")}:${sec}` : `${m}:${sec}`;
  }
  // 进页/切笔记拉一遍剪辑表(id 快照防快速切换时旧响应覆盖新页)。
  $effect(() => {
    const forId = id;
    getNoteEdits(forId)
      .then((l) => {
        if (forId === id) edits = l.cuts;
      })
      .catch(() => {
        if (forId === id) edits = [];
      });
  });
  async function deleteRange() {
    const r = exportRange();
    if (!r || !player) return;
    editsErr = "";
    try {
      const list = await addNoteCut(id, r.start, r.end, Math.round(player.durationMs()));
      edits = list.cuts;
      // 撤销目标 = 表里覆盖本次区间的那条(后端可能已把它与旧区间吞并)。
      lastCut = list.cuts.find((c) => c.start_ms <= r.start && c.end_ms >= r.end) ?? null;
      clearTimeout(undoTimer);
      undoTimer = setTimeout(() => (lastCut = null), 10_000);
      rangeStartMs = null;
      rangeEndMs = null;
    } catch (e) {
      editsErr = t("notes.edits.failed", { e });
    }
  }
  async function restoreCut(c: CutRange) {
    editsErr = "";
    try {
      const list = await removeNoteCut(id, c.start_ms, c.end_ms);
      edits = list.cuts;
      if (lastCut && lastCut.start_ms === c.start_ms && lastCut.end_ms === c.end_ms) lastCut = null;
      if (edits.length === 0) editsManage = false;
    } catch (e) {
      editsErr = t("notes.edits.failed", { e });
    }
  }
  /** 完全落在删减区间内的段(逐字稿灰显;跨界段不标——里面还有没删的话音)。 */
  const cutSeqs = $derived.by(() => {
    const s = new Set<number>();
    if (edits.length === 0) return s;
    for (const sg of displaySegments) {
      if (edits.some((c) => sg.start_ms >= c.start_ms && sg.end_ms <= c.end_ms)) s.add(sg.seq);
    }
    return s;
  });

  // wheel 上滑 = 主动离开(平滑滚动只产生 scroll 事件,不会误判);内容不足一屏不触发。
  $effect(() => {
    if (!transcriptEl) return;
    const sc = scrollParent();
    if (!sc) return;
    const onWheel = (e: WheelEvent) => {
      if (e.deltaY < 0 && playerPlaying && sc.scrollHeight > sc.clientHeight + 4) follow = false;
    };
    sc.addEventListener("wheel", onWheel, { passive: true });
    return () => sc.removeEventListener("wheel", onWheel);
  });

  /** mixed 态段落 seek 修正:live 成品轨里各源内容整体后移其首帧偏移(spec §口径差),
      点段落要把这段加回去,否则 system 段系统性偏 offset_sys。regen 轨按 offset_ms
      定位、seek_offset_ms 为空表 → 修正恒 0;dual 态原样返回。 */
  const seekFix = (ms: number, source?: string) =>
    playbackScheme === "mixed" ? ms + (mixedInfo?.seek_offset_ms[source ?? ""] ?? 0) : ms;

  /** 段落时间点反查所属源:调用方(onPlayFrom 回调链)只有毫秒数,而 seek 修正
      按源区分。点击值即段起点,半开区间查找是精确命中;refined 段落无 source,
      按其时间落点归到底层原始段,同一时间轴语义一致。 */
  const segSourceAt = (ms: number) =>
    displaySegments.find((s) => s.start_ms <= ms && ms < s.end_ms)?.source;

  /** 只需要 start_ms：原始段(SegmentRecord)与 Aing 段(RefinedParagraph)都结构兼容,共用同一播放逻辑。 */
  function playFrom(pos: { start_ms: number; source?: string }) {
    if (!player) return;
    const ms = seekFix(pos.start_ms, pos.source ?? segSourceAt(pos.start_ms));
    // 起点落在音频覆盖范围之外(该轨写失败提早停/音频比转写短):忽略点击,
    // 否则 seek 被钳到末尾、play 又视作"播完重来",会莫名跳回 0:00。
    if (ms >= player.durationMs()) return;
    player.seek(ms);
    player.play();
    resumeFollow(); // 点时间戳跳播 = 想跟着听
  }

  /** segments 模式徽章:按 seq 回查 note.segments 拿权威 speaker/source(与 note.speakers
      命名/关联人物一致)——NodeView 透传的 attrs 只是点击当下的渲染快照,不作数
      (MarkdownEditor 顶部注释同款告诫)。 */
  function segBadge(attrs: BadgeAttrs): { label: string; bg: string; ink: string } {
    const seg = note?.segments.find((s) => s.seq === attrs.seq);
    const speaker = seg?.speaker ?? null;
    const source = seg?.source ?? "mic";
    return {
      label: speakerLabel(speaker, source, note?.speakers ?? {}),
      bg: speakerColor(speaker, source, note?.speakers),
      ink: speakerInk(speaker, source, note?.speakers),
    };
  }

  /** 段级提交(见组件顶部保存生命周期契约):成功后必须先调 markSegmentSaved
      把 segBase 基线更新为刚落盘的值,再 await refresh()——否则 refresh 的
      setSegments 被 hasFocus() 守卫挡住时(常驻编辑态继续打字),同一段后续
      提交仍会带着旧的 expectedText,在后端撞 CAS 冲突。失败路径不调
      markSegmentSaved:refresh() 成功后 setSegments 会整体重建 segBase。 */
  async function doEditSegment(seq: number, expectedText: string, newText: string) {
    try {
      await editSegment(id, seq, expectedText, newText);
      segEditor?.markSegmentSaved(seq, newText);
      await refresh();
    } catch (err) {
      error = t("notes.detail.editFailed", { e: err });
      await refresh();
      // 冲突回滚:被拒的编辑必须立即还原,不能指望 hasFocus 守卫下的 effect
      // (WKWebView 下点击/失焦时序不保证 hasFocus() 已变 false)。成功分支不强制
      // ——markSegmentSaved 已确立基线,重建交给守卫下的 effect,避免打断连续编辑。
      syncSegments(true);
    }
  }

  /** 浮层没有整段对象,按 seq 查;canEdit 复核见调用处(onBadgeClick/onDeleteClick
      的页面回调)——NodeView 的 canEdit 是构造时快照,不能只信它。 */
  async function doDeleteSeg(seq: number) {
    segDeletePop = null;
    if (!canEdit) return;
    const seg = note?.segments.find((s) => s.seq === seq);
    if (!seg) return;
    try {
      await deleteSegment(id, seg.seq, seg.text);
      await refresh();
      // 离散命令(点删除确认按钮):WKWebView 下按钮点击不转移焦点,hasFocus()
      // 恒 true,守卫下的 effect 吞掉重建会留幽灵段在屏——强制入口。
      syncSegments(true);
    } catch (e) {
      error = t("common.deleteFailed", { e });
      await refresh();
      syncSegments(true);
    }
  }

  async function doSetSpeaker(seq: number, speakerId: string) {
    segMenuPop = null;
    if (!canEdit) return;
    const seg = note?.segments.find((s) => s.seq === seq);
    if (!seg) return;
    try {
      await setSegmentSpeaker(id, seg.seq, seg.text, speakerId);
      await refresh();
      // 离散命令(点说话人菜单项):同 doDeleteSeg,强制入口绕开 WKWebView 下恒真的
      // hasFocus() 守卫,否则徽章不刷新。
      syncSegments(true);
    } catch (e) {
      error = t("notes.detail.setSpeakerFailed", { e });
      await refresh();
      syncSegments(true);
    }
  }

  /** 批量改派(甲作用范围/乙勾选共用):moves 逐段带 expected_text CAS。 */
  async function doSetSpeakerBatch(seqs: number[], speakerId: string) {
    segMenuPop = null;
    if (!canEdit || seqs.length === 0) return;
    const bySeq = new Map((note?.segments ?? []).map((s) => [s.seq, s]));
    const moves: [number, string][] = [];
    for (const q of seqs) {
      const seg = bySeq.get(q);
      if (seg) moves.push([q, seg.text]);
    }
    try {
      if (moves.length === 1) {
        await setSegmentSpeaker(id, moves[0][0], moves[0][1], speakerId);
      } else {
        await setSegmentsSpeaker(id, moves, speakerId);
      }
      await refresh();
      syncSegments(true);
    } catch (e) {
      error = t("notes.detail.setSpeakerFailed", { e });
      await refresh();
      syncSegments(true);
    }
  }

  // ── 乙:勾选多段模式(2026-08-22 批量改说话人) ──
  let selMode = $state(false);
  let selected = $state<number[]>([]);
  let lastSel: number | null = null;
  let selPick = $state<DOMRect | null>(null);
  function exitSelMode() {
    selDeleteConfirm = false;
    selMode = false;
    selected = [];
    lastSel = null;
    selPick = null;
  }
  function toggleSel(seq: number, shiftKey: boolean) {
    if (shiftKey && lastSel !== null) {
      const range = seqRange(displaySegments, lastSel, seq);
      selected = [...new Set([...selected, ...range])];
    } else if (selected.includes(seq)) {
      selected = selected.filter((q) => q !== seq);
    } else {
      selected = [...selected, seq];
    }
    lastSel = seq;
  }
  // 勾选视觉:直接给编辑器 DOM 的 .md-seg 上/撤 sel-on 类(编辑器重建后由
  // displaySegments 依赖触发重涂;v1 取巧但零编辑器改造)。
  $effect(() => {
    void displaySegments;
    const on = new Set(selMode ? selected : []);
    const host = transcriptEl;
    if (!host) return;
    requestAnimationFrame(() => {
      for (const el of host.querySelectorAll<HTMLElement>(".md-seg")) {
        el.classList.toggle("sel-on", on.has(Number(el.dataset.seq)));
      }
    });
  });
  let selDeleteConfirm = $state(false);
  async function applySelDelete() {
    const bySeq = new Map((note?.segments ?? []).map((s) => [s.seq, s]));
    const moves: [number, string][] = [];
    for (const q of selected) {
      const seg = bySeq.get(q);
      if (seg) moves.push([q, seg.text]);
    }
    selDeleteConfirm = false;
    if (moves.length === 0) return;
    try {
      await deleteSegments(id, moves);
      await refresh();
      syncSegments(true);
    } catch (e) {
      error = `${e}`;
      await refresh();
      syncSegments(true);
    }
    exitSelMode();
  }

  async function applySelPick(sid: string) {
    const seqs = [...selected];
    selPick = null;
    await doSetSpeakerBatch(seqs, sid);
    exitSelMode();
  }

  /** note.segments → 编辑器文档重建。默认受 hasFocus() 守卫(常驻编辑态不打断
      正在输入)。force=true:调用方是用户刚下的离散命令(删段/改说话人/Esc 放弃/
      冲突回滚)——WKWebView(macOS 实际运行时)下点悬浮菜单按钮不转移焦点,
      hasFocus() 会恒为 true,若不强制,这些命令的重建会被守卫永久吞掉(幽灵段
      留屏、徽章不刷、被拒文本不还原)。丢光标是这类离散命令的预期代价。 */
  function syncSegments(force = false) {
    const ed = segEditor;
    if (!ed || !note || effectiveView === "refined") return;
    if (!force && ed.hasFocus()) return;
    ed.setSegments(displaySegments, note.speakers ?? {});
  }

  // note.segments 变化(载入/提交回执/冲突重载)→ 重建编辑器文档;输入中不打断
  // (与精修稿的同步 effect 同一套身份守卫哲学,这里只需 hasFocus,不需要
  // syncedRefined 那层对象身份闸门——setSegments 本身就是幂等重建,没有"刚保存
  // 成功回写又被自己吹掉"的问题,因为提交回执走 markSegmentSaved 而非整份换新
  // note 对象触发的重复 setSegments)。显式引用 displaySegments/note/note.speakers/
  // effectiveView/segEditor 以保持依赖追踪(syncSegments 内部重读同一批,读法
  // 重复但保证 $effect 依赖不漏)。
  $effect(() => {
    void displaySegments;
    void note?.speakers;
    void effectiveView;
    void segEditor;
    syncSegments();
  });

  // 播放高亮/AI 过滤灰显:NodeView DOM 不吃 Svelte class 指令,直接按 data-seq 贴类。
  // segRenderTick 依赖:.md-seg 由 setSegments 异步产出(含 onMount 补放路径),
  // 若只依赖 activeSeqs/discardedSeqs,精修稿切原始稿等场景下渲染未落地时
  // querySelectorAll 会拿空集,AI 灰显/tooltip 丢失直到下一次播放/切换才自愈
  // ——segmentsrendered 到达时 tick 一变,本 effect 重跑即可补上。
  $effect(() => {
    const el = transcriptEl;
    const active = activeSeqs;
    const discarded = discardedSeqs;
    const cut = cutSeqs;
    const hint = rangeHintSeq;
    const edge = rangeHintEdge;
    void segRenderTick;
    if (!el || effectiveView === "refined") return;
    for (const node of el.querySelectorAll<HTMLElement>(".md-seg")) {
      const seq = Number(node.dataset.seq);
      node.classList.toggle("playing", active.has(seq));
      node.classList.toggle("range-hint", seq === hint);
      node.classList.toggle("hint-start", seq === hint && edge === "start");
      node.classList.toggle("hint-end", seq === hint && edge === "end");
      node.classList.toggle("discarded", discarded.has(seq));
      node.classList.toggle("cut", cut.has(seq));
      if (cut.has(seq)) node.title = t("notes.seg.cutAway");
      else if (discarded.has(seq)) node.title = t("notes.seg.filtered");
      else node.removeAttribute("title");
    }
    // 拖游标触发的修订稿→原始稿自动切换:切换当拍 .md-seg 尚未渲染,联动段的
    // 首次滚动在这里补(segmentsrendered → segRenderTick 变化驱动;幂等,已滚过不重复)。
    if (hint !== null && rangeDragging) scrollHintIntoView(hint);
  });

  // Escape 还原桥接:segments 编辑器在段内 Escape 时派发 segescape(不冒泡,与
  // entityhover/entityleave 同款 capture-only 事件,见上方那处桥接 effect 的
  // 注释),页面收到后 refresh() 触发上面的数据同步 effect 重建文档,把被放弃的
  // 编辑还原为盘上原文——此时编辑器已在派发前同步 blur,hasFocus() 为 false,
  // 不会被同步 effect 的焦点守卫拦下(移交义务 2)。syncSegments(true) 是保险:
  // Esc 放弃本就该立即还原,blur 后 hasFocus() 通常已 false 不需要强制,但
  // WKWebView 语义下焦点转移时序不完全可信,强制入口兜底。
  //
  // segmentsrendered 桥接(同一处 addEventListener,capture 阶段):MarkdownEditor
  // 的 setSegments 实际完成文档替换后派发,页面收到即 segRenderTick++,驱动上方
  // 高亮 effect 重跑——见该 effect 注释。
  $effect(() => {
    const el = transcriptEl;
    if (!el) return;
    const onSegEscape = () => {
      refresh().then(() => syncSegments(true));
    };
    const onSegRendered = () => {
      // untrack 必须有:setSegments 的 dispatchEvent 是同步的,本监听器在数据同步
      // effect 的追踪上下文内执行;`segRenderTick++` 先读后写,裸写会把「读」登记成
      // 那个 effect 的依赖、「写」又立刻使它失效——effect 读写同一状态,每轮自增自
      // 触发,effect_update_depth_exceeded 死循环,整页 effect 树被护栏杀死、UI 失去
      // 响应(2026-08-09 定位,自 PR#65 即存在;被 hasFocus 守卫常态遮蔽,编辑器无
      // 焦点时的离散命令〔改说话人/删段〕才踩中)。
      untrack(() => (segRenderTick += 1));
    };
    el.addEventListener("segescape", onSegEscape, true);
    el.addEventListener("segmentsrendered", onSegRendered, true);
    return () => {
      el.removeEventListener("segescape", onSegEscape, true);
      el.removeEventListener("segmentsrendered", onSegRendered, true);
    };
  });

  function beginRename() {
    if (!note) return;
    editing = true;
    editingTitle = note.meta.title;
  }

  async function commitRename() {
    if (!editing || !note) return;
    editing = false;
    try {
      await renameNote(id, editingTitle);
      recording.bumpNotes();
    } catch (e) {
      error = t("notes.detail.renameFailed", { e });
    }
  }

  let exportMenuOpen = $state(false);

  async function doExport(format: "md") {
    exportMsg = "";
    if (!note) return;
    // await 前快照:保存对话框可以开很久,期间精修完成会把 effectiveView 从
    // 原始稿翻成修订稿(refine 事件监听常驻)、路由/标题也可能变——导出的必须
    // 是点击导出那一刻用户看到的那份。
    const noteId = id;
    const preferRefined = effectiveView === "refined";
    const range = exportRange(); // 同样在 await 前快照:对话框开着时游标可能被再拖
    try {
      // 保存对话框让用户挑落盘位置(冒烟反馈:旧流程写进数据目录再开 Finder,
      // 用户以为"只是打开了文件夹")。默认名 = 标题+录音时间;取消返回 null,
      // 静默收手不算失败。所见即所得:看着修订稿点导出就导修订稿。
      const dest = await save({
        defaultPath: exportFileName(note.meta.title, note.meta.started_at),
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
      if (!dest) return;
      const path = await exportNote(noteId, format, preferRefined, dest, range);
      exportMsg = t("notes.export.done", { path });
      // 清掉早前失败留下的红色横幅,避免"上面报失败下面报成功"并存。
      // 前缀判定:导出失败文案去掉 {e} 占位后的固定头部。
      if (error.startsWith(t("notes.export.failed", { e: "" }))) error = "";
    } catch (e) {
      error = t("notes.export.failed", { e });
    }
  }

  /** 导出成品轨音频:与 doExport 同款快照/保存对话框/提示纪律,扩展名取成品轨实际后缀。 */
  async function doExportAudio() {
    exportMsg = "";
    if (!note) return;
    const track = mixedInfo?.track;
    if (!track) return;
    const noteId = id;
    const ext = track.path.split(".").pop() || "m4a";
    const range = exportRange(); // await 前快照,同 doExport
    try {
      const dest = await save({
        defaultPath: exportFileName(note.meta.title, note.meta.started_at, ext),
        filters: [{ name: "Audio", extensions: [ext] }],
      });
      if (!dest) return;
      const path = await exportNoteAudio(noteId, dest, range);
      exportMsg = t("notes.export.done", { path });
      if (error.startsWith(t("notes.export.failed", { e: "" }))) error = "";
    } catch (e) {
      error = t("notes.export.failed", { e });
    }
  }

  async function doOpenDir() {
    try {
      await openNoteDir(id);
    } catch (e) {
      error = t("notes.dir.openFailed", { e });
    }
  }

  // 一波说话人(2026-08-21):说话人名字/关联都住在 speakers.json,重新 Aing 只重建
  // 段落文本,不再有"改名被冲掉"的问题——原二段确认(refineWouldLoseNames)删除。
  async function rerunRefine() {
    refineErr = "";
    refineRunFailed = false;
    refining = true; // 乐观置位:避免事件到达前的空隙内重复点击触发二次 Aing
    try {
      await refineNote(id);
    } catch (e) {
      refining = false;
      refineRunFailed = true;
      refineErr = t("notes.refine.rerunFailed", { e });
    }
  }

  /** 发起文件重转写:input 二选一(双轨/成品轨),破坏性(覆盖原始逐字稿),二段确认
      已在按钮态处理(retransConfirm),这里只管发起。
      Fix 4(codex 第二轮)快失败竞态:后端 worker 的终态事件(锁占用/模型缺失等
      快失败)可能在 `await retranscribeNote` 决议之前就已经到达并把 retranscribing
      清成 false——这里若无条件在 invoke 成功后把 retranscribing 置回 true,会覆盖
      掉已经正确落地的终态,永久卡在"重转写中"没人再纠正(事件只在状态变化时触发
      一次)。复用既有的 retransEventSeen 旗:invoke 前先清旗,invoke 成功后只有
      "还没见过任何事件"才自己置 running 态——running 事件与终态事件都已到过,
      说明事件通道已经接管了 retranscribing 的真相,以事件状态为准。 */
  async function startRetranscribe(input: "dual" | "mixed", engine?: string) {
    retransConfirm = false;
    retransErr = "";
    retransEventSeen = false;
    try {
      await retranscribeNote(id, input, engine);
      if (!retransEventSeen) {
        retranscribing = true;
        retransStage = "decode";
      }
    } catch (e) {
      retransErr = t("notes.retrans.failed", { e });
    }
  }

  async function doResume() {
    // 续录会重新打开同一套麦克风与系统采集链,风险与新建录制完全一样,
    // 必须走同一道门(Codex review P1:这里原先绕过了,而它有完整 UI 上下文,
    // 不属于快捷键/托盘那种刻意的无 UI 例外)。
    if (!(await recordRiskGate.guard())) return;
    const ok = await recording.resume(id);
    if (ok) goto("/record");
    else
      error = recording.status.startsWith("error:")
        ? recording.status
        : t("notes.resume.blocked");
  }
</script>

<svelte:window
  onclick={() => {
    exportMenuOpen = false;
    editsManage = false;
  }}
  onkeydown={(e) => {
    if (e.key === "Escape") {
      exportMenuOpen = false;
      editsManage = false;
    }
  }}
/>

<main class="container">
  {#if error}
    <div class="banner banner-danger">{error}</div>
  {/if}

  {#if note}
    <!-- 操作栏吸顶:标题/播放器/视图切换钉在滚动视口顶端,长转写滚动或播放跟随时操作不失联 -->
    <div class="topbar">
      <div class="header">
        <div class="header-main">
          {#if editing}
            <!-- svelte-ignore a11y_autofocus -->
            <input
              class="rename"
              autofocus
              bind:value={editingTitle}
              onkeydown={(e) => {
                if (e.key === "Enter") commitRename();
                if (e.key === "Escape") editing = false;
              }}
              onblur={commitRename}
            />
          {:else}
            <h1 class="title">
              <button class="title-btn" title={t("notes.title.renameHint")} onclick={beginRename}>{note.meta.title}</button>
            </h1>
          {/if}

          <p class="meta">
            {formatDate(note.meta.started_at)} · {formatDuration(durationSecs(note))}
            {#if note.meta.state === "recording"}
              <span class="state interrupted">{t("notes.state.interrupted")}</span>
            {/if}
          </p>
          {#if calPerm !== "unavailable"}
            <p class="meta cal-row">
              {#if note.meta.calendar}
                <span title={note.meta.calendar.attendees.map((a) => (a.is_me ? t("notes.calendar.me", { name: a.name }) : a.name)).join("、")}>
                  📅 {note.meta.calendar.title}
                  {#if note.meta.calendar.attendees.length > 0}
                    · {t("notes.calendar.attendeeN", { n: note.meta.calendar.attendees.length })}
                  {/if}
                </span>
                <button class="mini plain" disabled={calBusy} onclick={() => void openCalMenu()}>{t("notes.calendar.reselect")}</button>
                <button class="mini plain" disabled={calBusy} onclick={() => void pickCalEvent(null)}>{t("notes.calendar.clear")}</button>
              {:else if calPerm === "full"}
                <button class="mini plain" disabled={calBusy} onclick={() => void openCalMenu()}>{t("notes.calendar.link")}</button>
              {:else}
                <span class="cal-hint">{t("notes.calendar.needAuth")}</span>
              {/if}
            </p>
            {#if calMenuOpen}
              <div class="cal-menu">
                {#if calCandidates.length === 0}
                  <p class="hint">{t("notes.calendar.noCandidates")}</p>
                {:else}
                  {#each calCandidates as c (c.event_id)}
                    <button class="cal-item" disabled={calBusy} onclick={() => void pickCalEvent(c.event_id)}>
                      <span class="cal-title">{c.title}</span>
                      <span class="cal-time">{fmtCalTime(c.start_ms)}–{fmtCalTime(c.end_ms)}{#if c.overlap_ms > 0} · {t("notes.calendar.overlapMin", { n: Math.round(c.overlap_ms / 60000) })}{/if}</span>
                    </button>
                  {/each}
                {/if}
              </div>
            {/if}
          {/if}
        </div>

        <!-- 头部动作组:重新推断身份 / 导出(MD、成品轨音频)浮层菜单 / 打开目录,
             均为幽灵族(.ghost)纯图标按钮(冒烟反馈:文字太占行,图标+tooltip 交代
             名称与说明;aria-label 保读屏可达)。运行态图标加 .spin 旋转反馈。
             txt 渲染能力在导出层与 CLI(notes get --format txt)保留,GUI 不再暴露。 -->
        <div class="row">
          <button
            class="ghost"
            disabled={identifying}
            title={identifying ? t("notes.identify.running") : `${t("notes.identify.rerun")}——${t("notes.identify.rerunHint")}`}
            aria-label={t("notes.identify.rerun")}
            onclick={() => void rerunIdentify()}
          >
            <svg class:spin={identifying} width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <circle cx="6" cy="5" r="2.5" />
              <path d="M2.2 13.8c0-2.2 1.7-4 3.8-4 1 0 1.9.4 2.6 1" />
              <path d="M13.8 11.2a2.9 2.9 0 1 1-.9-2.1" />
              <path d="M13.9 6.9v2.4h-2.4" />
            </svg>
          </button>
          <div class="export-wrap">
            <button
              class="ghost"
              aria-haspopup="menu"
              aria-expanded={exportMenuOpen}
              title={t("notes.export.button")}
              aria-label={t("notes.export.button")}
              onclick={(e) => {
                e.stopPropagation();
                exportMenuOpen = !exportMenuOpen;
              }}
            >
              <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M9.5 1.8H4.2a.9.9 0 0 0-.9.9v10.6c0 .5.4.9.9.9h7.6c.5 0 .9-.4.9-.9V5z" />
                <path d="M9.5 1.8V5h3.2" />
                <path d="M5.6 11.6V8.4l1.7 1.9 1.7-1.9v3.2" stroke-width="1.2" />
              </svg>
              <svg width="10" height="10" viewBox="0 0 10 10" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                <path d="M2.5 4 5 6.5 7.5 4" />
              </svg>
            </button>
            {#if exportMenuOpen}
              <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events, a11y_interactive_supports_focus -->
              <div class="export-menu" role="menu" onclick={(e) => e.stopPropagation()}>
                {#if exportRange()}
                  {@const r = exportRange()!}
                  <p class="export-range-note">{t("notes.export.rangeNote", { start: formatTs(r.start), end: formatTs(r.end) })}</p>
                {/if}
                <button
                  class="export-item"
                  role="menuitem"
                  onclick={() => {
                    exportMenuOpen = false;
                    void doExport("md");
                  }}>{t("notes.export.md")}</button
                >
                <button
                  class="export-item"
                  role="menuitem"
                  disabled={!mixedInfo?.track}
                  title={mixedInfo?.track ? "" : t("notes.export.audioNone")}
                  onclick={() => {
                    exportMenuOpen = false;
                    void doExportAudio();
                  }}>{t("notes.export.audio")}</button
                >
              </div>
            {/if}
          </div>
          <button
            class="ghost"
            title={`${t("notes.dir.open")}——${t("notes.dir.openHint")}`}
            aria-label={t("notes.dir.open")}
            onclick={() => void doOpenDir()}
          >
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M1.8 4.2c0-.5.4-.9.9-.9h3.1l1.5 1.6h5.9c.5 0 .9.4.9.9v6.9c0 .5-.4.9-.9.9H2.7a.9.9 0 0 1-.9-.9z" />
            </svg>
          </button>
        </div>
      </div>

      <!-- 提示系统(2026-08-22 方案甲):本页常驻提示统一收敛到这一条通知条,
           一次只打扰一条,其余进抽屉;「知道了」按笔记记住。 -->
      <NoticeStrip notices={pageNotices} storageKey={"vn-notices:" + id} />
      {#if exportMsg}<p class="hint export-msg">{exportMsg}</p>{/if}

      <!-- 控制行(录音机式,整合一行):播放/暂停 + 波形音轨,行尾圆形红点录音键。
           录制中(含暂停)不出播放器:文件正在写,不做边写边播的半态。 -->
      <div class="transport">
        {#if canEdit && tracks.length > 0}
          {@const editsRange = exportRange()}
          {@const editsRowOn = !!editsRange || edits.length > 0 || !!editsErr}
          <div class="player-slot" class:has-edits={editsRowOn}>
            <!-- cuts 试听期间临时清空:试听段可能恰好落在被删区间里,内核跳段会把
                 它跳成"无声"(2026-09-05 用户实报);试听是"听清这段是谁"的动作,
                 必须绕过删减,结束即恢复。 -->
            <AudioPlayer bind:this={player} tracks={playerTracks} {waveform} bind:currentMs={playerMs} bind:playing={playerPlaying} bind:rangeStartMs bind:rangeEndMs {onRangeDrag} {onRangeDragEnd} cuts={preview ? [] : edits} onLoaded={onPlayerLoaded} onUserPause={() => endPreview("user-pause")} noteId={note?.meta.id} title={note?.meta.title} />
            <!-- 音频剪辑行(非破坏性删减):播放器卡片的第二行(同底、hairline 分隔),
                 与下方说话人区自然分组。删除只记入剪辑表(edits.json),播放跳过、
                 导出剔除,原始录音不动。 -->
            {#if editsRowOn}
              <div class="edits-row">
                {#if editsRange}
                  <button
                    class="edits-btn edits-del"
                    title={t("notes.edits.deleteTitle", { start: formatTs(editsRange.start), end: formatTs(editsRange.end) })}
                    onclick={() => void deleteRange()}
                  >
                    <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                      <circle cx="4.3" cy="4.6" r="1.9" />
                      <circle cx="4.3" cy="11.4" r="1.9" />
                      <path d="M5.9 5.9 13.6 12.4M5.9 10.1 13.6 3.6" />
                    </svg>
                    {t("notes.edits.deleteRange", { dur: fmtDur(editsRange.end - editsRange.start) })}
                  </button>
                {/if}
                {#if edits.length > 0}
                  <!-- svelte-ignore a11y_no_static_element_interactions, a11y_click_events_have_key_events -->
                  <div class="edits-menu" onclick={(e) => e.stopPropagation()}>
                    <button
                      class="edits-btn"
                      class:open={editsManage}
                      aria-expanded={editsManage}
                      aria-haspopup="menu"
                      onclick={() => (editsManage = !editsManage)}
                    >
                      {t("notes.edits.count", { n: edits.length, dur: fmtDur(edits.reduce((a, c) => a + c.end_ms - c.start_ms, 0)) })}
                      <svg class="chev" class:open={editsManage} width="10" height="10" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M4 6l4 4 4-4" />
                      </svg>
                    </button>
                    {#if editsManage}
                      <div class="edits-pop" role="menu">
                        <p class="edits-pop-hint">{t("notes.edits.popHint")}</p>
                        {#each edits as c (c.start_ms)}
                          <div class="edits-item">
                            <span class="edits-span">{formatTs(c.start_ms)} – {formatTs(c.end_ms)}</span>
                            <button class="edits-restore" onclick={() => void restoreCut(c)}>{t("notes.edits.restore")}</button>
                          </div>
                        {/each}
                        {#if edits.length > 1}
                          <button
                            class="edits-restore edits-restore-all"
                            onclick={async () => {
                              for (const c of [...edits]) await restoreCut(c);
                            }}>{t("notes.edits.restoreAll")}</button
                          >
                        {/if}
                      </div>
                    {/if}
                  </div>
                  {#if lastCut}
                    {@const undoTarget = lastCut}
                    <button
                      class="edits-btn"
                      title={t("notes.edits.undoTitle", { start: formatTs(undoTarget.start_ms), end: formatTs(undoTarget.end_ms) })}
                      onclick={() => void restoreCut(undoTarget)}
                    >
                      <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
                        <path d="M3.4 6.6h5.8a3.5 3.5 0 1 1 0 7H5.2" />
                        <path d="M6 3.9 3.3 6.6l2.7 2.7" />
                      </svg>
                      {t("notes.edits.undo")}
                    </button>
                  {/if}
                {/if}
                {#if editsErr}<span class="edits-err">{editsErr}</span>{/if}
              </div>
            {/if}
          </div>
          <!-- 回放方案 A/B(二期):可选项由该笔记实际产物决定——无成品轨给「生成」
               动作段;有但不可信(mixed_untrusted)置灰并 tooltip 给原因。 -->
          <div class="mix-switch" title={t("notes.mix.title")}>
            <Segmented
              size="sm"
              items={mixItems}
              value={playbackScheme}
              onSelect={(id) => switchScheme(id as "dual" | "mixed")}
              onAction={() => startRegen()}
            />
          </div>
        {/if}
        <button
          class="rec-btn"
          disabled={recording.isLive}
          title={recording.isLive ? t("notes.record.busy") : t("notes.record.resume")}
          aria-label={t("notes.record.resume")}
          onclick={doResume}
        >
          <span class="rec-dot"></span>
        </button>
      </div>


      <!-- 说话人条:修订稿与原始逐字稿同一份 speakers.json、同一套交互,不随视图切换变化
           (2026-08-28 用户实报:修订稿上不可点、逐字稿上可点)。改名/选人/取消关联
           只写 speakers.json,Aing 整写的是 aing.json,互不相扰,故 Aing 中照常可编;
           删除与拆分后端在 Aing 中拒绝(管线随后整写会引用旧表项),前端不藏、置灰并写原因
           (blockedReason;藏掉会被当成"没有这个功能",2026-08-31 用户实报)。
           录制中(canEdit=false)不给选人区(后端 writer 独占)。 -->
      <SpeakerChips
        speakers={note.speakers}
        noteId={id}
        editable={true}
        counts={segCounts}
        people={canEdit ? people : undefined}
        onPick={canEdit
          ? (sid, personId, sample) =>
              assignNoteSpeakerPerson(id, sid, personId, sample?.auditedSeq ?? lastAuditioned[sid], sample?.selectedSeqs?.length ? sample.selectedSeqs : undefined)
          : undefined}
        onDetachClip={canEdit ? detachClip : undefined}
        onDelete={canEdit ? (sid) => deleteNoteSpeaker(id, sid) : undefined}
        onUnlink={canEdit ? (sid) => clearNoteSpeakerPerson(id, sid) : undefined}
        onMarkMulti={canEdit ? runAutoSplit : undefined}
        blockedReason={refining ? t("speakers.chipBlockedAing") : null}
        onPreview={canEdit && tracks.length > 0 ? previewSpeaker : undefined}
        previewClips={canEdit && tracks.length > 0 ? previewClips : undefined}
        onPreviewClip={canEdit && tracks.length > 0 ? previewSpeakerClip : undefined}
        previewingId={preview?.sid ?? null}
        previewingSeq={preview?.seq ?? null}
        onRenamed={() => {
          refresh();
          recording.bumpNotes();
        }}
      />

      {#if autoSplitRunning}
        <div class="banner">
          {t("speakers.autosplit.progress")}
          {#if autoSplitProg}{t("speakers.autosplit.progressN", { done: autoSplitProg.done, total: autoSplitProg.total })}{/if}
        </div>
      {:else if autoToast}
        <div class="banner">
          {#if autoToast.kind === "split"}
            {t("speakers.autosplit.done", { n: autoToast.out.groups.length })}
            {#if autoToast.out.kept > 0}{t("speakers.autosplit.kept", { n: autoToast.out.kept })}{/if}
            <button class="link" onclick={undoSplitToast}>{t("speakers.autosplit.undo")}</button>
          {:else if autoToast.kind === "nochange"}
            {t("speakers.autosplit.nochange")}
          {:else if autoToast.kind === "undone"}
            {t("speakers.autosplit.undone")}
          {:else}
            {t("speakers.autosplit.failed", { e: autoToast.msg })}
          {/if}
          <button class="link" onclick={() => (autoToast = null)}>{t("speakers.autosplit.dismiss")}</button>
        </div>
      {/if}

      <div class="view-switch">
        <Segmented items={viewItems} value={effectiveView} onSelect={(id) => (viewMode = id as "refined" | "raw")} />
        <span class="spacer"></span>
        <!-- 行内进度只需 refining:aingProg 来自分块事件,页面重载后要等下一个分块
             才有——这期间(以及长块/重试中)也必须有可见指示,否则整理中的页面只剩
             一排灰按钮,读起来像"Aing 丢了"(2026-08-26 用户实报,3h 会议整理 70 分钟
             无任何指示)。无分块数据时显示通用"整理中",有再升级为 n/m + ETA。 -->
        {#if refining}
          <span class="aing-inline">
            {#if aingProg}
              {t(aingProg.stage === "llm_retry" ? "notes.progress.llmRetry" : "notes.progress.llm", {
                done: aingProg.done,
                total: aingProg.total,
              })}
              {#if aingEtaMin !== null}· {t("notes.progress.eta", { m: aingEtaMin })}{/if}
            {:else}
              {t("notes.progress.llmWarmup")}
            {/if}
          </span>
        {/if}
        <!-- 魔杖(重新 Aing)恒渲染:PR#141 重构误把上面的 {/if} 挪到按钮之后,魔杖被
             吞进 refining && aingProg——普通完成笔记从此没有这个按钮,整理中还会
             整个消失。按钮自身的 disabled/casting 已按 refining 处理,无需条件包裹。 -->
        <button
            class="reaing"
            class:casting={refining}
            disabled={refining || note.meta.state !== "complete"}
            onclick={rerunRefine}
            title={aiState === "running" ? t("notes.refine.running") : aiState === "complete" ? t("notes.refine.completeHint") : aiState === "failed" ? t("notes.refine.failedHint") : t("notes.refine.run")}
          >
            <svg class="wand" viewBox="0 0 22 22" width="22" height="22" aria-hidden="true">
              <path
                class="wand-stick"
                d="M3.5 18.5 11.5 10.5"
                fill="none"
                stroke="currentColor"
                stroke-width="1.7"
                stroke-linecap="round"
              />
              <path class="wand-star" d="M15 2.8 16 6 19.2 7 16 8 15 11.2 14 8 10.8 7 14 6Z" />
              <path
                class="spark spark-a"
                d="M6.5 3.6 6.9 4.6 7.9 5 6.9 5.4 6.5 6.4 6.1 5.4 5.1 5 6.1 4.6Z"
              />
              <path
                class="spark spark-b"
                d="M18.5 12.7 18.85 13.65 19.8 14 18.85 14.35 18.5 15.3 18.15 14.35 17.2 14 18.15 13.65Z"
              />
              <path
                class="spark spark-c"
                d="M10 15.4 10.3 16.2 11.1 16.5 10.3 16.8 10 17.6 9.7 16.8 8.9 16.5 9.7 16.2Z"
              />
            </svg>
            <AiStateLabel state={aiState} />
          </button>

        <!-- 文件重转写(三期):离线用盘上音频重新转写全文,破坏性(覆盖原始逐字稿,
             自动备份为 segments.orig.jsonl),二段确认走 retransConfirm 胶囊。
             来源二选一:双轨(mic+system 分轨)/成品轨(单混音轨,mixedInputStatus
             判定可用性并置灰+tooltip 给原因)。 -->
        {#if retransConfirm}
          <div class="confirm-capsule">
            <span class="refine-warn">{t("notes.retrans.warn")}</span>
            <button class="link danger" onclick={() => startRetranscribe("dual")}>
              {t("notes.retrans.confirmDual")}
            </button>
            <button
              class="link danger"
              disabled={mixedReason !== null}
              title={mixedReason ?? ""}
              onclick={() => startRetranscribe("mixed")}
            >
              {t("notes.retrans.confirmMixed")}
            </button>
            <button class="link" onclick={() => (retransConfirm = false)}>{t("notes.cancel")}</button>
          </div>
        {:else}
          <button
            class="ghost"
            disabled={retranscribing || refining || recording.isLive || note.meta.state !== "complete"}
            title={retranscribing ? t("notes.retrans.running", { stage: retransStage }) : `${t("notes.retrans.run")}——${t("notes.retrans.hint")}`}
            aria-label={t("notes.retrans.run")}
            onclick={() => (retransConfirm = true)}
          >
            <svg class:spin={retranscribing} width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M13.2 8a5.2 5.2 0 1 1-1.6-3.8" />
              <path d="M13.4 1.8v2.8h-2.8" />
            </svg>
          </button>
        {/if}
      </div>
    </div>


    <div class="transcript" class:live={playerPlaying} bind:this={transcriptEl}>
      {#if effectiveView === "refined" && refined}
        <!-- 精修稿 WYSIWYG:MarkdownEditor(可编辑,mode="refined")+ 实体/徽章浮层。
             host div 只用来接收组件在编辑器根上派发的 entityhover/entityleave(不冒泡,
             靠 capture 阶段的祖先监听,见上方 script 区的桥接 effect)。 -->
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div class="refined-editor-host" bind:this={refinedHostEl} onmouseleave={scheduleHideEntityPop}>
          <MarkdownEditor
            bind:this={refinedEditor}
            mode="refined"
            editable={canEdit}
            speakerBadge={refinedBadge}
            onSaveRefined={doSaveRefined}
            onDrainRefined={queueRefinedDrain}
            onBadgeClick={(attrs, rect) => (refinedBadgePop = { attrs, rect })}
            onPlayFrom={(ms) => playFrom({ start_ms: ms })}
          />
        </div>
        {#if refined.paragraphs.length === 0}
          <p class="hint">{t("notes.hint.emptyRefined")}</p>
        {/if}
      {:else}
        <!-- 原始稿 WYSIWYG:MarkdownEditor(可编辑,mode="segments")段结构锁定 + 浮层
             菜单/删除确认。canEdit 翻转是 NodeView 构造期快照(PM 不会为未变节点
             重建 NodeView,详见组件顶部/segmentSchema.ts 注释),{#key canEdit}
             强制整份重挂,拿到与当前权限一致的徽章禁用态/删除按钮(移交义务 4)。
             空转写时 editable 强制 false:没有 transcript_segment 节点时组件退回
             一个占位空 paragraph,可打字但永不落盘,不给这个误导性入口(移交义务 7)。 -->
        {#key canEdit}
          <MarkdownEditor
            bind:this={segEditor}
            mode="segments"
            editable={canEdit && displaySegments.length > 0}
            speakerBadge={segBadge}
            onEditSegment={doEditSegment}
            onBadgeClick={(attrs, rect, shiftKey) => {
              if (!canEdit) return;
              if (selMode) {
                toggleSel(attrs.seq!, shiftKey ?? false);
                return;
              }
              // WKWebView 焦点语义保险:点悬浮菜单按钮不像 Chromium 那样把焦点从编辑
              // 宿主夺走,focusout 不触发,段内未提交的文字进不了 commitSegment,随后
              // 命令路径的 syncSegments(true) 整份重建会把它静默吹掉。这里显式 blur
              // 当前 activeElement,同步触发 focusout 先把待提交段存掉,再开浮层
              // ——Chromium 下 activeElement 已经是这个按钮本身,blur 无害。
              (document.activeElement as HTMLElement | null)?.blur();
              segMenuPop = { seq: attrs.seq!, rect };
            }}
            onDeleteClick={(seq, rect) => {
              if (!canEdit) return;
              // 同上(onBadgeClick 注释):先失焦提交,再开删除确认浮层。
              (document.activeElement as HTMLElement | null)?.blur();
              segDeletePop = { seq, rect };
            }}
            onPlayFrom={(ms) => playFrom({ start_ms: ms })}
          />
        {/key}
        {#if displaySegments.length === 0}
          <p class="hint">{t("notes.hint.emptyTranscript")}</p>
        {/if}
      {/if}
    </div>

    {#if segMenuPop && note}
      <!-- 段落说话人选择弹窗(2026-08-22 批量化):与胸牌菜单同一套视觉;
           甲=作用范围单选(仅这段/连续同说话人/该说话人全部),乙=「勾选多段…」入口。 -->
      {@const runN = contiguousRun(displaySegments, segMenuPop.seq).length}
      {@const allN = segMenuSpeaker ? displaySegments.filter((sg) => sg.speaker === segMenuSpeaker).length : 1}
      <SegSpeakerPop
        speakers={note.speakers}
        currentSid={segMenuSpeaker}
        counts={segCounts}
        rect={segMenuPop.rect}
        scopes={[
          { key: "one", label: t("notes.segpick.scopeOne"), n: 1 },
          { key: "run", label: t("notes.segpick.scopeRun", { n: runN }), n: runN },
          { key: "all", label: t("notes.segpick.scopeAll", { n: allN }), n: allN },
        ]}
        showMultiEntry={true}
        onEnterMulti={() => {
          const seq = segMenuPop!.seq;
          segMenuPop = null;
          selMode = true;
          selected = [seq];
          lastSel = seq;
        }}
        onPick={(sid, scope) => {
          const seq = segMenuPop!.seq;
          const seqs =
            scope === "run"
              ? contiguousRun(displaySegments, seq)
              : scope === "all" && segMenuSpeaker
                ? displaySegments.filter((sg) => sg.speaker === segMenuSpeaker).map((sg) => sg.seq)
                : [seq];
          void doSetSpeakerBatch(seqs, sid);
        }}
        onClose={() => (segMenuPop = null)}
      />
    {/if}
    {#if selMode}
      <div class="sel-bar">
        <span>{t("notes.segpick.selectedN", { n: selected.length })}</span>
        <button
          class="link"
          disabled={selected.length === 0}
          onclick={(e) => (selPick = (e.currentTarget as HTMLElement).getBoundingClientRect())}
        >
          {t("notes.segpick.changeTo")}
        </button>
        {#if selDeleteConfirm}
          <button class="link danger" onclick={applySelDelete}>{t("notes.segpick.deleteConfirm", { n: selected.length })}</button>
          <button class="link" onclick={() => (selDeleteConfirm = false)}>{t("notes.cancel")}</button>
        {:else}
          <button class="link danger" disabled={selected.length === 0} onclick={() => (selDeleteConfirm = true)}>{t("notes.segpick.deleteSel")}</button>
        {/if}
        <button class="link" disabled={selected.length === 0} onclick={() => (selected = [])}>{t("notes.segpick.clearSel")}</button>
        <button class="link" onclick={exitSelMode}>{t("notes.segpick.exitSel")}</button>
      </div>
    {/if}
    {#if selPick && note}
      <SegSpeakerPop
        speakers={note.speakers}
        currentSid={null}
        counts={segCounts}
        rect={selPick}
        scopes={null}
        onPick={(sid) => void applySelPick(sid)}
        onClose={() => (selPick = null)}
      />
    {/if}
    {#if multiPanel && note}
      <MultiSpeakerPanel
        noteId={id}
        speakers={note.speakers}
        candidateSpeakers={multiPanel.candidates}
        existingOp={multiPanel.existingOp}
        segments={note.segments}
        people={people.map((p) => ({ id: p.id, name: p.name }))}
        onAuditionSeg={tracks.length > 0 ? auditionSegment : undefined}
        onBeforeCommit={() => refinedEditor?.flushRefined(true)}
        onClose={() => (multiPanel = null)}
        onChanged={onMultiChanged}
      />
    {/if}
    {#if segDeletePop}
      <div
        class="badge-menu floating"
        style="position: fixed; left: {segDeletePop.rect.left}px; top: {segDeletePop.rect.bottom + 4}px;"
      >
        <button class="menu-item danger" onclick={() => doDeleteSeg(segDeletePop!.seq)}>{t("notes.menu.confirmDelete")}</button>
        <button class="menu-item" onclick={() => (segDeletePop = null)}>{t("notes.cancel")}</button>
      </div>
    {/if}

    {#if entityPop}
      <!-- svelte-ignore a11y_no_static_element_interactions -->
      <!-- 浮层是 rect.bottom+4 的兄弟节点,不在触发它的实体 span 内部:鼠标离开 span
           去点这里的按钮途中会先经过两者间的空隙。onmouseenter 取消宿主/entityleave
           排的 pending 收起,onmouseleave 再排一次——不这样按钮永远点不到。 -->
      <div
        class="entity-pop"
        style="position: fixed; left: {entityPop.rect.left}px; top: {entityPop.rect.bottom + 4}px;"
        onmouseenter={cancelHideEntityPop}
        onmouseleave={scheduleHideEntityPop}
      >
        <span>{entityName(entityPop.entityId)}</span>
        {#if entityLinks[entityPop.entityId]}
          <button
            class="link"
            onclick={() => {
              cancelHideEntityPop();
              gotoEntity(entityPop!.entityId);
              entityPop = null;
            }}
          >
            {t("notes.entity.openGraph")}
          </button>
        {/if}
      </div>
    {/if}
    {#if refinedBadgePop}
      <!-- 说话人徽章浮层:只做身份提示,改名/选人仍走页面顶部的 SpeakerChips 说话人条。 -->
      <div
        class="entity-pop"
        style="position: fixed; left: {refinedBadgePop.rect.left}px; top: {refinedBadgePop.rect.bottom + 4}px;"
      >
        <span>{refinedBadge(refinedBadgePop.attrs).label}</span>
        <button class="link" onclick={() => (refinedBadgePop = null)}>{t("notes.close")}</button>
      </div>
    {/if}

    {#if related.length > 0}
      <section class="card col related">
        <div class="card-title">{t("notes.related.title")}</div>
        <ul class="appear-list">
          {#each related as n (n.id)}
            <li class="appear-row">
              <a href="/notes/{n.id}">{n.title}</a>
              <span class="appear-meta">{t("notes.related.shared", { n: n.shared_entities })}</span>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <!-- 跟随被用户上滑打断时的返回入口(与录制页同款):sticky 钉滚动视口底部 -->
    <div class="jump-anchor">
      {#if !follow && playerPlaying}
        <button class="jump" onclick={resumeFollow}>{t("notes.jumpBack")}</button>
      {/if}
    </div>
  {:else if !error}
    <!-- 加载态:切换会议到 note 就绪之间的空窗(长会议 load 可能数百毫秒),
         给一个安静的占位,避免"点了没反应"的错觉。error 分支已在上方单独渲染。 -->
    <p class="loading">{t("notes.loading")}</p>
  {/if}
</main>

<style>
  .aing-inline {
    font-size: 12.5px;
    opacity: 0.8;
    margin-right: 8px;
    white-space: nowrap;
  }

  .container {
    padding: 1.5rem;
    font-family: -apple-system, system-ui, sans-serif;
  }
  .title {
    cursor: text;
    margin: 0 0 0.25rem;
  }
  /* editable-text（标题）：静态时无边，hover accent-tint 底 + rounded-sm，focus accent outline */
  .title-btn {
    background: none;
    border: none;
    padding: 0;
    margin: 0;
    font: inherit;
    color: inherit;
    cursor: text;
    text-align: left;
    border-radius: var(--radius-sm);
  }
  .title-btn:hover {
    background: var(--accent-tint);
  }
  .title-btn:focus-visible {
    outline: 2px solid var(--accent);
    border-radius: var(--radius-sm);
  }
  .rename {
    font-size: 1.6em;
    font-weight: 500;
    width: 100%;
    box-sizing: border-box;
    padding: 0.1em 0.3em;
    border-radius: var(--radius-lg);
    border: 1px solid var(--accent);
    background: var(--canvas);
    color: var(--ink);
  }
  .meta {
    color: var(--ink-secondary);
    margin: 0 0 1rem;
  }
  /* 操作栏吸顶:canvas 不透明底钉在滚动视口顶端,转写从底下滚过;
     底缘用渐隐代替分隔线,未滚动时不显突兀,滚动时文字平滑没入。 */
  .topbar {
    position: sticky;
    top: 0;
    z-index: 10;
    background: var(--canvas);
    padding-top: 0.4rem;
    margin-top: -0.4rem;
  }
  .topbar::after {
    content: "";
    position: absolute;
    top: 100%;
    left: 0;
    right: 0;
    height: 14px;
    background: linear-gradient(var(--canvas), transparent);
    pointer-events: none;
  }
  /* 标题行:左标题+时间,右上角动作按钮(冒烟反馈:按钮移右上)。
     窄窗(默认 800x600)可换行:动作组 flex:none 不收缩,标题区给 min-width 下限,
     空间不足时整组动作换到标题下一行右对齐——否则标题被挤成一列多行(冒烟实锤)。 */
  .header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 0.25rem 1rem;
    flex-wrap: wrap;
  }
  .header-main {
    flex: 1;
    min-width: 14rem;
  }
  .row {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    flex: none;
    justify-content: flex-end;
    padding-top: 0.2rem;
    margin-left: auto;
  }
  /* 头部动作钮:幽灵族(.ghost)+图标文字并排;「导出」带浮层菜单(DESIGN 浮层规范) */
  .export-wrap {
    position: relative;
  }
  .export-menu {
    position: absolute;
    top: calc(100% + 4px);
    right: 0;
    z-index: 30;
    min-width: 9rem;
    display: flex;
    flex-direction: column;
    padding: 4px;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
  }
  /* 音频剪辑行:播放器卡片的第二行——同底色、hairline 分隔、下缘补回卡片圆角 */
  .edits-row {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 0.5rem;
    margin: 0;
    padding: 0.45rem 0.9rem;
    background: var(--surface);
    border-top: 1px solid var(--hairline);
    border-radius: 0 0 var(--radius-lg) var(--radius-lg);
  }
  /* 胶囊按钮:track-btn 同语言(hairline-strong 边、圆角满、图标+文字),
     数字走 tabular 对齐;按压下沉 0.5px 给触感 */
  .edits-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.4em;
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.3em 0.8em;
    font-size: 0.78rem;
    font-variant-numeric: tabular-nums;
    cursor: pointer;
    white-space: nowrap;
    transition:
      background 120ms ease,
      color 120ms ease,
      border-color 120ms ease;
  }
  .edits-btn:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .edits-btn:active {
    transform: translateY(0.5px);
  }
  .edits-btn.open {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .edits-btn .chev {
    transition: transform 120ms ease;
    opacity: 0.7;
  }
  .edits-btn .chev.open {
    transform: rotate(180deg);
  }
  /* 删除入口:danger 族(墨/边/悬停底全用 danger token),一眼分清这是破坏性动作
     的入口——虽然实际非破坏,但语义上是"删" */
  .edits-btn.edits-del {
    color: var(--danger);
    border-color: var(--danger-line);
  }
  .edits-btn.edits-del:hover {
    background: var(--danger-tint);
    color: var(--danger-ink);
  }
  .edits-err {
    color: var(--danger);
    font-size: 0.78rem;
  }
  /* 管理浮层:track-pop 同语言(surface-press 底、hairline 边、popover 阴影),
     在按钮下方展开 */
  .edits-menu {
    position: relative;
  }
  .edits-pop {
    position: absolute;
    top: calc(100% + 6px);
    left: 0;
    z-index: 20;
    min-width: 15rem;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 0.5rem;
  }
  .edits-pop-hint {
    margin: 0 0 0.45rem;
    padding: 0 0.15rem;
    color: var(--ink-secondary);
    font-size: 0.75rem;
  }
  .edits-item {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 0.9rem;
    padding: 0.28em 0.15rem;
    font-size: 0.85rem;
  }
  .edits-span {
    color: var(--ink);
    font-variant-numeric: tabular-nums;
  }
  /* 浮层内恢复按钮:小号 secondary(有边有形,不是裸链接) */
  .edits-restore {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.12em 0.65em;
    font-size: 0.75rem;
    cursor: pointer;
    transition:
      background 120ms ease,
      color 120ms ease;
  }
  .edits-restore:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .edits-restore:active {
    transform: translateY(0.5px);
  }
  .edits-restore-all {
    margin-top: 0.35rem;
    width: 100%;
  }
  /* 圈定范围提示:游标圈了真子范围时出现在菜单顶部,交代两个导出项的作用域 */
  .export-range-note {
    margin: 0;
    padding: 0.35em 0.7em 0.45em;
    font-size: 0.75rem;
    color: var(--ink-secondary);
    border-bottom: 1px solid var(--hairline);
    white-space: nowrap;
    font-variant-numeric: tabular-nums;
  }
  .export-item {
    border: none;
    background: none;
    box-shadow: none;
    text-align: left;
    padding: 0.45em 0.7em;
    font-size: 0.85rem;
    font-weight: 500;
    color: var(--ink);
    border-radius: var(--radius-md);
    transition: background 120ms ease;
  }
  .export-item:hover:not(:disabled) {
    background: var(--surface-soft);
  }
  .export-item:disabled {
    opacity: 0.45;
    cursor: default;
  }
  /* 幽灵按钮族:低频动作统一形态——无边框、次级墨色+16px 线性图标,
     hover 浮现 surface-soft 底并提亮(悬停显影原则),按压下沉 0.5px。 */
  /* 运行态图标旋转(重新推断/重转写进行中):按钮已 disabled,旋转是唯一进行中信号。 */
  .ghost svg.spin {
    animation: ghost-spin 1.2s linear infinite;
  }
  @keyframes ghost-spin {
    to { transform: rotate(360deg); }
  }
  @media (prefers-reduced-motion: reduce) {
    .ghost svg.spin { animation: none; }
  }
  .ghost {
    display: inline-flex;
    align-items: center;
    gap: 0.4em;
    border: none;
    background: transparent;
    padding: 0.4em 0.55em; /* 纯图标形态:横向收紧,点击区仍 ≥28px */
    font-size: 0.85rem;
    font-weight: 500;
    letter-spacing: 0.2px;
    color: var(--ink-secondary);
    border-radius: var(--radius-md);
    transition:
      background 120ms ease,
      color 120ms ease,
      transform 120ms ease;
  }
  .ghost:hover:not(:disabled) {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .ghost:active:not(:disabled) {
    transform: translateY(0.5px);
  }
  .ghost svg {
    flex: none;
  }
  /* 控制行:录音 + 播放器整合一行(录音机式) */
  .transport {
    display: flex;
    /* flex-start + 右侧控件按第一行高度(3.1rem=播放键 2.1rem+上下 0.5rem 内边距)
       自行垂直居中:播放器卡片长出剪辑第二行时,右侧簇不再被整块高度拽下去 */
    align-items: flex-start;
    gap: 0.5rem 0.75rem;
    margin: 0 0 1rem;
    flex-wrap: wrap;
  }
  /* 播放器槽的 min-width 下限 ≈ AudioPlayer 的 min-content(播放键+双时间+波形下限
     +音轨选择器+内边距):低于它内容会从槽里溢出垫到右侧控件底下(冒烟实锤,
     AudioPlayer 内波形条自适应同一教训)。空间不足时 segmented+录制键换到下一行。 */
  .player-slot {
    flex: 1;
    min-width: 23rem;
  }
  /* 剪辑行出现时播放器卡片下缘拉直,与第二行(同底色)无缝相接 */
  .player-slot.has-edits :global(.player) {
    border-bottom-left-radius: 0;
    border-bottom-right-radius: 0;
  }
  /* 回放方案 A/B 切换(二期):Segmented(sm)承载,容器只管行内定位。
     margin-left:auto 让「segmented+录制键」聚成一簇靠右——宽窗时播放器 flex:1
     已占满空间无感;窄窗换行后这簇整体落在第二行右端,不与录制键分居两端(冒烟反馈)。 */
  .mix-switch {
    display: inline-flex;
    gap: 0.15rem;
    flex: none;
    align-items: center;
    height: 3.1rem; /* 锚定播放器第一行高度,transport flex-start 下保持视觉居中 */
    margin-left: auto;
  }
  /* 继续录制:录音机标志式圆形录音键(圆环 + 居中红点),行尾右置,与播放键同语言。
     纯图标是用户拍板的特例(2026-07-07:录音红点属录音机通识符号,文字反而挤占
     音轨宽度),悬停 title/aria-label 兜底可达性。靠右由 .mix-switch 的 auto margin
     统一负责;无播放器(录制中只剩录制键)时它是首子元素,自己补 auto 仍靠右。 */
  .rec-btn {
    width: 2.4rem;
    height: 2.4rem;
    margin-top: 0.35rem; /* (3.1rem 第一行高 − 2.4rem)/2:transport flex-start 下对齐播放器行中线 */
    padding: 0;
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-full);
  }
  .rec-btn:first-child {
    margin-left: auto;
  }
  .rec-dot {
    width: 12px;
    height: 12px;
    border-radius: var(--radius-full);
    background: var(--record);
    flex: none;
  }
  .rec-btn:disabled .rec-dot {
    background: var(--ink-faint);
  }
  .export-msg {
    margin: 0 0 0.75rem;
    font-size: 0.85rem;
  }
  /* button-secondary：导出/继续录制，透明底 + hairline-strong 边，无阴影 */
  button {
    border-radius: var(--radius-md);
    border: 1px solid var(--hairline-strong);
    padding: 0.5em 1.2em;
    font-size: 0.9rem;
    font-weight: 500;
    cursor: pointer;
    background: transparent;
    color: var(--ink);
  }
  button:hover {
    background: var(--surface-soft);
  }
  button:disabled {
    opacity: 0.6;
    cursor: default;
  }
  /* transcript-container：surface 底、rounded-xl，正文用 transcript 字级(1.02rem/1.7) */
  .transcript {
    background: var(--surface);
    border-radius: var(--radius-xl);
    padding: 20px;
    font-size: 1.02rem;
    line-height: 1.7;
  }
  .transcript p {
    margin: 0 0 6px;
  }
  /* 播放中(歌词式,与录制页同构):底部 50vh 留白让最后几段也能居中;
     历史/未播段退次级墨色,当前段放大高亮钉屏幕中央。暂停即全部还原。 */
  .transcript.live {
    /* 顶部留白:开头几段也能被 scrollIntoView 推到中央(上方内容不够高时没它推不动) */
    padding-top: 40vh;
    padding-bottom: 50vh;
  }
  .transcript.live :global(.md-seg) {
    color: var(--ink-secondary);
  }
  /* 原始稿段落(MarkdownEditor 的 transcript_segment NodeView 产出,DOM 由 PM 命令式
     创建,没有 Svelte 的 scope hash,必须 :global 才能命中,同 .md-para 同款注释)。 */
  .transcript :global(.md-seg) {
    margin: 0 0 6px;
    line-height: 1.7;
    border-radius: var(--radius-sm);
    transition:
      background 120ms ease,
      font-size 0.2s ease,
      color 0.2s ease;
  }
  /* 播放跟随:当前段 accent-tint 底,与 editable hover 同色系,安静不抢内容 */
  .transcript :global(.md-seg.playing) {
    background: var(--accent-tint);
  }
  /* 圈选游标联动:拖动游标时离它最近的段 accent-tint 底 + 切线交代边界方向——
     开始游标的线在文字上方(从这里起导出)、结束游标在下方(到这里为止)。
     线用 box-shadow 而非 border:不挤动版面(段间 6px margin 里画得下 2px 线);
     拖完 1.6s 自动消散。 */
  .transcript :global(.md-seg.range-hint) {
    background: var(--accent-tint);
  }
  .transcript :global(.md-seg.range-hint.hint-start) {
    box-shadow: 0 -2px 0 0 var(--accent);
  }
  .transcript :global(.md-seg.range-hint.hint-end) {
    box-shadow: 0 2px 0 0 var(--accent);
  }
  /* 被 Aing 过滤掉的段(原始稿视角):灰显但保留可读,不做删除线/隐藏 */
  .transcript :global(.md-seg.discarded) {
    opacity: 0.38;
  }
  /* 被删减的段(剪辑表):灰显 + danger 删除线,与 AI 过滤灰显区分——这是用户
     亲手删的,语义是"音频里已剪掉",恢复入口在播放器下方 */
  .transcript :global(.md-seg.cut) {
    opacity: 0.38;
    text-decoration: line-through;
    text-decoration-color: var(--danger);
    text-decoration-thickness: 1px;
  }
  /* 修订稿段落(MarkdownEditor 的 refined_paragraph NodeView 产出,DOM 由 PM 命令式
     创建,没有 Svelte 的 scope hash,必须 :global 才能命中):与 .md-seg 同排版语言,
     可编辑(WYSIWYG)。 */
  .transcript :global(.md-para) {
    margin: 0 0 6px;
    line-height: 1.7;
  }
  .transcript :global(.md-para .para-text) {
    white-space: pre-wrap;
  }
  /* editable-text(精修稿正文):组件把默认 outline 摘了,焦点态自己补——accent
     2px outline,与标题/段落输入同一套规范。 */
  .transcript :global(.ProseMirror:focus-visible) {
    outline: 2px solid var(--accent);
    border-radius: var(--radius-sm);
  }
  /* 实体提及高亮:正文单色,静态无底(不染正文),hover 才浮 accent-tint 底 + accent 字。
     :global 原因同上——原始稿(.md-seg 内)与修订稿(NodeView 的 .md-para 内)共用同一套
     class,两者都是 PM 命令式创建的 DOM,没有 Svelte scope hash。 */
  .transcript :global(.entity-mention) {
    border-radius: var(--radius-sm);
    cursor: default;
    transition:
      background 120ms ease,
      color 120ms ease;
  }
  .transcript :global(.entity-mention:hover) {
    background: var(--accent-tint);
    color: var(--accent);
  }
  /* 可导航的实体提及(能解析到全局 id):区别于纯 tooltip 态,给出可点信号 */
  .transcript :global(.entity-mention.linkable) {
    cursor: pointer;
  }
  .transcript :global(.entity-mention.linkable:hover) {
    text-decoration: underline;
    text-decoration-color: var(--accent);
  }
  /* 当前播放段(仅播放中):放大 + 主墨色 + 轻投影,歌词感;负边距抵掉内缩,行左缘对齐不跳 */
  .transcript.live :global(.md-seg.playing) {
    font-size: 1.5em;
    line-height: 1.55;
    color: var(--ink);
    padding: 0.3em 0.55em;
    margin-left: -0.55em;
    margin-right: -0.55em;
    border-radius: var(--radius-md);
    box-shadow: 0 4px 14px light-dark(rgba(0, 0, 0, 0.12), rgba(0, 0, 0, 0.45));
  }

  /* 「回到播放位置」药丸:零高锚点 + sticky bottom(与录制页同款)。
     flex-end 替代 translateY(-100%):零高容器 stretch 会压扁按钮使百分比位移失效。 */
  .jump-anchor {
    position: sticky;
    bottom: 1rem;
    height: 0;
    display: flex;
    justify-content: center;
    align-items: flex-end;
  }
  .jump {
    border: none;
    border-radius: var(--radius-full);
    background: var(--primary);
    color: var(--on-primary);
    font-size: 0.85rem;
    font-weight: 500;
    padding: 0.4em 1em;
    cursor: pointer;
    box-shadow: var(--shadow-popover);
  }
  .jump:hover {
    background: var(--primary-pressed);
  }
  /* :global——原始稿(.md-seg)与修订稿 NodeView(.md-para)共用同一套 .badge/.ts 徽章
     class,两者都是 PM 命令式创建的 DOM,没有 Svelte scope hash,见上方 .entity-mention
     同款注释。 */
  .transcript :global(.badge.as-btn) {
    border: none;
    cursor: pointer;
    font-family: inherit;
  }
  .transcript :global(.badge.as-btn:disabled) {
    cursor: default;
  }
  /* editable-text（段落正文）：静态时无边，hover accent-tint 底 + rounded-sm；
     焦点态不在这里补(ProseMirror 编辑区是整份 contentEditable,不是逐段 focus/blur),
     由上方 .transcript :global(.ProseMirror:focus-visible) 统一给编辑器整体 outline。 */
  .transcript :global(.md-seg .seg-text) {
    cursor: text;
    border-radius: var(--radius-sm);
  }
  .transcript :global(.md-seg .seg-text:hover) {
    background: var(--accent-tint);
  }
  /* 行级操作默认隐身，悬停浮现，保持列表安静 */
  .transcript :global(.md-seg .seg-actions) {
    visibility: hidden;
    margin-left: 0.4em;
  }
  .transcript :global(.md-seg:hover .seg-actions) {
    visibility: visible;
  }
  /* 精修稿实体/徽章浮层与原始稿说话人菜单/删除确认共用的 fixed 定位浮层需要盖过
     transcript 内容(z-index:10 的 topbar 之上),badge-menu 本身默认无 z-index。 */
  .badge-menu.floating {
    z-index: 30;
  }
  /* button-link：无底无边，accent 字，悬停加下划线 */
  .link {
    background: none;
    border: none;
    color: var(--accent);
    cursor: pointer;
    padding: 0.1em 0.25em;
    font-size: 0.8em;
  }
  .link:hover {
    text-decoration: underline;
  }
  .link.danger {
    color: var(--danger);
    font-weight: 500;
  }
  .link:disabled {
    opacity: 0.45;
    cursor: default;
  }
  .link:disabled:hover {
    text-decoration: none;
  }
  /* 视图切换条:Segmented(修订稿/原始逐字稿) + 右侧内容级动作(AI 魔杖 / 重转写幽灵钮) */
  /* 同款窄窗纪律:二段确认警示胶囊展开时宽于窗口即换行,不挤压 segmented */
  .view-switch {
    display: flex;
    align-items: center;
    gap: 0.5rem;
    margin: 0 0 0.75rem;
    flex-wrap: wrap;
  }
  .view-switch .spacer {
    flex: 1;
  }
  /* 重新 Aing 二段确认的警示语:warning 色小字,和确认/取消链接排一行 */
  .refine-warn {
    color: var(--warning-ink);
    font-size: 0.8rem;
  }
  /* 破坏性二段确认的警示胶囊:warning 三件套 token 包裹整组(文案+确认+取消),
     120ms 淡入下移 2px,行内占位不换行不跳版。 */
  .confirm-capsule {
    display: inline-flex;
    align-items: center;
    gap: 0.5rem;
    padding: 0.25rem 0.5rem 0.25rem 0.75rem;
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    border-radius: var(--radius-lg);
    animation: capsule-in 120ms ease;
  }
  @keyframes capsule-in {
    from {
      opacity: 0;
      transform: translateY(-2px);
    }
    to {
      opacity: 1;
      transform: translateY(0);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .confirm-capsule {
      animation: none;
    }
  }
  /* 重新 Aing = Aing 的「施法键」:22px 彩色魔杖(手柄 currentColor,杖头金色星芒 + 紫/青/粉星火,禁 emoji)。
     idle 已是彩色魔杖;hover 星火向外迸射、金星芒放大旋转带光晕;施法(casting)时魔杖大幅挥动 +
     金星芒 360° 旋转脉动发光 + 三色星火依次飞出闪烁。用户要「彩色/更大/动效夸张」——放开 DESIGN 的克制,
     但仍克制在一颗按钮内;respect prefers-reduced-motion。 */
  .reaing {
    display: inline-flex;
    align-items: center;
    gap: 0.5em;
    --wand-gold: #f6b02e;
    --wand-violet: #a678ff;
    --wand-cyan: #46bcff;
    --wand-pink: #ff5fae;
  }
  .reaing .wand {
    flex: none;
    overflow: visible;
  }
  .reaing .wand-star,
  .reaing .spark {
    transform-box: fill-box;
    transform-origin: center;
  }
  .reaing .wand-star {
    fill: var(--wand-gold);
    transition:
      transform 0.3s cubic-bezier(0.2, 0.9, 0.2, 1),
      filter 0.3s ease;
  }
  .reaing .spark {
    opacity: 0;
    transform: scale(0.3);
    transition:
      opacity 0.25s ease,
      transform 0.3s cubic-bezier(0.2, 0.9, 0.2, 1);
  }
  .reaing .spark-a {
    fill: var(--wand-violet);
  }
  .reaing .spark-b {
    fill: var(--wand-cyan);
  }
  .reaing .spark-c {
    fill: var(--wand-pink);
  }
  /* hover:星火向外迸射、金星芒放大旋转带光晕 */
  .reaing:hover:not(:disabled) .wand-star {
    transform: scale(1.3) rotate(20deg);
    filter: drop-shadow(0 0 3px rgba(246, 176, 46, 0.75));
  }
  .reaing:hover:not(:disabled) .spark {
    opacity: 1;
  }
  .reaing:hover:not(:disabled) .spark-a {
    transform: translate(-2px, -2px) scale(1.25);
  }
  .reaing:hover:not(:disabled) .spark-b {
    transform: translate(2.5px, 2px) scale(1.25);
    transition-delay: 0.05s;
  }
  .reaing:hover:not(:disabled) .spark-c {
    transform: translate(-1.5px, 2.5px) scale(1.25);
    transition-delay: 0.1s;
  }
  /* 施法态:覆盖 disabled 变暗(进行中≠不可用),夸张动效全开 */
  .reaing.casting {
    opacity: 1;
    color: var(--ink-secondary);
  }
  .reaing.casting .wand {
    transform-origin: 18% 84%;
    animation: wand-wave-big 1.15s ease-in-out infinite;
  }
  .reaing.casting .wand-star {
    animation: star-spin 1.6s ease-in-out infinite;
  }
  .reaing.casting .spark-a {
    animation: spark-fly-a 1.1s ease-in-out infinite;
  }
  .reaing.casting .spark-b {
    animation: spark-fly-b 1.1s ease-in-out infinite 0.35s;
  }
  .reaing.casting .spark-c {
    animation: spark-fly-c 1.1s ease-in-out infinite 0.7s;
  }
  @keyframes wand-wave-big {
    0%,
    100% {
      transform: rotate(-14deg);
    }
    50% {
      transform: rotate(16deg);
    }
  }
  @keyframes star-spin {
    0% {
      transform: rotate(0) scale(0.85);
      filter: drop-shadow(0 0 0 rgba(246, 176, 46, 0));
    }
    50% {
      transform: rotate(180deg) scale(1.32);
      filter: drop-shadow(0 0 5px rgba(246, 176, 46, 0.9));
    }
    100% {
      transform: rotate(360deg) scale(0.85);
      filter: drop-shadow(0 0 0 rgba(246, 176, 46, 0));
    }
  }
  @keyframes spark-fly-a {
    0%,
    100% {
      opacity: 0;
      transform: translate(0, 0) scale(0.3);
    }
    50% {
      opacity: 1;
      transform: translate(-3px, -3px) scale(1.4);
    }
  }
  @keyframes spark-fly-b {
    0%,
    100% {
      opacity: 0;
      transform: translate(0, 0) scale(0.3);
    }
    50% {
      opacity: 1;
      transform: translate(3.5px, 2.5px) scale(1.4);
    }
  }
  @keyframes spark-fly-c {
    0%,
    100% {
      opacity: 0;
      transform: translate(0, 0) scale(0.3);
    }
    50% {
      opacity: 1;
      transform: translate(-1.5px, 3.5px) scale(1.4);
    }
  }
  @media (prefers-reduced-motion: reduce) {
    .reaing.casting .wand,
    .reaing.casting .wand-star,
    .reaing.casting .spark {
      animation: none;
    }
    .reaing.casting .spark {
      opacity: 0.7;
      transform: scale(1);
    }
    .reaing:hover:not(:disabled) .wand-star {
      transform: none;
    }
  }
  /* 精修稿实体/徽章浮层：position:fixed 定位在触发元素下方（entityPop/refinedBadgePop
     的 rect 来自 getBoundingClientRect），与 .badge-menu 同一套 popover 视觉语言。 */
  .entity-pop {
    z-index: 30;
    display: flex;
    gap: 8px;
    align-items: center;
    padding: 6px 10px;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    font-size: 0.85rem;
  }
  /* menu/popover（改说话人菜单）：surface-press 底、hairline 边、rounded-lg、shadow-popover
     （暗色下 canvas 比承载面更黑，浮层用 canvas 会成"洞"，故底走 surface-press）。 */
  .badge-menu {
    display: inline-flex;
    flex-wrap: wrap;
    gap: 0.25em;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 0.2em 0.4em;
    margin-right: 0.4em;
  }
  .menu-item {
    background: none;
    border: none;
    color: var(--accent);
    cursor: pointer;
    font-size: 0.8em;
    padding: 0.15em 0.4em;
  }
  /* 破坏性菜单项(段删除确认)与 .link.danger 同一色钩:红字提示不可撤销。 */
  .menu-item.danger {
    color: var(--danger);
  }
  /* speaker-badge：soft 底 + 内联配对文字色、rounded-sm、micro 字级
     （底色与文字色均由内联 style 按说话人取，此处不设默认 color——设了也恒被覆盖）。
     :global 见上方 .entity-mention 同款注释:原始稿/修订稿共用同一套 class。 */
  .transcript :global(.badge) {
    display: inline-block;
    min-width: 2.2em;
    text-align: center;
    font-size: 0.78rem;
    font-weight: 500;
    border-radius: var(--radius-sm);
    padding: 0.05em 0.4em;
    margin-right: 0.4em;
  }
  .transcript :global(.ts) {
    color: var(--ink-faint);
    font-size: 0.8em;
    margin-right: 0.4em;
    font-variant-numeric: tabular-nums;
  }
  /* 时间戳按钮化(有音频时):无底无边,hover 变 accent 提示可点播 */
  .transcript :global(.ts-btn) {
    background: none;
    border: none;
    cursor: pointer;
    padding: 0;
    font-family: inherit;
    border-radius: var(--radius-sm);
  }
  .transcript :global(.ts-btn:hover) {
    color: var(--accent);
    text-decoration: underline;
  }
  /* 已中断：沿用 warning 色系，与侧栏同款状态徽标一致 */
  .state.interrupted {
    background: var(--warning-line);
    color: var(--warning-ink);
    font-size: 0.7em;
    font-weight: 500;
    border-radius: var(--radius-md);
    padding: 0.1em 0.45em;
    margin-left: 0.4em;
  }
  /* banner：提示/警告横幅默认 warning 色系（中断提示/跳过行提示） */
  .banner {
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  /* 错误横幅换 danger 色系（加载/编辑/删除等失败提示） */
  .banner.banner-danger {
    background: var(--danger-tint);
    border-color: var(--danger-line);
    color: var(--danger-ink);
  }
  .hint {
    color: var(--ink-faint);
  }
  /* 切换会议的加载占位:安静的次级墨色文字,不抢眼 */
  .loading {
    color: var(--ink-faint);
    padding: 1rem 0;
  }
  /* 相关笔记卡(照会议搭子详情页 .card.col/.appear-list) */
  .related.card.col {
    display: block;
    background: var(--surface);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    padding: 14px 16px;
    margin: 0.75rem 0 0;
  }
  .related .card-title {
    font-size: 0.85rem;
    font-weight: 500;
    margin-bottom: 0.45rem;
  }
  .related .appear-list {
    list-style: none;
    margin: 0;
    padding: 0;
    max-height: 14rem;
    overflow-y: auto;
  }
  .related .appear-row {
    display: flex;
    align-items: baseline;
    gap: 0.6rem;
    padding: 0.28rem 0;
  }
  .related .appear-row a {
    color: var(--ink);
    text-decoration: none;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .related .appear-row a:hover {
    color: var(--accent);
    text-decoration: underline;
  }
  .related .appear-meta {
    color: var(--ink-faint);
    font-size: 0.78rem;
    flex: none;
  }

  /* ── P3 日历行 ── */
  .cal-row {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-wrap: wrap;
  }
  .cal-row .mini {
    border: none;
    background: none;
    color: var(--accent, #4a7dff);
    cursor: pointer;
    font-size: 12px;
    padding: 0 2px;
  }
  .cal-row .mini:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .cal-hint {
    opacity: 0.6;
    font-size: 12px;
  }
  .cal-menu {
    margin: 4px 0 0;
    padding: 6px;
    border: 1px solid var(--border, #3333);
    border-radius: 8px;
    max-width: 420px;
    max-height: 220px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .cal-item {
    display: flex;
    justify-content: space-between;
    gap: 10px;
    padding: 5px 8px;
    border: none;
    background: none;
    border-radius: 6px;
    cursor: pointer;
    text-align: left;
    font-size: 13px;
  }
  .cal-item:hover {
    background: var(--hover, #8881);
  }
  .cal-item .cal-time {
    opacity: 0.6;
    white-space: nowrap;
  }
  /* 勾选多段(乙):选中段高亮 + 底部浮条 */
  :global(.md-seg.sel-on) {
    outline: 2px solid rgba(90, 160, 255, 0.65);
    outline-offset: 1px;
    border-radius: 6px;
  }
  .sel-bar {
    position: fixed;
    left: 50%;
    transform: translateX(-50%);
    bottom: 18px;
    z-index: 55;
    display: flex;
    gap: 14px;
    align-items: center;
    background: var(--pop-bg, #1c1e22);
    border: 1px solid rgba(255, 255, 255, 0.12);
    border-radius: 10px;
    padding: 8px 14px;
    font-size: 13px;
    box-shadow: 0 8px 28px rgba(0, 0, 0, 0.45);
  }
</style>
