<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onPlayerPos, onPlayerStopped } from "$lib/events";
  import { formatTs, type TrackInfo } from "$lib/notes";
  import { t } from "$lib/i18n/index.svelte";
  import { playback, shouldStopOnCleanup } from "$lib/playback.svelte";

  /* 多轨播放器(原生引擎):音频在 Rust 里单条 cpal 输出流按 offset 混音——WebView
     只画 UI。此前 <audio> 方案在打包版(tauri:// 文档源)被 WKWebView 按自动播放策略
     处理:窗口不可见 5 秒宽限后媒体会话被打断,后台播放必停(2026-07-10 系统日志实锤);
     Web Audio 增益路由更是整体静音。原生化后这一类 WebView 媒体坑全消。
     时钟在 Rust:player_pos 事件(~200ms,播/停/seek 立即补发)驱动 currentMs/playing;
     双轨对齐由单输出流按构造保证,前端不再有同步循环。 */
  let {
    tracks,
    waveform = [],
    currentMs = $bindable(0),
    playing = $bindable(false),
    noteId,
    title,
    onLoaded,
    onUserPause,
    rangeStartMs = $bindable(null),
    rangeEndMs = $bindable(null),
    onRangeDrag,
    onRangeDragEnd,
    cuts = [],
  }: {
    tracks: TrackInfo[];
    /** 音轨波形(0..1 归一条高,按时间等分;由页面从段落 rms 聚合)。空数组退化为平轨。 */
    waveform?: number[];
    currentMs?: number;
    playing?: boolean;
    /** 本播放器所属笔记的身份:建立播放会话用(迷你条要显示标题、点标题要能跳回)。
        不传则不建立会话——本组件也被别处复用时保持现状行为。 */
    noteId?: string;
    title?: string;
    /** 装载成功回调(可选):tracks 变化触发的每次 player_load resolve 后调用。
        宿主用它在重装后恢复播放位置(A/B 切换保位置,codex P2)——原生核心
        每次装载都从 0/paused 起,不回调宿主无从得知何时可以 seek。 */
    onLoaded?: () => void;
    /** 用户亲手点了暂停键(可选)。宿主的「试听」要区分「用户主动停」和心跳事件里
        seek/play 间隙采到的 playing=false——从 playing 翻转推断必有竞态
        (2026-08-21 用户实测:整篇一直放),只有这里是确定的用户意图。 */
    onUserPause?: () => void;
    /** 圈选范围两游标(导出用,会话态):null=未圈定,等效 0/全长。绑定给宿主,
        导出时由宿主判定是否只导圈定段。 */
    rangeStartMs?: number | null;
    rangeEndMs?: number | null;
    /** 拖动游标期间逐帧回调(which=哪个游标,ms=当前位置):宿主做逐字稿联动高亮。 */
    onRangeDrag?: (which: "start" | "end", ms: number) => void;
    onRangeDragEnd?: () => void;
    /** 音频删减区间(非破坏性剪辑表):波形上红显,并推给内核让播放跳过。 */
    cuts?: { start_ms: number; end_ms: number }[];
  } = $props();

  const totalMs = $derived(tracks.reduce((m, t) => Math.max(m, t.offset_ms + t.duration_ms), 0));

  /** 装载失败/播放失败的可视化(排障关键:错误必须浮出水面,不许静默)。 */
  let trackErrors = $state<string[]>([]);
  function reportError(source: string, detail: string) {
    const msg = `${source}: ${detail}`;
    if (!trackErrors.includes(msg)) trackErrors = [...trackErrors, msg];
  }

  // 装载:tracks 变化(进页/续录/transcode_done 重拉/A/B 切换)即重载原生播放器。
  // m4a 首次播放需解码到缓存(秒级、命令内完成),play() await 此 promise 即拿到就绪;
  // 缓存跨会话复用,二次装载瞬时。卸载/切笔记 → player_stop 停流放资源。
  //
  // 串行链 + 代次守卫(codex 第三轮 P1):player_load 后端在异步解码/对齐完成后才
  // 写全局 PlayerHandle,并发装载谁后完成谁生效——快速切 A/B 时旧装载可能最后落地,
  // UI 显示 B 实播 A。新装载排在旧 promise settle 之后(旧写入先落定,last-write
  // 恒为最新意图);onLoaded 只在代次仍是最新时回调,过期装载不消耗恢复现场。
  let loadPromise: Promise<number> | null = null;
  let loadGen = 0;
  /** 装载进行中(解码/对齐/门控可能要几十秒,长录音首次尤甚):给出可见反馈,
   * 免得用户点播放"毫无反应"(2026-08-10 排障:装载期间点击只是静默排队)。 */
  let loading = $state(false);
  // 串行链单独存放:effect 的 cleanup 会先把 loadPromise 置 null 再跑下一轮,
  // 若用 loadPromise 当前驱,新一轮读到的恒是 null,串行化形同虚设。本变量只进
  // 不清(settle 后即释放引用,无泄漏),cleanup 不碰它。
  let loadChain: Promise<unknown> = Promise.resolve();
  /** 过期装载的取消哨兵:排队中的 play/seek 链在 catch 里识别后静默复位(playing
   * 回 false),不当错误上报——它不是故障,只是用户切走了。 */
  const SUPERSEDED = "player-load-superseded";
  /** 本实例最后一次成功装载的**后端**代次:cleanup 的条件停止(ifGen)用——旧组件
   * fire-and-forget 的 stop 可能晚于新组件的装载执行,带条件后后端代次已前进就
   * no-op,不作废别人的装载(Codex 十轮 P1)。 */
  let lastBackendGen: number | null = null;
  /** 重装触发器:托盘停播后自增一次,让下面的装载 effect 再跑一轮(见 onPlayerStopped)。 */
  let reloadKey = $state(0);
  /** 每轨静音(源名 → 静音):Rust 混音时跳过该轨,双轨同步与时钟零影响。
   * 用途:双轨串音的笔记(外放+蓝牙延迟致 AEC 失效)静掉一轨即无回音。
   * 声明提前到装载 effect 之上:每次装载成功要按它补发一遍(见下方)。 */
  let muted = $state<Record<string, boolean>>({});
  $effect(() => {
    // 托盘停播后的重装入口:核被后端拆了,本 effect 必须再跑一轮才有得播(见下方
    // onPlayerStopped)。读一下即建立依赖,值本身无意义。
    void reloadKey;
    trackErrors = [];
    if (tracks.length === 0) {
      loadPromise = null;
    } else {
      const gen = ++loadGen;
      const payload = tracks.map((t) => ({ path: t.path, offset_ms: t.offset_ms, source: t.source }));
      const prev = loadChain;
      loading = true;
      const p = (async () => {
        try {
          await prev;
        } catch {
          /* 旧装载失败不阻断新装载 */
        }
        // 排队期间实例已卸载/换代(切笔记):放弃发起,不让过期装载进后端抢内核。
        // 后端另有代次守卫兜底(跨实例并发窗口),这里省掉一次注定作废的重装载。
        // 必须 reject 而非 resolve(Codex P1):成功 resolve 会让排在本 promise 上的
        // play()/seek() 继续 invoke,打到新装载的**另一篇笔记**内核上。
        if (gen !== loadGen) throw SUPERSEDED;
        // 后端 player_load 进门就 stop_stream 杀旧核,会话必须在**发起装载之前**
        // 同步作废——放在成功之后同步的话,装载失败/被取代/在途离页三条路径都会
        // 漏掉,留下一个指着已死内核的会话:迷你条挂着旧笔记、进度僵住、按钮点不动。
        const prevSession = playback.session
          ? { session: playback.session, atMs: playback.currentMs, playing: playback.playing }
          : null;
        playback.clear();
        const res = await invoke<{ total_ms: number; gen: number }>("player_load", { tracks: payload }).catch((e) => {
          // 后端代次门的拒绝统一折算成取消哨兵(Codex P2):凡 invoke 失败且本地已
          // 换代,必是切换导致的取代而非故障——不按本地化文案匹配,按换代事实判定;
          // 否则排队的 play 会把「已取代」当播放错误刷进新轨的 UI。
          if (gen !== loadGen) throw SUPERSEDED;
          throw e;
        });
        // invoke 期间换代/卸载:后端已发布(装载成功)但本实例意图已作废——发条件
        // 停止回收自己刚发布的核(代次已被更新者接管则 no-op),本地拒绝让排队的
        // play/seek 不对导航后的新状态开火。
        if (gen !== loadGen) {
          void invoke("player_stop", { ifGen: res.gen }).catch(() => {});
          throw SUPERSEDED;
        }
        lastBackendGen = res.gen;
        // 静音是**内核态**,新核每轨一律 muted=false;不补发的话开关显示"已静音"而声音
        // 照出,再点一次还只是发 false(看着像开关坏了)。凡装载成功即按本地表补一遍——
        // 不止托盘停播这条新路径,转码完成重拉/续录/A-B 切换一直都缺这一步(Codex P2)。
        // 独奏态同样是内核态,重装后一并补发(否则试听中途换轨会漏掉压制)。
        for (const tr of tracks) {
          if (effMuted(tr.source)) void invoke("player_set_muted", { source: tr.source, muted: true }).catch(() => {});
        }
        // 同一篇笔记重装(转码完成重拉/续录/A-B 切换):会话按新代次恢复,位置与
        // 播放态沿用重装前的现场。装的是别的笔记则不恢复——上面已经作废了。
        // totalMs 用本次装载返回的 res.total_ms:续录会让笔记变长,沿用重装前的
        // 旧值会让迷你条的进度环卡在旧总长上(算出来的百分比偏大甚至提前满格)。
        if (prevSession && noteId && prevSession.session.noteId === noteId) {
          playback.restore(
            { ...prevSession.session, gen: res.gen, totalMs: res.total_ms },
            prevSession.atMs,
            prevSession.playing,
          );
        }
        onLoaded?.();
        return res.total_ms;
      })();
      loadChain = p.catch(() => {});
      p.then(
        () => {
          if (gen === loadGen) loading = false;
        },
        () => {
          if (gen === loadGen) loading = false;
        },
      );
      loadPromise = p.catch((e) => {
        if (gen === loadGen && e !== SUPERSEDED) reportError(t("notes.player.errLoad"), `${e}`);
        throw e;
      });
      // 终端兜底(Codex P2):无人排队 play/seek 时,上面的 rethrow 会成为无消费的
      // 拒绝,快速切换笔记就刷 unhandledrejection。挂一个空 catch 收尾——排队命令
      // 链在 loadPromise 上,仍能看到拒绝,不受影响。
      loadPromise.catch(() => {});
    }
    return () => {
      // 换代:让已排队但尚未发起的旧装载放弃(上方 gen 检查),防旧实例的装载
      // 在切笔记后才进入后端、掐掉新页面刚起的播放(2026-08-10 排障)。
      loadGen++;
      loadPromise = null;
      // 条件停止(Codex 十轮 P1):只回收**自己**最后一次成功装载的核——迟到的
      // 本 stop 若发现后端代次已前进(新组件已在装载),no-op 不拆别人;从未成功
      // 装载过则无核可收,不发。在途装载的回收由上方 invoke 后的换代分支自理。
      //
      // 所有权:内核归**会话**所有,不再归组件所有。本组件装的核若正是活动会话,
      // 卸载不停它——那正是"切页继续播放"。其余情形语义同现状。
      // 注意 cleanup 不等于组件卸载:本 effect 依赖 tracks,转码完成重拉、续录、
      // A/B 切换都会重跑它,所以这里绝不能做"登记会话"之类的副作用。
      const g = lastBackendGen;
      lastBackendGen = null;
      if (shouldStopOnCleanup(g, playback.session?.gen ?? null)) {
        void invoke("player_stop", { ifGen: g }).catch(() => {});
      }
    };
  });

  // Rust 时钟 → UI:位置事件驱动进度/歌词跟随;播完事件自带 playing=false。
  $effect(() => {
    const un = onPlayerPos((e) => {
      currentMs = Math.min(e.pos_ms, totalMs);
      playing = e.playing;
    });
    return () => {
      un.then((f) => f());
    };
  });

  /** 托盘「停止播放」拆掉了本组件在用的核:不复位的话按钮停在「播放中」、进度僵住,
      再点播放会打到空内核报「尚未装载音轨」。复位后自增 reloadKey 重装一次,回到
      "进页面刚装好、停在 0"的状态,随时可以再播。

      两种"在用"都要认(Codex P2):
      ① 代次相同 = 停的正是本实例已装好的核;
      ② **本实例正在装**(loading):托盘停播是无条件的,后端进门就推进代次,在途装载
         必在发布段被判过期——而它的代次前端此刻还拿不到(cleanup 已把 lastBackendGen
         清成 null),只能按"正在装"认领。此时还要同步推进本地 loadGen,让那次在途装载
         按既有的「已取代」语义静默失败:否则它会以装载错误弹在页面上,而播放器一直
         停在未装载,得等切轨或重挂载才恢复。
      两条都不沾则事不关己(停的是别的实例装的核),什么都不做。 */
  $effect(() => {
    const un = onPlayerStopped((e) => {
      const mine = lastBackendGen !== null && e.gen === lastBackendGen;
      if (!mine && !loading) return;
      playing = false;
      currentMs = 0;
      // 先清代次再重装:装载 effect 的 cleanup 会拿它发条件停止,核都没了不必再停一次。
      lastBackendGen = null;
      if (loading) loadGen++;
      reloadKey++;
    });
    return () => {
      un.then((f) => f());
    };
  });

  /** 试听独奏中的音轨(null=没在独奏)。与 muted 分开存:muted 是用户的意图,
      独奏只是试听期间的临时压制,结束要原样还原,不能把用户的开关改掉。 */
  let soloSource = $state<string | null>(null);
  /** 送给内核的最终静音态:独奏期间只认独奏,否则认用户开关。 */
  const effMuted = (source: string) => (soloSource ? source !== soloSource : !!muted[source]);
  function pushMute(source: string) {
    void invoke("player_set_muted", { source, muted: effMuted(source) }).catch(() => {});
  }

  /** 试听独奏:只放 source 这一条轨(传 null 取消)。
      播放器是多轨混音的——试听某人一段 mic 音频时,同一时刻 system 轨里远端的人
      在说话会一起响,听感就是「试听的样本不是同一个人」(2026-08-20 实测:一篇真实
      笔记里 4~5/5 的试听片段都存在跨轨重叠)。 */
  export function soloTrack(source: string | null) {
    // 成品单轨(mixed)等装载:请求独奏的源不在轨道列表时降级为不独奏——否则唯一的
    // 轨道被整条静音,试听全程无声(2026-08-22 用户实测)。单轨本就无跨轨串音可隔。
    if (source && !tracks.some((t) => t.source === source)) source = null;
    if (soloSource === source) return;
    soloSource = source;
    for (const tr of tracks) pushMute(tr.source);
  }

  function toggleMute(source: string) {
    // 用户手动动开关 = 显式意图,优先于试听独奏:先解除独奏再按新意图下发。
    if (soloSource) {
      soloSource = null;
      muted = { ...muted, [source]: !muted[source] };
      for (const tr of tracks) pushMute(tr.source);
      return;
    }
    muted = { ...muted, [source]: !muted[source] };
    pushMute(source);
  }

  // ── 音轨菜单(收纳每轨静音开关):双轨会议才有,主控制行只留一个「音轨」按钮 ──
  let menuOpen = $state(false);
  let menuEl = $state<HTMLElement | null>(null);
  /** 任一轨被静音:按钮点亮 + 换静音图标,收起状态也能看出「动过」。 */
  const anyMuted = $derived(tracks.some((t) => muted[t.source]));
  // 点面板外或按 Esc 关闭(仅开启时挂监听)。capture 阶段:开关按钮本身在 menuEl 内不误关。
  $effect(() => {
    if (!menuOpen) return;
    const onDown = (e: PointerEvent) => {
      if (menuEl && !menuEl.contains(e.target as Node)) menuOpen = false;
    };
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") menuOpen = false;
    };
    document.addEventListener("pointerdown", onDown, true);
    document.addEventListener("keydown", onKey);
    return () => {
      document.removeEventListener("pointerdown", onDown, true);
      document.removeEventListener("keydown", onKey);
    };
  });
  export function play() {
    if (!loadPromise) return;
    playing = true; // 乐观置位:事件到达前按钮即时反馈;失败在 catch 复位
    loadPromise
      .then(() => invoke("player_play"))
      .then(() => {
        // 会话由**真正开始播放**建立(spec:装载不算)。缺身份则不建会话。
        if (noteId && lastBackendGen !== null) {
          playback.begin({ gen: lastBackendGen, noteId, title: title ?? "", totalMs });
        }
      })
      .catch((e) => {
        playing = false;
        if (e !== SUPERSEDED) reportError(t("notes.player.errPlay"), `${e}`);
      });
  }

  export function pause() {
    playing = false;
    // 失败不能再静默吞:player_pause 若被拒,Rust 继续放而 UI 已翻停,下一拍
    // player_pos 事件又把 playing 拽回 true——正是「试听不停」一类悬案的候选真凶。
    void invoke("player_pause").catch((e) => console.warn("player_pause failed:", e));
  }

  export function seek(ms: number) {
    const target = Math.max(0, Math.min(ms, totalMs));
    currentMs = target; // 乐观更新:拖拽跟手,事件到达后以 Rust 为准
    if (!loadPromise) return;
    void loadPromise.then(() => invoke("player_seek", { ms: Math.round(target) })).catch(() => {});
  }

  /** 时间轴总长(页面用于判断某段是否落在音频覆盖范围内)。 */
  export function durationMs(): number {
    return totalMs;
  }

  function toggle() {
    if (playing) {
      pause();
      onUserPause?.();
    } else play();
  }

  const pct = $derived(totalMs > 0 ? (Math.min(currentMs, totalMs) / totalMs) * 100 : 0);

  // ── 波形音轨即进度条:点击/拖拽定位,方向键微调 ──
  /** 无波形数据(旧笔记全段无 rms 也会有零值数组;真空数组=无段落)退化为平轨。 */
  const srcBars = $derived(waveform.length > 0 ? waveform : new Array(90).fill(0));
  /** 容器实测宽度(bind:clientWidth),0=未挂载。 */
  let waveWidth = $state(0);
  /** 条数按容器宽度自适应(每条约 3px 含 gap),窄窗按 max 降采样——固定 260 条
      每条 min-width 1px + 1px gap 必然溢出容器,垫到右侧按钮底下(冒烟实锤)。 */
  const bars = $derived.by(() => {
    const n = Math.max(30, Math.min(srcBars.length, Math.floor(waveWidth / 3) || srcBars.length));
    if (n >= srcBars.length) return srcBars;
    const out: number[] = new Array(n).fill(0);
    for (let i = 0; i < srcBars.length; i++) {
      const b = Math.min(n - 1, Math.floor((i * n) / srcBars.length));
      if (srcBars[i] > out[b]) out[b] = srcBars[i];
    }
    return out;
  });
  const playedBars = $derived(Math.round((bars.length * pct) / 100));

  let waveEl = $state<HTMLElement | null>(null);
  let scrubbing = false;
  function waveSeek(e: PointerEvent) {
    if (!waveEl || totalMs <= 0) return;
    const r = waveEl.getBoundingClientRect();
    const ratio = Math.max(0, Math.min(1, (e.clientX - r.left) / r.width));
    seek(ratio * totalMs);
  }
  function onWaveDown(e: PointerEvent) {
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    scrubbing = true;
    waveSeek(e);
  }
  function onWaveMove(e: PointerEvent) {
    if (scrubbing) waveSeek(e);
  }
  function onWaveUp() {
    scrubbing = false;
  }
  function onWaveKey(e: KeyboardEvent) {
    const STEP = 5000;
    if (e.key === "ArrowLeft") seek(currentMs - STEP);
    else if (e.key === "ArrowRight") seek(currentMs + STEP);
    else if (e.key === "Home") seek(0);
    else if (e.key === "End") seek(totalMs);
    else return;
    e.preventDefault();
  }

  // ── 圈选游标(开始/结束):拖动圈定导出范围,拖动中回调宿主做逐字稿联动 ──
  /** 两游标最小间距:防拖成零长/交叉(导出空音频无意义)。 */
  const MIN_RANGE_MS = 1000;
  /** 游标生效位置:null 视作 0/全长。totalMs 装载后才非 0,钳位随之收敛。 */
  const effStart = $derived(Math.max(0, Math.min(rangeStartMs ?? 0, totalMs)));
  const effEnd = $derived(rangeEndMs == null ? totalMs : Math.max(0, Math.min(rangeEndMs, totalMs)));
  /** 圈定了真子范围(有游标离开端点):此时波形圈外条淡显。 */
  const rangeActive = $derived(totalMs > 0 && (effStart > 0 || effEnd < totalMs));
  let draggingHandle = $state<"start" | "end" | null>(null);
  function pointerMs(e: PointerEvent): number {
    if (!waveEl || totalMs <= 0) return 0;
    const r = waveEl.getBoundingClientRect();
    return Math.max(0, Math.min(1, (e.clientX - r.left) / r.width)) * totalMs;
  }
  function moveHandle(which: "start" | "end", ms: number) {
    if (totalMs <= 0) return;
    if (which === "start") {
      const v = Math.round(Math.max(0, Math.min(ms, effEnd - MIN_RANGE_MS)));
      rangeStartMs = v;
      onRangeDrag?.("start", v);
    } else {
      const v = Math.round(Math.min(totalMs, Math.max(ms, effStart + MIN_RANGE_MS)));
      rangeEndMs = v;
      onRangeDrag?.("end", v);
    }
  }
  function onHandleDown(which: "start" | "end", e: PointerEvent) {
    if (totalMs <= 0) return;
    e.stopPropagation(); // 不让底下的波形把这次按下当 seek
    (e.currentTarget as HTMLElement).setPointerCapture(e.pointerId);
    draggingHandle = which;
    moveHandle(which, pointerMs(e));
  }
  function onHandleMove(e: PointerEvent) {
    if (draggingHandle) moveHandle(draggingHandle, pointerMs(e));
  }
  function onHandleUp() {
    if (!draggingHandle) return;
    draggingHandle = null;
    onRangeDragEnd?.();
  }
  function onHandleKey(which: "start" | "end", e: KeyboardEvent) {
    const STEP = 1000;
    const cur = which === "start" ? effStart : effEnd;
    if (e.key === "ArrowLeft") moveHandle(which, cur - STEP);
    else if (e.key === "ArrowRight") moveHandle(which, cur + STEP);
    else return;
    e.preventDefault();
    e.stopPropagation();
    onRangeDragEnd?.();
  }
  /** 条 i 的中心时刻是否落在圈外(rangeActive 时淡显圈外)。 */
  function barOutside(i: number): boolean {
    if (!rangeActive || bars.length === 0) return false;
    const mid = ((i + 0.5) / bars.length) * totalMs;
    return mid < effStart || mid > effEnd;
  }

  // ── 剪辑表 → 内核:删减/恢复即时生效(装载路径自读盘上 edits.json,这里只管
  //    运行时变更;排在装载 promise 之后,打不到旧核)。 ──
  $effect(() => {
    const list = cuts.map((c) => [c.start_ms, c.end_ms]);
    const p = loadPromise;
    if (!p) return;
    void p.then(() => invoke("player_set_cuts", { cutsMs: list })).catch(() => {});
  });
