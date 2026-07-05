<script lang="ts">
  import { convertFileSrc } from "@tauri-apps/api/core";
  import { formatTs, type TrackInfo } from "$lib/notes";

  /* 多轨播放器:每轨一个隐藏 <audio>(asset 协议流式,内存恒定),自有时钟驱动
     UI 与文字跟随。有轨道覆盖当前时刻时以该轨 currentTime 为真时钟(音频即时钟),
     轨道间隙(offset 之前/短轨结束后)由墙钟推进。各轨每帧向时钟收敛:期望位置
     在界内则确保播放、偏差 >0.3s 回拉;界外则暂停。 */
  let {
    tracks,
    currentMs = $bindable(0),
    playing = $bindable(false),
  }: { tracks: TrackInfo[]; currentMs?: number; playing?: boolean } = $props();

  let els = $state<(HTMLAudioElement | null)[]>([]);
  const totalMs = $derived(tracks.reduce((m, t) => Math.max(m, t.offset_ms + t.duration_ms), 0));

  let raf = 0;
  // 墙钟锚点:无轨道可依时,currentMs = anchorMs + (now - anchorWall)。
  let anchorWall = 0;
  let anchorMs = 0;

  const DRIFT_MS = 300;

  function audibleClock(): number | null {
    for (let i = 0; i < tracks.length; i++) {
      const el = els[i];
      if (!el || el.paused || el.seeking) continue;
      const pos = el.currentTime * 1000 + tracks[i].offset_ms;
      if (pos <= tracks[i].offset_ms + tracks[i].duration_ms) return pos;
    }
    return null;
  }

  function syncTracks() {
    for (let i = 0; i < tracks.length; i++) {
      const el = els[i];
      if (!el) continue;
      const expected = currentMs - tracks[i].offset_ms;
      if (expected >= 0 && expected < tracks[i].duration_ms) {
        if (el.paused) {
          el.currentTime = expected / 1000;
          void el.play().catch(() => {});
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
      currentMs = clock;
      anchorMs = clock;
      anchorWall = performance.now();
    } else {
      currentMs = anchorMs + (performance.now() - anchorWall);
    }
    if (currentMs >= totalMs) {
      pause();
      currentMs = totalMs;
      return;
    }
    syncTracks();
    raf = requestAnimationFrame(tick);
  }

  export function play() {
    if (tracks.length === 0) return;
    if (currentMs >= totalMs) currentMs = 0; // 播完再按:从头来
    playing = true;
    anchorMs = currentMs;
    anchorWall = performance.now();
    syncTracks();
    cancelAnimationFrame(raf);
    raf = requestAnimationFrame(tick);
  }

  export function pause() {
    playing = false;
    cancelAnimationFrame(raf);
    for (const el of els) el?.pause();
  }

  export function seek(ms: number) {
    currentMs = Math.max(0, Math.min(ms, totalMs));
    anchorMs = currentMs;
    anchorWall = performance.now();
    if (playing) syncTracks();
  }

  function toggle() {
    if (playing) pause();
    else play();
  }

  function onScrub(e: Event) {
    seek(Number((e.currentTarget as HTMLInputElement).value));
  }

  // 组件卸载/笔记切换:停干净,不留幽灵声音。
  $effect(() => {
    void tracks;
    return () => pause();
  });

  const pct = $derived(totalMs > 0 ? (Math.min(currentMs, totalMs) / totalMs) * 100 : 0);
</script>

<div class="player">
  <button class="play-btn" onclick={toggle} title={playing ? "暂停" : "播放"}>
    {playing ? "⏸" : "▶"}
  </button>
  <span class="time">{formatTs(Math.min(currentMs, totalMs))}</span>
  <input
    class="scrub"
    type="range"
    min="0"
    max={totalMs}
    step="100"
    value={Math.min(currentMs, totalMs)}
    style="--pct: {pct}%"
    oninput={onScrub}
  />
  <span class="time">{formatTs(totalMs)}</span>
  {#each tracks as t, i (t.source)}
    <audio bind:this={els[i]} src={convertFileSrc(t.path)} preload="auto"></audio>
  {/each}
</div>

<style>
  /* 播放器容器:surface 卡片,与 transcript 容器同语言 */
  .player {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    background: var(--surface);
    border-radius: var(--radius-lg);
    padding: 0.5rem 0.9rem;
    margin: 0 0 1rem;
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
  .time {
    color: var(--ink-secondary);
    font-size: 0.8rem;
    font-variant-numeric: tabular-nums;
    flex: none;
  }
  /* 进度条:沿用 DESIGN.md 进度条形态——轨 hairline、填充 accent、高 6px、rounded-full */
  .scrub {
    flex: 1;
    -webkit-appearance: none;
    appearance: none;
    height: 6px;
    border-radius: 999px;
    background: linear-gradient(
      to right,
      var(--accent) 0 var(--pct),
      var(--hairline) var(--pct) 100%
    );
    cursor: pointer;
  }
  .scrub::-webkit-slider-thumb {
    -webkit-appearance: none;
    appearance: none;
    width: 14px;
    height: 14px;
    border-radius: 50%;
    background: var(--canvas);
    border: 1px solid var(--hairline-strong);
    box-shadow: var(--shadow-btn);
  }
  audio {
    display: none;
  }
</style>
