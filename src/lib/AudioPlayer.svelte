<script lang="ts">
  import { invoke } from "@tauri-apps/api/core";
  import { onPlayerPos } from "$lib/events";
  import { formatTs, type TrackInfo } from "$lib/notes";

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
  }: {
    tracks: TrackInfo[];
    /** 音轨波形(0..1 归一条高,按时间等分;由页面从段落 rms 聚合)。空数组退化为平轨。 */
    waveform?: number[];
    currentMs?: number;
    playing?: boolean;
  } = $props();

  const totalMs = $derived(tracks.reduce((m, t) => Math.max(m, t.offset_ms + t.duration_ms), 0));

  /** 装载失败/播放失败的可视化(排障关键:错误必须浮出水面,不许静默)。 */
  let trackErrors = $state<string[]>([]);
  function reportError(source: string, detail: string) {
    const msg = `${source}: ${detail}`;
    if (!trackErrors.includes(msg)) trackErrors = [...trackErrors, msg];
  }

  // 装载:tracks 变化(进页/续录/transcode_done 重拉)即重载原生播放器。
  // m4a 首次播放需解码到缓存(秒级、命令内完成),play() await 此 promise 即拿到就绪;
  // 缓存跨会话复用,二次装载瞬时。卸载/切笔记 → player_stop 停流放资源。
  let loadPromise: Promise<number> | null = null;
  $effect(() => {
    trackErrors = [];
    loadPromise =
      tracks.length === 0
        ? null
        : invoke<number>("player_load", {
            tracks: tracks.map((t) => ({ path: t.path, offset_ms: t.offset_ms, source: t.source })),
          }).catch((e) => {
            reportError("音轨装载", `${e}`);
            throw e;
          });
    return () => {
      loadPromise = null;
      void invoke("player_stop").catch(() => {});
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

  // 每轨静音(源名 → 静音):Rust 混音时跳过该轨,双轨同步与时钟零影响。
  // 用途:双轨串音的笔记(外放+蓝牙延迟致 AEC 失效)静掉一轨即无回音。
  let muted = $state<Record<string, boolean>>({});
  function toggleMute(source: string) {
    muted = { ...muted, [source]: !muted[source] };
    void invoke("player_set_muted", { source, muted: !!muted[source] }).catch(() => {});
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
      .catch((e) => {
        playing = false;
        reportError("播放", `${e}`);
      });
  }

  export function pause() {
    playing = false;
    void invoke("player_pause").catch(() => {});
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
    if (playing) pause();
    else play();
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
</script>

<div class="player">
  <!-- 图标遵循 DESIGN.md:16px 线性/实心 SVG(currentColor),禁用 Unicode 符号字符 -->
  <button class="play-btn" onclick={toggle} title={playing ? "暂停" : "播放"}>
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
    aria-label="播放进度"
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
      <span class="bar" class:played={i < playedBars} style="height: {6 + h * 94}%"></span>
    {/each}
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
        title={anyMuted ? "音轨(有音轨已静音)" : "音轨(回放有回音时可静掉一轨)"}
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
        音轨
        <svg class="chev" class:open={menuOpen} width="10" height="10" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M4 6l4 4 4-4" />
        </svg>
      </button>
      {#if menuOpen}
        <div class="track-pop" role="menu">
          <p class="track-pop-hint">回放有回音?静掉一轨</p>
          {#each tracks as t (t.source)}
            <label class="track-row">
              <input type="checkbox" checked={!muted[t.source]} onchange={() => toggleMute(t.source)} />
              <span class="track-row-name">{t.source === "mic" ? "麦克风" : "系统声"}</span>
              {#if muted[t.source]}<span class="track-row-tag">已静音</span>{/if}
            </label>
          {/each}
        </div>
      {/if}
    </div>
  {/if}
</div>
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
  /* 音轨错误可视化:danger 色小字,贴在播放器下方 */
  .track-errors {
    color: var(--danger);
    font-size: 0.8rem;
    margin: 0.3rem 0 0 0.2rem;
  }
</style>
