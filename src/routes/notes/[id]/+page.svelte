<script lang="ts">
  import { untrack } from "svelte";
  import { onTranscodeDone } from "$lib/events";
  import { page } from "$app/stores";
  import { goto } from "$app/navigation";
  import { revealItemInDir } from "@tauri-apps/plugin-opener";
  import { recording } from "$lib/recording.svelte";
  import { onRefine, onNoteRenamed } from "$lib/events";
  import {
    getNote,
    renameNote,
    exportNote,
    getRefined,
    refineNote,
    formatTs,
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
    type SegmentRecord,
    type TrackInfo,
    type RefinedDoc,
  } from "$lib/notes";
  import { listPeople, type PersonSummary } from "$lib/people";
  import SpeakerChips from "$lib/SpeakerChips.svelte";
  import AudioPlayer from "$lib/AudioPlayer.svelte";

  let note = $state<Note | null>(null);
  let error = $state("");
  let editing = $state(false);
  let editingTitle = $state("");
  let exportMsg = $state("");

  // 段落编辑状态(常驻编辑态:focusedSeq 只用于刷新守卫,防外部刷新吹掉输入中的内容)
  let focusedSeq = $state<number | null>(null);
  let confirmSeq = $state<number | null>(null);
  let speakerMenuSeq = $state<number | null>(null);

  // Aing 稿视图:refined 与 note 一样按 id 拉取、id 切换即复位(见下方 id-effect)。
  let refined = $state<RefinedDoc | null>(null);
  let refining = $state(false);
  let refineErr = $state("");
  let viewMode = $state<"refined" | "raw">("refined");
  // 会议搭子人物列表:Aing 稿说话人条的「选人」面板用。增值层,取失败静默按空处理。
  let people = $state<PersonSummary[]>([]);

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

  /** Aing 稿是否可展示：无 Aing 结果、或笔记尚未 complete（例如中断续录中）一律强制原始稿。 */
  const refinedAvailable = $derived(!!refined && note?.meta.state === "complete");
  /** 实际渲染的视图：viewMode 是用户意图，refinedAvailable=false 时无条件降级为 raw。 */
  const effectiveView = $derived(refinedAvailable ? viewMode : "raw");
  /** 原始稿中被 Aing 过滤掉的段（灰显用）。 */
  const discardedSeqs = $derived(new Set(refined?.discarded_seqs ?? []));

  /** Aing 稿视图的说话人条数据：从重聚类终稿段落聚合（R* 命名空间，与下方段落
      徽章一致）。在线聚类的 S* 表在此视图不展示——两套命名空间并排必然对不上。
      person_id 一并带上:关联库人物的说话人跨笔记同色、无名时按全局编号兜底。 */
  const refinedSpeakers = $derived.by(() => {
    const m: Record<string, { name: string; sources: string[]; person_id?: string | null }> = {};
    for (const p of refined?.paragraphs ?? []) {
      if (!m[p.speaker]) m[p.speaker] = { name: p.name ?? "", sources: ["mic"], person_id: p.person_id ?? null };
    }
    return m;
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

  // id 切换：无条件复位一切编辑态 + Aing 视图态（否则会短暂展示上一篇笔记的 Aing 稿/进度）。
  // 同时清空 note/error：切换到长会议时后端 load 可能耗时数百毫秒，不清空会一直挂着
  // 上一篇的正文直到新数据整页跳变（观感=点了没反应、卡一下），清空后立即出加载态。
  // 只在 id 变化时清（本 effect 唯一依赖 id）；编辑后的 refresh() 不经此处，不会闪屏。
  $effect(() => {
    void id;
    note = null;
    error = "";
    editing = false;
    focusedSeq = null;
    speakerMenuSeq = null;
    confirmSeq = null;
    refined = null;
    refining = false;
    refineErr = "";
    confirmRefine = false;
    viewMode = "refined";
  });

  // Aing 进度事件：按 id 注册/解绑（切页时旧监听必须解绑，否则会用旧 note_id 的事件误刷当前页）。
  // running 置 refining=true；stage="all" 是整体完成信号，done/failed 都要重新拉取 refined 并复位。
  $effect(() => {
    const forId = id;
    let unlisten: (() => void) | null = null;
    let disposed = false;
    onRefine((e) => {
      if (e.note_id !== forId) return;
      if (e.state === "running") refining = true;
      if (e.stage === "all" && (e.state === "done" || e.state === "failed")) {
        refining = false;
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
  // 刷新：任何编辑进行中都跳过（编辑态是 effect 依赖，编辑结束会自动重跑并刷新）。
  $effect(() => {
    void id;
    void recording.notesVersion;
    if (editing || focusedSeq !== null || speakerMenuSeq !== null) return;
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

  function segFocus(s: SegmentRecord) {
    focusedSeq = s.seq;
    speakerMenuSeq = null;
    confirmSeq = null;
  }

  /** 失焦提交:空文本或未变则还原显示(去段须走显式删除按钮)。
      失败时手动把 DOM 文本还原为提交前基线——canonical 未变时 Svelte 不会重设
      被用户敲过的文本节点,不还原会出现界面与落盘不一致。 */
  async function segBlur(e: FocusEvent, s: SegmentRecord) {
    const el = e.currentTarget as HTMLElement;
    focusedSeq = null;
    const text = (el.textContent ?? "").trim();
    if (!text || text === s.text) {
      el.textContent = s.text;
      return;
    }
    try {
      await editSegment(id, s.seq, s.text, text);
      await refresh();
    } catch (err) {
      el.textContent = s.text;
      error = `编辑失败: ${err}`;
      await refresh(); // 乐观冲突：重载最新内容
    }
  }

  async function doDeleteSeg(s: SegmentRecord) {
    confirmSeq = null;
    try {
      await deleteSegment(id, s.seq, s.text);
      await refresh();
    } catch (e) {
      error = `删除失败: ${e}`;
      await refresh();
    }
  }

  async function doSetSpeaker(s: SegmentRecord, speakerId: string) {
    speakerMenuSeq = null;
    try {
      await setSegmentSpeaker(id, s.seq, s.text, speakerId);
      await refresh();
    } catch (e) {
      error = `修改说话人失败: ${e}`;
      await refresh();
    }
  }

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
    try {
      // 所见即所得:看着 Aing 稿点导出就导 Aing 稿,原始稿视图导原始逐字稿。
      const path = await exportNote(id, format, effectiveView === "refined");
      exportMsg = `已导出：${path}`;
      await revealItemInDir(path);
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
    refining = true; // 乐观置位:避免事件到达前的空隙内重复点击触发二次 Aing
    try {
      await refineNote(id);
    } catch (e) {
      refining = false;
      refineErr = `重新 Aing 失败: ${e}`;
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
        <!-- Aing 稿视图:只展示重聚类终稿的说话人,不摊开在线 S* 临时簇。
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
             与 Aing 稿同一面板,录制中(canEdit=false)不给选人区(后端 writer 独占)。 -->
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
          title={refinedAvailable ? "" : "尚无 Aing 稿"}
          onclick={() => (viewMode = "refined")}
        >
          Aing 稿
        </button>
        <button class="link" class:active={effectiveView === "raw"} onclick={() => (viewMode = "raw")}>
          原始逐字稿
        </button>
        <span class="spacer"></span>
        {#if confirmRefine}
          <!-- 二段确认(仅当存在未关联搭子的手工改名):整写 refined.json 会冲掉它们 -->
          <span class="refine-warn">未关联搭子的说话人改名将丢失</span>
          <button class="link danger" onclick={rerunRefine}>确认重新 Aing</button>
          <button class="link" onclick={() => (confirmRefine = false)}>取消</button>
        {:else}
          <button disabled={refining || note.meta.state !== "complete"} onclick={rerunRefine}>
            {refining ? "Aing 中…" : "重新 Aing"}
          </button>
        {/if}
      </div>
    </div>

    {#if refineErr}<div class="banner banner-danger">{refineErr}</div>{/if}
    {#if effectiveView === "refined" && refined}
      {#if refined.stages.llm === "partial"}
        <div class="banner">部分段落 Aing 失败，已保留原文，可重新 Aing。</div>
      {:else if refined.stages.llm === "failed"}
        <div class="banner banner-danger">LLM Aing 失败，当前展示本地 Aing 结果。</div>
      {/if}
    {/if}

    <div class="transcript" class:live={playerPlaying} bind:this={transcriptEl}>
      {#if effectiveView === "refined" && refined}
        {#each refined.paragraphs as p, i (i)}
          <div class="para">
            <span
              class="badge"
              style="background: {speakerColor(p.speaker, 'mic', refinedSpeakers)}; color: {speakerInk(p.speaker, 'mic', refinedSpeakers)}"
            >
              {speakerLabel(p.speaker, "mic", refinedSpeakers)}
            </span>
            {#if tracks.length > 0}
              <button class="ts ts-btn" title="从此处播放" onclick={() => playFrom({ start_ms: p.start_ms })}>
                {formatTs(p.start_ms)}
              </button>
            {:else}
              <span class="ts">{formatTs(p.start_ms)}</span>
            {/if}
            <span class="para-text">{p.text}</span>
          </div>
        {/each}
        {#if refined.paragraphs.length === 0}
          <p class="hint">（Aing 稿为空）</p>
        {/if}
      {:else}
        {#each displaySegments as seg (seg.seq)}
          <div
            class="seg"
            class:playing={activeSeqs.has(seg.seq)}
            class:discarded={discardedSeqs.has(seg.seq)}
            title={discardedSeqs.has(seg.seq) ? "已被 Aing 过滤" : undefined}
            data-seq={seg.seq}
          >
            {#if canEdit && speakerMenuSeq === seg.seq}
              <span class="badge-menu">
                {#each speakerIds as sid (sid)}
                  <button class="menu-item" onclick={() => doSetSpeaker(seg, sid)}>
                    {speakerLabel(sid, seg.source, note.speakers)}
                  </button>
                {/each}
                <button class="menu-item new" onclick={() => doSetSpeaker(seg, "new")}>＋ 新说话人</button>
                <button class="menu-item" onclick={() => (speakerMenuSeq = null)}>取消</button>
              </span>
            {:else}
              <button
                class="badge as-btn"
                style="background: {speakerColor(seg.speaker, seg.source, note.speakers)}; color: {speakerInk(seg.speaker, seg.source, note.speakers)}"
                disabled={!canEdit}
                title={canEdit ? "点击改说话人" : ""}
                onclick={() => (speakerMenuSeq = seg.seq)}
              >
                {speakerLabel(seg.speaker, seg.source, note.speakers)}
              </button>
            {/if}
            {#if tracks.length > 0}
              <button class="ts ts-btn" title="从此处播放" onclick={() => playFrom(seg)}>
                {formatTs(seg.start_ms)}
              </button>
            {:else}
              <span class="ts">{formatTs(seg.start_ms)}</span>
            {/if}
            {#if canEdit}
              <!-- 常驻编辑态(冒烟反馈):contenteditable 保持行内排版,点击即打字,无换态换布局。
                   失焦保存,Enter 提交,Esc 还原;删除仍走右侧按钮。 -->
              <span
                class="seg-text editable"
                contenteditable="plaintext-only"
                role="textbox"
                tabindex="0"
                spellcheck="false"
                onfocus={() => segFocus(seg)}
                onblur={(e) => segBlur(e, seg)}
                onkeydown={(e) => {
                  const el = e.currentTarget as HTMLElement;
                  if (e.key === "Enter") {
                    e.preventDefault();
                    el.blur();
                  }
                  if (e.key === "Escape") {
                    el.textContent = seg.text;
                    el.blur();
                  }
                }}>{seg.text}</span>
              <span class="seg-actions">
                {#if confirmSeq === seg.seq}
                  <button class="link danger" onclick={() => doDeleteSeg(seg)}>确认删除</button>
                  <button class="link" onclick={() => (confirmSeq = null)}>取消</button>
                {:else}
                  <button class="link" onclick={() => (confirmSeq = seg.seq)}>删除</button>
                {/if}
              </span>
            {:else}
              <span class="seg-text">{seg.text}</span>
            {/if}
          </div>
        {/each}
        {#if displaySegments.length === 0}
          <p class="hint">（这场会议没有转写内容）</p>
        {/if}
      {/if}
    </div>

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
  .transcript.live .seg {
    color: var(--ink-secondary);
  }
  .seg {
    margin: 0 0 6px;
    line-height: 1.7;
    border-radius: var(--radius-sm);
    transition:
      background 120ms ease,
      font-size 0.2s ease,
      color 0.2s ease;
  }
  /* 播放跟随:当前段 accent-tint 底,与 editable hover 同色系,安静不抢内容 */
  .seg.playing {
    background: var(--accent-tint);
  }
  /* 被 Aing 过滤掉的段(原始稿视角):灰显但保留可读,不做删除线/隐藏 */
  .seg.discarded {
    opacity: 0.38;
  }
  /* Aing 稿段落:与 .seg 同排版语言,文本只读(无 editable/hover 态) */
  .para {
    margin: 0 0 6px;
    line-height: 1.7;
  }
  .para-text {
    white-space: pre-wrap;
  }
  /* 当前播放段(仅播放中):放大 + 主墨色 + 轻投影,歌词感;负边距抵掉内缩,行左缘对齐不跳 */
  .transcript.live .seg.playing {
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
  .badge.as-btn {
    border: none;
    cursor: pointer;
    font-family: inherit;
  }
  .badge.as-btn:disabled {
    cursor: default;
  }
  /* editable-text（段落）：静态时无边，hover accent-tint 底 + rounded-sm，focus accent outline */
  .seg-text.editable {
    cursor: text;
    border-radius: var(--radius-sm);
  }
  .seg-text.editable:hover {
    background: var(--accent-tint);
  }
  .seg-text.editable:focus {
    outline: 2px solid var(--accent);
    background: var(--canvas);
  }
  /* 行级操作默认隐身，悬停浮现，保持列表安静 */
  .seg-actions {
    visibility: hidden;
    margin-left: 0.4em;
  }
  .seg:hover .seg-actions {
    visibility: visible;
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
  /* 视图切换条:Aing 稿/原始逐字稿(btn-link,当前态 tint 底高亮) + 重新 Aing(默认 button-secondary)。 */
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
  /* speaker-badge：soft 底 + 内联配对文字色、rounded-sm、micro 字级
     （底色与文字色均由内联 style 按说话人取，此处不设默认 color——设了也恒被覆盖）。 */
  .badge {
    display: inline-block;
    min-width: 2.2em;
    text-align: center;
    font-size: 0.78rem;
    font-weight: 500;
    border-radius: var(--radius-sm);
    padding: 0.05em 0.4em;
    margin-right: 0.4em;
  }
  .ts {
    color: var(--ink-faint);
    font-size: 0.8em;
    margin-right: 0.4em;
    font-variant-numeric: tabular-nums;
  }
  /* 时间戳按钮化(有音频时):无底无边,hover 变 accent 提示可点播 */
  .ts-btn {
    background: none;
    border: none;
    cursor: pointer;
    padding: 0;
    font-family: inherit;
    border-radius: var(--radius-sm);
  }
  .ts-btn:hover {
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
</style>
