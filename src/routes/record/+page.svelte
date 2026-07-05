<script lang="ts">
  import { onMount } from "svelte";
  import { openUrl } from "@tauri-apps/plugin-opener";
  import { recording } from "$lib/recording.svelte";
  import { speakerLabel, speakerColor } from "$lib/notes";
  import SpeakerChips from "$lib/SpeakerChips.svelte";
  import { modelsStatus, type ModelsStatus } from "$lib/models";
  import ModelDownloadCard from "$lib/ModelDownloadCard.svelte";
  import { formatTs } from "$lib/notes";

  let models = $state<ModelsStatus | null>(null);
  async function refreshModels() {
    try {
      models = await modelsStatus();
    } catch {
      /* 查询失败按就绪处理，不挡老用户 */
    }
  }
  onMount(refreshModels);

  function isError(s: string) {
    return s.startsWith("error:");
  }
  async function openScreenRecordingSettings() {
    await openUrl(
      "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
    );
  }

  async function startRecording() {
    await recording.start(); // 已在录制页，无需跳转
  }
  const levelPct = $derived.by(() => {
    if (!recording.isLive || recording.level <= 0) return 0;
    const db = 20 * Math.log10(recording.level);
    return Math.max(0, Math.min(100, ((db + 50) / 50) * 100)); // -50dBFS..0dBFS → 0..100%
  });
</script>

