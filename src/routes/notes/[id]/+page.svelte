<script lang="ts">
  import { onDestroy, untrack } from "svelte";
  import { onTranscodeDone } from "$lib/events";
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { save } from "@tauri-apps/plugin-dialog";
  import { recording } from "$lib/recording.svelte";
  import AiStateLabel from "$lib/AiStateLabel.svelte";
  import { onRefine, onNoteRenamed } from "$lib/events";
  import {
    getNote,
    renameNote,
    exportNote,
    exportFileName,
    getRefined,
    refineNote,
    formatDate,
    formatDuration,
    speakerLabel,
    speakerColor,
    speakerInk,
    speakerIdCompare,
    editSegment,
    deleteSegment,
    setSegmentSpeaker,
    noteAudioInfo,
    renameRefinedSpeaker,
    assignRefinedPerson,
    assignNoteSpeakerPerson,
    type Note,
    type TrackInfo,
    type RefinedDoc,
    noteRelated,
    type RelatedNote,
    saveRefined,
    type ParagraphPayload,
  } from "$lib/notes";
  import { noteEntityLinks, type EntityLink } from "$lib/graph";
  import { listPeople, type PersonSummary } from "$lib/people";
  import SpeakerChips from "$lib/SpeakerChips.svelte";
  import AudioPlayer from "$lib/AudioPlayer.svelte";
  import MarkdownEditor, { type BadgeAttrs } from "$lib/editor/MarkdownEditor.svelte";

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
  let refining = $state(false);
  let refineRunFailed = $state(false);
  let refineErr = $state("");
  let viewMode = $state<"refined" | "raw">("refined");
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

  /** 展示序:filter+sort 已下沉 NoteStore::load(单一真值源),后端保证无空白段、
      按 (start_ms, seq) 升序,前端直接消费。 */
  const displaySegments = $derived(note ? note.segments : []);
  /** 本笔记正在录制（含暂停）时禁用一切编辑入口（后端另有 guard 兜底）。 */
  const canEdit = $derived(!(recording.isLive && recording.noteId === id));
  const speakerIds = $derived(note ? Object.keys(note.speakers).sort(speakerIdCompare) : []);

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
  /** 原始稿中被 Aing 过滤掉的段（灰显用）。 */
  const discardedSeqs = $derived(new Set(refined?.discarded_seqs ?? []));

  /** 修订稿视图的说话人条数据：从重聚类终稿段落聚合（R* 命名空间，与下方段落
      徽章一致）。在线聚类的 S* 表在此视图不展示——两套命名空间并排必然对不上。
      person_id 一并带上:关联库人物的说话人跨笔记同色、无名时按全局编号兜底。 */
  const refinedSpeakers = $derived.by(() => {
    const m: Record<string, { name: string; sources: string[]; person_id?: string | null }> = {};
    for (const p of refined?.paragraphs ?? []) {
      if (!m[p.speaker]) m[p.speaker] = { name: p.name ?? "", sources: ["mic"], person_id: p.person_id ?? null };
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
    try {
      const newRev = await saveRefined(targetId, payload.revision, payload.paragraphs);
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
      try {
        const latest = await getRefined(targetId);
        // await 后重验守卫:回读期间用户可能已经切走了笔记。
        if (targetId === id && loadedRefinedId === targetId) {
          if (latest) {
            refined = latest;
            syncedRefined = latest;
            syncedEditor = refinedEditor;
          }
          // latest 为 null(极端情况,如笔记目录被清):保持 refined 原样不动,不因
          // 一次回读失败就把已展示的内容整篇清空。
        }
      } catch {
        /* 回读失败:refined 保持原状;本次保存本身已经成功(markSaved 已确认落定) */
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
      const msg = `精修稿保存失败: ${err}`;
      if (msg !== refinedSaveErr) refinedSaveErr = msg;
      // revision 冲突(乐观并发):当前编辑已经落空,重载盘上最新内容重建文档。
      // 非冲突失败(Aing 中/录制中被拒):只保留错误提示,让编辑按 idle 定时器重试
      // (markSaveFailed 已排好下一次)。
      if (String(err).includes("已在别处更新")) {
        try {
          const latest = await getRefined(targetId);
          if (targetId === id && loadedRefinedId === targetId) {
            refined = latest;
            if (latest) {
              // 这里显式 setRefined 已经把编辑器文档重建到位;同时把 latest/当前编辑器
              // 实例记进 syncedRefined/syncedEditor,让下面的同步 effect 认出"已经
              // 同步过"直接跳过——否则 effect 会因 refined 换了身份再 setRefined 一次,
              // 属于重复重建。
              refinedEditor?.setRefined(latest);
              syncedRefined = latest;
              syncedEditor = refinedEditor;
            }
          }
        } catch {
          /* 重载失败保持错误横幅 */
        }
      }
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
    refinedEditor?.flushRefined();
    cancelHideEntityPop();
  });

  // ── 说话人试听:chips 面板「试听他的声音」——不听声音没法确认「说话人 N」是谁。
  //    播该说话人时长最长的一段(代表性最好),重复点击按时长降序换下一段(取前 5,
  //    循环);单段最多听 15s,段尾自动停;用户手动暂停/拖走即退出试听态。 ──
  const PREVIEW_MAX_MS = 15_000;
  let preview = $state<{ sid: string; idx: number; endMs: number } | null>(null);

  function previewSpeaker(sid: string) {
    const source: { speaker?: string | null; start_ms: number; end_ms: number }[] =
      effectiveView === "refined" ? (refined?.paragraphs ?? []) : displaySegments;
    const segs = source
      .filter((p) => p.speaker === sid)
      .sort((a, b) => (b.end_ms - b.start_ms) - (a.end_ms - a.start_ms))
      .slice(0, 5);
    if (segs.length === 0 || !player) return;
    const idx = preview?.sid === sid ? (preview.idx + 1) % segs.length : 0;
    const seg = segs[idx];
    preview = { sid, idx, endMs: Math.min(seg.end_ms, seg.start_ms + PREVIEW_MAX_MS) };
    player.seek(seg.start_ms);
    player.play();
  }

  // 段尾自动停:只在试听态生效,停完清态(不影响用户随后正常播放)。
  $effect(() => {
    if (preview && playerPlaying && playerMs >= preview.endMs) {
      player?.pause();
      preview = null;
    }
  });
  // 用户手动暂停(未到段尾)即视为退出试听;换笔记同样清态。
  $effect(() => {
    if (preview && !playerPlaying && playerMs < preview.endMs - 200) preview = null;
  });
  $effect(() => {
    void id;
    preview = null;
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

  async function refresh() {
    // 并行发起，note 失败才是真正的加载失败；refined/people 是增值层，取不到静默降级。
    const notePromise = getNote(id);
    const refinedPromise = getRefined(id).catch(() => null);
    const peoplePromise = listPeople().catch(() => []);
    try {
      note = await notePromise;
      error = "";
    } catch (e) {
      error = `加载失败: ${e}`;
    }
    refined = await refinedPromise;
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
  // 只在 id 变化时清（本 effect 唯一依赖 id）；编辑后的 refresh() 不经此处，不会闪屏。
  $effect(() => {
    void id;
    // 落盘先于复位:flushRefined 内部用 loadedRefinedId(不是下面即将复位的
    // refined/id)找旧笔记,必须在清空编辑态之前调用。
    refinedEditor?.flushRefined();
    note = null;
    error = "";
    editing = false;
    segMenuPop = null;
    segDeletePop = null;
    refined = null;
    syncedRefined = null;
    syncedEditor = null;
    refining = false;
    refineRunFailed = false;
    refineErr = "";
    confirmRefine = false;
    viewMode = "refined";
    cancelHideEntityPop();
    entityPop = null;
    refinedBadgePop = null;
    refinedSaveErr = "";
  });

  // Aing 进度事件：按 id 注册/解绑（切页时旧监听必须解绑，否则会用旧 note_id 的事件误刷当前页）。
  // running 置 refining=true；stage="all" 是整体完成信号，done/failed 都要重新拉取 refined 并复位。
  $effect(() => {
    const forId = id;
    let unlisten: (() => void) | null = null;
    let disposed = false;
    onRefine((e) => {
      if (e.note_id !== forId) return;
      if (e.state === "running") {
        refining = true;
        refineRunFailed = false;
      }
      if (e.stage === "all" && (e.state === "done" || e.state === "failed")) {
        refining = false;
        refineRunFailed = e.state === "failed";
        getRefined(forId).then((r) => {
          if (forId === id) refined = r;
        });
      }
    }).then((u) => {
      if (disposed) u();
      else unlisten = u;
    });
    return () => {
      disposed = true;
      unlisten?.();
    };
  });

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

  // ── 波形音轨:按音频总长等分 260 桶,取桶内段落 rms 峰值。观感三件套(首版全高
  //    平顶像方块阵,冒烟反馈"不像声音波形"):①按本条录音的 rms 峰值归一(AGC 后
  //    普遍 0.2+,固定封顶会齐刷刷顶格)+γ0.7 拉开动态;②确定性抖动纹理(段级 rms
  //    在段内是平台,乘 ±18% 伪随机破平顶,真实响度包络仍在);③桶多条细。 ──
  const WAVE_BARS = 260;
  const audioTotalMs = $derived(tracks.reduce((m, t) => Math.max(m, t.offset_ms + t.duration_ms), 0));
  const waveform = $derived.by(() => {
    if (audioTotalMs <= 0) return [];
    // 真实音频波形优先(后端转码预计算/懒回填):有声音就有波形,与录音机直觉一致。
    // 多轨(mic/system)按全局时间轴对位取 max;真实数据自带起伏,不加抖动纹理。
    const real: number[] = new Array(WAVE_BARS).fill(0);
    let hasReal = false;
    for (const t of tracks) {
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
      if (playerMs >= seg.start_ms && playerMs < seg.end_ms) s.add(seg.seq);
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
    if (!playerPlaying || !follow) return;
    if (currentSeq !== null && currentSeq !== lastScrolledSeq) {
      lastScrolledSeq = currentSeq;
      centerCurrent();
    }
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

  /** 只需要 start_ms：原始段(SegmentRecord)与 Aing 段(RefinedParagraph)都结构兼容,共用同一播放逻辑。 */
  function playFrom(pos: { start_ms: number }) {
    if (!player) return;
    // 起点落在音频覆盖范围之外(该轨写失败提早停/音频比转写短):忽略点击,
    // 否则 seek 被钳到末尾、play 又视作"播完重来",会莫名跳回 0:00。
    if (pos.start_ms >= player.durationMs()) return;
    player.seek(pos.start_ms);
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
      error = `编辑失败: ${err}`;
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
      error = `删除失败: ${e}`;
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
      error = `修改说话人失败: ${e}`;
      await refresh();
      syncSegments(true);
    }
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
    void segRenderTick;
    if (!el || effectiveView === "refined") return;
    for (const node of el.querySelectorAll<HTMLElement>(".md-seg")) {
      const seq = Number(node.dataset.seq);
      node.classList.toggle("playing", active.has(seq));
      node.classList.toggle("discarded", discarded.has(seq));
      if (discarded.has(seq)) node.title = "已被 AI 过滤";
      else node.removeAttribute("title");
    }
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
      segRenderTick++;
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
      error = `改名失败: ${e}`;
    }
  }

  async function doExport(format: "md") {
    exportMsg = "";
    if (!note) return;
    // await 前快照:保存对话框可以开很久,期间精修完成会把 effectiveView 从
    // 原始稿翻成修订稿(refine 事件监听常驻)、路由/标题也可能变——导出的必须
    // 是点击导出那一刻用户看到的那份。
    const noteId = id;
    const preferRefined = effectiveView === "refined";
    try {
      // 保存对话框让用户挑落盘位置(冒烟反馈:旧流程写进数据目录再开 Finder,
      // 用户以为"只是打开了文件夹")。默认名 = 标题+录音时间;取消返回 null,
      // 静默收手不算失败。所见即所得:看着修订稿点导出就导修订稿。
      const dest = await save({
        defaultPath: exportFileName(note.meta.title, note.meta.started_at),
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
      if (!dest) return;
      const path = await exportNote(noteId, format, preferRefined, dest);
      exportMsg = `已导出：${path}`;
      // 清掉早前失败留下的红色横幅,避免"上面报失败下面报成功"并存。
      if (error.startsWith("导出失败")) error = "";
    } catch (e) {
      error = `导出失败: ${e}`;
    }
  }

  /** 重新 Aing 会整写 refined.json:未关联搭子的说话人改名会被冲掉,这种情况下二段确认。 */
  const refineWouldLoseNames = $derived(
    !!refined?.paragraphs.some((p) => p.name && !p.person_id),
  );
  let confirmRefine = $state(false);

  async function rerunRefine() {
    if (refineWouldLoseNames && !confirmRefine) {
      confirmRefine = true;
      return;
    }
    confirmRefine = false;
    refineErr = "";
    refineRunFailed = false;
    refining = true; // 乐观置位:避免事件到达前的空隙内重复点击触发二次 Aing
    try {
      await refineNote(id);
    } catch (e) {
      refining = false;
      refineRunFailed = true;
      refineErr = `重新执行 AI 失败：${e}`;
    }
  }

  async function doResume() {
    const ok = await recording.resume(id);
    if (ok) goto("/record");
    else
      error = recording.status.startsWith("error:")
        ? recording.status
        : "无法继续录制:请确认没有正在进行的录制";
  }
</script>

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
              <button class="title-btn" title="点击改名" onclick={beginRename}>{note.meta.title}</button>
            </h1>
          {/if}

          <p class="meta">
            {formatDate(note.meta.started_at)} · {formatDuration(durationSecs(note))}
            {#if note.meta.state === "recording"}
              <span class="state interrupted">已中断</span>
            {/if}
          </p>
        </div>

        <!-- 导出动作:图标+文字(冒烟反馈:纯图标看不出功能),button-secondary 形态。
             只留 MD(冒烟反馈:TXT 用不上,按钮撤了);txt 渲染能力在导出层与
             CLI(notes get --format txt)保留,GUI 不再暴露。 -->
        <div class="row">
          <button class="act-btn" onclick={() => doExport("md")}>
            <svg width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
              <path d="M9.5 1.8H4.2a.9.9 0 0 0-.9.9v10.6c0 .5.4.9.9.9h7.6c.5 0 .9-.4.9-.9V5z" />
              <path d="M9.5 1.8V5h3.2" />
              <path d="M5.6 11.6V8.4l1.7 1.9 1.7-1.9v3.2" stroke-width="1.2" />
            </svg>
            导出 MD
          </button>
        </div>
      </div>

      {#if note.meta.state === "recording"}
        <div class="banner">这场录音曾意外中断，中断前的内容已保存。点击下方播放器右侧的红色录音键可接着录。</div>
      {/if}
      {#if note.skipped_lines > 0}
        <div class="banner">有 {note.skipped_lines} 行记录损坏被跳过。</div>
      {/if}
      {#if exportMsg}<p class="hint export-msg">{exportMsg}</p>{/if}

      <!-- 控制行(录音机式,整合一行):播放/暂停 + 波形音轨,行尾圆形红点录音键。
           录制中(含暂停)不出播放器:文件正在写,不做边写边播的半态。 -->
      <div class="transport">
        {#if canEdit && tracks.length > 0}
          <div class="player-slot">
            <AudioPlayer bind:this={player} {tracks} {waveform} bind:currentMs={playerMs} bind:playing={playerPlaying} />
          </div>
        {/if}
        <button
          class="rec-btn"
          disabled={recording.isLive}
          title={recording.isLive ? "已有录制进行中" : "继续录制"}
          aria-label="继续录制"
          onclick={doResume}
        >
          <span class="rec-dot"></span>
        </button>
      </div>

      {#if effectiveView === "refined"}
        <!-- 修订稿视图:只展示重聚类终稿的说话人,不摊开在线 S* 临时簇。
             可直接改名/从会议搭子选人:改名同步声纹库,选人采用库中现名。
             Aing 中禁编辑(管线随后整写 refined.json,后端同款 guard 兜底)。 -->
        <SpeakerChips
          speakers={refinedSpeakers}
          noteId={id}
          editable={!refining}
          {people}
          onRename={(sid, name) => renameRefinedSpeaker(id, sid, name)}
          onPick={(sid, personId) => assignRefinedPerson(id, sid, personId)}
          onPreview={canEdit && tracks.length > 0 ? previewSpeaker : undefined}
          previewingId={preview?.sid ?? null}
          onRenamed={() => {
            refresh();
            recording.bumpNotes();
          }}
        />
      {:else}
        <!-- 原始稿说话人条:改名仍是笔记内本地名;选人关联(写 speakers.json person_id)
             与修订稿同一面板,录制中(canEdit=false)不给选人区(后端 writer 独占)。 -->
        <SpeakerChips
          speakers={note.speakers}
          noteId={id}
          editable={true}
          counts={segCounts}
          people={canEdit ? people : undefined}
          onPick={canEdit ? (sid, personId) => assignNoteSpeakerPerson(id, sid, personId) : undefined}
          onPreview={canEdit && tracks.length > 0 ? previewSpeaker : undefined}
          previewingId={preview?.sid ?? null}
          onRenamed={() => {
            refresh();
            recording.bumpNotes();
          }}
        />
      {/if}

      <div class="view-switch">
        <button
          class="link"
          class:active={effectiveView === "refined"}
          disabled={!refinedAvailable}
          title={refinedAvailable ? "" : "尚无修订稿"}
          onclick={() => (viewMode = "refined")}
        >
          修订稿
        </button>
        <button class="link" class:active={effectiveView === "raw"} onclick={() => (viewMode = "raw")}>
          原始逐字稿
        </button>
        <span class="spacer"></span>
        {#if confirmRefine}
          <!-- 二段确认(仅当存在未关联搭子的手工改名):整写 refined.json 会冲掉它们 -->
          <span class="refine-warn">未关联搭子的说话人改名将丢失</span>
          <button class="link danger" onclick={rerunRefine}>确认重新 AI</button>
          <button class="link" onclick={() => (confirmRefine = false)}>取消</button>
        {:else}
          <button
            class="reaing"
            class:casting={refining}
            disabled={refining || note.meta.state !== "complete"}
            onclick={rerunRefine}
            title={aiState === "running" ? "Aing，正在执行" : aiState === "complete" ? "AI 已完成，点击重新执行" : aiState === "failed" ? "AI 执行失败，点击重试" : "执行 AI"}
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
        {/if}
      </div>
    </div>

    {#if refineErr}<div class="banner banner-danger">{refineErr}</div>{/if}
    {#if effectiveView === "refined" && refined}
      <!-- 精修稿保存错误:独立粘性 banner,不复用共享 error——那个会被 refresh()
           成功悄悄清掉,持续性拒绝(Aing 中反复被拒)会因此再无提示。 -->
      {#if refinedSaveErr}<div class="banner banner-danger">{refinedSaveErr}</div>{/if}
      {#if refined.stages.llm === "partial"}
        <div class="banner">部分段落 AI 处理失败，已保留原文，可重新执行。</div>
      {:else if refined.stages.llm === "failed"}
        <div class="banner banner-danger">在线 AI 处理失败，当前展示本地处理结果。</div>
      {/if}
    {/if}

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
            onBadgeClick={(attrs, rect) => (refinedBadgePop = { attrs, rect })}
            onPlayFrom={(ms) => playFrom({ start_ms: ms })}
          />
        </div>
        {#if refined.paragraphs.length === 0}
          <p class="hint">（修订稿为空，可直接输入补充内容）</p>
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
            onBadgeClick={(attrs, rect) => {
              if (!canEdit) return;
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
          <p class="hint">（这场会议没有转写内容）</p>
        {/if}
      {/if}
    </div>

    {#if segMenuPop && note}
      <!-- 原始稿说话人浮层:锚定被点击徽章的屏幕矩形,与 entity-pop/refinedBadgePop
           同一套 position:fixed 套路;菜单项复用旧版 badge-menu 的按钮结构与样式。 -->
      <div
        class="badge-menu floating"
        style="position: fixed; left: {segMenuPop.rect.left}px; top: {segMenuPop.rect.bottom + 4}px;"
      >
        {#each speakerIds as sid (sid)}
          <button class="menu-item" onclick={() => doSetSpeaker(segMenuPop!.seq, sid)}>
            {speakerLabel(sid, "mic", note.speakers)}
          </button>
        {/each}
        <button class="menu-item new" onclick={() => doSetSpeaker(segMenuPop!.seq, "new")}>＋ 新说话人</button>
        <button class="menu-item" onclick={() => (segMenuPop = null)}>取消</button>
      </div>
    {/if}
    {#if segDeletePop}
      <div
        class="badge-menu floating"
        style="position: fixed; left: {segDeletePop.rect.left}px; top: {segDeletePop.rect.bottom + 4}px;"
      >
        <button class="menu-item danger" onclick={() => doDeleteSeg(segDeletePop!.seq)}>确认删除</button>
        <button class="menu-item" onclick={() => (segDeletePop = null)}>取消</button>
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
            打开知识图谱
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
        <button class="link" onclick={() => (refinedBadgePop = null)}>关闭</button>
      </div>
    {/if}

    {#if related.length > 0}
      <section class="card col related">
        <div class="card-title">相关笔记</div>
        <ul class="appear-list">
          {#each related as n (n.id)}
            <li class="appear-row">
              <a href="/notes/{n.id}">{n.title}</a>
              <span class="appear-meta">共享 {n.shared_entities} 个实体</span>
            </li>
          {/each}
        </ul>
      </section>
    {/if}

    <!-- 跟随被用户上滑打断时的返回入口(与录制页同款):sticky 钉滚动视口底部 -->
    <div class="jump-anchor">
      {#if !follow && playerPlaying}
        <button class="jump" onclick={resumeFollow}>↓ 回到播放位置</button>
      {/if}
    </div>
  {:else if !error}
    <!-- 加载态:切换会议到 note 就绪之间的空窗(长会议 load 可能数百毫秒),
         给一个安静的占位,避免"点了没反应"的错觉。error 分支已在上方单独渲染。 -->
    <p class="loading">加载中…</p>
  {/if}
</main>

<style>
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
  /* 标题行:左标题+时间,右上角动作按钮(冒烟反馈:按钮移右上) */
  .header {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 1rem;
  }
  .header-main {
    flex: 1;
    min-width: 0;
  }
  .row {
    display: flex;
    gap: 0.5rem;
    align-items: center;
    flex: none;
    justify-content: flex-end;
    padding-top: 0.2rem;
  }
  /* 头部动作钮:button-secondary 形态 + 图标与文字并排(纯图标看不出功能) */
  .act-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.4em;
    padding: 0.4em 0.8em;
    font-size: 0.85rem;
    color: var(--ink-secondary);
  }
  .act-btn:hover {
    color: var(--ink);
  }
  /* 控制行:录音 + 播放器整合一行(录音机式) */
  .transport {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin: 0 0 1rem;
  }
  .player-slot {
    flex: 1;
    min-width: 0;
  }
  /* 继续录制:录音机标志式圆形录音键(圆环 + 居中红点),行尾右置,与播放键同语言。
     纯图标是用户拍板的特例(2026-07-07:录音红点属录音机通识符号,文字反而挤占
     音轨宽度),悬停 title/aria-label 兜底可达性。无播放器时 margin-left:auto 仍靠右。 */
  .rec-btn {
    width: 2.4rem;
    height: 2.4rem;
    padding: 0;
    flex: none;
    margin-left: auto;
    display: inline-flex;
    align-items: center;
    justify-content: center;
    border-radius: var(--radius-full);
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
  /* 被 Aing 过滤掉的段(原始稿视角):灰显但保留可读,不做删除线/隐藏 */
  .transcript :global(.md-seg.discarded) {
    opacity: 0.38;
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
  /* 视图切换条:修订稿/原始逐字稿(btn-link,当前态 tint 底高亮) + 重新 Aing(默认 button-secondary)。 */
  .view-switch {
    display: flex;
    align-items: center;
    gap: 0.2rem;
    margin: 0 0 0.75rem;
  }
  .view-switch .link {
    font-size: 0.85rem;
    font-weight: 500;
    padding: 0.35em 0.7em;
    border-radius: var(--radius-md);
  }
  .view-switch .link.active {
    background: var(--accent-tint);
    color: var(--accent);
  }
  .view-switch .spacer {
    flex: 1;
  }
  /* 重新 Aing 二段确认的警示语:warning 色小字,和确认/取消链接排一行 */
  .refine-warn {
    color: var(--warning-ink);
    font-size: 0.8rem;
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
  .menu-item.new {
    font-weight: 500;
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
</style>
