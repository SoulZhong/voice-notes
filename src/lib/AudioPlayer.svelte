<script lang="ts">
  import { convertFileSrc } from "@tauri-apps/api/core";
  import { formatTs, type TrackInfo } from "$lib/notes";

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

  // 组件卸载/笔记切换:停干净,不留幽灵声音。
  $effect(() => {
    void tracks;
    return () => pause();
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
  {#if tracks.length > 1}
    <!-- 轨道静音开关:双轨串音笔记(外放+蓝牙延迟致 AEC 失效)静掉一轨即无回音 -->
    <div class="track-toggles">
      {#each tracks as t (t.source)}
        <button
          class="track-toggle"
          class:off={muted[t.source]}
          onclick={() => toggleMute(t.source)}
          title={muted[t.source] ? "恢复播放该音轨" : "静音该音轨(回放有回音时静掉一轨)"}
        >{t.source === "mic" ? "麦克风" : "系统声"}</button>
      {/each}
    </div>
  {/if}
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
  /* 轨道静音开关:开=正常字色,关(静音)=划线退灰。文字按钮,与全应用「图标必带文字」一致 */
  .track-toggles {
    display: inline-flex;
    gap: 4px;
    flex: none;
  }
  .track-toggle {
    border: 1px solid var(--hairline-strong);
    background: transparent;
    color: var(--ink-secondary);
    border-radius: var(--radius-full);
    padding: 0.15em 0.7em;
    font-size: 0.75rem;
    cursor: pointer;
    white-space: nowrap;
  }
  .track-toggle:hover {
    background: var(--surface-soft);
    color: var(--ink);
  }
  .track-toggle.off {
    text-decoration: line-through;
    color: var(--ink-faint);
    border-style: dashed;
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