<div class="container">
  <h1>实时转写</h1>

  <!-- 单实例:compact 由 recording_ready 驱动。若拆成两个 if 分支,识别模型下完
       切小提示条时组件会销毁重建,进行中的下载进度/订阅状态全部清零。 -->
  {#if models && !(models.recording_ready && models.diarization_ready)}
    <ModelDownloadCard status={models} compact={models.recording_ready} onComplete={refreshModels} />
  {/if}

  {#if !models || models.recording_ready}
    <div class="controls">
      {#if !recording.isLive}
        <button class="ctl primary" disabled={recording.pending} onclick={startRecording}>
          <span class="sym dot on-blue"></span>开始录制
        </button>
      {:else}
        {#if recording.paused}
          <button class="ctl" disabled={recording.pending} onclick={() => recording.unpause()}>恢复</button>
        {:else}
          <button class="ctl" disabled={recording.pending} onclick={() => recording.pause()}>暂停</button>
        {/if}
        <button class="ctl danger" disabled={recording.pending} onclick={() => recording.stop()}>
          <span class="sym square"></span>停止
        </button>
      {/if}
      <span class="timer" class:pausedTimer={recording.paused}>{formatTs(recording.elapsedMs)}</span>
      <div class="meter" title="麦克风电平"><div class="meter-fill" style="width:{levelPct}%"></div></div>
      {#if recording.paused}<span class="paused-tag">已暂停</span>{/if}
    </div>

    <p class="status" class:error={isError(recording.status)}>
      <span class="status-dot" class:live={recording.isLive && !recording.paused}></span>{recording.status}
    </p>

    {#if recording.isLive && recording.systemAudio !== "on" && recording.systemAudio !== ""}
      <div class="banner">
        系统声音不可用（未授权屏幕录制）。仅麦克风在录。
        <button class="link" onclick={openScreenRecordingSettings}>打开系统设置</button>
        <span class="hint">授权后重新开录生效。</span>
      </div>
    {/if}

    {#if recording.isLive && recording.diarization === "unavailable"}
      <div class="banner">说话人区分不可用（声纹模型缺失）。转写与录音不受影响。</div>
    {/if}

    {#if recording.storageDegraded}
      <div class="banner">落盘异常：内容暂存内存并自动重试，请检查磁盘空间。录制不受影响。</div>
    {/if}

    <SpeakerChips speakers={recording.speakers} noteId={recording.noteId} editable={true} />

    <div class="transcript">
      {#each recording.finals as line}
        <p class="final">
          <span class="badge" style="background: {speakerColor(line.speaker, line.source)}">
            {speakerLabel(line.speaker, line.source, recording.speakers)}
          </span>
          {line.text}
        </p>
      {/each}
      {#if recording.partialMic}
        <p class="partial"><span class="badge mic">我</span>{recording.partialMic}</p>
      {/if}
      {#if recording.partialSystem}
        <p class="partial"><span class="badge system">对方</span>{recording.partialSystem}</p>
      {/if}
      {#if recording.finals.length === 0 && !recording.partialMic && !recording.partialSystem}
        <p class="hint">（开始说话…）</p>
      {/if}
    </div>
  {/if}
</div>

<style>
  .container {
    padding: 1.5rem;
  }

  h1 {
    margin: 0 0 0.25rem;
  }

  .controls {
    display: flex;
    align-items: center;
    gap: 0.75rem;
    margin: 0 0 0.75rem;
  }
  /* 录制控制条：裸 .ctl 是 button-secondary（暂停/恢复）；.primary 是开始录制的
     唯一主动作；.danger（停止）形态同 secondary，只是字色换 record，呼应
     “录制红点是唯一常驻彩色信号”。 */
  .ctl {
    display: inline-flex;
    align-items: center;
    gap: 0.45em;
    border-radius: var(--radius-md);
    border: none;
    padding: 0.45em 1.1em;
    font-weight: 500;
    font-size: 0.9rem;
    cursor: pointer;
    background: var(--canvas);
    color: var(--ink);
    box-shadow: var(--shadow-btn);
  }
  .ctl:hover { background: var(--surface-soft); }
  .ctl:disabled { opacity: 0.6; cursor: default; }
  .ctl.primary { background: var(--accent); color: var(--on-accent); }
  .ctl.primary:hover { background: var(--accent-pressed); }
  .ctl.danger { color: var(--record); font-weight: 600; }
  /* 录制符号用 CSS 图形而非 Unicode 字符(●■▶ 各平台字形/基线不一,显糙) */
  .sym {
    width: 9px;
    height: 9px;
    flex-shrink: 0;
  }
  .sym.dot { border-radius: var(--radius-full); background: var(--record); }
  .sym.dot.on-blue { background: var(--on-accent); }
  .sym.square { border-radius: 2px; background: var(--record); }
  /* 计时用等宽数字：秒数跳动时数字宽度不抖动，视觉更稳定 */
  .timer {
    font-variant-numeric: tabular-nums;
    font-weight: 600;
    font-size: 1rem;
    color: var(--ink-secondary);
  }
  .timer.pausedTimer { color: var(--ink-faint); }
  /* 电平表：细轨(5px)圆头,信息条不该比按钮抢眼 */
  .meter {
    width: 96px;
    height: 5px;
    background: var(--hairline);
    border-radius: var(--radius-full);
    overflow: hidden;
  }
  .meter-fill {
    height: 100%;
    background: var(--success);
    transition: width 0.1s linear;
  }
  .paused-tag {
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    font-size: 0.75em;
    font-weight: 600;
    border-radius: var(--radius-md);
    padding: 0.1em 0.5em;
  }

  /* 状态行降为 caption 级:辅助信息不与正文争夺注意力;状态点是唯一动态信号 */
  .status {
    display: flex;
    align-items: center;
    gap: 0.4em;
    color: var(--ink-faint);
    font-size: 0.85rem;
    margin: 0 0 1rem;
  }
  .status-dot {
    width: 7px;
    height: 7px;
    border-radius: var(--radius-full);
    background: var(--ink-faint);
  }
  .status-dot.live {
    background: var(--record);
  }

  .status.error {
    color: var(--danger);
    font-weight: 600;
  }

  /* transcript-container：surface 底、rounded-xl、正文用 transcript 字级(1.02rem/1.7) */
  .transcript {
    min-height: 8rem;
    background: var(--surface);
    border-radius: var(--radius-xl);
    padding: 20px;
    font-size: 1.02rem;
    line-height: 1.7;
  }

  .transcript p {
    margin: 0 0 6px 0;
  }

  .final {
    color: var(--ink);
  }

  .partial {
    color: var(--ink-faint);
    font-style: italic;
  }

  /* 空态居中:一大块灰底里孤零零一行左对齐文字显得没做完 */
  .transcript .hint {
    color: var(--ink-faint);
    text-align: center;
    padding: 2.6rem 0;
    margin: 0;
  }

  /* speaker-badge：粉彩底 + ink 字、rounded-sm、micro 字级；mic/system 是尚未
     解析出说话人时的占位色，固定取 tint-sky/tint-mint，与 speakerColor() 的
     兜底分支保持一致视觉。 */
  .badge {
    display: inline-block;
    min-width: 2.2em;
    text-align: center;
    font-size: 0.78rem;
    font-weight: 500;
    border-radius: var(--radius-sm);
    padding: 0.05em 0.4em;
    margin-right: 0.4em;
    color: var(--ink);
  }
  .badge.mic { background: var(--tint-sky); }
  .badge.system { background: var(--tint-mint); }

  .banner {
    background: var(--warning-tint);
    border: 1px solid var(--warning-line);
    color: var(--warning-ink);
    border-radius: var(--radius-lg);
    padding: 0.6rem 0.8rem;
    margin: 0.5rem 0 1rem;
    font-size: 0.95rem;
  }
  .banner .link {
    background: none;
    border: none;
    color: var(--accent);
    text-decoration: underline;
    cursor: pointer;
    padding: 0 0.2em;
    font-size: inherit;
  }
  .banner .hint { color: var(--warning-ink); }
</style>