</script>

<div class="player">
  <!-- 图标遵循 DESIGN.md:16px 线性/实心 SVG(currentColor),禁用 Unicode 符号字符 -->
  <button class="play-btn" onclick={toggle} title={playing ? t("notes.player.pause") : t("notes.player.play")}>
    {#if playing}
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
        <rect x="3" y="2.5" width="3.4" height="11" rx="1" fill="currentColor" />
        <rect x="9.6" y="2.5" width="3.4" height="11" rx="1" fill="currentColor" />
      </svg>
    {:else}
      <svg width="14" height="14" viewBox="0 0 16 16" aria-hidden="true">
        <path d="M4.5 2.8v10.4c0 .8.9 1.3 1.6.9l8-5.2c.6-.4.6-1.4 0-1.8l-8-5.2c-.7-.4-1.6.1-1.6.9z" fill="currentColor" />
      </svg>
    {/if}
  </button>
  <span class="time">{formatTs(Math.min(currentMs, totalMs))}</span>
  <!-- 波形音轨(即进度条):条高来自段落 rms,已播部分 accent;点击/拖拽定位 -->
  <div
    class="wave"
    bind:this={waveEl}
    bind:clientWidth={waveWidth}
    role="slider"
    tabindex="0"
    aria-label={t("notes.player.progress")}
    aria-valuemin={0}
    aria-valuemax={totalMs}
    aria-valuenow={Math.min(currentMs, totalMs)}
    aria-valuetext={formatTs(Math.min(currentMs, totalMs))}
    onpointerdown={onWaveDown}
    onpointermove={onWaveMove}
    onpointerup={onWaveUp}
    onpointercancel={onWaveUp}
    onkeydown={onWaveKey}
  >
    {#each bars as h, i (i)}
      <span class="bar" class:played={i < playedBars} class:outside={barOutside(i)} style="height: {6 + h * 94}%"></span>
    {/each}
    <!-- 删减区间红显(非破坏性剪辑):被删段盖半透明 danger 罩,一眼看出播放会跳过哪里 -->
    {#if totalMs > 0}
      {#each cuts as c (c.start_ms)}
        <div
          class="cut-span"
          style="left: {(Math.min(c.start_ms, totalMs) / totalMs) * 100}%; width: {(Math.max(0, Math.min(c.end_ms, totalMs) - Math.min(c.start_ms, totalMs)) / totalMs) * 100}%"
          title={t("notes.player.cutSpan")}
        ></div>
      {/each}
    {/if}
    <!-- 圈选游标:开始/结束各一枚,拖动圈定导出范围。旗标朝内(开始在上、结束在下)
         区分两枚;拖动中或已离开端点时点亮 accent。 -->
    {#if totalMs > 0}
      <div
        class="cursor start"
        class:active={rangeActive || draggingHandle === "start"}
        style="left: {(effStart / totalMs) * 100}%"
        role="slider"
        tabindex="0"
        aria-label={t("notes.player.rangeStart")}
        aria-valuemin={0}
        aria-valuemax={totalMs}
        aria-valuenow={effStart}
        aria-valuetext={formatTs(effStart)}
        title={`${t("notes.player.rangeStart")} ${formatTs(effStart)}`}
        onpointerdown={(e) => onHandleDown("start", e)}
        onpointermove={onHandleMove}
        onpointerup={onHandleUp}
        onpointercancel={onHandleUp}
        onkeydown={(e) => onHandleKey("start", e)}
      ></div>
      <div
        class="cursor end"
        class:active={rangeActive || draggingHandle === "end"}
        style="left: {(effEnd / totalMs) * 100}%"
        role="slider"
        tabindex="0"
        aria-label={t("notes.player.rangeEnd")}
        aria-valuemin={0}
        aria-valuemax={totalMs}
        aria-valuenow={effEnd}
        aria-valuetext={formatTs(effEnd)}
        title={`${t("notes.player.rangeEnd")} ${formatTs(effEnd)}`}
        onpointerdown={(e) => onHandleDown("end", e)}
        onpointermove={onHandleMove}
        onpointerup={onHandleUp}
        onpointercancel={onHandleUp}
        onkeydown={(e) => onHandleKey("end", e)}
      ></div>
    {/if}
  </div>
  <span class="time">{formatTs(totalMs)}</span>
  {#if tracks.length > 1}
    <!-- 音轨菜单:双轨会议才有。回放有回音(外放+蓝牙延迟致 AEC 失效,同句两轨各一份)时
         点开静掉一轨即无回音。收进菜单——主控制行保持干净,用途一句话只在点开时出现;
         有轨被静音时按钮点亮 accent、喇叭换静音图标,收起也看得出动过。 -->
    <div class="track-menu" bind:this={menuEl}>
      <button
        class="track-btn"
        class:has-touched={anyMuted}
        onclick={() => (menuOpen = !menuOpen)}
        aria-expanded={menuOpen}
        title={anyMuted ? t("notes.player.tracksMutedTitle") : t("notes.player.tracksTitle")}
      >
        {#if anyMuted}
          <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M8.4 3.2 5 6H2.7v4H5l3.4 2.8z" />
            <path d="M11.3 6.4l3.2 3.2M14.5 6.4l-3.2 3.2" />
          </svg>
        {:else}
          <svg width="15" height="15" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
            <path d="M8.4 3.2 5 6H2.7v4H5l3.4 2.8z" />
            <path d="M11.1 6.2a2.8 2.8 0 0 1 0 3.6" />
            <path d="M12.9 4.6a5.3 5.3 0 0 1 0 6.8" />
          </svg>
        {/if}
        {t("notes.player.tracks")}
        <svg class="chev" class:open={menuOpen} width="10" height="10" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M4 6l4 4 4-4" />
        </svg>
      </button>
      {#if menuOpen}
        <div class="track-pop" role="menu">
          <p class="track-pop-hint">{t("notes.player.echoHint")}</p>
          {#each tracks as track (track.source)}
            <label class="track-row">
              <input type="checkbox" checked={!muted[track.source]} onchange={() => toggleMute(track.source)} />
              <span class="track-row-name">{track.source === "mic" ? t("notes.player.mic") : t("notes.player.system")}</span>
              {#if muted[track.source]}<span class="track-row-tag">{t("notes.player.muted")}</span>{/if}
            </label>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</div>
{#if loading}
  <div class="loading-hint">{t("notes.player.loading")}</div>
{/if}
{#if trackErrors.length > 0}
  <div class="track-errors">
    {#each trackErrors as e (e)}
      <div>{e}</div>
    {/each}
  </div>
{/if}

<style>
  /* 播放器容器:surface 卡片,与 transcript 容器同语言 */
  .player {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.9rem;
    /* 间距由页面的 transport 行统一控制,组件自身不带外边距 */
    margin: 0;
  }
  /* button-secondary 形态的圆形播放键 */
  .play-btn {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink);
    border-radius: 50%;
    width: 2.1rem;
    height: 2.1rem;
    font-size: 0.85rem;
    cursor: pointer;
    flex: none;
    display: inline-flex;
    align-items: center;
    justify-content: center;
  }
  .play-btn:hover {
    background: var(--surface-soft);
  }
  /* 音轨菜单:主控制行只放一个「音轨」胶囊按钮(图标+文字+chevron),静音开关收进弹出面板 */
  .track-menu {
    position: relative;
    flex: none;
  }
  .track-btn {
    display: inline-flex;
    align-items: center;
    gap: 0.3em;
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.15em 0.6em;
    font-size: 0.75rem;
    cursor: pointer;
    white-space: nowrap;
  }
  .track-btn:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  /* 改过任一默认(静音/关响度):点亮 accent,收起状态也看得出动过 */
  .track-btn.has-touched {
    color: var(--accent);
    border-color: var(--accent);
  }
  .chev {
    transition: transform 120ms ease;
    opacity: 0.7;
  }
  .chev.open {
    transform: rotate(180deg);
  }
  /* 弹出面板:与改说话人菜单同语言(surface-press 底、hairline 边、rounded-lg、shadow-popover)。
     播放器贴近视口顶,故向上弹(bottom:100%),右对齐避免溢出右缘。 */
  .track-pop {
    position: absolute;
    bottom: calc(100% + 6px);
    right: 0;
    z-index: 20;
    min-width: 11rem;
    background: var(--surface-press);
    border: 1px solid var(--hairline);
    border-radius: var(--radius-lg);
    box-shadow: var(--shadow-popover);
    padding: 0.5rem;
  }
  .track-pop-hint {
    margin: 0 0 0.4rem;
    padding: 0 0.15rem;
    color: var(--ink-secondary);
    font-size: 0.75rem;
  }
  .track-row {
    display: flex;
    align-items: center;
    gap: 0.5em;
    padding: 0.3em 0.15rem;
    font-size: 0.85rem;
    color: var(--ink);
    cursor: pointer;
  }
  .track-row input {
    accent-color: var(--accent);
    cursor: pointer;
  }
  .track-row-name {
    flex: 1;
  }
  .track-row-tag {
    color: var(--ink-faint);
    font-size: 0.75rem;
  }
  .time {
    color: var(--ink-secondary);
    font-size: 0.8rem;
    font-variant-numeric: tabular-nums;
    flex: none;
  }
  /* 波形音轨:未播条 hairline-strong、已播条 accent(进度条色语言不变,形态升级);
     等分条宽靠 flex:1+gap,条高内联(rms 归一)。touch-action:none 保拖拽定位不被
     滚动手势抢走。focus 用 accent 外环(与 editable-text 同语言)。 */
  .wave {
    position: relative; /* 圈选游标的定位容器 */
    flex: 1;
    min-width: 0;
    height: 34px;
    display: flex;
    align-items: center;
    gap: 1px;
    cursor: pointer;
    touch-action: none;
    border-radius: var(--radius-sm);
  }
  .wave:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 2px;
  }
  .bar {
    flex: 1;
    min-width: 1px;
    min-height: 3px;
    border-radius: var(--radius-full);
    background: var(--hairline-strong);
  }
  .bar.played {
    background: var(--accent);
  }
  /* 圈外条淡显:圈定了真子范围时,一眼看出哪段会被导出 */
  .bar.outside {
    opacity: 0.3;
  }
  /* 删减区红罩:danger 半透明覆盖,不吃指针(点击穿透到波形定位) */
  .cut-span {
    position: absolute;
    top: 2px;
    bottom: 2px;
    background: var(--danger);
    opacity: 0.16;
    border-radius: var(--radius-sm);
    pointer-events: none;
    z-index: 1;
  }
  /* 圈选游标:2px 竖线 + 朝内小旗标(开始在上、结束在下)。热区 14px 宽居中,
     好抓;线与旗默认次级墨,拖动中/已圈定点亮 accent。 */
  .cursor {
    position: absolute;
    top: -3px;
    bottom: -3px;
    width: 14px;
    margin-left: -7px;
    cursor: ew-resize;
    touch-action: none;
    z-index: 2;
  }
  .cursor::before {
    content: "";
    position: absolute;
    left: 6px;
    top: 0;
    bottom: 0;
    width: 2px;
    border-radius: var(--radius-full);
    background: var(--ink-faint);
  }
  .cursor::after {
    content: "";
    position: absolute;
    left: 6px;
    width: 7px;
    height: 7px;
    background: var(--ink-faint);
  }
  .cursor.start::after {
    top: 0;
    border-radius: 0 var(--radius-sm) var(--radius-sm) 0;
  }
  .cursor.end::after {
    bottom: 0;
    left: auto;
    right: 6px;
    border-radius: var(--radius-sm) 0 0 var(--radius-sm);
  }
  .cursor.active::before,
  .cursor.active::after {
    background: var(--accent);
  }
  .cursor:focus-visible {
    outline: 2px solid var(--accent);
    outline-offset: 1px;
    border-radius: var(--radius-sm);
  }
  /* 音轨错误可视化:danger 色小字,贴在播放器下方 */
  .track-errors {
    color: var(--danger);
    font-size: 0.8rem;
    margin: 0.3rem 0 0 0.2rem;
  }
  /* 装载进行中提示:次级墨小字,同错误行位置(装载完成即消失) */
  .loading-hint {
    color: var(--ink-secondary);
    font-size: 0.8rem;
    margin: 0.3rem 0 0 0.2rem;
  }
</style>
