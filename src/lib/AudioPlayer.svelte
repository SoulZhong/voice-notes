<script lang="ts">
  import { convertFileSrc } from "@tauri-apps/api/core";
  import { formatTs, type TrackInfo } from "$lib/notes";
  import { computeNoteGain } from "$lib/gain";

  /* 多轨播放器:每轨一个隐藏 <audio>(asset 协议流式,内存恒定),自有时钟驱动
     UI 与文字跟随。有轨道覆盖当前时刻时以该轨 currentTime 为真时钟(音频即时钟),
     轨道间隙(offset 之前/短轨结束后)由墙钟推进。各轨每帧向时钟收敛:期望位置
     在界内则确保播放、偏差 >0.3s 回拉;界外则暂停。 */
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

  let els = $state<(HTMLAudioElement | null)[]>([]);
  const totalMs = $derived(tracks.reduce((m, t) => Math.max(m, t.offset_ms + t.duration_ms), 0));

  // ── 回放响度归一化(A1):el.volume 封顶 1.0 无法放大老笔记,经共享 GainNode 提升 ──
  const NORMALIZE_KEY = "vn.playbackNormalize";
  // 默认开(「自动」语义):localStorage 存 "0" 才关。SSR/无 localStorage 时按开。
  let normalize = $state(
    typeof localStorage !== "undefined" ? localStorage.getItem(NORMALIZE_KEY) !== "0" : true,
  );
  const noteGain = $derived(computeNoteGain(tracks));

  let audioCtx: AudioContext | null = null;
  let gainNode: GainNode | null = null;
  // 每个 <audio> 只能建一次 MediaElementSource;按元素缓存,tracks 变化时增量重连。
  const srcNodes = new Map<HTMLAudioElement, MediaElementAudioSourceNode>();

  function ensureGraph() {
    if (typeof AudioContext === "undefined") return;
    if (!audioCtx) {
      audioCtx = new AudioContext();
      gainNode = audioCtx.createGain();
      gainNode.connect(audioCtx.destination);
    }
    for (const el of els) {
      if (el && !srcNodes.has(el)) {
        const node = audioCtx.createMediaElementSource(el);
        node.connect(gainNode!);
        srcNodes.set(el, node);
      }
    }
  }

  function applyGain() {
    if (!audioCtx || !gainNode) return;
    const g = normalize ? noteGain : 1;
    // 平滑过渡防咔哒(~20ms 时间常数)。
    gainNode.gain.setTargetAtTime(g, audioCtx.currentTime, 0.02);
  }

  export function setNormalize(on: boolean) {
    normalize = on;
    if (typeof localStorage !== "undefined") localStorage.setItem(NORMALIZE_KEY, on ? "1" : "0");
    applyGain();
  }

  /** 轨道加载/播放失败的可视化(排障关键:加载失败时走表逻辑仍会推进度条,
      看起来在播实际无声——错误必须浮出水面,不许静默)。 */
  let trackErrors = $state<string[]>([]);
  function reportError(source: string, detail: string) {
    const msg = `${source} 音轨: ${detail}`;
    if (!trackErrors.includes(msg)) trackErrors = [...trackErrors, msg];
  }
  function onAudioError(i: number) {
    const el = els[i];
    const media = el?.error;
    const code =
      media?.code === 1 ? "加载被中止" :
      media?.code === 2 ? "网络/协议错误(资源读取失败)" :
      media?.code === 3 ? "解码失败(文件损坏或编码不支持)" :
      media?.code === 4 ? "资源不可用(路径被拒或文件不存在)" : `错误码 ${media?.code}`;
    reportError(tracks[i]?.source ?? `#${i}`, code);
  }

  // 驱动循环用 setInterval 而非 requestAnimationFrame:窗口被遮挡/最小化时
  // WebKit 停发 rAF,同步循环停摆(后台播放停止的根因之一,2026-07-08 实锤;
  // 另一半根因是页面级节流,由窗口配置 backgroundThrottling=disabled 关掉)。
  // interval 即使在未关节流的环境也只会被钳到 1s,同步照常存活。
  let timer: ReturnType<typeof setInterval> | 0 = 0;
  const TICK_MS = 100;
  // 连续播放位置(非响应式):驱动音频同步;currentMs 只按 100ms 粒度更新——
  // 高亮/进度条用不到更细,也避免高频触发全段落列表的派生重算。
  let pos = 0;

  // 每轨静音(源名 → 静音)。用 el.muted 而非暂停:静音轨仍在走表,可继续充当
  // audibleClock,双轨同步语义零改动。用途:双轨串音的笔记(外放+蓝牙延迟致
  // AEC 失效,同一句话两轨相隔数百毫秒各有一份)静掉一轨即无回音。
  let muted = $state<Record<string, boolean>>({});
  function toggleMute(source: string) {
    muted = { ...muted, [source]: !muted[source] };
    for (let i = 0; i < tracks.length; i++) {
      const el = els[i];
      if (el) el.muted = !!muted[tracks[i].source];
    }
  }

  // ── 音轨菜单(收纳每轨静音开关):双轨会议才有,主控制行只留一个「音轨」按钮 ──
  let menuOpen = $state(false);
  let menuEl = $state<HTMLElement | null>(null);
  /** 任一轨被静音:换静音图标。 */
  const anyMuted = $derived(tracks.some((t) => muted[t.source]));
  /** 改过任一默认(静音某轨 / 关掉响度归一化):按钮点亮,收起状态也能看出「动过」。 */
  const audioTouched = $derived(anyMuted || (noteGain > 1 && !normalize));
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
  // 墙钟锚点:无轨道可依时,pos = anchorMs + (now - anchorWall)。
  let anchorWall = 0;
  let anchorMs = 0;

  const DRIFT_MS = 300;
  const UI_STEP_MS = 100;

  function publishPos() {
    const q = Math.min(Math.floor(pos / UI_STEP_MS) * UI_STEP_MS, totalMs);
    if (q !== currentMs) currentMs = q;
  }

  function audibleClock(): number | null {
    for (let i = 0; i < tracks.length; i++) {
      const el = els[i];
      // 在播且未 seek 的轨道即真时钟(syncTracks 保证界外轨道已暂停)。
      if (!el || el.paused || el.seeking) continue;
      return el.currentTime * 1000 + tracks[i].offset_ms;
    }
    return null;
  }

  function syncTracks() {
    for (let i = 0; i < tracks.length; i++) {
      const el = els[i];
      if (!el) continue;
      const expected = pos - tracks[i].offset_ms;
      el.muted = !!muted[tracks[i].source];
      if (expected >= 0 && expected < tracks[i].duration_ms) {
        if (el.paused) {
          el.currentTime = expected / 1000;
          void el.play().catch((e) => {
            // 播放被拒也要浮出水面(自动播放策略/资源失效),不再静默吞掉。
            reportError(tracks[i]?.source ?? `#${i}`, `播放被拒: ${e?.name ?? e}`);
          });
        } else if (Math.abs(el.currentTime * 1000 - expected) > DRIFT_MS) {
          el.currentTime = expected / 1000;
        }
      } else if (!el.paused) {
        el.pause();
      }
    }
  }

  function tick() {
    // 后台/遮挡时 WKWebView 可能挂起 AudioContext;播放中每帧检查并恢复,守住后台播放
    // (本项目此前为后台播放做过两次根因修复,勿让经 AudioContext 路由后重新回归)。
    if (audioCtx?.state === "suspended") void audioCtx.resume();
    const clock = audibleClock();
    if (clock != null) {
      pos = clock;
      anchorMs = clock;
      anchorWall = performance.now();
    } else {
      pos = anchorMs + (performance.now() - anchorWall);
    }
    if (pos >= totalMs) {
      pos = totalMs;
      publishPos();
      pause();
      return;
    }
    publishPos();
    syncTracks();
  }

  export function play() {
    if (tracks.length === 0) return;
    ensureGraph();
    if (audioCtx?.state === "suspended") void audioCtx.resume();
    applyGain();
    if (pos >= totalMs) pos = 0; // 播完再按:从头来
    playing = true;
    anchorMs = pos;
    anchorWall = performance.now();
    publishPos();
    syncTracks();
    if (timer) clearInterval(timer);
    timer = setInterval(tick, TICK_MS);
  }

  export function pause() {
    playing = false;
    if (timer) clearInterval(timer);
    timer = 0;
    for (const el of els) el?.pause();
  }

  export function seek(ms: number) {
    pos = Math.max(0, Math.min(ms, totalMs));
    anchorMs = pos;
    anchorWall = performance.now();
    publishPos();
    if (playing) syncTracks();
  }

  /** 时间轴总长(页面用于判断某段是否落在音频覆盖范围内)。 */
  export function durationMs(): number {
    return totalMs;
  }

  function toggle() {
    if (playing) pause();
    else play();
  }

  // 组件卸载/笔记切换:停播,不留幽灵声音。不在此拆图——<audio> 跨笔记复用,
  // MediaElementSource 每元素一生只能建一次,拆了再建会 InvalidStateError。
  $effect(() => {
    void tracks;
    return () => pause();
  });

  // 仅组件真正卸载时关掉 AudioContext(effect 体不读任何响应式值→cleanup 只在销毁时跑)。
  $effect(() => {
    return () => {
      void audioCtx?.close();
      audioCtx = null;
      gainNode = null;
      srcNodes.clear();
    };
  });

  // tracks 变化(续录/transcode_done 重拉音轨)后:已建图则增量接新 <audio> 并重算增益。
  $effect(() => {
    void noteGain; // 追踪 tracks → noteGain
    if (audioCtx) {
      ensureGraph();
      applyGain();
    }
  });

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
  {#if tracks.length > 1 || noteGain > 1}
    <!-- 音频菜单:把回放相关的低频设置(双轨静音、响度归一化)收进一个「音频」按钮,
         主控制行保持干净,每项用途一句话只在点开时出现,解决「不知道能点/干嘛用」。
         静音=双轨会议才有(回音笔记静掉一轨即无回音);响度=本条真能被放大时才有。
         有轨被静音或关了响度=改过默认,按钮点亮 accent,收起也看得出动过。 -->
    <div class="track-menu" bind:this={menuEl}>
      <button
        class="track-btn"
        class:has-touched={audioTouched}
        onclick={() => (menuOpen = !menuOpen)}
        aria-expanded={menuOpen}
        title="音频设置(静音音轨 / 响度归一化)"
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
        音频
        <svg class="chev" class:open={menuOpen} width="10" height="10" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
          <path d="M4 6l4 4 4-4" />
        </svg>
      </button>
      {#if menuOpen}
        <div class="track-pop" role="menu">
          {#if tracks.length > 1}
            <p class="track-pop-hint">回放有回音?静掉一轨</p>
            {#each tracks as t (t.source)}
              <label class="track-row">
                <input type="checkbox" checked={!muted[t.source]} onchange={() => toggleMute(t.source)} />
                <span class="track-row-name">{t.source === "mic" ? "麦克风" : "系统声"}</span>
                {#if muted[t.source]}<span class="track-row-tag">已静音</span>{/if}
              </label>
            {/each}
          {/if}
          {#if noteGain > 1}
            {#if tracks.length > 1}<div class="track-pop-sep"></div>{/if}
            <label class="track-row">
              <input type="checkbox" checked={normalize} onchange={() => setNormalize(!normalize)} />
              <span class="track-row-name">响度归一化</span>
            </label>
            <p class="track-pop-sub">把偏轻的录音抬到正常响度</p>
          {/if}
        </div>
      {/if}
    </div>
  {/if}
  {#each tracks as t, i (t.source)}
    <audio bind:this={els[i]} src={convertFileSrc(t.path)} preload="auto" onerror={() => onAudioError(i)}></audio>
  {/each}
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
  /* 分组分隔线(静音组 / 响度组) */
  .track-pop-sep {
    height: 1px;
    background: var(--hairline);
    margin: 0.4rem 0;
  }
  /* 单项副说明(如响度归一化用途):贴在该项下方,次级墨色小字 */
  .track-pop-sub {
    margin: 0.1rem 0 0;
    padding: 0 0.15rem 0 1.7rem;
    color: var(--ink-faint);
    font-size: 0.72rem;
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
  audio {
    display: none;
  }
  /* 音轨错误可视化:danger 色小字,贴在播放器下方 */
  .track-errors {
    color: var(--danger);
    font-size: 0.8rem;
    margin: 0.3rem 0 0 0.2rem;
  }
</style>
